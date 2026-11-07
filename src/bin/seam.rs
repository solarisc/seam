#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    // The installer runs this as an administrator who may not use Seam.
    #[cfg(target_os = "windows")]
    if std::env::args().nth(1).as_deref() == Some("--quit-tray") {
        seam::windows_tray::close();
        return;
    }
    // A windowed program has no console; keep errors where support can find them.
    seam::logging::init(
        seam::config::config_dir()
            .ok()
            .map(|directory| directory.join("window.log"))
            .as_deref(),
    );
    #[cfg(target_os = "windows")]
    {
        seam::windows_dpi::enable_per_monitor_awareness();
        match std::env::args().nth(1).as_deref() {
            Some("--tray") => {
                if let Err(error) = seam::windows_tray::run() {
                    log::error!("tray stopped: {error:#}");
                }
                return;
            }
            _ => seam::windows_tray::ensure_running(),
        }
    }
    if let Err(error) = seam::gui::run() {
        log::error!("window stopped: {error:#}");
        report(&format!("Seam couldn't open its window.\n\n{error:#}"));
    }
}

#[cfg(target_os = "windows")]
fn report(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide("Seam").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn report(message: &str) {
    eprintln!("{message}");
}
