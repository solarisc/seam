//! Notice when the foreground app has taken the mouse, as games do for
//! mouse-look. Crossing to another computer then would pull the pointer out
//! from under the game, so edge crossing waits until it lets go, for example
//! after Alt+Tab. A controlled computer reports the same state, so its
//! controller sends raw motion and holds the edge while the game plays.
//!
//! Ordinary apps hold the cursor too, in ways that never mean a game: Windows
//! confines it while a window is moved or resized, menus and popups capture
//! it, an app keeps a drag's capture until it handles the button's release,
//! and video players hide a resting cursor. None of these holds the edge.
//! Neither do Windows' own screens (sign-in, lock, Ctrl+Alt+Del, UAC), which
//! hide the cursor and fill the screen like a game but never let go of it.

use std::{
    cell::Cell,
    collections::VecDeque,
    sync::OnceLock,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow},
    System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::WindowsAndMessaging::*,
};

/// While a lock holds, re-check at this interval instead of on every report
/// from a high-rate gaming mouse pushed against the edge. A check takes about
/// 6 µs.
const RECHECK: Duration = Duration::from_millis(25);

/// Some games let go of the cursor for a moment now and then, and a check can
/// land in that moment. Only a release that lasts this long counts: six frames
/// at 60 fps. FraudCrew, sampled every millisecond through 68 s of play, never
/// let go at all until its pause menu opened.
const RELEASE_DELAY: Duration = Duration::from_millis(100);

/// An app lets go of a drag's capture only once it handles the button's
/// release, which can trail the release Seam sees.
const SETTLE: Duration = Duration::from_millis(250);

/// A pause this long between motion reports means the pointer rested.
const REST: Duration = Duration::from_millis(100);

/// Apps that hide a resting cursor show it again within this long of it
/// moving. A browser page took 17 ms. A game that only hides the cursor is
/// recognized this long after the pointer starts moving.
const SHOW_DELAY: Duration = Duration::from_millis(50);

/// An app steering the cursor away from where Seam put it moved it by at
/// least this much; smaller differences are Windows still applying a move.
const WARP_DISTANCE: f64 = 16.0;

/// Windows' own full-screen screens: sign-in and lock, Ctrl+Alt+Del, UAC
/// prompts. No game runs there. The sign-in screen hides the cursor and fills
/// the screen, and held as a game it kept a controller's keyboard and mouse
/// on a computer nobody was signed in to, with no way back by the edge.
const WINDOWS_SCREENS: [&str; 4] = [
    "LogonUI.exe",
    "LockApp.exe",
    "consent.exe",
    "CredentialUIBroker.exe",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lock {
    /// The cursor is confined to part of the desktop (`ClipCursor`).
    Confined,
    /// The app holds mouse capture with no button down, as Blender does while
    /// moving an object or flying through a scene: the cursor wraps around.
    Captured,
    /// A full-screen app hides the cursor while it reads the mouse itself.
    HiddenFullScreen,
    /// On a controlled computer, the app moved the cursor away from where
    /// Seam put it during a drag or while hiding it.
    Warping,
}

/// What one look at the foreground app found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Locked(Lock),
    Free,
    /// Windows itself holds the cursor: a window is being moved or resized,
    /// a menu is open, or one of Windows' own screens is in front.
    System,
    /// The signs fit a drag that is ending, or a cursor hidden while it
    /// rested, as well as a game: they neither start nor end a lock.
    Unsure,
}

/// What Windows reports about the foreground app and the cursor.
#[derive(Clone, Copy, Debug, Default)]
struct Scene {
    /// The foreground thread is moving or sizing a window, or tracking a menu.
    system: bool,
    /// One of `WINDOWS_SCREENS` is in front.
    windows_screen: bool,
    /// The cursor clip leaves part of the desktop out of reach.
    confined: bool,
    /// The desktop, the taskbar or Seam itself is in front.
    nobody: bool,
    /// The foreground window or a window inside it holds mouse capture.
    captured: bool,
    hidden: bool,
    /// The foreground window fills its monitor. Read only while the cursor
    /// is hidden.
    full_screen: bool,
}

