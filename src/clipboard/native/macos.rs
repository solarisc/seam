use crate::clipboard::Placeholder;
use anyhow::{Result, ensure};
use clipboard_rs::{ContentFormat, RustImageData, common::RustImage};
use objc2::{
    AllocAnyThread, ClassType, DefinedClass, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{
    NSImage, NSPasteboard, NSPasteboardContentsOptions, NSPasteboardItem,
    NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use std::sync::{Arc, Mutex};

// AppKit caches pasteboard objects and type conversions inside the process.
// All access, including lookup and release, shares this boundary. The receipt
// mutex in clipboard.rs protects protocol ordering, not native object access.
static NATIVE_ACCESS: Mutex<()> = Mutex::new(());

fn with_native<T: Send>(operation: impl FnOnce() -> T) -> T {
    let _access = NATIVE_ACCESS.lock().unwrap();
    autoreleasepool(|_| operation())
}

pub(in crate::clipboard) struct NativeClipboard {
    // Keep only the board's identity across threads. Native objects are obtained,
    // used and released inside with_native; no unsafe Send promise is needed.
    name: String,
}

impl NativeClipboard {
    pub fn new() -> Result<Self> {
        Ok(with_native(|| Self {
            name: NSPasteboard::generalPasteboard().name().to_string(),
        }))
    }

    fn with_board<T: Send>(&self, operation: impl FnOnce(&NSPasteboard) -> T) -> T {
        with_native(|| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&self.name));
            operation(&board)
        })
    }

    pub fn generation(&self) -> u64 {
        self.with_board(|board| board.changeCount() as u64)
    }

    pub fn has(&self, format: ContentFormat) -> bool {
        self.with_board(|board| match format {
            ContentFormat::Image => {
                let types = NSArray::from_slice(&[unsafe { NSPasteboardTypePNG }, unsafe {
                    NSPasteboardTypeTIFF
                }]);
                board.availableTypeFromArray(&types).is_some()
            }
            _ => false,
        })
    }

    pub fn get_text(&self) -> Result<String> {
        self.with_board(|board| {
            board
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|text| text.to_string())
                .ok_or_else(|| anyhow::anyhow!("no clipboard text"))
        })
    }

    pub fn get_files(&self) -> Result<Vec<String>> {
        self.with_board(|board| {
            let classes = NSArray::arrayWithObject(NSURL::class());
            let mut paths = Vec::new();
            // Only NSURL is requested; each returned object is checked before use.
            if let Some(objects) = unsafe { board.readObjectsForClasses_options(&classes, None) } {
                for object in objects {
                    if let Ok(url) = object.downcast::<NSURL>()
                        && url.isFileURL()
                        && let Some(path) = url.path()
                    {
                        paths.push(path.to_string());
                    }
                }
            }
            Ok(paths)
        })
    }

    pub fn get_image(&self) -> Result<RustImageData> {
        let bytes = self.with_board(|board| {
            let bytes = board
                .dataForType(unsafe { NSPasteboardTypePNG })
                .or_else(|| {
                    NSImage::initWithPasteboard(NSImage::alloc(), board)?.TIFFRepresentation()
                })
                .ok_or_else(|| anyhow::anyhow!("no clipboard image"))?;
            Ok::<_, anyhow::Error>(bytes.to_vec())
        })?;
        // Image decoding can be expensive; it doesn't need native ownership.
        RustImageData::from_bytes(&bytes).map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    fn write(
        board: &NSPasteboard,
        objects: &[Retained<ProtocolObject<dyn NSPasteboardWriting>>],
    ) -> Result<()> {
        let objects = NSArray::from_retained_slice(objects);
        board.clearContents();
        ensure!(board.writeObjects(&objects), "clipboard write failed");
        Ok(())
    }

    pub fn set_text(&self, value: &str) -> Result<()> {
        self.with_board(|board| {
            let item = NSPasteboardItem::new();
            ensure!(
                item.setString_forType(&NSString::from_str(value), unsafe {
                    NSPasteboardTypeString
                }),
                "cannot prepare clipboard text"
            );
            Self::write(board, &[ProtocolObject::from_retained(item)])
        })
    }

    /// `image`, with the text that was copied alongside it.
    pub fn set_image(&self, image: &RustImageData, text: Option<&str>) -> Result<()> {
        self.set_image_with(image, None, text)
    }

    /// `image`, also as the file at `path` so it can be pasted into a folder.
    pub fn set_image_file(
        &self,
        image: &RustImageData,
        path: &std::path::Path,
        text: Option<&str>,
    ) -> Result<()> {
        self.set_image_with(image, Some(path), text)
    }

    fn set_image_with(
        &self,
        image: &RustImageData,
        path: Option<&std::path::Path>,
        text: Option<&str>,
    ) -> Result<()> {
        // Encode before clearing the clipboard, so conversion failure preserves it.
        let png = image.to_png().map_err(|e| anyhow::anyhow!(e.to_string()))?;
        self.with_board(|board| {
            let item = NSPasteboardItem::new();
            ensure!(
                item.setData_forType(&NSData::with_bytes(png.get_bytes()), unsafe {
                    NSPasteboardTypePNG
                }),
                "cannot prepare clipboard image"
            );
            if let Some(path) = path {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                let url_string = url
                    .absoluteString()
                    .ok_or_else(|| anyhow::anyhow!("invalid image URL"))?;
                ensure!(
                    item.setString_forType(&url_string, &NSString::from_str("public.file-url")),
                    "cannot prepare image file"
                );
            }
            if let Some(text) = text {
                ensure!(
                    item.setString_forType(&NSString::from_str(text), unsafe {
                        NSPasteboardTypeString
                    }),
                    "cannot prepare clipboard text"
                );
            }
            Self::write(board, &[ProtocolObject::from_retained(item)])
        })
    }

    /// Files another computer offered: one item for each, whose file is
    /// fetched when a paste asks for it.
    pub fn set_promised_files(&self, placeholder: &Arc<Placeholder>) -> Result<()> {
        self.with_board(|board| {
            let providers: Vec<Retained<FileProvider>> = (0..placeholder.count())
                .map(|index| FileProvider::new(index, placeholder.clone()))
                .collect();
            let types = NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]);
            let mut items = Vec::with_capacity(providers.len());
            for provider in &providers {
                let item = NSPasteboardItem::new();
                ensure!(
                    item.setDataProvider_forTypes(ProtocolObject::from_ref(&**provider), &types),
                    "cannot prepare offered files"
                );
                items.push(ProtocolObject::from_retained(item));
            }
            // Universal Clipboard would otherwise read the new contents at
            // once, fetching every offered file. This also clears the board.
            board.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
            ensure!(
                board.writeObjects(&NSArray::from_retained_slice(&items)),
                "clipboard write failed"
            );
            // Items don't keep their providers alive.
            *PROVIDERS.lock().unwrap() = Providers(providers);
            Ok(())
        })
    }

    #[cfg(test)]
    pub fn set_files(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            anyhow::bail!("empty clipboard file list");
        }
        self.with_board(|board| {
            let mut urls = Vec::with_capacity(paths.len());
            for path in paths {
                ensure!(
                    std::path::Path::new(path).exists(),
                    "received clipboard file is unavailable"
                );
                urls.push(ProtocolObject::from_retained(NSURL::fileURLWithPath(
                    &NSString::from_str(path),
                )));
            }
            Self::write(board, &urls)
        })
    }
}

