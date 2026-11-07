//! Native macOS capture and injection. Physical and peer input have separate
//! identities; injected input never updates the physical keyboard state.

use crate::{
    input::Controller,
    protocol::{MouseButton, ScrollUnit},
};
use anyhow::{Result, bail};
use crossbeam_channel::Sender;
use rdev::{Event, EventType, Key};
use std::{
    collections::HashSet,
    ffi::c_void,
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

struct TapContext {
    controller: Arc<Controller>,
    tap: *mut c_void,
}

const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const OTHER_DRAGGED: u32 = 27;
const SCROLL_WHEEL: u32 = 22;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const KEYBOARD_KEYCODE: u32 = 9;
const MOUSE_BUTTON_NUMBER: u32 = 3;
const GESTURE: u32 = 29;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xffff_fffe;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xffff_ffff;
const MOUSE_DELTA_X: u32 = 4;
const MOUSE_DELTA_Y: u32 = 5;
const SOURCE_PROCESS_ID: u32 = 41;
const SOURCE_USER_DATA: u32 = 42;
const SCROLL_LINE_Y: u32 = 11;
const SCROLL_LINE_X: u32 = 12;
const SCROLL_PIXEL_Y: u32 = 96;
const SCROLL_PIXEL_X: u32 = 97;
const SCROLL_CONTINUOUS: u32 = 88;
const INJECTED_INPUT_TAG: i64 = 0x5441_4e44_4553_4b01;
static CURSOR_HIDDEN: Mutex<bool> = Mutex::new(false);
static BACKGROUND_CURSOR: OnceLock<Result<(), String>> = OnceLock::new();
static CURSOR_LOCKED: AtomicBool = AtomicBool::new(false);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        context: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enabled: bool);
    fn CGEventGetLocation(event: *mut c_void) -> Point;
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetFlags(event: *mut c_void) -> u64;
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: *mut c_void, field: u32, value: f64);
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        kind: u32,
        position: Point,
        button: u32,
    ) -> *mut c_void;
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, keycode: u16, down: bool) -> *mut c_void;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        unit: u32,
        wheel_count: u32,
        wheel_y: i32,
        wheel_x: i32,
        wheel_z: i32,
    ) -> *mut c_void;
    fn CGEventPost(location: u32, event: *mut c_void);
    fn CGEventSetTimestamp(event: *mut c_void, timestamp: u64);
    fn CGMainDisplayID() -> u32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(associated: u32) -> i32;
    fn CGWarpMouseCursorPosition(point: Point) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct Timebase {
    numerator: u32,
    denominator: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

fn event_time() -> u64 {
    static TIMEBASE: OnceLock<Timebase> = OnceLock::new();
    let base = TIMEBASE.get_or_init(|| {
        let mut base = Timebase::default();
        let result = unsafe { mach_timebase_info(&mut base) };
        assert!(result == 0 && base.denominator != 0);
        base
    });
    (u128::from(unsafe { mach_absolute_time() }) * u128::from(base.numerator)
        / u128::from(base.denominator)) as u64
}

const FLAG_CAPS_LOCK: u64 = 1 << 16;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_OPTION: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;
const FLAG_FUNCTION: u64 = 1 << 23;

/// Post and release an owned event, including CoreGraphics' autoreleased
/// serialization buffers. Rust input threads do not supply a native pool.
///
/// # Safety
/// `event` must be a non-null, owned CGEvent reference; this consumes it.
unsafe fn post_owned_event(event: *mut c_void, flags: u64) {
    objc2::rc::autoreleasepool(|_| unsafe {
        CGEventSetFlags(event, flags);
        CGEventSetIntegerValueField(event, SOURCE_USER_DATA, INJECTED_INPUT_TAG);
        CGEventSetTimestamp(event, event_time());
        CGEventPost(0, event);
        CFRelease(event);
    });
}

// Device-specific modifier bits from Apple's IOLLEvent.h. Aggregate Shift,
// Control, Option and Command bits alone cannot distinguish the two sides.
fn modifier_down(key: Key, flags: u64) -> Option<bool> {
    let mask = match key {
        Key::ControlLeft => 0x0001,
        Key::ControlRight => 0x2000,
        Key::ShiftLeft => 0x0002,
        Key::ShiftRight => 0x0004,
        Key::MetaLeft => 0x0008,
        Key::MetaRight => 0x0010,
        Key::Alt => 0x0020,
        Key::AltGr => 0x0040,
        Key::Function => FLAG_FUNCTION,
        _ => return None,
    };
    Some(flags & mask != 0)
}

fn captured_event(controller: &Controller, kind: EventType) -> bool {
    controller
        .handle(Event {
            event_type: kind,
            time: SystemTime::now(),
            name: None,
        })
        .is_some()
}

/// A single source carries an entire peer keyboard session. Each event also
/// gets the complete held-modifier flags, so shortcuts work even when macOS
/// does not carry a synthesized modifier between separate event objects.
pub struct PeerKeyboard {
    source: *mut c_void,
    caps_lock: bool,
}

impl PeerKeyboard {
    pub fn new() -> Self {
        Self {
            source: unsafe { CGEventSourceCreate(1) },
            caps_lock: false,
        }
    }

    pub fn post(
        &mut self,
        key: Key,
        down: bool,
        held: &HashSet<Key>,
        fresh_down: bool,
    ) -> Result<()> {
        let Some(code) = key_code(key) else {
            bail!("unsupported Mac peer key: {key:?}");
        };
        if key == Key::CapsLock && down && fresh_down {
            self.caps_lock = !self.caps_lock;
        }
        let flags = modifier_flags(held, self.caps_lock);
        let event = unsafe { CGEventCreateKeyboardEvent(self.source, code, down) };
        if event.is_null() {
            bail!("cannot create Mac peer key event");
        }
        unsafe { post_owned_event(event, flags) };
        Ok(())
    }

    pub fn flags(&self, held: &HashSet<Key>) -> u64 {
        modifier_flags(held, self.caps_lock)
    }
}

impl Drop for PeerKeyboard {
    fn drop(&mut self) {
        if !self.source.is_null() {
            unsafe { CFRelease(self.source) };
        }
    }
}

fn modifier_flags(held: &HashSet<Key>, caps_lock: bool) -> u64 {
    let mut flags = if caps_lock { FLAG_CAPS_LOCK } else { 0 };
    if held.contains(&Key::ShiftLeft) || held.contains(&Key::ShiftRight) {
        flags |= FLAG_SHIFT;
    }
    if held.contains(&Key::ControlLeft) || held.contains(&Key::ControlRight) {
        flags |= FLAG_CONTROL;
    }
    if held.contains(&Key::Alt) || held.contains(&Key::AltGr) {
        flags |= FLAG_OPTION;
    }
    if held.contains(&Key::MetaLeft) || held.contains(&Key::MetaRight) {
        flags |= FLAG_COMMAND;
    }
    if held.contains(&Key::Function) {
        flags |= FLAG_FUNCTION;
    }
    flags
}

pub fn set_local_modifiers(keys: &HashSet<Key>, down: bool) {
    let caps_lock = unsafe { CGEventSourceFlagsState(1) } & FLAG_CAPS_LOCK != 0;
    let mut active = if down { HashSet::new() } else { keys.clone() };
    for key in crate::input::MODIFIER_KEYS {
        if !keys.contains(&key) {
            continue;
        }
        if down {
            active.insert(key);
        } else {
            active.remove(&key);
        }
        let Some(code) = key_code(key) else {
            continue;
        };
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), code, down) };
        if event.is_null() {
            continue;
        }
        unsafe { post_owned_event(event, modifier_flags(&active, caps_lock)) };
    }
}