/// What Seam knows about the mouse that the cursor's state does not show.
#[derive(Clone, Copy, Debug, Default)]
struct Mouse {
    /// Apps capture and clip the cursor for drags.
    buttons_held: bool,
    /// A button came up less than `SETTLE` ago.
    settling: bool,
    /// The pointer has moved for `SHOW_DELAY` without resting.
    moving: bool,
}

fn judge(scene: Scene, mouse: Mouse) -> Check {
    if scene.system || scene.windows_screen {
        return Check::System;
    }
    // A game hides the cursor it holds. A visible one held during a drag, or
    // just after one, is the drag's.
    let held = |lock| {
        if scene.hidden || !(mouse.buttons_held || mouse.settling) {
            Check::Locked(lock)
        } else {
            Check::Unsure
        }
    };
    // With the desktop or taskbar in front no game has the mouse, even if one
    // left the cursor confined when it lost the foreground.
    if scene.nobody {
        return Check::Free;
    }
    if scene.confined {
        return held(Lock::Confined);
    }
    // Apps capture the mouse for every drag; only without a button is it a grab.
    if scene.captured && !mouse.buttons_held {
        return held(Lock::Captured);
    }
    // A full-screen game on a single display can confine the cursor to the
    // whole desktop, which looks unconfined. It still hides the cursor.
    if !scene.hidden || !scene.full_screen {
        return Check::Free;
    }
    // Video players hide a resting cursor, and show it again once it moves.
    if mouse.moving {
        Check::Locked(Lock::HiddenFullScreen)
    } else {
        Check::Unsure
    }
}

/// When the mouse last did what `Mouse` describes.
#[derive(Default)]
struct History {
    released: Option<Instant>,
    moved: Option<Instant>,
    moving_since: Option<Instant>,
}

impl History {
    fn released(&mut self, now: Instant) {
        self.released = Some(now);
    }

    fn moved(&mut self, now: Instant) {
        if self
            .moved
            .is_none_or(|moved| now.duration_since(moved) >= REST)
        {
            self.moving_since = Some(now);
        }
        self.moved = Some(now);
    }

    /// When an app that hid the resting cursor has shown it again.
    fn shown_by(&self) -> Option<Instant> {
        self.moving_since.map(|since| since + SHOW_DELAY)
    }

    fn mouse(&self, buttons_held: bool, now: Instant) -> Mouse {
        let within =
            |at: Option<Instant>, limit| at.is_some_and(|at| now.duration_since(at) < limit);
        Mouse {
            buttons_held,
            settling: within(self.released, SETTLE),
            moving: within(self.moved, REST)
                && self
                    .moving_since
                    .is_some_and(|since| now.duration_since(since) >= SHOW_DELAY),
        }
    }
}

/// A lock takes effect at once and ends after `RELEASE_DELAY` without it, or
/// at once when Windows takes the cursor or another app comes to the front.
#[derive(Default)]
struct Debounce {
    locked: bool,
    released_since: Option<Instant>,
    /// The process whose app took the mouse.
    process: u32,
}

impl Debounce {
    /// `process` is the foreground app's, 0 when no window is in front.
    fn update(&mut self, check: Check, process: u32, now: Instant) -> bool {
        match check {
            Check::Locked(_) => {
                self.locked = true;
                self.released_since = None;
                self.process = process;
            }
            Check::Unsure => {}
            // Moving a window, a menu, or Alt+Tab to another app: no game is
            // about to take the mouse back.
            Check::System => self.release(),
            Check::Free if process != 0 && process != self.process => self.release(),
            Check::Free if self.locked => {
                let since = *self.released_since.get_or_insert(now);
                if now.duration_since(since) >= RELEASE_DELAY {
                    self.release();
                }
            }
            Check::Free => {}
        }
        self.locked
    }

    fn release(&mut self) {
        self.locked = false;
        self.released_since = None;
    }
}

pub(crate) struct PointerLock {
    probe: fn() -> (Scene, u32),
    held_until: Option<Instant>,
    debounce: Debounce,
    history: History,
}

