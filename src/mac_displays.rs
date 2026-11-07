//! Stable display selection and event-driven display refresh on macOS.
use anyhow::{Result, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex, OnceLock},
};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
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
    pub(crate) fn wait(&self) {
        let _ = self.changes.recv();
    }
}
