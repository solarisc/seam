//! Keeping this computer awake while Seam runs, when the person asks for it.
//! Sleep is always held off; the display only when they ask for that too.
use anyhow::Result;
#[cfg(any(windows, target_os = "macos"))]
use anyhow::ensure;

/// Shown by `powercfg /requests` on Windows and `pmset -g assertions` on macOS.
#[cfg(any(windows, target_os = "macos"))]
const REASON: &str = "Seam: Keep this computer awake is on";

/// While held, the computer doesn't go to sleep on its own, and with
/// `screen` its display doesn't turn off on its own either.
#[cfg(windows)]
pub(crate) struct StayAwake {
    request: windows_sys::Win32::Foundation::HANDLE,
    screen: bool,
}

#[cfg(windows)]
impl StayAwake {
    pub fn new(screen: bool) -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            System::{
                Power::{
                    PowerCreateRequest, PowerRequestDisplayRequired, PowerRequestSystemRequired,
                    PowerSetRequest,
                },
                Threading::{
                    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
                },
            },
        };
        // POWER_REQUEST_CONTEXT_VERSION, from a module Seam has no other use for.
        const VERSION: u32 = 0;
        let mut reason: Vec<u16> = REASON.encode_utf16().chain([0]).collect();
        let context = REASON_CONTEXT {
            Version: VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr(),
            },
        };
        let request = unsafe { PowerCreateRequest(&context) };
        ensure!(
            request != INVALID_HANDLE_VALUE,
            "cannot create a power request: {}",
            std::io::Error::last_os_error()
        );
        if unsafe { PowerSetRequest(request, PowerRequestSystemRequired) } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(request) };
            anyhow::bail!("cannot keep the computer awake: {error}");
        }
        // The computer staying awake matters more; keep that if this fails.
        let screen = screen && {
            let set = unsafe { PowerSetRequest(request, PowerRequestDisplayRequired) } != 0;
            if !set {
                log::warn!(
                    "cannot keep the screen on: {}",
                    std::io::Error::last_os_error()
                );
            }
            set
        };
        Ok(Self { request, screen })
    }
}

#[cfg(windows)]
impl Drop for StayAwake {
    fn drop(&mut self) {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Power::{
                PowerClearRequest, PowerRequestDisplayRequired, PowerRequestSystemRequired,
            },
        };
        unsafe {
            if self.screen {
                PowerClearRequest(self.request, PowerRequestDisplayRequired);
            }
            PowerClearRequest(self.request, PowerRequestSystemRequired);
            CloseHandle(self.request);
        }
    }
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: *const std::ffi::c_void,
        level: u32,
        name: *const std::ffi::c_void,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        name: *const std::ffi::c_void,
        kind: u32,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const std::ffi::c_void,
        string: *const std::ffi::c_char,
        encoding: u32,
    ) -> *const std::ffi::c_void;
    fn CFRelease(value: *const std::ffi::c_void);
}

/// A CFString for one IOKit call, released when dropped.
#[cfg(target_os = "macos")]
struct CfString(*const std::ffi::c_void);

#[cfg(target_os = "macos")]
impl CfString {
    fn new(text: &str) -> Option<Self> {
        const UTF8: u32 = 0x0800_0100;
        let text = std::ffi::CString::new(text).ok()?;
        let string = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
        (!string.is_null()).then_some(Self(string))
    }
}

#[cfg(target_os = "macos")]
impl Drop for CfString {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

/// While held, the computer doesn't go to sleep on its own, and with
/// `screen` its display doesn't turn off on its own either. A Mac laptop
/// still sleeps when its lid is closed without an external display.
#[cfg(target_os = "macos")]
pub(crate) struct StayAwake(Vec<u32>);

#[cfg(target_os = "macos")]
impl StayAwake {
    pub fn new(screen: bool) -> Result<Self> {
        // The system assertion even with the display one, which on its own
        // lets the Mac idle to sleep once the screen is put to sleep by hand.
        let mut awake = Self(Vec::new());
        awake.assert("PreventUserIdleSystemSleep")?;
        // The computer staying awake matters more; keep that if this fails.
        if screen && let Err(error) = awake.assert("PreventUserIdleDisplaySleep") {
            log::warn!("cannot keep the screen on: {error:#}");
        }
        Ok(awake)
    }

