//! Offered files go on the Windows clipboard as virtual files, the way
//! Remote Desktop offers files from another computer: a list of their names
//! and sizes, and for each file a stream of its contents. File Explorer reads
//! the list whenever the clipboard changes, but asks for contents only when
//! it pastes. The first read of any file fetches them all from the computer
//! they were copied on; Explorer shows its own progress while it waits.
//!
//! The clipboard's data object lives on a thread of its own, which OLE
//! requires to be single-threaded and to keep handling window messages.
use crate::clipboard::Placeholder;
use anyhow::{Result, anyhow, bail};
use std::{
    cell::RefCell,
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
use windows::Win32::{
    Foundation::{
        CloseHandle, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL,
        E_NOTIMPL, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED,
        S_OK, STG_E_ACCESSDENIED, WPARAM,
    },
    System::{
        Com::{
            COWAIT_DISPATCH_CALLS, COWAIT_DISPATCH_WINDOW_MESSAGES, CoWaitForMultipleHandles,
            DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IBindCtx, IDataObject,
            IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream,
            IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGM_READ, STGM_SHARE_DENY_NONE,
            STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, TYMED_HGLOBAL, TYMED_ISTREAM,
        },
        DataExchange::RegisterClipboardFormatW,
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::{DROPEFFECT_COPY, OleInitialize, OleSetClipboard, ReleaseStgMedium},
        Threading::CreateEventW,
    },
    UI::{
        Shell::{
            CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES,
            FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
            IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
            SHCreateStreamOnFileEx,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
            PostMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
        },
    },
};
use windows_core::{BOOL, HRESULT, HSTRING, PCWSTR, Ref, implement, w};

/// Windows stores a virtual file's path in 260 characters, ending in a null.
const MAX_NAME: usize = 259;

/// Puts `placeholder` on the clipboard as virtual files.
pub(super) fn offer(placeholder: &Arc<Placeholder>) -> Result<()> {
    if let Some(entry) = placeholder
        .entries()
        .iter()
        .find(|entry| entry.path.encode_utf16().count() > MAX_NAME)
    {
        bail!("“{}” has too long a path to paste here", entry.path);
    }
    let placeholder = placeholder.clone();
    on_clipboard_thread(move || {
        let object: IDataObject = OfferedFiles::new(placeholder).into();
        unsafe { OleSetClipboard(&object) }
            .map_err(|error| anyhow!("cannot offer files on the clipboard: {error}"))
    })?
}

type Task = Box<dyn FnOnce() + Send>;

/// Work for the clipboard thread, run when its window gets `RUN_TASKS`.
static TASKS: Mutex<VecDeque<Task>> = Mutex::new(VecDeque::new());
const RUN_TASKS: u32 = WM_APP + 1;

fn tasks() -> MutexGuard<'static, VecDeque<Task>> {
    TASKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `task` on the clipboard thread and returns its result.
fn on_clipboard_thread<T: Send + 'static>(task: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let window = window()?;
    let (done, result) = mpsc::channel();
    tasks().push_back(Box::new(move || {
        let _ = done.send(task());
    }));
    unsafe { PostMessageW(Some(window), RUN_TASKS, WPARAM(0), LPARAM(0)) }
        .map_err(|error| anyhow!("cannot reach the clipboard thread: {error}"))?;
    result
        .recv()
        .map_err(|_| anyhow!("the clipboard thread stopped"))
}

/// The clipboard thread's hidden window, which runs its tasks.
fn window() -> Result<HWND> {
    static WINDOW: OnceLock<std::result::Result<usize, String>> = OnceLock::new();
    WINDOW
        .get_or_init(|| {
            let (created, window) = mpsc::channel();
            std::thread::Builder::new()
                .name("clipboard files".into())
                .spawn(move || match start() {
                    Ok(window) => {
                        let _ = created.send(Ok(window.0 as usize));
                        let mut message = MSG::default();
                        while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                            unsafe { DispatchMessageW(&message) };
                        }
                    }
                    Err(error) => {
                        let _ = created.send(Err(format!("{error:#}")));
                    }
                })
                .map_err(|error| error.to_string())?;
            window.recv().map_err(|error| error.to_string())?
        })
        .clone()
        .map(|window| HWND(window as *mut _))
        .map_err(|error| anyhow!("cannot offer files on the clipboard: {error}"))
}