/// The providers of the offered files on the clipboard, replaced by the
/// next offer.
static PROVIDERS: Mutex<Providers> = Mutex::new(Providers(Vec::new()));
struct Providers(Vec<Retained<FileProvider>>);
// Retain and release are thread-safe; the providers are only created and
// replaced inside the native access boundary and called on the main thread.
unsafe impl Send for Providers {}

struct FileProviderIvars {
    /// Which of the offered items this provides.
    index: usize,
    placeholder: Arc<Placeholder>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and FileProvider
    // does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "SeamOfferedFileProvider"]
    #[ivars = FileProviderIvars]
    struct FileProvider;

    unsafe impl NSObjectProtocol for FileProvider {}

    unsafe impl NSPasteboardItemDataProvider for FileProvider {
        /// A paste asked for this item's file. Runs on the main thread,
        /// which waits until the offered files have arrived.
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            kind: &NSPasteboardType,
        ) {
            let file_url = unsafe { NSPasteboardTypeFileURL };
            if !kind.isEqualToString(file_url) {
                return;
            }
            let ivars = self.ivars();
            let Some(files) = ivars.placeholder.files() else {
                return;
            };
            // Links among the copied items aren't sent, so fewer may arrive.
            let Some(path) = files.get(ivars.index) else {
                return;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            if let Some(url) = url.absoluteString() {
                with_native(|| item.setString_forType(&url, file_url));
            }
        }

        /// The clipboard holds something else now, or every file was
        /// provided: let go of the offer.
        #[unsafe(method(pasteboardFinishedWithDataProvider:))]
        fn finished(&self, _pasteboard: &NSPasteboard) {
            let released = {
                let mut providers = PROVIDERS.lock().unwrap();
                let (released, kept) = std::mem::take(&mut providers.0)
                    .into_iter()
                    .partition(|provider| std::ptr::eq(&**provider, self));
                providers.0 = kept;
                released
            };
            drop::<Vec<Retained<FileProvider>>>(released);
        }
    }
);