// Keep capture and injection on the same physical-key table.
macro_rules! mac_key_codes {
    ($($key:path => $code:literal,)*) => {
        fn key_code(key: Key) -> Option<u16> {
            match key { $($key => Some($code),)* _ => None }
        }
        fn key_from_code(code: u16) -> Option<Key> {
            match code { $($code => Some($key),)* _ => None }
        }
    };
}

mac_key_codes! {
    Key::KeyA => 0,
    Key::KeyS => 1,
    Key::KeyD => 2,
    Key::KeyF => 3,
    Key::KeyH => 4,
    Key::KeyG => 5,
    Key::KeyZ => 6,
    Key::KeyX => 7,
    Key::KeyC => 8,
    Key::KeyV => 9,
    Key::IntlBackslash => 10,
    Key::KeyB => 11,
    Key::KeyQ => 12,
    Key::KeyW => 13,
    Key::KeyE => 14,
    Key::KeyR => 15,
    Key::KeyY => 16,
    Key::KeyT => 17,
    Key::Num1 => 18,
    Key::Num2 => 19,
    Key::Num3 => 20,
    Key::Num4 => 21,
    Key::Num6 => 22,
    Key::Num5 => 23,
    Key::Equal => 24,
    Key::Num9 => 25,
    Key::Num7 => 26,
    Key::Minus => 27,
    Key::Num8 => 28,
    Key::Num0 => 29,
    Key::RightBracket => 30,
    Key::KeyO => 31,
    Key::KeyU => 32,
    Key::LeftBracket => 33,
    Key::KeyI => 34,
    Key::KeyP => 35,
    Key::Return => 36,
    Key::KeyL => 37,
    Key::KeyJ => 38,
    Key::Quote => 39,
    Key::KeyK => 40,
    Key::SemiColon => 41,
    Key::BackSlash => 42,
    Key::Comma => 43,
    Key::Slash => 44,
    Key::KeyN => 45,
    Key::KeyM => 46,
    Key::Dot => 47,
    Key::Tab => 48,
    Key::Space => 49,
    Key::BackQuote => 50,
    Key::Backspace => 51,
    Key::Escape => 53,
    Key::MetaRight => 54,
    Key::MetaLeft => 55,
    Key::ShiftLeft => 56,
    Key::CapsLock => 57,
    Key::Alt => 58,
    Key::ControlLeft => 59,
    Key::ShiftRight => 60,
    Key::AltGr => 61,
    Key::ControlRight => 62,
    Key::Function => 63,
    Key::KpDelete => 65,
    Key::KpMultiply => 67,
    Key::KpPlus => 69,
    Key::NumLock => 71,
    Key::KpDivide => 75,
    Key::KpReturn => 76,
    Key::KpMinus => 78,
    Key::Kp0 => 82,
    Key::Kp1 => 83,
    Key::Kp2 => 84,
    Key::Kp3 => 85,
    Key::Kp4 => 86,
    Key::Kp5 => 87,
    Key::Kp6 => 88,
    Key::Kp7 => 89,
    Key::Kp8 => 91,
    Key::Kp9 => 92,
    Key::F1 => 122,
    Key::F2 => 120,
    Key::F3 => 99,
    Key::F4 => 118,
    Key::F5 => 96,
    Key::F6 => 97,
    Key::F7 => 98,
    Key::F8 => 100,
    Key::F9 => 101,
    Key::F10 => 109,
    Key::F11 => 103,
    Key::F12 => 111,
    Key::PrintScreen => 105,
    Key::ScrollLock => 107,
    Key::Pause => 113,
    Key::Insert => 114,
    Key::Home => 115,
    Key::PageUp => 116,
    Key::Delete => 117,
    Key::End => 119,
    Key::PageDown => 121,
    Key::LeftArrow => 123,
    Key::RightArrow => 124,
    Key::DownArrow => 125,
    Key::UpArrow => 126,
}