fn start() -> Result<HWND> {
    unsafe {
        OleInitialize(None)?;
        let instance = GetModuleHandleW(None)?;
        let class = w!("SeamClipboardFiles");
        let description = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&description) == 0 {
            bail!(
                "cannot register clipboard window: {}",
                std::io::Error::last_os_error()
            );
        }
        // A message-only window, never shown.
        Ok(CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?)
    }
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == RUN_TASKS {
        // One at a time, without holding the queue: a task may wait while
        // this thread handles other messages, including more tasks.
        while let Some(task) = tasks().pop_front() {
            task();
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn registered(name: PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// What a request for clipboard data asks for.
#[derive(Debug)]
enum Asked {
    List,
    Contents(usize),
    /// Whether a paste copies or moves.
    Effect,
}

fn global(memory: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: memory },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    }
}

/// `bytes` in memory the receiving app frees.
fn memory(bytes: &[u8]) -> windows_core::Result<HGLOBAL> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let start = GlobalLock(memory).cast::<u8>();
        if start.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), start, bytes.len());
        let _ = GlobalUnlock(memory);
        Ok(memory)
    }
}

/// What the clipboard offers: a list of files and folders, and each file's
/// contents.
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct OfferedFiles {
    placeholder: Arc<Placeholder>,
    descriptor: u16,
    contents: u16,
    effect: u16,
    /// Pasting apps ask whether they may read on a thread of their own,
    /// which keeps File Explorer responsive while the files arrive.
    asynchronous: AtomicBool,
    operating: AtomicBool,
}

impl OfferedFiles {
    fn new(placeholder: Arc<Placeholder>) -> Self {
        Self {
            placeholder,
            descriptor: registered(CFSTR_FILEDESCRIPTORW),
            contents: registered(CFSTR_FILECONTENTS),
            effect: registered(CFSTR_PREFERREDDROPEFFECT),
            asynchronous: AtomicBool::new(true),
            operating: AtomicBool::new(false),
        }
    }

    fn formats(&self) -> [FORMATETC; 3] {
        let format = |format, lindex, tymed: i32| FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed as u32,
        };
        [
            format(self.descriptor, -1, TYMED_HGLOBAL.0),
            format(self.contents, -1, TYMED_ISTREAM.0),
            format(self.effect, -1, TYMED_HGLOBAL.0),
        ]
    }

    /// Checks a request and says what it asks for.
    fn check(&self, format: &FORMATETC) -> std::result::Result<Asked, HRESULT> {
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_FORMATETC);
        }
        if format.cfFormat == self.descriptor || format.cfFormat == self.effect {
            if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED);
            }
            return Ok(if format.cfFormat == self.effect {
                Asked::Effect
            } else {
                Asked::List
            });
        }
        if format.cfFormat == self.contents {
            if format.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return Err(DV_E_TYMED);
            }
            let index = usize::try_from(format.lindex).map_err(|_| DV_E_LINDEX)?;
            return match self.placeholder.entries().get(index) {
                Some(entry) if entry.bytes.is_some() => Ok(Asked::Contents(index)),
                _ => Err(DV_E_LINDEX),
            };
        }
        Err(DV_E_FORMATETC)
    }

    /// The list of offered files, as FILEGROUPDESCRIPTORW.
    fn descriptor(&self) -> windows_core::Result<HGLOBAL> {
        let entries = self.placeholder.entries();
        let offset = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
        let size = offset + entries.len() * size_of::<FILEDESCRIPTORW>();
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, size)?;
            let start = GlobalLock(memory).cast::<u8>();
            if start.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(E_FAIL.into());
            }
            start.cast::<u32>().write_unaligned(entries.len() as u32);
            let descriptors = start.add(offset).cast::<FILEDESCRIPTORW>();
            for (index, entry) in entries.iter().enumerate() {
                let mut descriptor = FILEDESCRIPTORW {
                    dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
                    ..Default::default()
                };
                let bytes = match entry.bytes {
                    Some(bytes) => {
                        descriptor.dwFileAttributes = 0x80; // FILE_ATTRIBUTE_NORMAL
                        bytes
                    }
                    None => {
                        descriptor.dwFileAttributes = 0x10; // FILE_ATTRIBUTE_DIRECTORY
                        0
                    }
                };
                descriptor.nFileSizeHigh = (bytes >> 32) as u32;
                descriptor.nFileSizeLow = bytes as u32;
                // Within MAX_NAME, checked when offered. The structure is
                // packed, so its name is filled in whole.
                let mut name = [0u16; 260];
                for (slot, unit) in name
                    .iter_mut()
                    .zip(entry.path.replace('/', "\\").encode_utf16())
                {
                    *slot = unit;
                }
                descriptor.cFileName = name;
                descriptors.add(index).write_unaligned(descriptor);
            }
            let _ = GlobalUnlock(memory);
            Ok(memory)
        }
    }
}