impl Default for PointerLock {
    fn default() -> Self {
        Self {
            // Unit tests must not depend on the desktop of the computer running them.
            probe: if cfg!(test) {
                || (Scene::default(), 0)
            } else {
                look
            },
            held_until: None,
            debounce: Debounce::default(),
            history: History::default(),
        }
    }
}

impl PointerLock {
    /// Called only when the pointer is about to cross a seam.
    pub(crate) fn blocks_crossing(&mut self) -> bool {
        self.blocks_crossing_at(Instant::now())
    }

    /// Called for every local motion report.
    pub(crate) fn moved(&mut self) {
        self.history.moved(Instant::now());
    }

    pub(crate) fn button_released(&mut self) {
        self.history.released(Instant::now());
    }

    fn blocks_crossing_at(&mut self, now: Instant) -> bool {
        if self.held_until.is_some_and(|until| now < until) {
            return true;
        }
        let (scene, process) = (self.probe)();
        // No button is down when the pointer may cross.
        let check = judge(scene, self.history.mouse(false, now));
        let was_locked = self.debounce.locked;
        let locked = self.debounce.update(check, process, now);
        self.held_until = locked.then_some(now + RECHECK);
        match check {
            Check::Locked(lock) if !was_locked => log::info!(
                "edge crossing paused while {} has the mouse ({lock:?})",
                foreground_program()
            ),
            _ if was_locked && !locked => log::info!("edge crossing resumed"),
            _ => {}
        }
        locked
    }
}

/// A controlled computer's view: whether an app here steers the mouse itself,
/// so its controller should send raw motion instead of positions.
#[derive(Default)]
pub(crate) struct DestinationLock {
    debounce: Debounce,
    history: History,
    locked: bool,
    next_check: Option<Instant>,
    /// The last few positions Seam placed the cursor at, newest last.
    placed: VecDeque<(f64, f64)>,
    warping: bool,
}

impl DestinationLock {
    pub(crate) fn locked(&self) -> bool {
        self.locked
    }

    /// Before placing the cursor again: an app that moved it since is
    /// steering it, e.g. Blender wrapping it at the window edge.
    pub(crate) fn notice_warp(&mut self) {
        if let Some(cursor) = crate::windows_raw::cursor_position()
            && is_warp(cursor, &self.placed)
        {
            self.warping = true;
            self.next_check = None;
        }
    }

    pub(crate) fn placed_at(&mut self, x: f64, y: f64) {
        if self.placed.len() == 3 {
            self.placed.pop_front();
        }
        self.placed.push_back((x, y));
    }

    /// Relative motion moves the cursor somewhere Seam cannot predict.
    pub(crate) fn moved_relatively(&mut self) {
        self.placed.clear();
    }

    pub(crate) fn button_released(&mut self) {
        self.history.released(Instant::now());
    }

    /// Called for every motion from the controller, before applying it, and
    /// checks at most every `RECHECK`. Returns the new state when it changes.
    pub(crate) fn update(&mut self, buttons_held: bool) -> Option<bool> {
        // The sign-in helper only ever shows Windows' sign-in screen: whatever
        // happens to the cursor there, no app has taken the mouse.
        static SIGN_IN_HELPER: OnceLock<bool> = OnceLock::new();
        if *SIGN_IN_HELPER.get_or_init(crate::windows_signin::is_helper) {
            return None;
        }
        let now = Instant::now();
        self.history.moved(now);
        if self.next_check.is_some_and(|at| now < at) {
            return None;
        }
        let (scene, process) = look();
        let mouse = self.history.mouse(buttons_held, now);
        // Steering lasts for the drag, or while the app hides or captures the
        // cursor. Windows moving a window or showing a menu steers nothing.
        self.warping &= !scene.system && (buttons_held || scene.hidden || scene.captured);
        let check = match judge(scene, mouse) {
            Check::Free | Check::Unsure if self.warping => Check::Locked(Lock::Warping),
            check => check,
        };
        // A cursor hidden while it rested is judged again as soon as the app
        // has had time to show it, so a game that hides it is held quickly.
        self.next_check = match self.history.shown_by() {
            Some(shown) if check == Check::Unsure && !mouse.moving => Some(shown),
            _ => Some(now + RECHECK),
        };
        let locked = self.debounce.update(check, process, now);
        if locked == self.locked {
            return None;
        }
        self.locked = locked;
        self.placed.clear();
        match check {
            Check::Locked(lock) => log::info!("{} took the mouse ({lock:?})", foreground_program()),
            _ => log::info!("the app here released the mouse"),
        }
        Some(locked)
    }
}