/// Mark peer-owned pointer events before posting them into the HID stream.
/// Their source PID is not stable when CoreGraphics creates a system-state
/// event, so a dedicated user-data tag prevents a remote pointer feedback loop.
pub fn simulate_peer_pointer(
    x: f64,
    y: f64,
    previous: Option<(f64, f64)>,
    dragging: Option<MouseButton>,
    flags: u64,
) -> Result<()> {
    let event = pointer_event(Point { x, y }, previous, dragging)?;
    unsafe { post_owned_event(event, flags) };
    Ok(())
}

fn pointer_event(
    point: Point,
    previous: Option<(f64, f64)>,
    dragging: Option<MouseButton>,
) -> Result<*mut c_void> {
    let (kind, button) = match dragging {
        Some(button) => (
            match button {
                MouseButton::Left => LEFT_DRAGGED,
                MouseButton::Right => RIGHT_DRAGGED,
                _ => OTHER_DRAGGED,
            },
            button_number(button)?,
        ),
        None => (MOUSE_MOVED, 0),
    };
    let event = mouse_event(kind, point, button)?;
    let previous = previous.unwrap_or((point.x, point.y));
    // Relative-input applications need the actual displacement, including any
    // skipped buffered positions, rather than CoreGraphics' default zero delta.
    unsafe {
        CGEventSetDoubleValueField(event, MOUSE_DELTA_X, point.x - previous.0);
        CGEventSetDoubleValueField(event, MOUSE_DELTA_Y, point.y - previous.1);
    }
    Ok(event)
}