impl IDataObject_Impl for OfferedFiles_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(E_FAIL)?;
        match self.check(format)? {
            Asked::List => Ok(global(self.descriptor()?)),
            // A copy: the offered files stay offered after a paste.
            Asked::Effect => Ok(global(memory(&DROPEFFECT_COPY.0.to_le_bytes())?)),
            Asked::Contents(index) => {
                let stream: IStream = OfferedStream::new(self.placeholder.clone(), index).into();
                Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: std::mem::ManuallyDrop::new(Some(stream)),
                    },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                })
            }
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        match unsafe { format.as_ref() } {
            Some(format) => match self.check(format) {
                Ok(_) => S_OK,
                Err(error) => error,
            },
            None => E_FAIL,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    /// File Explorer reports what it did with the files; nothing to keep.
    fn SetData(
        &self,
        _: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows_core::Result<()> {
        if release.as_bool() && !medium.is_null() {
            unsafe { ReleaseStgMedium(medium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows_core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows_core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl IDataObjectAsyncCapability_Impl for OfferedFiles_Impl {
    fn SetAsyncMode(&self, asynchronous: BOOL) -> windows_core::Result<()> {
        self.asynchronous
            .store(asynchronous.as_bool(), Ordering::Relaxed);
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows_core::Result<BOOL> {
        Ok(self.asynchronous.load(Ordering::Relaxed).into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows_core::Result<()> {
        self.operating.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn InOperation(&self) -> windows_core::Result<BOOL> {
        Ok(self.operating.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows_core::Result<()> {
        self.operating.store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// One offered file's contents. Reading it fetches the offered files, the
/// first time, and then reads the file as it arrived.
#[implement(IStream)]
struct OfferedStream {
    placeholder: Arc<Placeholder>,
    index: usize,
    file: RefCell<Option<IStream>>,
}

impl OfferedStream {
    fn new(placeholder: Arc<Placeholder>, index: usize) -> Self {
        Self {
            placeholder,
            index,
            file: RefCell::new(None),
        }
    }

    fn size(&self) -> u64 {
        self.placeholder.entries()[self.index]
            .bytes
            .unwrap_or_default()
    }

    /// The arrived file, once every offered file has.
    fn file(&self) -> windows_core::Result<IStream> {
        if let Some(file) = self.file.borrow().as_ref() {
            return Ok(file.clone());
        }
        let folder = arrived(&self.placeholder)?;
        let path = self.placeholder.entries()[self.index]
            .path
            .split('/')
            .fold(folder, |path, part| path.join(part));
        let file = unsafe {
            SHCreateStreamOnFileEx(
                &HSTRING::from(path.as_os_str()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )
        }?;
        *self.file.borrow_mut() = Some(file.clone());
        Ok(file)
    }
}

/// Asks for the offered files and waits until they have arrived: the folder
/// holding them. The clipboard thread keeps handling messages and calls
/// meanwhile, so other apps can still copy and paste.
fn arrived(placeholder: &Placeholder) -> windows_core::Result<PathBuf> {
    let never = Event(unsafe { CreateEventW(None, true, false, None) }?);
    loop {
        if let Some(outcome) = placeholder.try_folder() {
            return outcome.map_err(|error| {
                log::warn!("cannot paste offered files: {error:#}");
                windows_core::Error::new(E_FAIL, format!("{error:#}"))
            });
        }
        // Times out after 100 ms, having handled what came in meanwhile.
        let _ = unsafe {
            CoWaitForMultipleHandles(
                (COWAIT_DISPATCH_CALLS.0 | COWAIT_DISPATCH_WINDOW_MESSAGES.0) as u32,
                100,
                &[never.0],
            )
        };
    }
}

struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl ISequentialStream_Impl for OfferedStream_Impl {
    fn Read(&self, buffer: *mut core::ffi::c_void, size: u32, read: *mut u32) -> HRESULT {
        match self.file() {
            Ok(file) => unsafe { file.Read(buffer, size, (!read.is_null()).then_some(read)) },
            Err(error) => error.code(),
        }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for OfferedStream_Impl {
    fn Seek(
        &self,
        distance: i64,
        origin: STREAM_SEEK,
        position: *mut u64,
    ) -> windows_core::Result<()> {
        let file = self.file()?;
        unsafe { file.Seek(distance, origin, (!position.is_null()).then_some(position)) }
    }

    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        target: Ref<IStream>,
        size: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows_core::Result<()> {
        let file = self.file()?;
        unsafe {
            file.CopyTo(
                target.as_ref(),
                size,
                (!read.is_null()).then_some(read),
                (!written.is_null()).then_some(written),
            )
        }
    }

    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    /// Answered from the list, so asking for the size fetches nothing.
    fn Stat(&self, stat: *mut STATSTG, _: &STATFLAG) -> windows_core::Result<()> {
        let stat = unsafe { stat.as_mut() }.ok_or(E_FAIL)?;
        *stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size(),
            grfMode: STGM_READ,
            ..Default::default()
        };
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// Reads the clipboard as File Explorer pastes virtual files: the list, and
/// then the contents of the files in it.
#[cfg(test)]
pub(in crate::clipboard) mod paste {
    use super::*;
    use windows::Win32::System::Ole::OleGetClipboard;

    pub struct Clipboard(IDataObject);

    impl Clipboard {
        /// On a thread that called `OleInitialize`.
        pub fn open() -> Self {
            Self(unsafe { OleGetClipboard() }.unwrap())
        }

        fn get(&self, format: u16, lindex: i32, tymed: i32) -> windows_core::Result<STGMEDIUM> {
            let format = FORMATETC {
                cfFormat: format,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex,
                tymed: tymed as u32,
            };
            unsafe { self.0.GetData(&format) }
        }

        /// The listed paths, `None` for files that aren't virtual.
        pub fn listed(&self) -> Option<Vec<String>> {
            let mut medium = self
                .get(registered(CFSTR_FILEDESCRIPTORW), -1, TYMED_HGLOBAL.0)
                .ok()?;
            let mut paths = Vec::new();
            unsafe {
                let start = GlobalLock(medium.u.hGlobal).cast::<u8>();
                let count = start.cast::<u32>().read_unaligned() as usize;
                let offset = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
                for index in 0..count {
                    let descriptor = start
                        .add(offset)
                        .cast::<FILEDESCRIPTORW>()
                        .add(index)
                        .read_unaligned();
                    let name = descriptor.cFileName;
                    let end = name.iter().position(|&unit| unit == 0).unwrap_or(260);
                    paths.push(String::from_utf16_lossy(&name[..end]));
                }
                let _ = GlobalUnlock(medium.u.hGlobal);
                ReleaseStgMedium(&mut medium);
            }
            Some(paths)
        }

        /// The contents of listed file `index`, read to the end.
        pub fn contents(&self, index: i32) -> windows_core::Result<Vec<u8>> {
            let mut medium = self.get(registered(CFSTR_FILECONTENTS), index, TYMED_ISTREAM.0)?;
            let stream = unsafe { medium.u.pstm.as_ref() }.cloned().ok_or(E_FAIL)?;
            unsafe { ReleaseStgMedium(&mut medium) };
            let mut contents = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let mut read = 0;
                unsafe { stream.Read(chunk.as_mut_ptr().cast(), 4096, Some(&mut read)) }.ok()?;
                if read == 0 {
                    return Ok(contents);
                }
                contents.extend_from_slice(&chunk[..read as usize]);
            }
        }
    }
}