/// The cursor is far from every recent placement: something else moved it.
fn is_warp(cursor: (f64, f64), placed: &VecDeque<(f64, f64)>) -> bool {
    let Some(&latest) = placed.back() else {
        return false;
    };
    let near = |(x, y): (f64, f64), slack: f64| {
        (cursor.0 - x.round()).abs() <= slack && (cursor.1 - y.round()).abs() <= slack
    };
    !placed.iter().any(|&p| near(p, 2.0)) && !near(latest, WARP_DISTANCE)
}

/// Read the scene, and the process of the foreground app.
fn look() -> (Scene, u32) {
    let window = unsafe { GetForegroundWindow() };
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(thread, &mut info) } == 0 {
        info = GUITHREADINFO::default();
    }
    // With no mouse attached Windows hides the cursor itself (Seam's Mouse
    // Keys lease shows it unless that setting is off): then a hidden cursor
    // says nothing about the app, and every full-screen window looked like a
    // game.
    let hidden = cursor_hidden() && unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0;
    let windows_screen = !window.is_null() && is_windows_screen_process(process);
    // Seam's own cursor owner hides the cursor during remote control.
    let nobody = window.is_null()
        || is_shell(window)
        || process == unsafe { GetCurrentProcessId() }
        || windows_screen;
    let scene = Scene {
        system: info.flags & (GUI_INMOVESIZE | GUI_INMENUMODE) != 0,
        windows_screen,
        confined: cursor_confined(),
        nobody,
        // Menus and popups, such as a browser's bar for selected text,
        // capture the mouse in a window of their own.
        captured: !nobody
            && !info.hwndCapture.is_null()
            && unsafe { GetAncestor(info.hwndCapture, GA_ROOT) } == window,
        hidden,
        full_screen: hidden && !nobody && fills_monitor(window),
    };
    (scene, process)
}

fn cursor_confined() -> bool {
    let desktop = unsafe {
        RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    };
    let mut clip = empty_rect();
    let read = unsafe { GetClipCursor(&mut clip) } != 0;
    read && confines(&clip, &desktop)
}

fn fills_monitor(window: HWND) -> bool {
    let mut bounds = empty_rect();
    if unsafe { GetWindowRect(window, &mut bounds) } == 0 {
        return false;
    }
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONULL) };
    if monitor.is_null() {
        return false;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let read = unsafe { GetMonitorInfoW(monitor, &mut info) } != 0;
    read && covers(&bounds, &info.rcMonitor)
}

fn empty_rect() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }
}

/// The clip leaves part of the desktop out of reach. An unconfined cursor
/// reports the whole virtual desktop.
fn confines(clip: &RECT, desktop: &RECT) -> bool {
    clip.left > desktop.left
        || clip.top > desktop.top
        || clip.right < desktop.right
        || clip.bottom < desktop.bottom
}

/// The window fills its monitor. One pixel of slack absorbs rounding.
fn covers(window: &RECT, monitor: &RECT) -> bool {
    window.left <= monitor.left + 1
        && window.top <= monitor.top + 1
        && window.right >= monitor.right - 1
        && window.bottom >= monitor.bottom - 1
}

/// The same test Seam uses to verify its own hidden cursor. Touch and pen
/// input suppress the cursor too, but mouse motion brings it back.
fn cursor_hidden() -> bool {
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { GetCursorInfo(&mut info) } == 0 || info.flags & CURSOR_SUPPRESSED != 0 {
        return false;
    }
    info.flags & CURSOR_SHOWING == 0 || info.hCursor.is_null()
}

