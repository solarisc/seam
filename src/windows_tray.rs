//! Independent notification-area host. Closing settings leaves this process and
//! the installed agent running; all status/network work stays off its UI loop.
use crate::{
    config::Config,
    status::{self, Snapshot},
};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use std::{fs::OpenOptions, process::Command, ptr, time::Duration};
use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

const CALLBACK: u32 = WM_APP + 19;
const UPDATE_READY: u32 = WM_APP + 20;
/// Posted by the agent when the settings window needs attention.
const SHOW_SETTINGS: u32 = WM_APP + 21;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const OPEN: usize = 1;
const TOGGLE: usize = 2;
const RECONNECT: usize = 3;
const LOGS: usize = 4;
const EXIT: usize = 5;
const CLASS: &str = "SeamTrayWindow";
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn copy_wide<const N: usize>(dest: &mut [u16; N], text: &str) {
    dest.fill(0);
    // Leave space for NUL and never cut a surrogate pair in half.
    let mut at = 0;
    for character in text.chars() {
        let mut encoded = [0; 2];
        let units = character.encode_utf16(&mut encoded);
        if at + units.len() >= N {
            break;
        }
        dest[at..at + units.len()].copy_from_slice(units);
        at += units.len();
    }
}

enum Action {
    Start,
    Pause,
    Reconnect,
    Exit,
}
struct Update {
    snapshot: Option<Snapshot>,
    notice: Option<String>,
    exit: bool,
}
struct Tray {
    icon: HICON,
    explorer_message: u32,
    commands: Sender<Action>,
    updates: Receiver<Update>,
    snapshot: Option<Snapshot>,
    busy: bool,
}
impl Tray {
    fn label(&self) -> String {
        if self.busy {
            return "Seam · Updating sharing…".into();
        }
        match &self.snapshot {
            Some(s) if s.pairing.is_some() => "Seam · Pairing request waiting".into(),
            Some(s) if s.links.len() > 1 => format!(
                "Seam · {} of {} computers connected",
                s.connection_count(),
                s.links.len()
            ),
            Some(s) => match s.links.first() {
                Some(link) if link.ready() => format!("Seam · Connected to {}", link.name),
                Some(link) if link.connected => "Seam · Finishing setup".into(),
                _ => "Seam · Waiting for computers".into(),
            },
            None => "Seam · Sharing paused".into(),
        }
    }
    fn data(&self, window: HWND) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = CALLBACK;
        data.hIcon = self.icon;
        copy_wide(&mut data.szTip, &self.label());
        data
    }
    fn add_icon(&self, window: HWND) {
        let mut data = self.data(window);
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
        }
    }
    fn notice(&self, window: HWND, message: &str) {
        let mut data = self.data(window);
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_WARNING;
        copy_wide(&mut data.szInfoTitle, "Seam");
        copy_wide(&mut data.szInfo, message);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
    fn menu(&mut self, window: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&self.label()).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            AppendMenuW(menu, MF_STRING, OPEN, wide("Open Seam").as_ptr());
            SetMenuDefaultItem(menu, OPEN as u32, 0);
            let enabled = if self.busy { MF_GRAYED } else { MF_ENABLED };
            AppendMenuW(
                menu,
                MF_STRING | enabled,
                TOGGLE,
                wide(if self.snapshot.is_some() {
                    "Pause sharing"
                } else {
                    "Start sharing"
                })
                .as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING
                    | if self.busy || self.snapshot.is_none() {
                        MF_GRAYED
                    } else {
                        MF_ENABLED
                    },
                RECONNECT,
                wide("Return control and reconnect").as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            AppendMenuW(menu, MF_STRING, LOGS, wide("Open log folder").as_ptr());
            AppendMenuW(menu, MF_STRING | enabled, EXIT, wide("Quit Seam").as_ptr());
            let mut point: POINT = std::mem::zeroed();
            GetCursorPos(&mut point);
            SetForegroundWindow(window);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                window,
                ptr::null(),
            ) as usize;
            PostMessageW(window, WM_NULL, 0, 0);
            DestroyMenu(menu);
            match selected {
                OPEN => {
                    if let Err(error) = open_settings() {
                        self.notice(window, &error.to_string());
                    }
                }
                LOGS => {
                    if let Ok(path) = crate::config::config_dir() {
                        let _ = Command::new("explorer.exe").arg(path).spawn();
                    }
                }
                TOGGLE | RECONNECT | EXIT => {
                    let action = match selected {
                        RECONNECT => Action::Reconnect,
                        EXIT => Action::Exit,
                        _ if self.snapshot.is_some() => Action::Pause,
                        _ => Action::Start,
                    };
                    if self.commands.try_send(action).is_ok() {
                        self.busy = true;
                    }
                }
                _ => {}
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut Tray;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    let tray = unsafe { &mut *pointer };
    if message == tray.explorer_message {
        tray.add_icon(window);
        return 0;
    }
    match message {
        CALLBACK => {
            match (lparam as u32) & 0xffff {
                WM_LBUTTONUP | NIN_SELECT | NIN_KEYSELECT => {
                    let _ = open_settings();
                }
                WM_CONTEXTMENU | WM_RBUTTONUP => tray.menu(window),
                _ => {}
            }
            0
        }
        SHOW_SETTINGS => {
            let _ = open_settings();
            0
        }
        UPDATE_READY => {
            let previous_label = tray.label();
            while let Ok(update) = tray.updates.try_recv() {
                tray.snapshot = update.snapshot;
                tray.busy = false;
                if let Some(notice) = update.notice {
                    tray.notice(window, &notice);
                }
                if update.exit {
                    unsafe {
                        DestroyWindow(window);
                    }
                    return 0;
                }
            }
            if tray.label() != previous_label {
                unsafe {
                    Shell_NotifyIconW(NIM_MODIFY, &tray.data(window));
                }
            }
            0
        }
        WM_CLOSE => {
            unsafe {
                DestroyWindow(window);
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &tray.data(window));
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

pub fn focus_settings() -> bool {
    let Some(window) = settings_window() else {
        return false;
    };
    unsafe {
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
    }
    true
}

/// The settings window: titled "Seam" and owned by seam.exe. The title alone
/// also matches File Explorer showing the Seam log folder.
fn settings_window() -> Option<HWND> {
    unsafe extern "system" fn visit(window: HWND, found: LPARAM) -> windows_sys::core::BOOL {
        let mut title = [0u16; 8];
        let length = unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) };
        if unsafe { IsWindowVisible(window) } == 0
            || String::from_utf16_lossy(&title[..length.max(0) as usize]) != "Seam"
        {
            return 1;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if crate::ports::process_name(pid).is_some_and(|name| name.eq_ignore_ascii_case("seam.exe"))
        {
            unsafe { *(found as *mut HWND) = window };
            return 0;
        }
        1
    }
    let mut found: HWND = ptr::null_mut();
    unsafe { EnumWindows(Some(visit), (&raw mut found) as LPARAM) };
    (!found.is_null()).then_some(found)
}

pub fn open_settings() -> Result<()> {
    if focus_settings() {
        return Ok(());
    }
    Command::new(std::env::current_exe()?)
        .spawn()
        .context("Opening Seam settings")?;
    Ok(())
}

/// Ask a running tray to open settings in its own (unelevated) session.
pub fn request_settings() -> bool {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), ptr::null());
        !window.is_null() && PostMessageW(window, SHOW_SETTINGS, 0, 0) != 0
    }
}