    fn assert(&mut self, kind: &str) -> Result<()> {
        const LEVEL_ON: u32 = 255;
        let mut id = 0;
        let result = match (CfString::new(kind), CfString::new(REASON)) {
            (Some(kind), Some(name)) => unsafe {
                IOPMAssertionCreateWithName(kind.0, LEVEL_ON, name.0, &mut id)
            },
            _ => -1,
        };
        ensure!(
            result == 0,
            "cannot keep the Mac awake (IOKit error {result:#x})"
        );
        self.0.push(id);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for StayAwake {
    fn drop(&mut self) {
        for &id in &self.0 {
            unsafe { IOPMAssertionRelease(id) };
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) struct StayAwake;

#[cfg(not(any(windows, target_os = "macos")))]
impl StayAwake {
    pub fn new(_screen: bool) -> Result<Self> {
        anyhow::bail!("keeping the computer awake is not supported on this system")
    }
}

/// Tells macOS that someone is using this Mac through Seam, as its own
/// keyboard and mouse would: the screen wakes when the pointer arrives, and
/// neither it nor the computer idles off while their input keeps coming.
#[cfg(target_os = "macos")]
pub(crate) struct UserActivity {
    id: u32,
    last: Option<std::time::Instant>,
    failed: bool,
}

#[cfg(target_os = "macos")]
impl UserActivity {
    /// Often enough that no idle timer, which counts in minutes, runs out.
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new() -> Self {
        Self {
            id: 0,
            last: None,
            failed: false,
        }
    }

    /// Input arrived from the controller; `arriving` when the pointer has
    /// just come onto this Mac and the screen should wake now.
    pub fn declare(&mut self, arriving: bool) {
        let now = std::time::Instant::now();
        if !arriving && self.last.is_some_and(|last| now - last < Self::INTERVAL) {
            return;
        }
        self.last = Some(now);
        // kIOPMUserActiveLocal: as if at this Mac's own keyboard.
        const LOCAL: u32 = 0;
        let result = match CfString::new("Seam: input from another computer") {
            // A non-zero id updates the same assertion instead of adding one.
            Some(name) => unsafe { IOPMAssertionDeclareUserActivity(name.0, LOCAL, &mut self.id) },
            None => -1,
        };
        if result != 0 && !self.failed {
            log::warn!("cannot tell macOS this Mac is in use (IOKit error {result:#x})");
        }
        self.failed = result != 0;
    }
}

#[cfg(target_os = "macos")]
impl Drop for UserActivity {
    fn drop(&mut self) {
        if self.id != 0 {
            unsafe { IOPMAssertionRelease(self.id) };
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_power_request_is_made_and_withdrawn() {
        let awake = StayAwake::new(false).unwrap();
        drop(awake);
        let awake = StayAwake::new(true).unwrap();
        drop(awake);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// This process's assertions as `pmset` lists them, one per line.
    fn assertions() -> Vec<String> {
        let output = std::process::Command::new("/usr/bin/pmset")
            .args(["-g", "assertions"])
            .output()
            .unwrap();
        let mine = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.trim_start().starts_with(&mine))
            .map(str::to_owned)
            .collect()
    }

    fn holds(kind: &str, name: &str) -> bool {
        assertions()
            .iter()
            .any(|line| line.contains(&format!(" {kind} ")) && line.contains(name))
    }

    // One test, as the assertions belong to the whole test process.
    #[test]
    fn assertions_are_taken_and_released() {
        let awake = StayAwake::new(false).unwrap();
        assert!(holds("PreventUserIdleSystemSleep", REASON));
        assert!(!holds("PreventUserIdleDisplaySleep", REASON));
        drop(awake);
        assert!(!holds("PreventUserIdleSystemSleep", REASON));

        let awake = StayAwake::new(true).unwrap();
        assert!(holds("PreventUserIdleDisplaySleep", REASON));
        assert!(holds("PreventUserIdleSystemSleep", REASON));
        drop(awake);
        assert!(!holds("PreventUserIdleDisplaySleep", REASON));
        assert!(!holds("PreventUserIdleSystemSleep", REASON));

        let name = "Seam: input from another computer";
        let mut activity = UserActivity::new();
        activity.declare(true);
        assert!(!activity.failed);
        assert!(holds("UserIsActive", name));
        let id = activity.id;
        activity.declare(true);
        activity.declare(false);
        assert_eq!(activity.id, id, "the same assertion is renewed");
        assert_eq!(
            assertions()
                .iter()
                .filter(|line| line.contains(name))
                .count(),
            1
        );
        drop(activity);
        assert!(!holds("UserIsActive", name));
    }
}
