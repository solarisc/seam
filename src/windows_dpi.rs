//! Keep monitor bounds and low-level mouse coordinates in the same physical
//! pixel space, including when Windows display scaling is above 100%.

#[link(name = "user32")]
unsafe extern "system" {
    fn SetProcessDpiAwarenessContext(context: isize) -> i32;
}

pub fn enable_per_monitor_awareness() {
    // Windows defines DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 as -4.
    // A manifest can set awareness first; in that case this call fails with
    // access denied while the process is already configured correctly.
    if unsafe { SetProcessDpiAwarenessContext(-4) } == 0 {
        log::debug!(
            "Windows DPI awareness was already set or could not be changed: {}",
            std::io::Error::last_os_error()
        );
    }
}