fn button_number(button: MouseButton) -> Result<u32> {
    Ok(match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Auxiliary(n @ 6..) => u32::from(n - 1),
        MouseButton::Auxiliary(_) => bail!("unsupported Mac mouse button"),
    })
}

fn captured_button(kind: u32, number: i64) -> Option<MouseButton> {
    Some(match kind {
        1 | 2 => MouseButton::Left,
        3 | 4 => MouseButton::Right,
        25 | 26 => match number {
            2 => MouseButton::Middle,
            3 => MouseButton::Back,
            4 => MouseButton::Forward,
            5..=254 => MouseButton::Auxiliary(number as u8 + 1),
            _ => return None,
        },
        _ => return None,
    })
}

pub fn simulate_peer_button(
    button: MouseButton,
    down: bool,
    position: Option<(f64, f64)>,
    flags: u64,
) -> Result<()> {
    let number = button_number(button)?;
    let kind = match button {
        MouseButton::Left => {
            if down {
                1
            } else {
                2
            }
        }
        MouseButton::Right => {
            if down {
                3
            } else {
                4
            }
        }
        _ => {
            if down {
                25
            } else {
                26
            }
        }
    };
    // CGEventPost is asynchronous. Querying the system position after posting
    // motion can still return the old position and move the click backwards.
    let point = if let Some((x, y)) = position {
        Point { x, y }
    } else {
        let current = unsafe { CGEventCreate(ptr::null()) };
        if current.is_null() {
            bail!("cannot read Mac pointer location");
        }
        let point = unsafe { CGEventGetLocation(current) };
        unsafe { CFRelease(current) };
        point
    };
    let event = mouse_event(kind, point, number)?;
    unsafe { post_owned_event(event, flags) };
    Ok(())
}

/// Construct an owned native event only if CoreGraphics preserves its button.
/// Some out-of-range button numbers wrap in the native representation. Never
/// turn an unsupported auxiliary button into a different physical button.
fn mouse_event(kind: u32, point: Point, number: u32) -> Result<*mut c_void> {
    let event = unsafe { CGEventCreateMouseEvent(ptr::null(), kind, point, number) };
    if event.is_null() {
        bail!("cannot create Mac peer mouse event");
    }
    // The five named buttons have fixed native representations. Only the
    // extensible auxiliary range needs validation; ordinary motion stays lean.
    if number > 4
        && unsafe { CGEventGetIntegerValueField(event, MOUSE_BUTTON_NUMBER) } != i64::from(number)
    {
        unsafe { CFRelease(event) };
        bail!("CoreGraphics cannot represent this mouse button");
    }
    Ok(event)
}

pub fn simulate_peer_scroll(dx: i64, dy: i64, unit: ScrollUnit, flags: u64) -> Result<()> {
    let mac_unit = match unit {
        ScrollUnit::Lines => 1,
        ScrollUnit::Pixels => 0,
    };
    let event = unsafe {
        CGEventCreateScrollWheelEvent2(
            ptr::null(),
            mac_unit,
            2,
            dy.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            dx.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            0,
        )
    };
    if event.is_null() {
        bail!("cannot create macOS peer scroll event");
    }
    unsafe { post_owned_event(event, flags) };
    Ok(())
}

pub(crate) fn hide_cursor() -> bool {
    let mut hidden = CURSOR_HIDDEN.lock().unwrap();
    if *hidden {
        return true;
    }
    if let Err(error) = BACKGROUND_CURSOR.get_or_init(enable_background_cursor) {
        log::error!("cannot acquire background cursor ownership: {error}");
        return false;
    }
    let result = unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
    if result != 0 {
        log::error!("cannot hide Mac cursor: {result}");
        return false;
    }
    *hidden = true;
    true
}