impl FileProvider {
    fn new(index: usize, placeholder: Arc<Placeholder>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FileProviderIvars { index, placeholder });
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PrivateBoard(NativeClipboard);
    impl PrivateBoard {
        fn clipboard(&self) -> NativeClipboard {
            NativeClipboard {
                name: self.0.name.clone(),
            }
        }
        fn new() -> Self {
            Self(with_native(|| NativeClipboard {
                name: NSPasteboard::pasteboardWithUniqueName().name().to_string(),
            }))
        }
    }
    impl Drop for PrivateBoard {
        fn drop(&mut self) {
            // Cleanup selector remains supported for uniquely named boards.
            self.0.with_board(|board| {
                let _: () = unsafe { objc2::msg_send![board, releaseGlobally] };
            });
        }
    }

    #[test]
    fn recopying_received_text_is_a_new_native_clipboard_update() {
        let board = PrivateBoard::new();
        crate::clipboard::tests::assert_native_recopy_is_delivered(
            board.clipboard(),
            board.clipboard(),
        );
    }

    #[test]
    fn concurrent_capture_does_not_echo_peer_clipboard_writes() {
        let board = PrivateBoard::new();
        crate::clipboard::tests::assert_concurrent_native_readback_stays_quiet(
            board.clipboard(),
            board.clipboard(),
        );
    }

    #[test]
    fn native_clipboard_preserves_unicode_pixels_and_file_urls() {
        let board = PrivateBoard::new();
        let clipboard = board.clipboard();
        crate::clipboard::tests::assert_image_file_formats(&clipboard);
        let text = "Line one\n字 🖱️";
        clipboard.set_text(text).unwrap();
        assert_eq!(clipboard.get_text().unwrap(), text);

        let pixels = [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0];
        let image = RustImageData::from_dynamic_image(::image::DynamicImage::ImageRgba8(
            ::image::RgbaImage::from_raw(3, 1, pixels.to_vec()).unwrap(),
        ));
        clipboard.set_image(&image, None).unwrap();
        assert!(clipboard.has(ContentFormat::Image));
        let readback = clipboard.get_image().unwrap();
        assert_eq!(readback.get_size(), (3, 1));
        assert_eq!(readback.to_rgba8().unwrap().as_raw(), &pixels);

        // Native Mac applications also publish TIFF without a PNG flavor.
        // Let AppKit produce that representation instead of relying on our
        // decoder to generate its own expected fixture.
        let opaque = [18, 45, 200, 255, 10, 210, 70, 255];
        clipboard
            .set_image(
                &RustImageData::from_dynamic_image(::image::DynamicImage::ImageRgba8(
                    ::image::RgbaImage::from_raw(2, 1, opaque.to_vec()).unwrap(),
                )),
                None,
            )
            .unwrap();
        clipboard.with_board(|board| {
            let native_image = NSImage::initWithPasteboard(NSImage::alloc(), board).unwrap();
            let tiff = native_image.TIFFRepresentation().unwrap();
            let item = NSPasteboardItem::new();
            assert!(item.setData_forType(&tiff, unsafe { NSPasteboardTypeTIFF }));
            NativeClipboard::write(board, &[ProtocolObject::from_retained(item)]).unwrap();
        });
        let readback = clipboard
            .get_image()
            .expect("a native TIFF-only clipboard must be readable");
        assert_eq!(readback.get_size(), (2, 1));
        assert_eq!(readback.to_rgba8().unwrap().as_raw(), &opaque);

        let folder = tempfile::tempdir().unwrap();
        let paths: Vec<String> = ["space name.txt", "字#%.txt"]
            .iter()
            .map(|name| {
                let path = folder.path().join(name);
                std::fs::write(&path, []).unwrap();
                path.to_str().unwrap().to_owned()
            })
            .collect();
        clipboard.set_files(&paths).unwrap();
        assert_eq!(clipboard.get_files().unwrap(), paths);
    }
}
