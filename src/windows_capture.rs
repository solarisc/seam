//! Native low-level Windows capture. VK_SHIFT/VK_CONTROL/VK_MENU are generic
//! in a low-level keyboard hook; the scan code and extended bit identify the
//! actual side. Sending an OS-specific unknown VK across the wire would make
//! macOS interpret its number as an unrelated Mac key code.

use crate::{
    input::Controller, protocol::MouseButton, windows_input::INJECTED_INPUT_TAG,
    windows_raw::Report,
};
use anyhow::{Result, bail};
use rdev::{Event, EventType, Key};
use std::{
    ffi::c_void,
    mem, ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicI32, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const WM_KEYDOWN: usize = 0x0100;
const WM_KEYUP: usize = 0x0101;
const WM_SYSKEYDOWN: usize = 0x0104;
const WM_SYSKEYUP: usize = 0x0105;
const WM_MOUSEMOVE: usize = 0x0200;
const WM_LBUTTONDOWN: usize = 0x0201;
const WM_LBUTTONUP: usize = 0x0202;
const WM_RBUTTONDOWN: usize = 0x0204;
const WM_RBUTTONUP: usize = 0x0205;
const WM_MBUTTONDOWN: usize = 0x0207;
const WM_MBUTTONUP: usize = 0x0208;
const WM_MOUSEWHEEL: usize = 0x020a;
const WM_XBUTTONDOWN: usize = 0x020b;
const WM_XBUTTONUP: usize = 0x020c;
const WM_MOUSEHWHEEL: usize = 0x020e;
const LLKHF_EXTENDED: u32 = 0x01;

/// Whether Scroll Lock is on. The toggle state is shared across threads, so
/// the hook thread sees what the keyboard light shows.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn scroll_lock_on() -> bool {
    const VK_SCROLL: i32 = 0x91;
    unsafe { GetKeyState(VK_SCROLL) & 1 != 0 }
}