/// CoreGraphics documents that hiding normally requires the foreground app.
/// A KVM agent must preserve application focus, so opt its WindowServer
/// connection into background cursor changes. This compatibility API is not
/// public: resolve it at runtime and refuse cursor acquisition if unavailable.
/// Never replace the system cursor or assume a successful CG call hid it.
fn enable_background_cursor() -> Result<(), String> {
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const std::ffi::c_char,
            encoding: u32,
        ) -> *const c_void;
        static kCFBooleanTrue: *const c_void;
    }
    type Connection = unsafe extern "C" fn() -> u32;
    type SetProperty = unsafe extern "C" fn(u32, u32, *const c_void, *const c_void) -> i32;
    // Darwin's RTLD_DEFAULT searches already-loaded images; no optional private
    // symbol becomes a mandatory load-time dependency of the executable.
    let default = (-2isize) as *mut c_void;
    let connection = unsafe { dlsym(default, c"_CGSDefaultConnection".as_ptr()) };
    let property = unsafe { dlsym(default, c"CGSSetConnectionProperty".as_ptr()) };
    if connection.is_null() || property.is_null() {
        return Err("this macOS version lacks background cursor control".into());
    }
    let connection: Connection = unsafe { std::mem::transmute(connection) };
    let property: SetProperty = unsafe { std::mem::transmute(property) };
    let name = unsafe {
        CFStringCreateWithCString(ptr::null(), c"SetsCursorInBackground".as_ptr(), 0x0800_0100)
    };
    if name.is_null() {
        return Err("cannot allocate cursor capability name".into());
    }
    let id = unsafe { connection() };
    let result = unsafe { property(id, id, name, kCFBooleanTrue) };
    unsafe { CFRelease(name) };
    if result != 0 {
        return Err(format!(
            "WindowServer rejected background cursor control: {result}"
        ));
    }
    Ok(())
}

pub(crate) fn show_cursor() {
    let mut hidden = CURSOR_HIDDEN.lock().unwrap();
    if *hidden {
        let result = unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
        if result != 0 {
            log::error!("cannot show Mac cursor: {result}");
            return;
        }
        *hidden = false;
    }
}

/// Lock before the edge event leaves the HID tap. Doing this on a worker
/// leaves a window where the physical mouse still drives the Mac cursor.
pub fn lock_local_cursor(park_x: f64, park_y: f64) -> bool {
    if CURSOR_LOCKED.load(Ordering::Acquire) {
        return true;
    }
    if !hide_cursor() {
        return false;
    }
    let result = unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
    if result != 0 {
        show_cursor();
        log::error!("cannot detach physical mouse from Mac cursor: {result}");
        return false;
    }
    let park = unsafe {
        CGWarpMouseCursorPosition(Point {
            x: park_x,
            y: park_y,
        })
    };
    if park != 0 {
        unsafe {
            CGAssociateMouseAndMouseCursorPosition(1);
        }
        show_cursor();
        log::error!("cannot park Mac source cursor: {park}");
        return false;
    }
    CURSOR_LOCKED.store(true, Ordering::Release);
    true
}

pub fn restore_local_cursor(x: f64, y: f64) {
    if !CURSOR_LOCKED.swap(false, Ordering::AcqRel) {
        return;
    }
    let warp = unsafe { CGWarpMouseCursorPosition(Point { x, y }) };
    let associate = unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
    show_cursor();
    if warp != 0 || associate != 0 {
        log::error!("cannot fully restore Mac cursor: warp={warp}, associate={associate}");
    }
}

