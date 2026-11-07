//! A native input window captures the stationary source cursor. ShowCursor
//! alone only changes the calling thread's display counter; it cannot keep a
//! cursor owned by another application's window hidden.
use crate::input::CursorCommand;
use anyhow::{Context, Result, bail};
use crossbeam_channel::Receiver;
use std::{ptr, time::Duration};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

const WAKE: u32 = WM_APP + 41;
/// Tries left after the first when the pointer has left this computer. Each
/// can take the foreground from the app in use for a moment, so keep them few.
const HIDE_RETRIES: u32 = 2;
const HIDE_RETRY_DELAY_MS: u32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escalation {
    /// A tagged move at the same pixel, so Windows looks again at which
    /// window owns the cursor.
    Route,
    /// Take the foreground.
    Activate,
}

/// How far hiding the cursor has gone since it was last shown.
#[derive(Default)]
struct Attempts {
    routed: bool,
    activated: bool,
    /// Windows granted the foreground, as opposed to refusing it.
    leased: bool,
    routed_after_lease: bool,
}

impl Attempts {
    /// What to try next while the cursor is still showing, if anything is
    /// left. Taking the foreground doesn't move the cursor itself: without a
    /// move afterwards, Windows can keep it with the previous owner.
    fn next(&mut self) -> Option<Escalation> {
        if !self.routed {
            self.routed = true;
            Some(Escalation::Route)
        } else if !self.activated {
            self.activated = true;
            Some(Escalation::Activate)
        } else if self.leased && !self.routed_after_lease {
            self.routed_after_lease = true;
            Some(Escalation::Route)
        } else {
            None
        }
    }
}

/// The program that owns `window`, by file name only: logs are shared in bug
/// reports, and a full path can name the user.
fn program_of(window: windows_sys::Win32::Foundation::HWND) -> Option<String> {
    let mut process_id = 0u32;
    if window.is_null() || unsafe { GetWindowThreadProcessId(window, &mut process_id) } == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    let read = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut length)
    };
    unsafe { CloseHandle(process) };
    if read == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..length as usize]);
    Some(path.rsplit('\\').next().unwrap_or_default().to_string())
}