/// The desktop and taskbar never own the mouse.
fn is_shell(window: HWND) -> bool {
    if window == unsafe { GetShellWindow() } {
        return true;
    }
    let mut class = [0u16; 32];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    let class = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    matches!(
        class.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

fn is_windows_screen(program: &str) -> bool {
    WINDOWS_SCREENS
        .iter()
        .any(|screen| screen.eq_ignore_ascii_case(program))
}

/// Whether `process` is one of Windows' own screens. Every look asks, and
/// naming a process opens it, so the answer for the last one is kept.
fn is_windows_screen_process(process: u32) -> bool {
    thread_local! {
        static LAST: Cell<(u32, bool)> = const { Cell::new((0, false)) };
    }
    LAST.with(|last| {
        let (known, screen) = last.get();
        if known == process {
            return screen;
        }
        let screen = program_name(process).is_some_and(|name| is_windows_screen(&name));
        last.set((process, screen));
        screen
    })
}

/// A process's file name, such as `LogonUI.exe`.
fn program_name(process: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process) };
    if handle.is_null() {
        return None;
    }
    let mut path = [0u16; 512];
    let mut length = path.len() as u32;
    let read = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut length)
    };
    unsafe { CloseHandle(handle) };
    if read == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..length as usize]);
    Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
}