/// Whether Windows considers `key` held right now, or `None` for a key with
/// no Windows equivalent. Authoritative only while this computer's hooks see
/// the keyboard, that is while it has the pointer.
pub(crate) fn key_down(key: Key) -> Option<bool> {
    let (vk, _) = key_to_windows(key)?;
    Some(unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
}

/// Whether the physical button behind `button` is held right now.
pub(crate) fn button_down(button: crate::protocol::MouseButton) -> bool {
    use crate::protocol::MouseButton;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
    // GetAsyncKeyState reports physical buttons; hooks report logical ones.
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    let vk = match button {
        MouseButton::Left if swapped => 0x02,
        MouseButton::Left => 0x01,
        MouseButton::Right if swapped => 0x01,
        MouseButton::Right => 0x02,
        MouseButton::Middle => 0x04,
        MouseButton::Back => 0x05,
        MouseButton::Forward => 0x06,
        // Not something Windows tracks; keep the captured state.
        MouseButton::Auxiliary(_) => return true,
    };
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// Release on this computer every key held when the pointer crosses away:
/// their physical releases go to the other computer, so Windows would
/// otherwise keep them down. Keys are never pressed again on return; the set
/// of held keys can miss a release (a hook timeout, a secure desktop), and a
/// synthesized press without its release leaves Ctrl or Alt stuck.
///
/// Never call this on the hook thread: see `release_keys_elsewhere`.
pub(crate) fn release_local_keys(keys: &std::collections::HashSet<Key>) {
    let mut held: Vec<Key> = keys
        .iter()
        .copied()
        .filter(|&key| key_to_windows(key).is_some())
        .collect();
    held.sort_by_key(crate::input::release_rank);
    if held
        .iter()
        .any(|key| crate::input::MODIFIER_KEYS.contains(key))
        && let Err(error) = crate::windows_input::mask_modifiers()
    {
        log::warn!("cannot mask held modifiers: {error:#}");
    }
    for key in held {
        if let Err(error) = crate::windows_input::simulate(&EventType::KeyRelease(key)) {
            log::error!("cannot release local key {key:?}: {error:#}");
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct KeyboardHook {
    vk: u32,
    scan: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
struct MouseHook {
    point: Point,
    data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
struct Message {
    window: *mut c_void,
    kind: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}

type HookCallback = unsafe extern "system" fn(i32, usize, isize) -> isize;

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(
        kind: i32,
        callback: HookCallback,
        module: *mut c_void,
        thread: u32,
    ) -> *mut c_void;
    fn CallNextHookEx(hook: *mut c_void, code: i32, wparam: usize, lparam: isize) -> isize;
    fn UnhookWindowsHookEx(hook: *mut c_void) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn GetKeyState(key: i32) -> i16;
    fn GetMessageW(message: *mut Message, window: *mut c_void, min: u32, max: u32) -> i32;
    fn PeekMessageW(
        message: *mut Message,
        window: *mut c_void,
        min: u32,
        max: u32,
        remove: u32,
    ) -> i32;
    fn PostThreadMessageW(thread: u32, message: u32, wparam: usize, lparam: isize) -> i32;
    fn SetTimer(window: *mut c_void, id: usize, elapse: u32, callback: *const c_void) -> usize;
    fn KillTimer(window: *mut c_void, id: usize) -> i32;
    fn TranslateMessage(message: *const Message) -> i32;
    fn DispatchMessageW(message: *const Message) -> isize;
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> isize;
    fn GetThreadDesktop(thread: u32) -> isize;
    fn CloseDesktop(desktop: isize) -> i32;
    fn GetUserObjectInformationW(
        object: isize,
        index: i32,
        info: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

static CONTROLLER: OnceLock<Arc<Controller>> = OnceLock::new();

fn event(kind: EventType) -> Event {
    Event {
        event_type: kind,
        time: SystemTime::now(),
        name: None,
    }
}

unsafe extern "system" fn keyboard_callback(code: i32, message: usize, data: isize) -> isize {
    KEYBOARD_HOOK_CALLS.fetch_add(1, Relaxed);
    if code != HC_ACTION || data == 0 {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let hook = unsafe { &*(data as *const KeyboardHook) };
    if hook.extra == INJECTED_INPUT_TAG {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let Some(controller) = CONTROLLER.get() else {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    };
    let Some(key) = key_from_windows(hook.vk, hook.scan, hook.flags) else {
        // Do not leak unrecognized key presses into the source desktop during
        // remote control, and do not transmit their native VK as a Mac key
        // code. Releases always pass: one held while crossing must not stay
        // down here.
        return if down
            && controller
                .remote_active
                .load(std::sync::atomic::Ordering::Acquire)
        {
            1
        } else {
            unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
        };
    };
    let kind = if down {
        EventType::KeyPress(key)
    } else {
        EventType::KeyRelease(key)
    };
    controller.local_pointer_activity();
    if controller.handle(event(kind)).is_none() {
        1
    } else {
        unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
    }
}

/// Wheel movement not sent to the other computer yet, in Windows' wheel units
/// (120 a notch). Precision touchpads and free-spinning wheels report a
/// fraction of a notch at a time; dividing each report by 120 lost them.
static WHEEL_REST_X: AtomicI32 = AtomicI32::new(0);
static WHEEL_REST_Y: AtomicI32 = AtomicI32::new(0);

/// Whole notches to send for a wheel report, keeping the fraction for the
/// next one. Only while another computer has the mouse: here apps get the
/// report as it is.
fn wheel_notches(rest: &AtomicI32, data: u32, remote: bool) -> i64 {
    let delta = i32::from((data >> 16) as i16);
    if !remote {
        rest.store(0, Relaxed);
        return i64::from(delta / 120);
    }
    let (notches, left) = take_notches(rest.load(Relaxed), delta);
    rest.store(left, Relaxed);
    notches
}

/// Adds `delta` to what was left and splits off whole notches. Turning the
/// other way starts afresh, so a leftover never swallows the first step back.
fn take_notches(rest: i32, delta: i32) -> (i64, i32) {
    let rest = if (rest < 0) == (delta < 0) { rest } else { 0 };
    let total = rest + delta;
    (i64::from(total / 120), total % 120)
}

unsafe extern "system" fn mouse_callback(code: i32, message: usize, data: isize) -> isize {
    MOUSE_HOOK_CALLS.fetch_add(1, Relaxed);
    if code != HC_ACTION || data == 0 {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let hook = unsafe { &*(data as *const MouseHook) };
    if hook.extra == INJECTED_INPUT_TAG {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    }
    let Some(controller) = CONTROLLER.get() else {
        return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) };
    };
    if matches!(
        message,
        WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
    ) {
        controller.local_pointer_activity();
        let pass = captured_button(message, hook.data).map_or_else(
            || {
                !controller
                    .remote_active
                    .load(std::sync::atomic::Ordering::Acquire)
            },
            |(button, down)| controller.handle_button(button, down),
        );
        return if pass {
            unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
        } else {
            1
        };
    }
    let remote = controller
        .remote_active
        .load(std::sync::atomic::Ordering::Acquire);
    let kind = match message {
        WM_MOUSEMOVE => EventType::MouseMove {
            x: f64::from(hook.point.x),
            y: f64::from(hook.point.y),
        },
        WM_MOUSEWHEEL => EventType::Wheel {
            delta_x: 0,
            delta_y: wheel_notches(&WHEEL_REST_Y, hook.data, remote),
        },
        WM_MOUSEHWHEEL => EventType::Wheel {
            delta_x: wheel_notches(&WHEEL_REST_X, hook.data, remote),
            delta_y: 0,
        },
        _ => return unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) },
    };
    controller.local_pointer_activity();
    if controller.handle(event(kind)).is_none() {
        1
    } else {
        unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
    }
}

fn captured_button(message: usize, data: u32) -> Option<(MouseButton, bool)> {
    let button = match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => MouseButton::Left,
        WM_RBUTTONDOWN | WM_RBUTTONUP => MouseButton::Right,
        WM_MBUTTONDOWN | WM_MBUTTONUP => MouseButton::Middle,
        WM_XBUTTONDOWN | WM_XBUTTONUP => match data >> 16 {
            1 => MouseButton::Back,
            2 => MouseButton::Forward,
            _ => return None,
        },
        _ => return None,
    };
    Some((
        button,
        matches!(
            message,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        ),
    ))
}

/// Convert documented Windows VKs to platform-independent physical keys.
/// The scan code distinguishes both Shift keys even when the VK is generic.
fn key_from_windows(vk: u32, scan: u32, flags: u32) -> Option<Key> {
    Some(match vk {
        0x10 => {
            if scan == 0x36 {
                Key::ShiftRight
            } else {
                Key::ShiftLeft
            }
        }
        0x11 => {
            if flags & LLKHF_EXTENDED != 0 {
                Key::ControlRight
            } else {
                Key::ControlLeft
            }
        }
        0x12 => {
            if flags & LLKHF_EXTENDED != 0 {
                Key::AltGr
            } else {
                Key::Alt
            }
        }
        0xa0 => Key::ShiftLeft,
        0xa1 => Key::ShiftRight,
        0xa2 => Key::ControlLeft,
        0xa3 => Key::ControlRight,
        0xa4 => Key::Alt,
        0xa5 => Key::AltGr,
        0x5b => Key::MetaLeft,
        0x5c => Key::MetaRight,
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0d => {
            if flags & LLKHF_EXTENDED != 0 {
                Key::KpReturn
            } else {
                Key::Return
            }
        }
        0x14 => Key::CapsLock,
        0x13 => Key::Pause,
        0x1b => Key::Escape,
        0x20 => Key::Space,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::LeftArrow,
        0x26 => Key::UpArrow,
        0x27 => Key::RightArrow,
        0x28 => Key::DownArrow,
        0x2c => Key::PrintScreen,
        0x2d => Key::Insert,
        0x2e => Key::Delete,
        0x30 => Key::Num0,
        0x31 => Key::Num1,
        0x32 => Key::Num2,
        0x33 => Key::Num3,
        0x34 => Key::Num4,
        0x35 => Key::Num5,
        0x36 => Key::Num6,
        0x37 => Key::Num7,
        0x38 => Key::Num8,
        0x39 => Key::Num9,
        0x41 => Key::KeyA,
        0x42 => Key::KeyB,
        0x43 => Key::KeyC,
        0x44 => Key::KeyD,
        0x45 => Key::KeyE,
        0x46 => Key::KeyF,
        0x47 => Key::KeyG,
        0x48 => Key::KeyH,
        0x49 => Key::KeyI,
        0x4a => Key::KeyJ,
        0x4b => Key::KeyK,
        0x4c => Key::KeyL,
        0x4d => Key::KeyM,
        0x4e => Key::KeyN,
        0x4f => Key::KeyO,
        0x50 => Key::KeyP,
        0x51 => Key::KeyQ,
        0x52 => Key::KeyR,
        0x53 => Key::KeyS,
        0x54 => Key::KeyT,
        0x55 => Key::KeyU,
        0x56 => Key::KeyV,
        0x57 => Key::KeyW,
        0x58 => Key::KeyX,
        0x59 => Key::KeyY,
        0x5a => Key::KeyZ,
        0x60 => Key::Kp0,
        0x61 => Key::Kp1,
        0x62 => Key::Kp2,
        0x63 => Key::Kp3,
        0x64 => Key::Kp4,
        0x65 => Key::Kp5,
        0x66 => Key::Kp6,
        0x67 => Key::Kp7,
        0x68 => Key::Kp8,
        0x69 => Key::Kp9,
        0x6a => Key::KpMultiply,
        0x6b => Key::KpPlus,
        0x6d => Key::KpMinus,
        0x6e => Key::KpDelete,
        0x6f => Key::KpDivide,
        0x70 => Key::F1,
        0x71 => Key::F2,
        0x72 => Key::F3,
        0x73 => Key::F4,
        0x74 => Key::F5,
        0x75 => Key::F6,
        0x76 => Key::F7,
        0x77 => Key::F8,
        0x78 => Key::F9,
        0x79 => Key::F10,
        0x7a => Key::F11,
        0x7b => Key::F12,
        0x90 => Key::NumLock,
        0x91 => Key::ScrollLock,
        0xba => Key::SemiColon,
        0xbb => Key::Equal,
        0xbc => Key::Comma,
        0xbd => Key::Minus,
        0xbe => Key::Dot,
        0xbf => Key::Slash,
        0xc0 => Key::BackQuote,
        0xdb => Key::LeftBracket,
        0xdc => Key::BackSlash,
        0xdd => Key::RightBracket,
        0xde => Key::Quote,
        0xe2 => Key::IntlBackslash,
        _ => return None,
    })
}

/// Preserve the extended-key distinction for navigation and keypad Enter.
pub(crate) fn key_to_windows(key: Key) -> Option<(u16, bool)> {
    let vk = match key {
        Key::ShiftLeft => 0xa0,
        Key::ShiftRight => 0xa1,
        Key::ControlLeft => 0xa2,
        Key::ControlRight => 0xa3,
        Key::Alt => 0xa4,
        Key::AltGr => 0xa5,
        Key::MetaLeft => 0x5b,
        Key::MetaRight => 0x5c,
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::CapsLock => 0x14,
        Key::Pause => 0x13,
        Key::Escape => 0x1b,
        Key::Space => 0x20,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::End => 0x23,
        Key::Home => 0x24,
        Key::LeftArrow => 0x25,
        Key::UpArrow => 0x26,
        Key::RightArrow => 0x27,
        Key::DownArrow => 0x28,
        Key::PrintScreen => 0x2c,
        Key::Insert => 0x2d,
        Key::Delete => 0x2e,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::KeyA => 0x41,
        Key::KeyB => 0x42,
        Key::KeyC => 0x43,
        Key::KeyD => 0x44,
        Key::KeyE => 0x45,
        Key::KeyF => 0x46,
        Key::KeyG => 0x47,
        Key::KeyH => 0x48,
        Key::KeyI => 0x49,
        Key::KeyJ => 0x4a,
        Key::KeyK => 0x4b,
        Key::KeyL => 0x4c,
        Key::KeyM => 0x4d,
        Key::KeyN => 0x4e,
        Key::KeyO => 0x4f,
        Key::KeyP => 0x50,
        Key::KeyQ => 0x51,
        Key::KeyR => 0x52,
        Key::KeyS => 0x53,
        Key::KeyT => 0x54,
        Key::KeyU => 0x55,
        Key::KeyV => 0x56,
        Key::KeyW => 0x57,
        Key::KeyX => 0x58,
        Key::KeyY => 0x59,
        Key::KeyZ => 0x5a,
        Key::Kp0 => 0x60,
        Key::Kp1 => 0x61,
        Key::Kp2 => 0x62,
        Key::Kp3 => 0x63,
        Key::Kp4 => 0x64,
        Key::Kp5 => 0x65,
        Key::Kp6 => 0x66,
        Key::Kp7 => 0x67,
        Key::Kp8 => 0x68,
        Key::Kp9 => 0x69,
        Key::KpMultiply => 0x6a,
        Key::KpPlus => 0x6b,
        Key::KpMinus => 0x6d,
        Key::KpDelete => 0x6e,
        Key::KpDivide => 0x6f,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7a,
        Key::F12 => 0x7b,
        Key::NumLock => 0x90,
        Key::ScrollLock => 0x91,
        Key::SemiColon => 0xba,
        Key::Equal => 0xbb,
        Key::Comma => 0xbc,
        Key::Minus => 0xbd,
        Key::Dot => 0xbe,
        Key::Slash => 0xbf,
        Key::BackQuote => 0xc0,
        Key::LeftBracket => 0xdb,
        Key::BackSlash => 0xdc,
        Key::RightBracket => 0xdd,
        Key::Quote => 0xde,
        Key::IntlBackslash => 0xe2,
        Key::Return => 0x0d,
        Key::KpReturn => 0x0d,
        _ => return None,
    };
    let extended = matches!(
        key,
        Key::ControlRight
            | Key::AltGr
            | Key::MetaLeft
            | Key::MetaRight
            | Key::Insert
            | Key::Delete
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
            | Key::LeftArrow
            | Key::RightArrow
            | Key::UpArrow
            | Key::DownArrow
            | Key::KpReturn
            | Key::KpDivide
            | Key::NumLock
            | Key::PrintScreen
    );
    Some((vk, extended))
}

fn stop_registration(shutdown: &Arc<crate::shutdown::Signal>) -> crate::shutdown::Registration {
    // Ensure the message queue exists before another thread can post WM_QUIT.
    let mut message = unsafe { mem::zeroed::<Message>() };
    unsafe {
        PeekMessageW(&mut message, ptr::null_mut(), 0, 0, 0);
    }
    let thread = unsafe { GetCurrentThreadId() };
    shutdown.register(move || {
        if unsafe { PostThreadMessageW(thread, 0x0012, 0, 0) } == 0 {
            log::error!(
                "cannot wake Windows capture for shutdown: {}",
                std::io::Error::last_os_error()
            );
        }
    })
}

/// Hook calls, counted so the capture loop can tell when Windows has removed
/// a hook: Windows drops each one on its own.
static MOUSE_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static KEYBOARD_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

/// Raw reports in a row with no call to that device's hook that mean the
/// hook is gone: far more than the few that can arrive before their hook
/// calls (Pause and Print Screen send two raw reports for one key), and for
/// the mouse, well under a second at common report rates.
const SILENT_MOUSE_REPORTS: u32 = 200;
const SILENT_KEYBOARD_REPORTS: u32 = 20;

/// Hooks that stay silent after being installed again are not lost but
/// passed over, for example by another program's hook that keeps the input.
/// Don't take control back from the other computer for that more often.
const REINSTALL_INTERVAL: Duration = Duration::from_secs(10);

struct HookWatch {
    limit: u32,
    calls: u64,
    raw_since: u32,
}

impl HookWatch {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            calls: 0,
            raw_since: 0,
        }
    }

    /// Called for each raw report with the current count of hook calls.
    fn hooks_lost(&mut self, calls: u64) -> bool {
        if calls != self.calls {
            self.calls = calls;
            self.raw_since = 0;
            return false;
        }
        self.raw_since += 1;
        if self.raw_since < self.limit {
            return false;
        }
        self.raw_since = 0;
        true
    }
}

/// While another computer has the keyboard and mouse, how often to check that
/// this one's desktop still has input. Ctrl+Alt+Del, a UAC prompt or Windows
/// locking moves it to the secure desktop; two checks in a row there bring
/// the keyboard and mouse back. Windows handles Ctrl+Alt+Del itself, so this
/// way home works when nothing else does: an app on the other computer
/// holding the mouse, a lost hook, a forgotten shortcut.
const SECURE_DESKTOP_CHECK: Duration = Duration::from_millis(250);
const SECURE_DESKTOP_CHECKS: u32 = 2;

/// Counts checks in a row that found another desktop receiving input while
/// the keyboard and mouse were on another computer.
#[derive(Default)]
struct SecureDesktopWatch {
    away: u32,
}

impl SecureDesktopWatch {
    /// Returns `true` once, when control should come back here.
    fn check(&mut self, remote: bool, on_input_desktop: impl FnOnce() -> bool) -> bool {
        if !remote || on_input_desktop() {
            self.away = 0;
            return false;
        }
        self.away += 1;
        self.away == SECURE_DESKTOP_CHECKS
    }
}

/// Whether this thread's desktop is the one receiving input. Hooks are not
/// called while another desktop has it, such as the secure desktop behind
/// Ctrl+Alt+Del or a UAC prompt, so their silence then means nothing.
fn on_input_desktop() -> bool {
    const DESKTOP_READOBJECTS: u32 = 0x0001;
    let input = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input == 0 {
        return false;
    }
    let own = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let same = desktop_name(input).is_some_and(|name| desktop_name(own) == Some(name));
    unsafe { CloseDesktop(input) };
    same
}

fn desktop_name(desktop: isize) -> Option<Vec<u16>> {
    const UOI_NAME: i32 = 2;
    let mut name = [0u16; 256];
    let mut needed = 0;
    let read = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            mem::size_of_val(&name) as u32,
            &mut needed,
        )
    };
    if read == 0 {
        return None;
    }
    let length = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Some(name[..length].to_vec())
}

fn install_hooks() -> Result<(*mut c_void, *mut c_void)> {
    let keyboard =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, keyboard_callback, ptr::null_mut(), 0) };
    if keyboard.is_null() {
        bail!(
            "cannot install Windows keyboard hook: {}",
            std::io::Error::last_os_error()
        );
    }
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, mouse_callback, ptr::null_mut(), 0) };
    if mouse.is_null() {
        unsafe { UnhookWindowsHookEx(keyboard) };
        bail!(
            "cannot install Windows mouse hook: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok((keyboard, mouse))
}

pub fn run(controller: Arc<Controller>, shutdown: Arc<crate::shutdown::Signal>) -> Result<()> {
    let _priority = crate::windows_priority::InputPriority::enter();
    CONTROLLER
        .set(controller.clone())
        .map_err(|_| anyhow::anyhow!("Windows capture already running"))?;
    // A visible cursor on a mouse-less computer is a convenience. Windows
    // refuses the setting on some desktops (the sign-in screen), and that must
    // not stop keyboard and mouse sharing. The sign-in helper stays out of it:
    // it is killed at unlock, so a lease of its own would stay on with nobody
    // to give it back. The person's agent keeps its lease over the lock screen.
    let pointer_presence = if crate::windows_signin::is_helper() {
        None
    } else {
        let watched = controller.clone();
        crate::windows_pointer_presence::Keeper::start(controller.cursor_without_mouse, move || {
            watched.peer_control_active.load(Relaxed)
        })
        .map_err(|error| {
            log::warn!("native cursor for a mouse-less computer unavailable: {error:#}")
        })
        .ok()
    };
    let raw = crate::windows_raw::Capture::new()?;
    let (mut keyboard, mut mouse) = install_hooks()?;
    let mut mouse_watch = HookWatch::new(SILENT_MOUSE_REPORTS);
    let mut keyboard_watch = HookWatch::new(SILENT_KEYBOARD_REPORTS);
    let mut reinstalled: Option<Instant> = None;
    let registration = stop_registration(&shutdown);
    let mut secure_desktop = SecureDesktopWatch::default();
    // A thread timer, its messages coming to this loop with no window. It
    // runs only while another computer has the keyboard and mouse: started
    // after the input that hands them over, stopped once they are back.
    let mut timer = 0usize;
    let mut message = unsafe { mem::zeroed::<Message>() };
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        if message.kind == windows_sys::Win32::UI::WindowsAndMessaging::WM_TIMER
            && message.window.is_null()
            && timer != 0
            && message.wparam == timer
        {
            if secure_desktop.check(controller.remote_active.load(Relaxed), on_input_desktop) {
                log::info!(
                    "Windows' secure screen is up (Ctrl+Alt+Del, a UAC prompt or the lock screen); the keyboard and mouse are back on this computer"
                );
                controller.take_back_from_secure_screen();
            }
            if !controller.remote_active.load(Relaxed) {
                unsafe { KillTimer(ptr::null_mut(), timer) };
                timer = 0;
            }
            continue;
        }
        if message.kind == windows_sys::Win32::UI::WindowsAndMessaging::WM_INPUT_DEVICE_CHANGE
            && let Some(pointer_presence) = pointer_presence.as_ref()
        {
            pointer_presence.devices_changed();
        }
        let lost = match raw.handle_message(message.kind, message.lparam, &controller) {
            Some(Report::Mouse) => mouse_watch.hooks_lost(MOUSE_HOOK_CALLS.load(Relaxed)),
            Some(Report::Keyboard) => keyboard_watch.hooks_lost(KEYBOARD_HOOK_CALLS.load(Relaxed)),
            None => false,
        };
        if lost
            && reinstalled.is_none_or(|at| at.elapsed() >= REINSTALL_INTERVAL)
            && on_input_desktop()
        {
            // Windows drops a hook that once took too long to answer, without
            // telling anyone. Raw input keeps arriving; hook calls stop.
            log::warn!("Windows removed Seam's input hooks; installing them again");
            unsafe {
                UnhookWindowsHookEx(mouse);
                UnhookWindowsHookEx(keyboard);
            }
            reinstalled = Some(Instant::now());
            controller.recover_lost_hooks();
            (keyboard, mouse) = install_hooks()?;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        // Control leaves this computer in a hook or raw report handled above,
        // and every key or mouse report comes through here as raw input.
        if timer == 0 && controller.remote_active.load(Relaxed) {
            timer = unsafe {
                SetTimer(
                    ptr::null_mut(),
                    0,
                    SECURE_DESKTOP_CHECK.as_millis() as u32,
                    ptr::null(),
                )
            };
        }
    }
    drop(registration);
    controller.force_local();
    unsafe {
        UnhookWindowsHookEx(mouse);
        UnhookWindowsHookEx(keyboard);
    }
    if shutdown.is_stopped() {
        Ok(())
    } else {
        bail!("Windows input hook message loop exited")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchpad_scrolling_adds_up_to_whole_notches() {
        // A mouse wheel: a notch at a time, as before.
        assert_eq!(take_notches(0, 120), (1, 0));
        assert_eq!(take_notches(0, -240), (-2, 0));
        // A precision touchpad's small steps add up instead of being lost.
        let mut rest = 0;
        let mut sent = 0;
        for _ in 0..30 {
            let (notches, left) = take_notches(rest, 8);
            sent += notches;
            rest = left;
        }
        assert_eq!((sent, rest), (2, 0));
        assert_eq!(take_notches(0, 180), (1, 60));
        assert_eq!(take_notches(60, 60), (1, 0));
        // Turning back starts afresh: the leftover doesn't cancel the step.
        assert_eq!(take_notches(60, -120), (-1, 0));
        assert_eq!(take_notches(-90, 30), (0, 30));
    }

    #[test]
    fn the_secure_desktop_brings_the_keyboard_and_mouse_back_once() {
        let mut watch = SecureDesktopWatch::default();
        // Here, or on the other computer with this desktop in front: nothing.
        assert!(!watch.check(false, || false));
        assert!(!watch.check(true, || true));
        // Ctrl+Alt+Del while on the other computer: back after two checks,
        // and only once while the secure screen stays up.
        assert!(!watch.check(true, || false));
        assert!(watch.check(true, || false));
        assert!(!watch.check(true, || false));
        // One odd check in between starts the count again.
        let mut watch = SecureDesktopWatch::default();
        assert!(!watch.check(true, || false));
        assert!(!watch.check(true, || true));
        assert!(!watch.check(true, || false));
        assert!(watch.check(true, || false));
    }

    #[test]
    fn physical_state_is_read_for_keys_windows_knows() {
        // Whatever is held right now, Windows answers for keys it knows.
        assert!(key_down(Key::ControlLeft).is_some());
        assert_eq!(key_down(Key::Unknown(0)), None);
        let _ = button_down(MouseButton::Middle);
        assert!(button_down(MouseButton::Auxiliary(6)));
    }

    #[test]
    fn silent_hooks_are_noticed_only_after_many_raw_reports() {
        let mut watch = HookWatch::new(SILENT_MOUSE_REPORTS);
        // Raw reports racing ahead of their hook calls are normal.
        for report in 1..1000 {
            assert!(!watch.hooks_lost(report / 3));
        }
        let calls = 1000;
        for _ in 0..SILENT_MOUSE_REPORTS {
            assert!(!watch.hooks_lost(calls));
        }
        assert!(watch.hooks_lost(calls));
        // Counting starts over once the hooks are installed again.
        assert!(!watch.hooks_lost(calls));
        assert!(!watch.hooks_lost(calls + 1));
    }

    #[test]
    fn desktops_are_told_apart_by_name() {
        let own = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
        assert!(desktop_name(own).is_some_and(|name| !name.is_empty()));
        assert_eq!(desktop_name(0), None);
    }

    #[test]
    fn low_level_mouse_messages_decode_portable_buttons() {
        // WM_* messages place the X-button identity in the high word.
        // SendInput uses a different data format, covered at its own boundary.
        for (button, down, up, data) in [
            (MouseButton::Left, 0x201, 0x202, 0),
            (MouseButton::Right, 0x204, 0x205, 0),
            (MouseButton::Middle, 0x207, 0x208, 0),
            (MouseButton::Back, 0x20b, 0x20c, 0x10000),
            (MouseButton::Forward, 0x20b, 0x20c, 0x20000),
        ] {
            assert_eq!(captured_button(down, data), Some((button, true)));
            assert_eq!(captured_button(up, data), Some((button, false)));
        }
        for data in [0, 1, 2, 0x30000, 0xff0000] {
            assert!(captured_button(0x20b, data).is_none());
        }
        assert!(captured_button(0x200, 0).is_none());
    }
    #[test]
    fn injection_preserves_modifier_sides_and_keypad_enter() {
        for key in [
            Key::ShiftLeft,
            Key::ShiftRight,
            Key::ControlLeft,
            Key::ControlRight,
            Key::Alt,
            Key::AltGr,
            Key::MetaLeft,
            Key::MetaRight,
            Key::Return,
            Key::KpReturn,
            Key::LeftArrow,
            Key::Kp4,
            Key::Pause,
        ] {
            let (vk, extended) = key_to_windows(key).unwrap();
            assert_eq!(
                key_from_windows(u32::from(vk), 0, if extended { LLKHF_EXTENDED } else { 0 }),
                Some(key)
            );
        }
    }
    #[test]
    fn generic_modifiers_use_scan_and_extended_flags() {
        assert_eq!(key_from_windows(0x10, 0x2a, 0), Some(Key::ShiftLeft));
        assert_eq!(key_from_windows(0x10, 0x36, 0), Some(Key::ShiftRight));
        assert_eq!(
            key_from_windows(0x11, 0x1d, LLKHF_EXTENDED),
            Some(Key::ControlRight)
        );
        assert_eq!(
            key_from_windows(0x12, 0x38, LLKHF_EXTENDED),
            Some(Key::AltGr)
        );
    }
    #[test]
    fn unknown_native_vk_is_never_sent_as_an_unrelated_mac_key() {
        assert_eq!(key_from_windows(0xff, 0, 0), None);
    }
}