/// CoreGraphics may still move the hardware cursor while an HID event tap is
/// suppressing its app event. Keep it at the outer corner until control returns.
pub fn pin_local_cursor(x: f64, y: f64) {
    if CURSOR_LOCKED.load(Ordering::Acquire) {
        let result = unsafe { CGWarpMouseCursorPosition(Point { x, y }) };
        if result != 0 {
            log::warn!("cannot keep source cursor parked: {result}");
        }
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        tap: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CFRunLoopWakeUp(run_loop: *mut c_void);
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut RunLoopSourceContext,
    ) -> *mut c_void;
    fn CFRunLoopSourceSignal(source: *mut c_void);
    fn CFRunLoopSourceInvalidate(source: *mut c_void);
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRelease(object: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

unsafe extern "C" fn callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    context: *mut c_void,
) -> *mut c_void {
    // SAFETY: The boxed context stays alive until this tap's run loop exits.
    let context = unsafe { &mut *context.cast::<TapContext>() };
    if event_type == TAP_DISABLED_BY_TIMEOUT || event_type == TAP_DISABLED_BY_USER_INPUT {
        unsafe { CGEventTapEnable(context.tap, true) };
        return event;
    }
    if event_type == GESTURE {
        if context.controller.remote_active.load(Ordering::Acquire) {
            return ptr::null_mut();
        }
        context.controller.local_pointer_activity();
        return event;
    }
    // Bypass every injected event before decoding. A peer modifier must not
    // become a physical modifier or be recaptured after a direction change.
    let source = unsafe { CGEventGetIntegerValueField(event, SOURCE_PROCESS_ID) };
    let tag = unsafe { CGEventGetIntegerValueField(event, SOURCE_USER_DATA) };
    if tag == INJECTED_INPUT_TAG {
        return event;
    }
    if source == i64::from(std::process::id()) {
        return event;
    }
    context.controller.local_pointer_activity();
    if context.controller.observes_local_input_only() {
        return event;
    }
    if matches!(event_type, KEY_DOWN | KEY_UP | FLAGS_CHANGED) {
        let code = unsafe { CGEventGetIntegerValueField(event, KEYBOARD_KEYCODE) };
        let key = u16::try_from(code).ok().and_then(key_from_code);
        let Some(key) = key else {
            return if context.controller.remote_active.load(Ordering::Acquire) {
                ptr::null_mut()
            } else {
                event
            };
        };
        let pass = if event_type == FLAGS_CHANGED && key == Key::CapsLock {
            // Caps Lock emits a toggle notification, not a normal down/up
            // pair. Both its on and off transitions are complete presses.
            let pass = captured_event(&context.controller, EventType::KeyPress(key));
            captured_event(&context.controller, EventType::KeyRelease(key));
            pass
        } else {
            let down = if event_type == FLAGS_CHANGED {
                let flags = unsafe { CGEventGetFlags(event) };
                let Some(down) = modifier_down(key, flags) else {
                    return event;
                };
                down
            } else {
                event_type == KEY_DOWN
            };
            captured_event(
                &context.controller,
                if down {
                    EventType::KeyPress(key)
                } else {
                    EventType::KeyRelease(key)
                },
            )
        };
        return if pass { event } else { ptr::null_mut() };
    }
    if matches!(event_type, 1 | 2 | 3 | 4 | 25 | 26) {
        let button = captured_button(event_type, unsafe {
            CGEventGetIntegerValueField(event, MOUSE_BUTTON_NUMBER)
        });
        let pass = button.map_or_else(
            || !context.controller.remote_active.load(Ordering::Acquire),
            |button| {
                context
                    .controller
                    .handle_button(button, matches!(event_type, 1 | 3 | 25))
            },
        );
        return if pass { event } else { ptr::null_mut() };
    }
    if event_type == SCROLL_WHEEL {
        let continuous = unsafe { CGEventGetIntegerValueField(event, SCROLL_CONTINUOUS) } != 0;
        let (dx, dy, unit) = if continuous {
            (
                unsafe { CGEventGetIntegerValueField(event, SCROLL_PIXEL_X) },
                unsafe { CGEventGetIntegerValueField(event, SCROLL_PIXEL_Y) },
                ScrollUnit::Pixels,
            )
        } else {
            (
                unsafe { CGEventGetIntegerValueField(event, SCROLL_LINE_X) },
                unsafe { CGEventGetIntegerValueField(event, SCROLL_LINE_Y) },
                ScrollUnit::Lines,
            )
        };
        if context.controller.handle_mac_scroll(dx, dy, unit) {
            return event;
        }
        return ptr::null_mut();
    }
    let point = unsafe { CGEventGetLocation(event) };
    let dx = unsafe { CGEventGetIntegerValueField(event, MOUSE_DELTA_X) } as f64;
    let dy = unsafe { CGEventGetIntegerValueField(event, MOUSE_DELTA_Y) } as f64;
    if context
        .controller
        .handle_mac_motion(point.x, point.y, dx, dy)
    {
        event
    } else {
        ptr::null_mut()
    }
}

// CFRunLoopSourceContext version 0. Unused callbacks are null; perform runs on
// the capture thread. A signaled source survives a stop request before Run().
#[repr(C)]
struct RunLoopSourceContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
    schedule: *const c_void,
    cancel: *const c_void,
    perform: unsafe extern "C" fn(*mut c_void),
}

