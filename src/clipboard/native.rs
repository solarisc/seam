//! Native clipboard access and its OS change generation.
// Windows clipboard ownership is associated with the opening window. Our
// contexts use a null HWND, so independent peer threads must not open/close or
// replace native clipboard memory while another context is copying it.
#[cfg(windows)]
pub(super) fn transaction() -> std::sync::MutexGuard<'static, ()> {
    static ACCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ACCESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(windows)]
mod virtual_files;
#[cfg(all(test, windows))]
pub(super) use virtual_files::paste;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::NativeClipboard;

#[cfg(not(target_os = "macos"))]
pub(super) struct NativeClipboard {
    context: clipboard_rs::ClipboardContext,
}

#[cfg(not(target_os = "macos"))]
impl NativeClipboard {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            context: clipboard_rs::ClipboardContext::new()
                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
        })
    }

    pub fn set_text(&self, text: &str) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let _clipboard = windows::open()?;
            windows::clear()?;
            windows::text(text)
        }
        #[cfg(not(windows))]
        {
            use clipboard_rs::Clipboard;
            self.context
                .set_text(text.to_owned())
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    /// Files another computer offered, fetched when a paste asks for them.
    pub fn set_promised_files(
        &self,
        placeholder: &std::sync::Arc<super::Placeholder>,
    ) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            virtual_files::offer(placeholder)
        }
        #[cfg(not(windows))]
        {
            let _ = placeholder;
            anyhow::bail!("offered files can't be pasted on this system")
        }
    }

    /// `image`, with the text that was copied alongside it.
    pub fn set_image(
        &self,
        image: &clipboard_rs::RustImageData,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        self.set_image_with(image, None, text)
    }

    /// `image`, also as the received file at `path` so it can be pasted into
    /// a folder. On Windows the file is offered as a cut, so a paste moves it
    /// out of Seam's cache: never pass a file the person copied themselves.
    pub fn set_image_file(
        &self,
        image: &clipboard_rs::RustImageData,
        path: &std::path::Path,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        self.set_image_with(image, Some(path), text)
    }

    fn set_image_with(
        &self,
        image: &clipboard_rs::RustImageData,
        path: Option<&std::path::Path>,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            // Encode before opening: other apps can't copy while it is open.
            let (png, bitmap) = windows::encode(image)?;
            let _clipboard = windows::open()?;
            windows::clear()?;
            windows::image(&png, &bitmap)?;
            if let Some(path) = path {
                clipboard_win::raw::set_file_list_with(
                    &[path.to_string_lossy()],
                    clipboard_win::options::NoClear,
                )
                .map_err(|e| anyhow::anyhow!("publishing image file: {e}"))?;
                windows::cut()?;
            }
            if let Some(text) = text {
                windows::text(text)?;
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            use clipboard_rs::{Clipboard, ClipboardContent};
            let mut contents = vec![ClipboardContent::Image(image.clone())];
            contents.extend(
                path.map(|path| ClipboardContent::Files(vec![path.to_string_lossy().into_owned()])),
            );
            contents.extend(text.map(|text| ClipboardContent::Text(text.to_owned())));
            self.context
                .set(contents)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    #[cfg(windows)]
    pub fn clear(&self) -> anyhow::Result<()> {
        let _clipboard = windows::open()?;
        windows::clear()
    }

    pub fn generation(&self) -> u64 {
        #[cfg(windows)]
        {
            unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
}

/// Direct clipboard writes. clipboard-rs clears the clipboard again for each
/// image it sets, so it cannot offer an image together with other formats.
#[cfg(windows)]
mod windows {
    use anyhow::{Result, anyhow};
    use clipboard_rs::{
        RustImageData,
        common::{RustImage, RustImageBuffer},
    };
    use clipboard_win::{options::NoClear, raw};

    /// Fails quickly while another app holds the clipboard open; the caller
    /// retries for longer.
    pub fn open() -> Result<clipboard_win::Clipboard> {
        clipboard_win::Clipboard::new_attempts(10)
            .map_err(|e| anyhow!("the clipboard is busy ({e})"))
    }

    pub fn clear() -> Result<()> {
        raw::empty().map_err(|e| anyhow!("clearing clipboard: {e}"))
    }

    pub fn text(text: &str) -> Result<()> {
        raw::set_string_with(text, NoClear).map_err(|e| anyhow!("publishing text: {e}"))
    }

    /// A received image's file waits in Seam's cache. Offered as a cut,
    /// pasting it into a folder moves it there, which on the same drive is a
    /// rename, instead of leaving a second copy behind. Apps such as chat
    /// apps ignore this and read it as before.
    pub fn cut() -> Result<()> {
        // DROPEFFECT_MOVE
        const MOVE: u32 = 2;
        let format = clipboard_win::register_format("Preferred DropEffect")
            .ok_or_else(|| anyhow!("cannot register drop effect clipboard format"))?;
        raw::set_without_clear(format.get(), &MOVE.to_le_bytes())
            .map_err(|e| anyhow!("publishing drop effect: {e}"))
    }

    /// PNG for apps that keep transparency, and a device-independent bitmap
    /// for every other app.
    pub fn encode(image: &RustImageData) -> Result<(RustImageBuffer, RustImageBuffer)> {
        let png = image.to_png().map_err(|e| anyhow!(e.to_string()))?;
        let bitmap = image.to_bitmap().map_err(|e| anyhow!(e.to_string()))?;
        Ok((png, bitmap))
    }

    pub fn image(png: &RustImageBuffer, bitmap: &RustImageBuffer) -> Result<()> {
        // CF_DIB is the bitmap file without its 14-byte file header.
        let dib = bitmap
            .get_bytes()
            .get(14..)
            .ok_or_else(|| anyhow!("invalid bitmap header"))?;
        let png = png.get_bytes();
        let png_format = clipboard_win::register_format("PNG")
            .ok_or_else(|| anyhow!("cannot register PNG clipboard format"))?;
        raw::set_without_clear(png_format.get(), png)
            .map_err(|e| anyhow!("publishing PNG: {e}"))?;
        raw::set_without_clear(clipboard_win::formats::CF_DIB, dib)
            .map_err(|e| anyhow!("publishing bitmap: {e}"))
    }
}

#[cfg(not(target_os = "macos"))]
impl std::ops::Deref for NativeClipboard {
    type Target = clipboard_rs::ClipboardContext;
    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{process::Command, ptr};
    use windows_sys::Win32::{
        System::StationsAndDesktops::*, UI::WindowsAndMessaging::WINSTA_ALL_ACCESS,
    };

    struct PrivateStation {
        original: HWINSTA,
        private: HWINSTA,
    }
    impl PrivateStation {
        /// `None` when Windows refuses a new window station, which needs an
        /// elevated terminal. The test is then skipped rather than run against
        /// the person's real clipboard, unless SEAM_REQUIRE_CLIPBOARD_TESTS is
        /// set (as the release script does).
        fn new() -> Option<Self> {
            let name: Vec<u16> = format!("SeamClipboardTest-{:032x}", rand::random::<u128>())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                let original = GetProcessWindowStation();
                assert!(!original.is_null());
                let private =
                    CreateWindowStationW(name.as_ptr(), 0, WINSTA_ALL_ACCESS as u32, ptr::null());
                if private.is_null() {
                    let error = std::io::Error::last_os_error();
                    assert!(
                        std::env::var_os("SEAM_REQUIRE_CLIPBOARD_TESTS").is_none(),
                        "cannot create isolated clipboard: {error}"
                    );
                    eprintln!(
                        "skipped: an isolated clipboard needs an elevated terminal ({error})"
                    );
                    return None;
                }
                if SetProcessWindowStation(private) == 0 {
                    CloseWindowStation(private);
                    panic!(
                        "cannot select isolated clipboard: {}",
                        std::io::Error::last_os_error()
                    );
                }
                Some(Self { original, private })
            }
        }
    }
    /// Pass on a child test's note that it skipped, which would otherwise be
    /// hidden behind a passing result.
    fn show_skip(output: &std::process::Output) {
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            if line.starts_with("skipped:") {
                eprintln!("{line}");
            }
        }
    }

    impl Drop for PrivateStation {
        fn drop(&mut self) {
            unsafe {
                assert_ne!(SetProcessWindowStation(self.original), 0);
                assert_ne!(CloseWindowStation(self.private), 0);
            }
        }
    }

    #[test]
    fn independent_peers_can_capture_and_replace_images_concurrently() {
        const CHILD: &str = "SEAM_CLIPBOARD_CONCURRENCY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "clipboard::native::tests::independent_peers_can_capture_and_replace_images_concurrently", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "isolated concurrent clipboard test failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            show_skip(&result);
            return;
        }
        let Some(_station) = PrivateStation::new() else {
            return;
        };
        crate::clipboard::tests::assert_independent_peer_image_access();
    }

    #[test]
    fn pasting_received_images_into_a_folder_takes_their_files_off_the_clipboard() {
        const CHILD: &str = "SEAM_CLIPBOARD_PASTE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "clipboard::native::tests::pasting_received_images_into_a_folder_takes_their_files_off_the_clipboard", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "isolated paste test failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            show_skip(&result);
            return;
        }
        let Some(_station) = PrivateStation::new() else {
            return;
        };
        crate::clipboard::tests::assert_pasted_images_leave_the_clipboard();
    }

    #[test]
    fn offered_files_are_fetched_when_pasted() {
        const CHILD: &str = "SEAM_CLIPBOARD_OFFER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "clipboard::native::tests::offered_files_are_fetched_when_pasted",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "isolated offer test failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            show_skip(&result);
            return;
        }
        let Some(_station) = PrivateStation::new() else {
            return;
        };
        crate::clipboard::tests::assert_offered_files_arrive_when_pasted();
    }

    #[test]
    fn recopying_received_text_is_a_new_native_clipboard_update() {
        // A window station has its own clipboard. Use a child test process so
        // selecting it cannot alter any concurrently running test or desktop.
        const CHILD: &str = "SEAM_CLIPBOARD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "clipboard::native::tests::recopying_received_text_is_a_new_native_clipboard_update", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "isolated native test failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            show_skip(&result);
            return;
        }
        let Some(_station) = PrivateStation::new() else {
            return;
        };
        crate::clipboard::tests::assert_image_file_formats(&NativeClipboard::new().unwrap());
        crate::clipboard::tests::assert_native_recopy_is_delivered(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
    }
}
