//! The displays macOS draws to, their persistent identities, and display
//! change announcements.
use anyhow::{Result, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

#[repr(C)]
struct Point {
    x: f64,
    y: f64,
}
#[repr(C)]
struct Size {
    width: f64,
    height: f64,
}
#[repr(C)]
struct Rect {
    origin: Point,
    size: Size,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGDisplayIsMain(display: u32) -> u32;
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *const c_void;
    fn CGDisplayRegisterReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        context: *mut c_void,
    ) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreateString(allocator: *const c_void, uuid: *const c_void) -> *const c_void;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut i8,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRelease(value: *const c_void);
}

/// A display macOS draws to, in global display coordinates: points, the
/// space pointer events use, with the main display's top-left corner at the
/// origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Active {
    pub(crate) id: u32,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) main: bool,
}

/// The displays macOS draws to now. CoreGraphics only, so it is safe off the
/// main thread and cheap enough to call every second. A sleeping display is
/// not active: while every display sleeps the list is empty.
pub(crate) fn active() -> Result<Vec<Active>> {
    let mut count = 0;
    ensure!(
        unsafe { CGGetActiveDisplayList(0, ptr::null_mut(), &mut count) } == 0,
        "Cannot count active displays"
    );
    // The inventory can grow between the two calls; the next look picks up
    // any additional display. The API writes at most the given capacity.
    let mut displays = vec![0; count as usize];
    ensure!(
        unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) } == 0,
        "Cannot read active displays"
    );
    displays.truncate(count as usize);
    Ok(displays
        .into_iter()
        .map(|id| {
            let bounds = unsafe { CGDisplayBounds(id) };
            Active {
                id,
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                width: bounds.size.width as u32,
                height: bounds.size.height as u32,
                main: unsafe { CGDisplayIsMain(id) } != 0,
            }
        })
        .collect())
}

pub(crate) fn identities() -> Result<BTreeMap<u32, String>> {
    let mut ids = [0; 64];
    let mut count = 0;
    ensure!(
        unsafe { CGGetOnlineDisplayList(64, ids.as_mut_ptr(), &mut count) } == 0,
        "Cannot read display identities"
    );
    let mut result = BTreeMap::new();
    for &id in &ids[..count as usize] {
        unsafe {
            let uuid = CGDisplayCreateUUIDFromDisplayID(id);
            if uuid.is_null() {
                continue;
            }
            let string = CFUUIDCreateString(ptr::null(), uuid);
            CFRelease(uuid);
            if string.is_null() {
                continue;
            }
            let mut bytes = [0i8; 64];
            let ok =
                CFStringGetCString(string, bytes.as_mut_ptr(), bytes.len() as isize, 0x08000100);
            CFRelease(string);
            if ok {
                result.insert(
                    id,
                    std::ffi::CStr::from_ptr(bytes.as_ptr())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    Ok(result)
}

// The callback has process lifetime and no borrowed context. Disconnected
// subscribers are retired on the next notification; no raw pointer outlives its owner.
static SUBSCRIBERS: Mutex<Vec<Sender<()>>> = Mutex::new(Vec::new());
static REGISTERED: OnceLock<i32> = OnceLock::new();
extern "C" fn changed(_: u32, flags: u32, _: *mut c_void) {
    if flags & 1 != 0 {
        return;
    } // Begin-configuration; wait for the completed change.
    if let Ok(mut subscribers) = SUBSCRIBERS.lock() {
        subscribers.retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
}

/// Display change announcements. They speed up noticing a change but cannot
/// be relied on: some macOS versions accept the subscription and then never
/// call back, so the displays must also be looked at regularly.
pub(crate) struct Watch {
    changes: Receiver<()>,
    _shutdown: crate::shutdown::Registration,
}
impl Watch {
    pub(crate) fn new(shutdown: &Arc<crate::shutdown::Signal>) -> Result<Self> {
        let status = *REGISTERED.get_or_init(|| unsafe {
            CGDisplayRegisterReconfigurationCallback(changed, ptr::null_mut())
        });
        ensure!(status == 0, "Cannot subscribe to display changes: {status}");
        let (sender, changes) = bounded(1);
        SUBSCRIBERS.lock().unwrap().push(sender.clone());
        let registration = shutdown.register(move || {
            let _ = sender.try_send(());
        });
        Ok(Self {
            changes,
            _shutdown: registration,
        })
    }
    /// Waits up to `timeout` for a display change or shutdown, and tells
    /// whether one arrived. `SUBSCRIBERS` keeps a sender for as long as this
    /// receiver lives, so the wait never ends early for want of one.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        self.changes.recv_timeout(timeout).is_ok()
    }
}