unsafe extern "C" fn stop_capture_loop(_: *mut c_void) {
    unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
}

pub fn run(
    controller: Arc<Controller>,
    ready: Sender<Result<()>>,
    shutdown: Arc<crate::shutdown::Signal>,
) -> Result<()> {
    let _priority = crate::input_priority::InputPriority::enter();
    let context = Box::into_raw(Box::new(TapContext {
        controller: controller.clone(),
        tap: ptr::null_mut(),
    }));
    let mask = [
        KEY_DOWN,
        KEY_UP,
        FLAGS_CHANGED,
        1,
        2,
        3,
        4,
        25,
        26,
        MOUSE_MOVED,
        LEFT_DRAGGED,
        RIGHT_DRAGGED,
        OTHER_DRAGGED,
        SCROLL_WHEEL,
        GESTURE,
    ]
    .into_iter()
    .fold(0u64, |bits, event| bits | (1u64 << event));
    // A receiver never suppresses local input. A listen-only tap lets macOS
    // deliver events without waiting for this process to acknowledge each one,
    // while retaining local cursor recovery and injected-event diagnostics.
    let options = u32::from(controller.configured_receive_only());
    let tap = unsafe { CGEventTapCreate(0, 0, options, mask, callback, context.cast()) };
    if tap.is_null() {
        unsafe { drop(Box::from_raw(context)) };
        let _ = ready.send(Err(anyhow::anyhow!(
            "macOS rejected the input tap; enable Accessibility and Input Monitoring for Seam"
        )));
        bail!("cannot create macOS input tap");
    }
    unsafe { (*context).tap = tap };
    let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe {
            CFRelease(tap);
            drop(Box::from_raw(context));
        }
        let _ = ready.send(Err(anyhow::anyhow!("cannot start macOS mouse event loop")));
        bail!("cannot create mouse tap run-loop source");
    }
    let mut stop_context = RunLoopSourceContext {
        version: 0,
        info: ptr::null_mut(),
        retain: ptr::null(),
        release: ptr::null(),
        copy_description: ptr::null(),
        equal: ptr::null(),
        hash: ptr::null(),
        schedule: ptr::null(),
        cancel: ptr::null(),
        perform: stop_capture_loop,
    };
    let stop_source = unsafe { CFRunLoopSourceCreate(ptr::null(), -1, &mut stop_context) };
    if stop_source.is_null() {
        unsafe {
            CFRelease(source);
            CFRelease(tap);
            drop(Box::from_raw(context));
        }
        bail!("cannot create capture shutdown source");
    }
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    unsafe {
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CFRunLoopAddSource(run_loop, stop_source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    // The registration is removed before either native handle is released.
    let (wake_loop, wake_source) = (run_loop as usize, stop_source as usize);
    let registration = shutdown.register(move || unsafe {
        CFRunLoopSourceSignal(wake_source as *mut c_void);
        CFRunLoopWakeUp(wake_loop as *mut c_void);
    });
    let _ = ready.send(Ok(()));
    unsafe { CFRunLoopRun() };
    drop(registration);
    controller.force_local();
    unsafe {
        CGEventTapEnable(tap, false);
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        CFRunLoopRemoveSource(run_loop, stop_source, kCFRunLoopCommonModes);
        CFRunLoopSourceInvalidate(stop_source);
        CFRelease(stop_source);
        CFRelease(source);
        CFRelease(tap);
        drop(Box::from_raw(context));
    }
    if shutdown.is_stopped() {
        Ok(())
    } else {
        bail!("macOS mouse event loop exited")
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;

    #[test]
    fn native_motion_preserves_absolute_position_and_accumulated_delta() {
        for dragging in [None, Some(MouseButton::Left), Some(MouseButton::Back)] {
            let event = pointer_event(Point { x: 500.0, y: 200.0 }, Some((450.0, 230.0)), dragging)
                .unwrap();
            unsafe {
                let position = CGEventGetLocation(event);
                assert_eq!((position.x, position.y), (500.0, 200.0));
                assert_eq!(CGEventGetIntegerValueField(event, MOUSE_DELTA_X), 50);
                assert_eq!(CGEventGetIntegerValueField(event, MOUSE_DELTA_Y), -30);
                CFRelease(event);
            }
        }
    }

    #[test]
    fn side_buttons_preserve_their_physical_identity_across_platforms() {
        // Independent CoreGraphics event/button constants. The old Windows
        // Back/Forward wire IDs were misinterpreted as native Mac buttons 1/2.
        for (button, number, down, up) in [
            (MouseButton::Left, 0, 1, 2),
            (MouseButton::Right, 1, 3, 4),
            (MouseButton::Middle, 2, 25, 26),
            (MouseButton::Back, 3, 25, 26),
            (MouseButton::Forward, 4, 25, 26),
            (MouseButton::Auxiliary(6), 5, 25, 26),
            (MouseButton::Auxiliary(32), 31, 25, 26),
        ] {
            assert_eq!(button_number(button).unwrap(), number);
            for kind in [down, up] {
                assert_eq!(captured_button(kind, i64::from(number)), Some(button));
                let event = mouse_event(
                    kind,
                    Point { x: 100.0, y: 100.0 },
                    button_number(button).unwrap(),
                )
                .unwrap();
                let actual = unsafe { CGEventGetIntegerValueField(event, MOUSE_BUTTON_NUMBER) };
                unsafe { CFRelease(event) };
                assert_eq!(actual, i64::from(number));
            }
        }
        for invalid in [0, 1, 5] {
            assert!(button_number(MouseButton::Auxiliary(invalid)).is_err());
        }
        for invalid in [-1, 0, 1, 255, 256] {
            assert!(captured_button(25, invalid).is_none());
        }
        // The real API wraps this number to -2. The injection owner rejects
        // that lossy native conversion instead of posting a different button.
        assert!(
            mouse_event(
                25,
                Point { x: 100.0, y: 100.0 },
                button_number(MouseButton::Auxiliary(255)).unwrap()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    #[test]
    fn releasing_one_shift_does_not_release_the_other() {
        // Right Shift down while Left Shift is already held; then Left Shift
        // up. The aggregate Shift bit stays set throughout both events.
        let both = FLAG_SHIFT | 0x0002 | 0x0004;
        assert_eq!(modifier_down(Key::ShiftRight, both), Some(true));
        let right_only = FLAG_SHIFT | 0x0004;
        assert_eq!(modifier_down(Key::ShiftLeft, right_only), Some(false));
        assert_eq!(modifier_down(Key::ShiftRight, right_only), Some(true));
        assert_eq!(modifier_down(Key::ShiftRight, 0), Some(false));
    }

    #[test]
    fn unrelated_flag_changes_cannot_reverse_a_modifier_transition() {
        // Other modifiers or Caps Lock can raise the aggregate numeric flags
        // value even as this particular modifier is being released.
        assert_eq!(
            modifier_down(Key::ControlLeft, FLAG_COMMAND | 0x8),
            Some(false)
        );
        assert_eq!(
            modifier_down(Key::ControlRight, FLAG_CONTROL | 0x2000),
            Some(true)
        );
        assert_eq!(
            modifier_down(Key::Alt, FLAG_CAPS_LOCK | FLAG_OPTION | 0x40),
            Some(false)
        );
        assert_eq!(modifier_down(Key::AltGr, FLAG_OPTION | 0x40), Some(true));
    }

    #[test]
    fn capture_and_injection_round_trip_every_supported_mac_code() {
        for code in 0..=127 {
            if let Some(key) = key_from_code(code) {
                assert_eq!(key_code(key), Some(code));
            }
        }
        assert_eq!(key_from_code(0xffff), None);
    }

    #[test]
    fn peer_modifier_flags_are_explicit() {
        let held = HashSet::from([Key::MetaLeft, Key::ShiftRight]);
        assert_eq!(modifier_flags(&held, false), FLAG_COMMAND | FLAG_SHIFT);
        assert_eq!(modifier_flags(&HashSet::new(), true), FLAG_CAPS_LOCK);
    }

    #[test]
    fn common_navigation_and_numpad_keys_have_mac_codes() {
        for key in [
            Key::Return,
            Key::KpReturn,
            Key::Delete,
            Key::Home,
            Key::End,
            Key::Kp0,
        ] {
            assert!(key_code(key).is_some());
        }
        assert_eq!(key_code(Key::Unknown(17)), None);
    }
}