/// For the log only: the foreground program's file name.
fn foreground_program() -> String {
    let window = unsafe { GetForegroundWindow() };
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(window, &mut process) };
    program_name(process).unwrap_or_else(|| format!("process {process}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    const GAME: u32 = 7;
    const OTHER_APP: u32 = 8;
    const LOCKED: Check = Check::Locked(Lock::Confined);

    #[test]
    fn a_lock_holds_briefly_then_is_checked_again() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static HOLDS: AtomicBool = AtomicBool::new(true);
        let mut lock = PointerLock {
            probe: || {
                let confined = HOLDS.load(Ordering::Relaxed);
                (
                    Scene {
                        confined,
                        hidden: confined,
                        ..Scene::default()
                    },
                    GAME,
                )
            },
            ..PointerLock::default()
        };
        let start = Instant::now();
        assert!(lock.blocks_crossing_at(start));
        // The game lets go, and crossing resumes once that lasts.
        HOLDS.store(false, Ordering::Relaxed);
        assert!(lock.blocks_crossing_at(start + RECHECK / 2));
        assert!(lock.blocks_crossing_at(start + RECHECK));
        assert!(lock.blocks_crossing_at(start + RECHECK + RELEASE_DELAY / 2));
        assert!(!lock.blocks_crossing_at(start + RECHECK + RELEASE_DELAY));
        assert!(!lock.blocks_crossing_at(start + RECHECK * 2 + RELEASE_DELAY));
        assert!(!PointerLock::default().blocks_crossing());
    }

    #[test]
    fn a_momentary_release_does_not_end_the_lock() {
        let start = Instant::now();
        let half = RELEASE_DELAY / 2;
        let mut debounce = Debounce::default();
        assert!(!debounce.update(Check::Free, GAME, start));
        assert!(debounce.update(LOCKED, GAME, start));
        assert!(debounce.update(Check::Free, GAME, start + half / 2));
        // The game takes the cursor back: the release starts over.
        assert!(debounce.update(LOCKED, GAME, start + half));
        assert!(debounce.update(Check::Free, GAME, start + RELEASE_DELAY));
        assert!(debounce.update(Check::Free, GAME, start + RELEASE_DELAY + half));
        assert!(!debounce.update(Check::Free, GAME, start + RELEASE_DELAY * 2));
        assert!(!debounce.update(Check::Free, GAME, start + RELEASE_DELAY * 3));
    }

    #[test]
    fn another_app_in_front_or_windows_holding_the_cursor_ends_the_lock_at_once() {
        let start = Instant::now();
        let mut debounce = Debounce::default();
        assert!(debounce.update(LOCKED, GAME, start));
        // Alt+Tab to another app.
        assert!(!debounce.update(Check::Free, OTHER_APP, start));
        assert!(debounce.update(LOCKED, GAME, start));
        // Dragging the game's window, or its system menu.
        assert!(!debounce.update(Check::System, GAME, start));
        // A moment without a foreground window is not another app.
        assert!(debounce.update(LOCKED, GAME, start));
        assert!(debounce.update(Check::Free, 0, start));
    }

    #[test]
    fn unsure_checks_neither_start_nor_end_a_lock() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut debounce = Debounce::default();
        assert!(!debounce.update(Check::Unsure, GAME, at(0)));
        assert!(debounce.update(LOCKED, GAME, at(0)));
        assert!(debounce.update(Check::Unsure, OTHER_APP, at(100)));
        assert!(debounce.update(Check::Unsure, GAME, at(2000)));
        assert!(debounce.update(Check::Free, GAME, at(2000)));
        assert!(!debounce.update(Check::Free, GAME, at(2500)));
    }

    #[test]
    fn moving_a_window_or_opening_a_menu_never_holds_the_mouse() {
        // Windows confines the cursor to the work area while a window moves
        // or resizes, and menus capture the mouse with no button down.
        let scene = Scene {
            system: true,
            confined: true,
            captured: true,
            ..Scene::default()
        };
        assert_eq!(judge(scene, Mouse::default()), Check::System);
        let hidden = Scene {
            hidden: true,
            full_screen: true,
            ..scene
        };
        assert_eq!(judge(hidden, Mouse::default()), Check::System);
    }

    #[test]
    fn windows_own_screens_never_hold_the_mouse() {
        // The sign-in screen as Seam saw it: full screen, the cursor hidden
        // and moving. A game that looked like that would be held.
        let sign_in = Scene {
            windows_screen: true,
            hidden: true,
            full_screen: true,
            ..Scene::default()
        };
        let moving = Mouse {
            moving: true,
            ..Mouse::default()
        };
        assert_eq!(
            judge(
                Scene {
                    windows_screen: false,
                    ..sign_in
                },
                moving
            ),
            Check::Locked(Lock::HiddenFullScreen)
        );
        assert_eq!(judge(sign_in, moving), Check::System);
        // Nor when it confines or captures the cursor.
        let grabbing = Scene {
            confined: true,
            captured: true,
            ..sign_in
        };
        assert_eq!(judge(grabbing, moving), Check::System);
        // A game held when the computer locks lets go at once.
        let start = Instant::now();
        let mut debounce = Debounce::default();
        assert!(debounce.update(LOCKED, GAME, start));
        assert!(!debounce.update(judge(sign_in, moving), OTHER_APP, start));
        for program in [
            "LogonUI.exe",
            "logonui.exe",
            "LockApp.exe",
            "consent.exe",
            "CredentialUIBroker.exe",
        ] {
            assert!(is_windows_screen(program), "{program}");
        }
        for program in [
            "FraudCrew.exe",
            "firefox.exe",
            "explorer.exe",
            "LogonUI.exe.bak",
        ] {
            assert!(!is_windows_screen(program), "{program}");
        }
    }

    #[test]
    fn a_visible_cursor_held_for_a_drag_is_not_a_game() {
        let confined = Scene {
            confined: true,
            ..Scene::default()
        };
        let captured = Scene {
            captured: true,
            ..Scene::default()
        };
        let dragging = Mouse {
            buttons_held: true,
            ..Mouse::default()
        };
        let released = Mouse {
            settling: true,
            ..Mouse::default()
        };
        assert_eq!(judge(confined, dragging), Check::Unsure);
        assert_eq!(judge(confined, released), Check::Unsure);
        assert_eq!(judge(captured, dragging), Check::Free);
        assert_eq!(judge(captured, released), Check::Unsure);
        // A strategy game confining a visible cursor, and Blender's grab.
        assert_eq!(judge(confined, Mouse::default()), LOCKED);
        assert_eq!(
            judge(captured, Mouse::default()),
            Check::Locked(Lock::Captured)
        );
        // A game hides the cursor it holds, even while firing.
        let hidden = |scene| Scene {
            hidden: true,
            ..scene
        };
        assert_eq!(judge(hidden(confined), dragging), LOCKED);
        assert_eq!(judge(hidden(confined), released), LOCKED);
        assert_eq!(
            judge(hidden(captured), released),
            Check::Locked(Lock::Captured)
        );
    }

    #[test]
    fn a_cursor_hidden_while_resting_is_not_a_game() {
        let video = Scene {
            hidden: true,
            full_screen: true,
            ..Scene::default()
        };
        let moving = Mouse {
            moving: true,
            ..Mouse::default()
        };
        assert_eq!(judge(video, Mouse::default()), Check::Unsure);
        assert_eq!(judge(video, moving), Check::Locked(Lock::HiddenFullScreen));
        let windowed = Scene {
            full_screen: false,
            ..video
        };
        assert_eq!(judge(windowed, moving), Check::Free);
        let desktop = Scene {
            nobody: true,
            ..video
        };
        assert_eq!(judge(desktop, moving), Check::Free);
        assert_eq!(judge(Scene::default(), moving), Check::Free);
        // A game that left the cursor confined when Alt+Tab took it to the
        // desktop holds nothing there.
        let left_confined = Scene {
            confined: true,
            ..desktop
        };
        assert_eq!(judge(left_confined, moving), Check::Free);
        assert_eq!(
            judge(
                Scene {
                    nobody: false,
                    ..left_confined
                },
                moving
            ),
            LOCKED
        );
    }

    #[test]
    fn the_mouse_is_moving_once_it_has_moved_for_a_while_without_resting() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut history = History::default();
        assert!(!history.mouse(false, at(0)).moving);
        for ms in (0..50).step_by(8) {
            history.moved(at(ms));
            assert!(!history.mouse(false, at(ms)).moving);
        }
        assert_eq!(history.shown_by(), Some(at(0) + SHOW_DELAY));
        history.moved(at(56));
        assert!(history.mouse(false, at(56)).moving);
        assert!(history.mouse(false, at(150)).moving);
        // Resting, then moving again starts over.
        assert!(!history.mouse(false, at(156)).moving);
        history.moved(at(300));
        assert!(!history.mouse(false, at(300)).moving);
        assert_eq!(history.shown_by(), Some(at(300) + SHOW_DELAY));
        history.moved(at(330));
        history.moved(at(350));
        assert!(history.mouse(false, at(350)).moving);

        assert!(!history.mouse(false, at(400)).settling);
        history.released(at(400));
        assert!(history.mouse(false, at(400)).settling);
        assert!(history.mouse(true, at(649)).settling);
        assert!(history.mouse(true, at(649)).buttons_held);
        assert!(!history.mouse(false, at(650)).settling);
    }

    #[test]
    fn a_cursor_moved_away_from_seams_placements_is_a_warp() {
        let mut placed = VecDeque::new();
        assert!(!is_warp((500.0, 500.0), &placed));
        placed.extend([(100.0, 100.0), (110.0, 100.0), (120.4, 100.0)]);
        // Where Seam put it, or Windows still applying an earlier move.
        assert!(!is_warp((120.0, 100.0), &placed));
        assert!(!is_warp((100.0, 101.0), &placed));
        assert!(!is_warp((130.0, 108.0), &placed));
        // Re-centered, or wrapped to the other side of the window.
        assert!(is_warp((960.0, 540.0), &placed));
        assert!(is_warp((1900.0, 100.0), &placed));
    }

    #[test]
    fn only_a_clip_smaller_than_the_desktop_confines() {
        let desktop = rect(-1920, 0, 2560, 1440);
        assert!(!confines(&desktop, &desktop));
        assert!(!confines(&rect(-2000, -10, 2600, 1500), &desktop));
        // A game confining the cursor to one display or to its window.
        assert!(confines(&rect(0, 0, 2560, 1440), &desktop));
        assert!(confines(&rect(100, 100, 1380, 820), &desktop));
        assert!(confines(&rect(960, 540, 961, 541), &desktop));
    }

    #[test]
    fn full_screen_means_filling_the_monitor() {
        let monitor = rect(1920, 0, 4480, 1440);
        assert!(covers(&monitor, &monitor));
        assert!(covers(&rect(1919, -1, 4481, 1441), &monitor));
        assert!(covers(&rect(1921, 1, 4479, 1439), &monitor));
        // A maximized window stops at the taskbar.
        assert!(!covers(&rect(1912, -8, 4488, 1400), &monitor));
        assert!(!covers(&rect(2000, 100, 3000, 900), &monitor));
    }
}