pub fn close() {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), ptr::null());
        if !window.is_null() {
            PostMessageW(window, WM_CLOSE, 0, 0);
        }
    }
}

pub fn ensure_running() {
    unsafe {
        if FindWindowW(wide(CLASS).as_ptr(), ptr::null()).is_null()
            && let Ok(exe) = std::env::current_exe()
        {
            let _ = Command::new(exe).arg("--tray").spawn();
        }
    }
}

// Keep routine tray polling separate from the input transport. No UI timer or
// repeated Explorer icon update is needed while connection state is unchanged.
fn spawn_status_worker(window: usize, actions: Receiver<Action>, updates: Sender<Update>) {
    std::thread::spawn(move || {
        loop {
            let (notice, exit) = match actions.recv_timeout(Duration::from_secs(3)) {
                Ok(action) => {
                    let quitting = matches!(action, Action::Exit);
                    let result = (|| -> Result<()> {
                        let config = Config::load_or_create()?;
                        let remember_paused = |paused: bool| {
                            crate::config::edit(|saved| {
                                saved.paused = paused;
                                Ok(())
                            })
                            .map(drop)
                        };
                        match action {
                            Action::Start => {
                                remember_paused(false)?;
                                crate::service::start(&config)
                            }
                            Action::Pause => {
                                crate::service::stop(&config)?;
                                remember_paused(true)
                            }
                            Action::Exit => crate::service::stop(&config),
                            Action::Reconnect => status::agent(&config).reset_input(),
                        }
                    })();
                    let exit = quitting && result.is_ok();
                    (result.err().map(|e| format!("{e:#}")), exit)
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => (None, false),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            let snapshot = Config::load_or_create()
                .ok()
                .and_then(|c| status::agent(&c).status().ok());
            if updates
                .send(Update {
                    snapshot,
                    notice,
                    exit,
                })
                .is_err()
            {
                break;
            }
            // A message wakes the UI only when a fresh snapshot is available.
            // Commands wake recv_timeout immediately, independent of polling.
            if unsafe { PostMessageW(window as HWND, UPDATE_READY, 0, 0) } == 0 || exit {
                break;
            }
        }
    });
}

pub fn run() -> Result<()> {
    std::fs::create_dir_all(crate::config::config_dir()?)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(crate::config::config_dir()?.join("tray.lock"))?;
    if file.try_lock().is_err() {
        return Ok(());
    }
    let (commands, actions) = crossbeam_channel::bounded::<Action>(1);
    let (updates, results) = crossbeam_channel::bounded(4);
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class = wide(CLASS);
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&definition) == 0 {
            bail!("Registering Seam tray: {}", std::io::Error::last_os_error());
        }
        // The executable embeds the app icon as resource 1.
        let icon = LoadImageW(
            instance,
            1 as _,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        ) as HICON;
        if icon.is_null() {
            bail!(
                "Loading Seam tray icon: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut tray = Box::new(Tray {
            icon,
            explorer_message: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            commands,
            updates: results,
            snapshot: None,
            busy: false,
        });
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("Seam background").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if window.is_null() {
            DestroyIcon(icon);
            bail!("Creating Seam tray: {}", std::io::Error::last_os_error());
        }
        SetWindowLongPtrW(window, GWLP_USERDATA, (&mut *tray as *mut Tray) as isize);
        tray.add_icon(window);
        spawn_status_worker(window as usize, actions, updates);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        DestroyIcon(icon);
        UnregisterClassW(class.as_ptr(), instance);
    }
    Ok(())
}