pub(crate) fn wake(thread: u32) -> Result<()> {
    if thread == 0 || unsafe { PostThreadMessageW(thread, WAKE, 0, 0) } == 0 {
        bail!(
            "cursor message loop unavailable: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub(crate) fn start(commands: Receiver<CursorCommand>) -> Result<u32> {
    let (ready, wait) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let cursor = match CursorWindow::new() {
            Ok(cursor) => cursor,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let mut message = unsafe { std::mem::zeroed::<MSG>() };
        // Ensure the thread queue exists before publishing its wakeup handle.
        unsafe {
            PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
        }
        if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_err() {
            return;
        }
        cursor.run(commands);
    });
    wait.recv_timeout(Duration::from_secs(2))
        .context("cursor window startup timed out")?
}

unsafe extern "system" fn window_proc(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match message {
        WM_SETCURSOR => {
            unsafe {
                SetCursor(ptr::null_mut());
            }
            1
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

struct CursorWindow {
    window: windows_sys::Win32::Foundation::HWND,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    class: Vec<u16>,
    hidden: bool,
    attempts: Attempts,
    hide_point: (f64, f64),
    retries_left: u32,
    tries: u32,
    decrements: usize,
    previous_foreground: windows_sys::Win32::Foundation::HWND,
    keyboard_paused: bool,
}

impl CursorWindow {
    fn new() -> Result<Self> {
        let class: Vec<u16> = format!("SeamCursorOwner-{}\0", unsafe { GetCurrentThreadId() })
            .encode_utf16()
            .collect();
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        if unsafe { RegisterClassW(&definition) } == 0 {
            bail!(
                "register cursor window: {}",
                std::io::Error::last_os_error()
            );
        }
        let window = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if window.is_null() {
            unsafe {
                UnregisterClassW(class.as_ptr(), instance);
            }
            bail!("create cursor window: {}", std::io::Error::last_os_error());
        }
        Ok(Self {
            window,
            instance,
            class,
            hidden: false,
            attempts: Attempts::default(),
            hide_point: (0.0, 0.0),
            retries_left: 0,
            tries: 1,
            decrements: 0,
            previous_foreground: ptr::null_mut(),
            keyboard_paused: false,
        })
    }

    fn hide(&mut self, point: (f64, f64)) -> Result<()> {
        self.attempts.routed = false;
        self.attempts.routed_after_lease = false;
        // Cover only the stationary hotspot, without activating a window or
        // changing the user's system cursor theme. No cursor warp is needed.
        if unsafe {
            SetWindowPos(
                self.window,
                HWND_TOPMOST,
                point.0 as i32,
                point.1 as i32,
                1,
                1,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        } == 0
        {
            bail!("position cursor owner: {}", std::io::Error::last_os_error());
        }
        if !self.hidden {
            unsafe {
                SetCapture(self.window);
            }
            if unsafe { GetCapture() } != self.window {
                self.show();
                bail!("cannot acquire cursor capture");
            }
            unsafe {
                SetCursor(ptr::null_mut());
            }
            // Balance only our own decrements on return. Never reset another
            // application's count or replace desktop-wide system cursors.
            self.hide_display_count()?;
            self.hidden = true;
        }
        unsafe {
            SetCursor(ptr::null_mut());
        }
        Ok(())
    }

    fn hide_display_count(&mut self) -> Result<()> {
        loop {
            let count = unsafe { ShowCursor(0) };
            self.decrements += 1;
            if count < 0 {
                return Ok(());
            }
            if self.decrements >= 64 {
                bail!("cursor display count did not become hidden");
            }
        }
    }

    fn acquire_foreground(&mut self) -> Result<()> {
        // A foreground queue can retain the desktop cursor even after
        // background capture. Do not attach input queues: that resets keyboard
        // state. Activation is a lease, restored before returning local input.
        self.previous_foreground = unsafe { GetForegroundWindow() };
        // With raw keyboard reports on, Windows would stop calling the
        // keyboard hook for the whole lease: no key would reach the other
        // computer and no shortcut would bring the pointer back.
        crate::windows_raw::keyboard_reports(false)?;
        self.keyboard_paused = true;
        if unsafe { SetForegroundWindow(self.window) } == 0 {
            bail!("Windows denied cursor ownership");
        }
        self.attempts.leased = true;
        unsafe {
            SetCapture(self.window);
            SetCursor(ptr::null_mut());
        }
        self.hide_display_count()?;
        Ok(())
    }

    fn verify_hidden(&self) -> Result<()> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        if !self.hidden
            || unsafe { GetCursorInfo(&mut info) } == 0
            || (info.flags & CURSOR_SHOWING != 0 && !info.hCursor.is_null())
        {
            bail!(
                "desktop cursor remained visible after acquiring capture (foreground_owner={}, capture_owner={}, flags={}, null_shape={}, decrements={})",
                unsafe { GetForegroundWindow() } == self.window,
                unsafe { GetCapture() } == self.window,
                info.flags,
                info.hCursor.is_null(),
                self.decrements,
            );
        }
        Ok(())
    }

    /// A tagged move at the SAME pixel. Suppressing the seam event can leave
    /// the shell taskbar owning cursor routing; this lets Windows hit-test our
    /// owner window without changing pointer position or emitting motion to
    /// the peer. Do this after the source hook has returned, never while it
    /// waits for the initial Hide acknowledgement.
    fn route(&self) -> Result<()> {
        let (x, y) = crate::windows_raw::cursor_position()
            .context("cursor position unavailable while acquiring ownership")?;
        crate::windows_input::simulate(&rdev::EventType::MouseMove { x, y })
    }

    fn queue_verification(&self, generation: usize) -> Result<usize> {
        // WM_TIMER follows activation/input messages. A posted thread message
        // can overtake them and inspect the old foreground cursor routing.
        self.queue_timer(generation, USER_TIMER_MINIMUM)
    }

    fn queue_timer(&self, generation: usize, delay_ms: u32) -> Result<usize> {
        let timer = unsafe { SetTimer(self.window, generation, delay_ms, None) };
        if timer == 0 {
            bail!(
                "queue cursor verification: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(timer)
    }

    fn show(&mut self) {
        for _ in 0..self.decrements {
            unsafe {
                ShowCursor(1);
            }
        }
        if unsafe { GetCapture() } == self.window {
            unsafe {
                // The app that supplied the old HCURSOR may have closed while
                // we owned input. A borrowed cursor handle is not a lifetime
                // guarantee. Restore a shared system arrow; the window under
                // the next mouse event chooses its own shape normally.
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW));
                ReleaseCapture();
            }
        }
        self.decrements = 0;
        self.hidden = false;
        self.attempts = Attempts::default();
        if !self.previous_foreground.is_null()
            && unsafe { IsWindow(self.previous_foreground) } != 0
            && unsafe { GetForegroundWindow() } == self.window
        {
            unsafe {
                SetForegroundWindow(self.previous_foreground);
            }
        }
        self.previous_foreground = ptr::null_mut();
        unsafe {
            ShowWindow(self.window, SW_HIDE);
        }
        // Hidden, the window has given up the foreground even when the
        // previous one could not take it back.
        if std::mem::take(&mut self.keyboard_paused)
            && let Err(error) = crate::windows_raw::keyboard_reports(true)
        {
            log::error!("{error:#}");
        }
    }

    fn run(mut self, commands: Receiver<CursorCommand>) {
        let mut generation = 0usize;
        let mut pending = None;
        let mut source_timer = 0usize;
        // The running timer starts hiding over, rather than checking on it.
        let mut retrying = false;
        let mut message = unsafe { std::mem::zeroed::<MSG>() };
        loop {
            if unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } <= 0 {
                break;
            }
            if message.message == WAKE {
                for command in commands.try_iter() {
                    if matches!(
                        command,
                        CursorCommand::Hide(..)
                            | CursorCommand::Show(..)
                            | CursorCommand::RestoreSource(..)
                    ) {
                        // A restore or newer acquisition retires any queued
                        // visibility check before it can affect a new owner.
                        generation = generation.wrapping_add(1).max(1);
                        pending = None;
                        retrying = false;
                        if source_timer != 0 {
                            unsafe { KillTimer(self.window, source_timer) };
                            source_timer = 0;
                        }
                    }
                    match command {
                        CursorCommand::Hide(point, idle_receiver, done) => {
                            self.hide_point = point;
                            // An idle receiver is waiting for one answer.
                            self.retries_left = if idle_receiver { 0 } else { HIDE_RETRIES };
                            self.tries = 1;
                            let result = self.hide(point);
                            match result {
                                Ok(()) if !idle_receiver => {
                                    // A source hook is awaiting this result.
                                    // Verify once after it returns; suppressing
                                    // the seam event can leave the old foreground
                                    // queue owning the desktop cursor. This timer
                                    // runs only at acquisition, never per motion.
                                    match self.queue_verification(generation) {
                                        Ok(timer) => source_timer = timer,
                                        Err(error) => {
                                            self.show();
                                            if let Some(done) = done {
                                                let _ = done.send(Err(error));
                                            }
                                            continue;
                                        }
                                    }
                                    if let Some(done) = done {
                                        let _ = done.send(Ok(()));
                                    }
                                }
                                Ok(()) => {
                                    // Let native activation/cursor messages run
                                    // before acknowledging an idle receiver.
                                    pending = done;
                                    match self.queue_verification(generation) {
                                        Ok(timer) => source_timer = timer,
                                        Err(error) => {
                                            self.show();
                                            if let Some(done) = pending.take() {
                                                let _ = done.send(Err(error));
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    self.show();
                                    if let Some(done) = done {
                                        let _ = done.send(Err(error));
                                    } else {
                                        log::error!("cursor hide: {error:#}");
                                    }
                                }
                            }
                        }
                        CursorCommand::Show(done) => {
                            self.show();
                            if let Some(done) = done {
                                let _ = done.send(());
                            }
                        }
                        CursorCommand::RestoreSource((x, y)) => {
                            self.show();
                            // Held keys are not pressed again here; see
                            // windows_capture::release_local_keys.
                            if let Err(error) =
                                crate::windows_input::simulate(&rdev::EventType::MouseMove { x, y })
                            {
                                log::error!("cursor restore: {error:#}");
                            }
                        }
                        CursorCommand::ReleaseKeys(keys) => {
                            crate::windows_capture::release_local_keys(&keys);
                        }
                        CursorCommand::Barrier(done) => {
                            let _ = done.send(());
                        }
                        CursorCommand::Quit => return,
                    }
                }
            } else if message.message == WM_TIMER && message.hwnd == self.window {
                if message.wParam != generation {
                    continue;
                }
                if source_timer == 0 {
                    continue;
                }
                unsafe { KillTimer(self.window, source_timer) };
                source_timer = 0;
                if std::mem::take(&mut retrying) {
                    let restarted = self
                        .hide(self.hide_point)
                        .and_then(|()| self.queue_verification(generation));
                    match restarted {
                        Ok(timer) => source_timer = timer,
                        Err(error) => {
                            self.show();
                            log::error!("cursor hide: {error:#}");
                        }
                    }
                    continue;
                }
                let mut result = self.verify_hidden();
                let mut escalated = None;
                while result.is_err() && escalated.is_none() {
                    // Background capture does not always change cursor routing.
                    // Lease the foreground only when native verification shows
                    // it is necessary, and let activation messages settle.
                    let Some(step) = self.attempts.next() else {
                        break;
                    };
                    let attempt = match step {
                        Escalation::Route => self.route(),
                        Escalation::Activate => self.acquire_foreground(),
                    }
                    .and_then(|()| self.queue_verification(generation));
                    match attempt {
                        Ok(timer) => escalated = Some(timer),
                        Err(error) => result = Err(error),
                    }
                }
                if let Some(timer) = escalated {
                    source_timer = timer;
                    continue;
                }
                let tries = self.tries;
                if result.is_ok() && (tries > 1 || self.attempts.routed_after_lease) {
                    // Rare, and shows which of the later steps earn their keep.
                    log::info!(
                        "cursor hidden on try {tries} (move after foreground: {})",
                        self.attempts.routed_after_lease
                    );
                }
                let mut foreground = None;
                if result.is_err() {
                    foreground = program_of(if self.previous_foreground.is_null() {
                        unsafe { GetForegroundWindow() }
                    } else {
                        self.previous_foreground
                    });
                    self.show();
                    // A refusal can pass: Windows may let Seam have the
                    // foreground once the app in front settles. Otherwise this
                    // computer's arrow stays on screen until the pointer returns.
                    if pending.is_none() && self.retries_left > 0 {
                        self.retries_left -= 1;
                        self.tries += 1;
                        if let Ok(timer) = self.queue_timer(generation, HIDE_RETRY_DELAY_MS) {
                            source_timer = timer;
                            retrying = true;
                            continue;
                        }
                    }
                }
                if let Some(done) = pending.take() {
                    let _ = done.send(result);
                } else if let Err(error) = result {
                    log::error!(
                        "cursor hide: {error:#} (after {tries} tries; foreground program: {})",
                        foreground.as_deref().unwrap_or("unknown")
                    );
                }
            } else {
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
}

impl Drop for CursorWindow {
    fn drop(&mut self) {
        self.show();
        unsafe {
            DestroyWindow(self.window);
            UnregisterClassW(self.class.as_ptr(), self.instance);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_the_cursor_again_once_windows_grants_the_foreground() {
        let mut attempts = Attempts::default();
        assert_eq!(attempts.next(), Some(Escalation::Route));
        assert_eq!(attempts.next(), Some(Escalation::Activate));
        attempts.leased = true;
        assert_eq!(attempts.next(), Some(Escalation::Route));
        assert!(attempts.routed_after_lease);
        assert_eq!(attempts.next(), None);
    }

    #[test]
    fn a_refused_foreground_ends_this_try_without_another_move() {
        let mut attempts = Attempts::default();
        assert_eq!(attempts.next(), Some(Escalation::Route));
        assert_eq!(attempts.next(), Some(Escalation::Activate));
        assert_eq!(attempts.next(), None);
    }

    #[test]
    fn no_window_has_no_program() {
        assert_eq!(program_of(ptr::null_mut()), None);
    }
}
