use crate::transfers::{self, Transfer};
use crate::{
    config::Config,
    network::{SecureReader, SecureWriter},
    protocol::{Data, FILE_CHUNK_SIZE},
};
use anyhow::{Context, Result, bail, ensure};
#[cfg(not(target_os = "macos"))]
use clipboard_rs::Clipboard;
use clipboard_rs::{ClipboardHandler, ContentFormat};
#[cfg(not(target_os = "macos"))]
use clipboard_rs::{ClipboardWatcher, ClipboardWatcherContext};
use rand::{RngCore, rngs::OsRng};
use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use walkdir::WalkDir;

mod files;
mod image;
mod native;
mod outgoing;
mod pacing;
mod received;
mod relay;
use image::PreparedImage;
use native::NativeClipboard;
use outgoing::Outgoing;
use pacing::BulkPacer;
pub(crate) use pacing::InputSignals;

const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRANSFER_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_TRANSFER_FILES: usize = 100_000;

#[derive(Clone)]
enum Job {
    /// A transfer from the peer arrived.
    Receipt(u64),
    /// A transfer from the peer couldn't be taken, for `reason`.
    Refusal {
        id: u64,
        reason: String,
    },
    Text(String),
    Image {
        png: Vec<u8>,
        /// Text copied with the image, such as a spreadsheet selection's cells.
        text: Option<String>,
    },
    Files {
        paths: Vec<PathBuf>,
        /// A relayed copy's folder stays until it has been sent on.
        received: Option<Arc<received::Item>>,
    },
}

struct Applied {
    generation: u64,
    digest: [u8; 32],
    /// The received files or image the clipboard refers to.
    received: Option<Arc<received::Item>>,
}

impl Applied {
    fn matches(&self, generation: u64, digest: [u8; 32]) -> bool {
        self.generation == generation && self.digest == digest
    }
}

struct Handler {
    ctx: NativeClipboard,
    outgoing: Arc<Outgoing>,
    applied: Arc<Mutex<Option<Applied>>>,
    config: Config,
    cancel: Arc<crate::shutdown::Signal>,
}

impl Handler {
    fn capture(&self) -> Result<()> {
        if self.cancel.is_stopped() {
            return Ok(());
        }
        #[cfg(windows)]
        let _native = native::transaction();
        let (generation, replaced) = {
            let mut applied = self.applied.lock().unwrap();
            let generation = self.ctx.generation();
            // The OS generation changes on a new copy, including a re-copy of
            // identical content. Don't decode/hash an image we just installed.
            if generation != 0 && applied.as_ref().is_some_and(|a| a.generation == generation) {
                return Ok(());
            }
            // A copy made here replaced the received files on the clipboard.
            let replaced = applied
                .as_mut()
                .filter(|_| generation != 0)
                .and_then(|applied| applied.received.take());
            (generation, replaced)
        };
        if let Some(item) = replaced {
            received::retire(item);
        }
        if self.config.files {
            let files = self.ctx.get_files().unwrap_or_default();
            if !files.is_empty() {
                let paths: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
                if !paths.is_empty() {
                    let digest = digest_paths(&paths);
                    if !self.was_applied(digest, generation) {
                        self.publish(
                            generation,
                            digest,
                            Job::Files {
                                paths,
                                received: None,
                            },
                        );
                    }
                    return Ok(());
                }
            }
        }
        if self.config.images
            && self.ctx.has(ContentFormat::Image)
            && let Ok(img) = self.ctx.get_image()
        {
            let image = PreparedImage::from_native(img)?;
            // Spreadsheet cells and slides are copied as text and a picture;
            // send both so either can be pasted.
            let text = self
                .config
                .clipboard
                .then(|| self.ctx.get_text().ok())
                .flatten()
                .filter(|text| !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES);
            let digest = image_digest(image.identity, text.as_deref());
            if self.was_applied(digest, generation) {
                return Ok(());
            }
            let png = image.into_wire()?;
            if png.len() <= MAX_CLIPBOARD_BYTES {
                self.publish(generation, digest, Job::Image { png, text });
                return Ok(());
            }
        }
        if self.config.clipboard
            && let Ok(value) = self.ctx.get_text()
            && !value.is_empty()
            && value.len() <= MAX_CLIPBOARD_BYTES
        {
            let digest = *blake3::hash(value.as_bytes()).as_bytes();
            if !self.was_applied(digest, generation) {
                self.publish(generation, digest, Job::Text(value));
            }
        }
        Ok(())
    }
    fn was_applied(&self, digest: [u8; 32], generation: u64) -> bool {
        self.applied
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|applied| applied.matches(generation, digest))
    }
    fn publish(&self, generation: u64, digest: [u8; 32], job: Job) {
        let applied = self.applied.lock().unwrap();
        if self.ctx.generation() == generation
            && !applied
                .as_ref()
                .is_some_and(|applied| applied.matches(generation, digest))
        {
            self.outgoing.submit(job);
        }
    }
}

impl ClipboardHandler for Handler {
    fn on_clipboard_change(&mut self) {
        if let Err(error) = self.capture() {
            log::warn!("clipboard capture: {error:#}");
        }
    }
}

pub(crate) fn run_session(
    mut reader: SecureReader,
    mut writer: SecureWriter,
    config: Config,
    controls_peer: bool,
    parent_stop: Arc<crate::shutdown::Signal>,
    signals: InputSignals,
) -> Result<()> {
    let cancel = Arc::new(crate::shutdown::Signal::default());
    reader.set_cancellation(cancel.clone())?;
    let child = cancel.clone();
    let _parent = parent_stop.register(move || child.stop());
    let socket = reader.cancellation_socket()?;
    let _socket = cancel.register(move || {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    });
    let _finish = cancel.stop_on_drop();
    static APPLIED: std::sync::OnceLock<Arc<Mutex<Option<Applied>>>> = std::sync::OnceLock::new();
    let applied = APPLIED.get_or_init(|| Arc::new(Mutex::new(None))).clone();
    let context = transfers::Context {
        session: new_id(),
        peer: signals.peer_name(),
    };
    let outgoing = Arc::new(Outgoing::new());
    let _relay = relay::Registration::new(context.session, &outgoing, &config, controls_peer);
    let ctx = NativeClipboard::new()?;
    let handler = Handler {
        ctx,
        outgoing: outgoing.clone(),
        applied: applied.clone(),
        config: config.clone(),
        cancel: cancel.clone(),
    };
    // Sample before starting the watcher so a copy made during setup is observed.
    // Every Mac poll uses the same native access boundary.
    #[cfg(target_os = "macos")]
    let mut generation = handler.ctx.generation();
    // Observe new copies only: reconnecting must not re-upload a stale multi-GB selection.
    #[cfg(not(target_os = "macos"))]
    let (mut watcher, _watcher_stop) = {
        let mut watcher =
            ClipboardWatcherContext::new().map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let shutdown = watcher.add_handler(handler).get_shutdown_channel();
        let shutdown = Mutex::new(Some(shutdown));
        let stop = cancel.register(move || {
            if let Some(shutdown) = shutdown.lock().unwrap().take() {
                shutdown.stop();
            }
        });
        (watcher, stop)
    };
    // Complete fallible setup before starting either worker.
    let mut incoming = Incoming::new(applied, config)?;
    incoming.context = context.clone();
    let replies = outgoing.clone();
    let sending_context = context.clone();
    let watcher_cancel = cancel.clone();
    let watcher_thread = std::thread::spawn(move || {
        let _finish = watcher_cancel.stop_on_drop();
        #[cfg(target_os = "macos")]
        {
            let mut handler = handler;
            // macOS has no pasteboard-change notification. Preserve the existing
            // 500 ms cadence and wake immediately when the session is cancelled.
            while !watcher_cancel.wait(std::time::Duration::from_millis(500)) {
                let next = handler.ctx.generation();
                if next != generation {
                    handler.on_clipboard_change();
                    generation = next;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        watcher.start_watch();
    });
    let writer_cancel = cancel.clone();
    let writer_thread = std::thread::spawn(move || {
        let _finish = writer_cancel.stop_on_drop();
        send_jobs(
            &mut writer,
            outgoing,
            &writer_cancel,
            signals,
            sending_context,
        )
    });
    let result = loop {
        let message = match reader.read::<Data>() {
            Ok(message) => message,
            Err(e) => break Err(e),
        };
        if cancel.is_stopped() {
            break Ok(());
        }
        match message {
            Data::TransferReceipt { id } => transfers::receipt(context.session, id),
            Data::TransferFailed { id, reason } => {
                log::warn!("{} could not take a copy: {reason}", context.peer);
                replies.refused(id);
                transfers::refused(context.session, id, &reason);
            }
            message => match incoming.apply(message) {
                Ok(Some(reply)) => replies.reply(reply),
                Ok(None) => {}
                // The peer broke the protocol; nothing it sends can be trusted.
                Err(e) => {
                    if let Some(progress) = &mut incoming.progress {
                        progress.fail(&e.to_string());
                    }
                    break Err(e);
                }
            },
        }
    };
    cancel.stop();
    transfers::disconnect(context.session);
    let watched = watcher_thread.join();
    let written = writer_thread.join();
    if watched.is_err() || written.is_err() {
        bail!("clipboard worker failed");
    }
    result
}

fn send_jobs(
    writer: &mut SecureWriter,
    outgoing: Arc<Outgoing>,
    cancel: &Arc<crate::shutdown::Signal>,
    signals: InputSignals,
    context: transfers::Context,
) -> Result<()> {
    let closed = outgoing.clone();
    let _stop = cancel.register(move || closed.close());
    let mut pacer = BulkPacer::new(signals);
    while let Some(job) = outgoing.recv() {
        if cancel.is_stopped() {
            break;
        }
        match job {
            Job::Receipt(id) => writer.write(&Data::TransferReceipt { id })?,
            Job::Refusal { id, reason } => writer.write(&Data::TransferFailed { id, reason })?,
            Job::Text(value) if value.len() < 60_000 => writer.write(&Data::Text(value))?,
            Job::Text(value) => send_blob(
                writer,
                BlobKind::Text,
                value.into_bytes(),
                &mut pacer,
                &context,
            )?,
            Job::Image { png, text } => {
                send_blob(writer, BlobKind::Image { text }, png, &mut pacer, &context)?
            }
            Job::Files {
                paths,
                received: _received,
            } => send_files(writer, paths, &outgoing, cancel, &mut pacer, &context)?,
        }
    }
    Ok(())
}

enum BlobKind {
    Text,
    Image { text: Option<String> },
}

fn new_id() -> u64 {
    OsRng.next_u64()
}

/// An image copy's identity, including the text copied with it.
fn image_digest(identity: [u8; 32], text: Option<&str>) -> [u8; 32] {
    let Some(text) = text else {
        return identity;
    };
    let mut hash = blake3::Hasher::new();
    hash.update(&identity);
    hash.update(text.as_bytes());
    *hash.finalize().as_bytes()
}

fn send_blob(
    writer: &mut SecureWriter,
    kind: BlobKind,
    bytes: Vec<u8>,
    pacer: &mut BulkPacer,
    context: &transfers::Context,
) -> Result<()> {
    let id = new_id();
    let mut progress = matches!(kind, BlobKind::Image { .. }).then(|| {
        Transfer::start(
            context,
            id,
            true,
            "Clipboard image.png".into(),
            bytes.len() as u64,
        )
    });
    if let Some(p) = &mut progress {
        p.total(bytes.len() as u64);
    }
    match &kind {
        BlobKind::Text => writer.write(&Data::TextStart {
            id,
            bytes: bytes.len() as u64,
        })?,
        BlobKind::Image { text } => {
            writer.write(&Data::ImageStart {
                id,
                bytes: bytes.len() as u64,
            })?;
            for chunk in text
                .iter()
                .flat_map(|text| text.as_bytes().chunks(FILE_CHUNK_SIZE))
            {
                writer.write(&Data::ImageTextChunk {
                    id,
                    bytes: chunk.to_vec(),
                })?;
            }
        }
    }
    for chunk in bytes.chunks(FILE_CHUNK_SIZE) {
        match kind {
            BlobKind::Text => writer.write(&Data::TextChunk {
                id,
                bytes: chunk.to_vec(),
            })?,
            BlobKind::Image { .. } => writer.write(&Data::ImageChunk {
                id,
                bytes: chunk.to_vec(),
            })?,
        }
        pacer.after_chunk(chunk.len());
        if let Some(p) = &mut progress {
            p.advance(chunk.len());
        }
    }
    match kind {
        BlobKind::Text => writer.write(&Data::TextEnd { id })?,
        BlobKind::Image { .. } => writer.write(&Data::ImageEnd { id })?,
    }
    if let Some(p) = progress {
        p.sent();
    }
    Ok(())
}

fn send_files(
    writer: &mut SecureWriter,
    roots: Vec<PathBuf>,
    outgoing: &Outgoing,
    cancel: &crate::shutdown::Signal,
    pacer: &mut BulkPacer,
    context: &transfers::Context,
) -> Result<()> {
    let id = new_id();
    let mut progress = Transfer::start(context, id, true, "Preparing files…".into(), 0);
    let (entries, total) = match list_files(roots, cancel) {
        Ok(listed) => listed,
        Err(error) => {
            log::warn!("cannot send copied files: {error:#}");
            progress.fail(&format!("{error:#}"));
            return Ok(());
        }
    };
    progress.total(total);
    writer.write(&Data::BatchStart {
        id,
        count: entries.len() as u32,
        bytes: total,
    })?;
    match send_entries(writer, entries, outgoing, cancel, pacer, id, &mut progress)? {
        None => {
            progress.finishing();
            writer.write(&Data::BatchEnd { id })?;
            progress.sent();
        }
        Some(stop) => {
            writer.write(&Data::BatchAbort { id })?;
            match stop {
                Stopped::Peer(outgoing::Stop::Refused) => progress.release(),
                Stopped::Peer(outgoing::Stop::Superseded) => {
                    progress.skip("Replaced by a newer copy")
                }
                Stopped::Local(error) => {
                    log::warn!("stopped sending copied files: {error:#}");
                    progress.fail(&format!("{error:#}"));
                }
            }
        }
    }
    Ok(())
}

/// Why an upload stopped while its connection still works.
enum Stopped {
    Peer(outgoing::Stop),
    /// A file couldn't be read or changed while it was sent.
    Local(anyhow::Error),
}

/// A file to send, or a folder when it has no size.
type Entry = (PathBuf, String, Option<u64>);

/// Everything under `roots`, checked before any of it is announced.
fn list_files(roots: Vec<PathBuf>, cancel: &crate::shutdown::Signal) -> Result<(Vec<Entry>, u64)> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut top_names = HashSet::new();
    let mut links = 0;
    for root in roots {
        // Links, including Windows junctions such as "My Music" inside
        // Documents, point at content that lives elsewhere. Skip them, as
        // following them could copy whole drives or loop forever.
        if root.is_symlink() {
            links += 1;
            continue;
        }
        let name = root
            .file_name()
            .context("A selected item has no name")?
            .to_string_lossy()
            .into_owned();
        if !root.exists() {
            bail!("“{name}” is no longer there");
        }
        if !top_names.insert(name.clone()) {
            bail!("Two selected items are both named “{name}”");
        }
        let parent = root.parent().context("A selected item has no folder")?;
        for entry in WalkDir::new(&root).follow_links(false) {
            if cancel.is_stopped() {
                bail!("file transfer cancelled");
            }
            let entry = entry?;
            if entry.path_is_symlink() {
                links += 1;
                continue;
            }
            let relative = entry.path().strip_prefix(parent)?;
            let wire_path = relative_to_wire(relative)?;
            if entry.file_type().is_dir() {
                entries.push((entry.path().to_path_buf(), wire_path, None));
            } else if entry.file_type().is_file() {
                let size = entry.metadata()?.len();
                total = total.checked_add(size).context("transfer size overflow")?;
                if total > MAX_TRANSFER_BYTES {
                    bail!("Copies larger than 1 TB can't be sent");
                }
                entries.push((entry.path().to_path_buf(), wire_path, Some(size)));
            } else {
                bail!(
                    "“{}” is not a file or folder",
                    entry.file_name().to_string_lossy()
                );
            }
            if entries.len() > MAX_TRANSFER_FILES {
                bail!("Copies of more than 100,000 files and folders can't be sent");
            }
        }
    }
    if links > 0 {
        log::info!("not sending {links} links among copied files");
    }
    if entries.is_empty() {
        bail!("Nothing to send: links to other places are skipped");
    }
    Ok((entries, total))
}

/// Sends every entry of batch `id`, unless something stops it first.
fn send_entries(
    writer: &mut SecureWriter,
    entries: Vec<Entry>,
    outgoing: &Outgoing,
    cancel: &crate::shutdown::Signal,
    pacer: &mut BulkPacer,
    id: u64,
    progress: &mut Transfer,
) -> Result<Option<Stopped>> {
    // SecureWriter serializes synchronously, so the same payload allocation can
    // hold every chunk, including the final short chunk of each file.
    let mut chunk = Data::FileChunk {
        id,
        bytes: Vec::with_capacity(FILE_CHUNK_SIZE),
    };
    for (path, wire_path, size) in entries {
        if let Some(stop) = outgoing.stop(id) {
            return Ok(Some(Stopped::Peer(stop)));
        }
        progress.name(&wire_path);
        let Some(size) = size else {
            writer.write(&Data::Directory {
                id,
                path: wire_path,
            })?;
            continue;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let unreadable = |error: std::io::Error| {
            Stopped::Local(anyhow::Error::new(error).context(format!("Cannot read “{name}”")))
        };
        let changed =
            || Stopped::Local(anyhow::anyhow!("“{name}” changed while it was being sent"));
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => return Ok(Some(unreadable(error))),
        };
        writer.write(&Data::FileStart {
            id,
            path: wire_path,
            bytes: size,
        })?;
        let mut sent = 0u64;
        loop {
            if cancel.is_stopped() {
                bail!("file transfer cancelled");
            }
            let read = {
                let Data::FileChunk { bytes, .. } = &mut chunk else {
                    unreachable!("file transfer owns a FileChunk frame")
                };
                bytes.resize(FILE_CHUNK_SIZE, 0);
                let read = file.read(bytes);
                bytes.truncate(*read.as_ref().unwrap_or(&0));
                read
            };
            let n = match read {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) => return Ok(Some(unreadable(error))),
            };
            if sent + n as u64 > size {
                return Ok(Some(changed()));
            }
            writer.write(&chunk)?;
            pacer.after_chunk(n);
            sent += n as u64;
            progress.advance(n);
            if let Some(stop) = outgoing.stop(id) {
                return Ok(Some(Stopped::Peer(stop)));
            }
        }
        if sent != size {
            return Ok(Some(changed()));
        }
        writer.write(&Data::FileEnd { id })?;
    }
    Ok(None)
}

fn relative_to_wire(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(value) = component else {
            bail!("unsafe path");
        };
        let part = value.to_str().with_context(|| {
            format!(
                "“{}” has a name other computers can't read",
                value.to_string_lossy()
            )
        })?;
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '/', '\0']) {
            bail!("“{part}” has a character other computers don't allow in names");
        }
        parts.push(part.to_owned());
    }
    if parts.is_empty() {
        bail!("empty path");
    }
    Ok(parts.join("/"))
}

fn safe_relative(path: &str) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    if path.len() > 4096 {
        bail!("path too long");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '/', '\0']) {
            bail!("unsafe incoming path");
        }
        #[cfg(windows)]
        validate_windows_name(part)?;
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        bail!("empty path");
    }
    Ok(result)
}

#[cfg(windows)]
fn validate_windows_name(part: &str) -> Result<()> {
    // Win32 normalizes trailing spaces/dots and interprets reserved device
    // names even with extensions. Reject before creating any directories so
    // received paths identify the same names on disk and in the shell.
    // https://learn.microsoft.com/windows/win32/fileio/naming-a-file
    if part.ends_with([' ', '.'])
        || part
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        bail!("“{part}” isn't a valid file name on Windows");
    }
    let stem = part.split('.').next().unwrap().trim_end_matches(' ');
    let reserved = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.get(..3)
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                && matches!(
                    stem.get(3..),
                    Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
                )
        });
    if reserved {
        bail!("“{part}” is a name Windows reserves for devices");
    }
    Ok(())
}

fn digest_paths(paths: &[PathBuf]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for p in paths {
        hash.update(p.to_string_lossy().as_bytes());
        hash.update(&[0]);
    }
    *hash.finalize().as_bytes()
}

struct Blob {
    id: u64,
    expected: u64,
    bytes: Vec<u8>,
    /// Text that came with an image.
    text: Vec<u8>,
}

struct Incoming {
    context: transfers::Context,
    progress: Option<Transfer>,
    clipboard: NativeClipboard,
    applied: Arc<Mutex<Option<Applied>>>,
    config: Config,
    files: Option<files::Receiver>,
    text: Option<Blob>,
    image: Option<Blob>,
    /// The clipboard's generation when the transfer in progress began.
    started: u64,
}

fn refusal(id: u64, reason: &str) -> Job {
    Job::Refusal {
        id,
        reason: reason.into(),
    }
}

impl Incoming {
    fn new(applied: Arc<Mutex<Option<Applied>>>, config: Config) -> Result<Self> {
        // Clears copies an earlier run left in the cache.
        let _ = received::directory();
        Ok(Self {
            context: transfers::Context::default(),
            progress: None,
            clipboard: NativeClipboard::new()?,
            applied,
            config,
            files: None,
            text: None,
            image: None,
            started: 0,
        })
    }

    /// Applies one message from the peer and says what to send back. An
    /// error means the peer broke the protocol and the session must end;
    /// a transfer this computer can't take is refused on its own instead.
    fn apply(&mut self, message: Data) -> Result<Option<Job>> {
        match message {
            Data::Text(text) => {
                if self.config.clipboard {
                    self.set_text(text);
                }
            }
            Data::TextStart { id, bytes } => {
                if self.config.clipboard {
                    ensure!(self.text.is_none(), "overlapping text items");
                    self.text = Some(start_blob(id, bytes)?);
                }
            }
            Data::TextChunk { id, bytes } => {
                if self.config.clipboard {
                    push_blob(&mut self.text, id, bytes)?;
                }
            }
            Data::TextEnd { id } => {
                if self.config.clipboard {
                    let blob = finish_blob(&mut self.text, id)?;
                    self.set_text(String::from_utf8(blob.bytes)?);
                }
            }
            Data::ImageStart { id, bytes } => {
                if !self.config.images {
                    return Ok(Some(refusal(
                        id,
                        "Images are turned off on the receiving computer",
                    )));
                }
                ensure!(self.image.is_none(), "overlapping images");
                self.image = Some(start_blob(id, bytes)?);
                self.begin(id, "Clipboard image.png", bytes);
            }
            Data::ImageTextChunk { id, bytes } => {
                if self.config.images {
                    let blob = self.image.as_mut().context("image text without image")?;
                    ensure!(
                        blob.id == id
                            && bytes.len() <= FILE_CHUNK_SIZE
                            && blob.text.len() + bytes.len() <= MAX_CLIPBOARD_BYTES,
                        "invalid image text"
                    );
                    blob.text.extend(bytes);
                }
            }
            Data::ImageChunk { id, bytes } => {
                if self.config.images {
                    if let Some(p) = &mut self.progress {
                        p.advance(bytes.len());
                    }
                    push_blob(&mut self.image, id, bytes)?;
                }
            }
            Data::ImageEnd { id } => {
                if self.config.images {
                    let blob = finish_blob(&mut self.image, id)?;
                    let text = (!blob.text.is_empty())
                        .then(|| String::from_utf8(blob.text))
                        .transpose()?;
                    if let Some(p) = &mut self.progress {
                        p.finishing();
                    }
                    let offered = self.offer_image(id, blob.bytes, text);
                    return Ok(Some(self.finish(id, offered)));
                }
            }
            message @ (Data::BatchStart { .. }
            | Data::Directory { .. }
            | Data::FileStart { .. }
            | Data::FileChunk { .. }
            | Data::FileEnd { .. }
            | Data::BatchEnd { .. }
            | Data::BatchAbort { .. }) => {
                if self.config.files {
                    return self.receive_files(message);
                }
                if let Data::BatchStart { id, .. } = message {
                    return Ok(Some(refusal(
                        id,
                        "Files are turned off on the receiving computer",
                    )));
                }
            }
            // Answers about this computer's own transfers, which the session
            // loop handles.
            Data::TransferReceipt { .. } | Data::TransferFailed { .. } => {}
        }
        Ok(None)
    }

    fn begin(&mut self, id: u64, name: &str, bytes: u64) {
        let mut progress = Transfer::start(&self.context, id, false, name.into(), bytes);
        progress.total(bytes);
        self.progress = Some(progress);
        self.started = self.clipboard.generation();
    }

    /// Whether something was copied on this computer, or arrived from another
    /// one, while the transfer in progress was arriving. The newer copy wins.
    fn replaced(&self) -> bool {
        self.clipboard.generation() != self.started
    }

    /// Ends the transfer in progress with what became of it: `Ok(false)` when
    /// a newer copy kept it off the clipboard.
    fn finish(&mut self, id: u64, offered: Result<bool>) -> Job {
        let progress = self.progress.take();
        match offered {
            Ok(true) => {
                if let Some(p) = progress {
                    p.complete();
                }
                Job::Receipt(id)
            }
            Ok(false) => {
                if let Some(p) = progress {
                    p.skip("Not pasted: something newer was copied");
                }
                Job::Receipt(id)
            }
            Err(error) => {
                log::warn!("cannot take a copy from {}: {error:#}", self.context.peer);
                let reason = format!("{error:#}");
                if let Some(mut p) = progress {
                    p.fail(&reason);
                }
                refusal(id, &reason)
            }
        }
    }

    fn receive_files(&mut self, message: Data) -> Result<Option<Job>> {
        match &message {
            Data::BatchStart { id, bytes, .. } => self.begin(*id, "Files", *bytes),
            Data::FileStart { path, .. } => {
                if let Some(p) = &mut self.progress {
                    p.name(path);
                }
            }
            Data::FileChunk { bytes, .. } => {
                if let Some(p) = &mut self.progress {
                    p.advance(bytes.len());
                }
            }
            Data::BatchEnd { .. } => {
                if let Some(p) = &mut self.progress {
                    p.finishing();
                }
            }
            _ => {}
        }
        if self.files.is_none() {
            self.files = Some(files::Receiver::new(received::directory()?));
        }
        Ok(match self.files.as_mut().unwrap().apply(message)? {
            files::Received::Pending => None,
            files::Received::Aborted => {
                if let Some(p) = self.progress.take() {
                    p.skip("Stopped by the sending computer");
                }
                None
            }
            files::Received::Failed { id, error } => Some(self.finish(id, Err(error))),
            files::Received::Complete { id, paths } => {
                let offered = self.offer_files(paths);
                Some(self.finish(id, offered))
            }
        })
    }

    fn offer_files(&self, paths: Vec<PathBuf>) -> Result<bool> {
        // A completed batch's top-level entries share its own folder.
        let item = received::Item::new(
            paths[0]
                .parent()
                .context("received files have no folder")?
                .to_path_buf(),
        );
        if self.replaced() {
            received::retire(item);
            return Ok(false);
        }
        let digest = digest_paths(&paths);
        let relay = Job::Files {
            paths: paths.clone(),
            received: Some(item.clone()),
        };
        let files: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.apply_native(digest, Some(item.clone()), |clipboard| {
            clipboard.set_files(&files)
        })?;
        #[cfg(windows)]
        self.when_moved(&item, paths, false);
        relay::publish(self.context.session, relay);
        Ok(true)
    }

    /// Received files are offered as a cut, so pasting them into a folder
    /// moves them there. Once all have gone there is nothing left to paste:
    /// take them off the clipboard, as Windows does after a cut and paste,
    /// and delete their emptied folder.
    #[cfg(windows)]
    fn when_moved(&self, item: &Arc<received::Item>, paths: Vec<PathBuf>, image: bool) {
        let applied = self.applied.clone();
        let folder = item.path().to_path_buf();
        let watched = item.watch(paths, move || {
            if let Err(error) = take_back(&applied, &folder, image) {
                log::warn!("taking pasted files off the clipboard: {error:#}");
            }
        });
        if let Err(error) = watched {
            log::warn!("cannot watch received files: {error:#}");
        }
    }

    fn offer_image(&self, id: u64, png: Vec<u8>, text: Option<String>) -> Result<bool> {
        if self.replaced() {
            return Ok(false);
        }
        let image = PreparedImage::from_wire(png.clone())?;
        let digest = image_digest(image.identity, text.as_deref());
        let native = image.into_native();
        if self.config.files {
            // A file copy as well lets the image be pasted into a folder.
            let item = received::Item::new(
                received::directory()?.join(format!("image-{id:016x}-{:016x}", new_id())),
            );
            std::fs::create_dir_all(item.path())?;
            let path = item.path().join("Clipboard image.png");
            std::fs::write(&path, &png)?;
            self.apply_native(digest, Some(item.clone()), |clipboard| {
                clipboard.set_image_file(&native, &path, text.as_deref())
            })?;
            #[cfg(windows)]
            self.when_moved(&item, vec![path], true);
        } else {
            self.apply_native(digest, None, |clipboard| {
                clipboard.set_image(&native, text.as_deref())
            })?;
        }
        relay::publish(self.context.session, Job::Image { png, text });
        Ok(true)
    }

    fn set_text(&self, text: String) {
        let digest = *blake3::hash(text.as_bytes()).as_bytes();
        match self.apply_native(digest, None, |clipboard| clipboard.set_text(&text)) {
            Ok(()) => relay::publish(self.context.session, Job::Text(text)),
            Err(error) => log::warn!(
                "cannot put text from {} on the clipboard: {error:#}",
                self.context.peer
            ),
        }
    }

    fn apply_native(
        &self,
        digest: [u8; 32],
        received: Option<Arc<received::Item>>,
        mut write: impl FnMut(&NativeClipboard) -> Result<()>,
    ) -> Result<()> {
        let replaced = while_busy(|| {
            // Match capture's lock order: native transaction, then echo state.
            #[cfg(windows)]
            let _native = native::transaction();
            let mut applied = self.applied.lock().unwrap();
            write(&self.clipboard)?;
            Ok(applied.replace(Applied {
                generation: self.clipboard.generation(),
                digest,
                received: received.clone(),
            }))
        })?;
        if let Some(previous) = replaced.and_then(|applied| applied.received) {
            received::retire(previous);
        }
        Ok(())
    }
}

/// Clipboard managers and other apps hold the clipboard open for a moment
/// after each change. Retries `write` while they do, without holding locks
/// in between.
fn while_busy<T>(mut write: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempts = 0;
    loop {
        match write() {
            Err(_) if attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

/// Takes the received copy in `folder`, which a paste has moved away, off
/// the clipboard if it is still there, keeping a received picture, and
/// deletes the folder.
#[cfg(windows)]
fn take_back(applied: &Mutex<Option<Applied>>, folder: &Path, image: bool) -> Result<()> {
    let clipboard = NativeClipboard::new()?;
    let taken = while_busy(|| {
        let _native = native::transaction();
        let mut applied = applied.lock().unwrap();
        let Some(current) = applied.as_mut().filter(|current| {
            current
                .received
                .as_ref()
                .is_some_and(|r| r.path() == folder)
        }) else {
            return Ok(None);
        };
        // Otherwise something else was copied, which capture handles.
        if clipboard.generation() == current.generation {
            if image {
                let picture = clipboard
                    .get_image()
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let text = clipboard.get_text().ok();
                clipboard.set_image(&picture, text.as_deref())?;
            } else {
                clipboard.clear()?;
            }
            current.generation = clipboard.generation();
        }
        Ok(current.received.take())
    })?;
    // Delete outside the locks.
    drop(taken);
    Ok(())
}

fn start_blob(id: u64, bytes: u64) -> Result<Blob> {
    if bytes > MAX_CLIPBOARD_BYTES as u64 {
        bail!("clipboard item too large");
    }
    Ok(Blob {
        id,
        expected: bytes,
        bytes: Vec::with_capacity(bytes as usize),
        text: Vec::new(),
    })
}
fn push_blob(target: &mut Option<Blob>, id: u64, bytes: Vec<u8>) -> Result<()> {
    let blob = target.as_mut().context("blob chunk without start")?;
    if blob.id != id
        || bytes.len() > FILE_CHUNK_SIZE
        || blob.bytes.len() + bytes.len() > blob.expected as usize
    {
        bail!("invalid clipboard chunk");
    }
    blob.bytes.extend(bytes);
    Ok(())
}
fn finish_blob(target: &mut Option<Blob>, id: u64) -> Result<Blob> {
    let blob = target.take().context("blob end without start")?;
    if blob.id != id || blob.bytes.len() != blob.expected as usize {
        bail!("incomplete clipboard item");
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn assert_image_file_formats(clipboard: &NativeClipboard) {
        #[cfg(windows)]
        let _native = native::transaction();
        use clipboard_rs::common::RustImage;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Clipboard image.png");
        let pixels = vec![90, 120, 200, 127, 1, 2, 3, 255];
        let image =
            clipboard_rs::RustImageData::from_dynamic_image(::image::DynamicImage::ImageRgba8(
                ::image::RgbaImage::from_raw(2, 1, pixels.clone()).unwrap(),
            ));
        std::fs::write(&path, image.to_png().unwrap().get_bytes()).unwrap();
        // As a spreadsheet selection is copied: its cells and their picture.
        let cells = "Region\tSales\r\nNorth\t42\r\n";
        clipboard
            .set_image_file(&image, &path, Some(cells))
            .unwrap();
        assert_eq!(
            clipboard.get_files().unwrap(),
            vec![path.to_string_lossy().into_owned()]
        );
        assert_eq!(
            clipboard.get_image().unwrap().to_rgba8().unwrap().as_raw(),
            &pixels
        );
        assert_eq!(clipboard.get_text().unwrap(), cells);
        #[cfg(windows)]
        assert_eq!(drop_effect(), Some(CUT.to_vec()));
        clipboard.set_image(&image, None).unwrap();
        assert!(clipboard.get_files().unwrap_or_default().is_empty());
        assert!(clipboard.get_text().is_err(), "an image alone has no text");
        #[cfg(windows)]
        assert_eq!(drop_effect(), None);
    }

    /// DROPEFFECT_MOVE, as File Explorer marks files it cuts.
    #[cfg(windows)]
    const CUT: [u8; 4] = 2u32.to_le_bytes();

    #[cfg(windows)]
    fn drop_effect() -> Option<Vec<u8>> {
        let _clipboard = clipboard_win::Clipboard::new_attempts(10).unwrap();
        let format = clipboard_win::register_format("Preferred DropEffect").unwrap();
        let mut effect = Vec::new();
        clipboard_win::raw::get_vec(format.get(), &mut effect).ok()?;
        Some(effect)
    }

    /// Waits for a paste's move to be noticed and the copy's folder deleted.
    #[cfg(windows)]
    fn wait_until_deleted(folder: &Path) {
        let start = std::time::Instant::now();
        while folder.exists() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "a received copy moved away by a paste must be deleted"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    pub(super) fn assert_pasted_copies_leave_the_clipboard() {
        use clipboard_rs::common::RustImage;
        use std::fs;
        let (mut handler, incoming) = native_handlers(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
        handler.config.files = true;
        handler.config.images = true;
        let folder = tempfile::tempdir().unwrap();

        // Files: pasting them into a folder leaves nothing to paste.
        let copy = folder.path().join("files");
        fs::create_dir(&copy).unwrap();
        let file = copy.join("a.txt");
        fs::write(&file, b"received").unwrap();
        assert!(incoming.offer_files(vec![file.clone()]).unwrap());
        assert_eq!(
            handler.ctx.get_files().unwrap(),
            vec![file.to_string_lossy().into_owned()]
        );
        assert_eq!(drop_effect(), Some(CUT.to_vec()));
        // As File Explorer pastes a cut on the same drive.
        let pasted = folder.path().join("a.txt");
        fs::rename(&file, &pasted).unwrap();
        wait_until_deleted(&copy);
        assert!(handler.ctx.get_files().unwrap_or_default().is_empty());
        assert_eq!(fs::read(&pasted).unwrap(), b"received");

        // A picture: pasting its file into a folder keeps the picture.
        let copy = folder.path().join("image");
        fs::create_dir(&copy).unwrap();
        let file = copy.join("Clipboard image.png");
        let pixels = vec![90, 120, 200, 127];
        let image =
            clipboard_rs::RustImageData::from_dynamic_image(::image::DynamicImage::ImageRgba8(
                ::image::RgbaImage::from_raw(1, 1, pixels.clone()).unwrap(),
            ));
        fs::write(&file, image.to_png().unwrap().get_bytes()).unwrap();
        let item = received::Item::new(copy.clone());
        incoming
            .apply_native([1; 32], Some(item.clone()), |clipboard| {
                clipboard.set_image_file(&image, &file, Some("caption"))
            })
            .unwrap();
        incoming.when_moved(&item, vec![file.clone()], true);
        drop(item);
        fs::rename(&file, folder.path().join("Clipboard image.png")).unwrap();
        wait_until_deleted(&copy);
        assert!(handler.ctx.get_files().unwrap_or_default().is_empty());
        assert_eq!(
            handler
                .ctx
                .get_image()
                .unwrap()
                .to_rgba8()
                .unwrap()
                .as_raw(),
            &pixels
        );
        assert_eq!(handler.ctx.get_text().unwrap(), "caption");

        // Neither change is a copy made on this computer.
        handler.capture().unwrap();
        assert_no_echo(handler.outgoing);
    }

    fn test_signals() -> InputSignals {
        InputSignals::new(
            crate::input::Controller::new(&Config::default()),
            crate::status::Stats::new(),
        )
    }

    fn encrypted_pipe() -> (SecureReader, SecureWriter) {
        use crate::network::SecureConnection;
        use std::{net::TcpListener, thread};

        let code = Config::default().pair_code;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let accepted = thread::spawn(move || {
            SecureConnection::accept(listener.accept().unwrap().0, &server_code).unwrap()
        });
        let client = SecureConnection::connect(&address.to_string(), &code).unwrap();
        let (reader, _) = client.split().unwrap();
        let (_, writer) = accepted.join().unwrap().split().unwrap();
        (reader, writer)
    }

    fn delivered_pending(outgoing: Arc<Outgoing>) -> Result<Data> {
        use std::{thread, time::Duration};

        let (mut reader, mut writer) = encrypted_pipe();
        reader.set_timeout(Duration::from_secs(2)).unwrap();
        writer.set_timeout(Duration::from_secs(2)).unwrap();
        let cancel = Arc::new(crate::shutdown::Signal::default());
        let stop = cancel.clone();
        let (done, finished) = crossbeam_channel::bounded(1);
        let sender = thread::spawn(move || {
            let result = send_jobs(
                &mut writer,
                outgoing,
                &stop,
                test_signals(),
                transfers::Context::default(),
            );
            done.send(result).unwrap();
        });
        let delivered = reader.read::<Data>();
        cancel.stop();
        finished
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        sender.join().unwrap();
        delivered
    }

    #[test]
    fn encrypted_sender_keeps_the_latest_pending_copy_when_capture_outpaces_transfer() {
        let outgoing = Arc::new(Outgoing::new());
        for n in 0..32 {
            outgoing.submit(Job::Text(format!("copy-{n}")));
        }
        match delivered_pending(outgoing).unwrap() {
            Data::Text(value) => assert_eq!(value, "copy-31"),
            other => panic!("expected latest text copy, received {other:?}"),
        }
    }

    #[test]
    fn encrypted_file_batches_preserve_bytes_across_chunk_and_file_boundaries() {
        use std::{fs, thread, time::Duration};

        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("bundle");
        fs::create_dir(&source).unwrap();
        let large: Vec<u8> = (0..(FILE_CHUNK_SIZE * 2 + 17))
            .map(|index| (index as u8).wrapping_mul(37))
            .collect();
        fs::write(source.join("large.bin"), &large).unwrap();
        fs::write(source.join("short.bin"), b"second file").unwrap();

        let (mut reader, mut writer) = encrypted_pipe();
        reader.set_timeout(Duration::from_secs(5)).unwrap();
        writer.set_timeout(Duration::from_secs(5)).unwrap();
        let sender = thread::spawn(move || {
            let mut pacer = BulkPacer::new(test_signals());
            send_files(
                &mut writer,
                vec![source],
                &Outgoing::new(),
                &crate::shutdown::Signal::default(),
                &mut pacer,
                &transfers::Context::default(),
            )
        });
        let mut receiver = files::Receiver::new(folder.path().join("received"));
        let completed = loop {
            if let files::Received::Complete { paths, .. } =
                receiver.apply(reader.read::<Data>().unwrap()).unwrap()
            {
                break paths;
            }
        };
        sender.join().unwrap().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(fs::read(completed[0].join("large.bin")).unwrap(), large);
        assert_eq!(
            fs::read(completed[0].join("short.bin")).unwrap(),
            b"second file"
        );
    }

    /// Sends `roots` as files with `outgoing` and returns what arrived,
    /// up to and including a text message sent after them.
    fn sent_files(roots: Vec<PathBuf>, outgoing: Outgoing) -> Vec<Data> {
        use std::{thread, time::Duration};

        let (mut reader, mut writer) = encrypted_pipe();
        reader.set_timeout(Duration::from_secs(5)).unwrap();
        writer.set_timeout(Duration::from_secs(5)).unwrap();
        let sender = thread::spawn(move || {
            let mut pacer = BulkPacer::new(test_signals());
            send_files(
                &mut writer,
                roots,
                &outgoing,
                &crate::shutdown::Signal::default(),
                &mut pacer,
                &transfers::Context::default(),
            )?;
            // The connection still works for the next copy.
            writer.write(&Data::Text("next copy".into()))
        });
        let mut arrived = Vec::new();
        loop {
            let message = reader.read::<Data>().unwrap();
            let last = matches!(message, Data::Text(_));
            arrived.push(message);
            if last {
                break;
            }
        }
        sender.join().unwrap().unwrap();
        arrived
    }

    #[test]
    fn a_copy_that_cannot_be_sent_fails_alone() {
        let folder = tempfile::tempdir().unwrap();
        let arrived = sent_files(vec![folder.path().join("deleted")], Outgoing::new());
        assert!(
            matches!(arrived[..], [Data::Text(_)]),
            "nothing is announced for a copy that can't be read: {arrived:?}"
        );
    }

    #[test]
    fn a_newer_copy_stops_an_upload_in_progress() {
        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("large");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.bin"), vec![1; FILE_CHUNK_SIZE * 3]).unwrap();
        let outgoing = Outgoing::new();
        outgoing.submit(Job::Text("newer".into()));
        let arrived = sent_files(vec![source], outgoing);
        assert!(
            matches!(
                arrived[..],
                [
                    Data::BatchStart { .. },
                    Data::BatchAbort { .. },
                    Data::Text(_)
                ]
            ),
            "{arrived:?}"
        );
    }

    fn native_handlers(source: NativeClipboard, target: NativeClipboard) -> (Handler, Incoming) {
        let applied = Arc::new(Mutex::new(None));
        let outgoing = Arc::new(Outgoing::new());
        let config = Config {
            images: false,
            files: false,
            ..Config::default()
        };
        let handler = Handler {
            ctx: source,
            outgoing: outgoing.clone(),
            applied: applied.clone(),
            config: config.clone(),
            cancel: Arc::new(crate::shutdown::Signal::default()),
        };
        let incoming = Incoming {
            context: transfers::Context::default(),
            progress: None,
            clipboard: target,
            applied,
            config,
            files: None,
            text: None,
            image: None,
            started: 0,
        };
        (handler, incoming)
    }

    fn assert_no_echo(outgoing: Arc<Outgoing>) {
        let error = delivered_pending(outgoing)
            .expect_err("our native write must not be echoed to the peer");
        assert!(
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                )),
            "expected a quiet connection, got {error:#}"
        );
    }

    /// Receives `png` as image `id`, which must arrive, even if a newer copy
    /// keeps it off the clipboard.
    #[cfg(windows)]
    fn receive_image(incoming: &mut Incoming, id: u64, png: &[u8]) {
        for message in [
            Data::ImageStart {
                id,
                bytes: png.len() as u64,
            },
            Data::ImageChunk {
                id,
                bytes: png.to_vec(),
            },
        ] {
            assert!(incoming.apply(message).unwrap().is_none());
        }
        assert!(matches!(
            incoming.apply(Data::ImageEnd { id }).unwrap(),
            Some(Job::Receipt(receipt)) if receipt == id
        ));
    }

    #[cfg(windows)]
    pub(super) fn assert_independent_peer_image_access() {
        use ::image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
        let (mut reader, _) = native_handlers(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
        reader.config.images = true;
        let pixels = vec![73u8; 640 * 360 * 4];
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&pixels, 640, 360, ExtendedColorType::Rgba8)
            .unwrap();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let png = &png;
                let barrier = &barrier;
                scope.spawn(move || {
                    let (_, mut writer) = native_handlers(
                        NativeClipboard::new().unwrap(),
                        NativeClipboard::new().unwrap(),
                    );
                    writer.config.images = true;
                    barrier.wait();
                    for id in 0..96 {
                        receive_image(&mut writer, id, png);
                    }
                });
            }
            barrier.wait();
            for _ in 0..512 {
                reader.capture().unwrap();
            }
        });
        // Concurrent copies can each give way to the other's newer one; make
        // sure one is on the clipboard.
        let (_, mut last) = native_handlers(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
        last.config.images = true;
        receive_image(&mut last, 96, &png);
        reader.capture().unwrap();
        let Job::Image { png: bytes, .. } = reader.outgoing.recv().unwrap() else {
            panic!("expected image")
        };
        let image = ::image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(image.dimensions(), (640, 360));
        assert_eq!(image.as_raw(), &pixels);
    }

    #[cfg(target_os = "macos")]
    pub(super) fn assert_concurrent_native_readback_stays_quiet(
        source: NativeClipboard,
        target: NativeClipboard,
    ) {
        use ::image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
        let (mut handler, mut incoming) = native_handlers(source, target);
        handler.config.images = true;
        incoming.config.images = true;
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&[10, 20, 30, 255], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        let start = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let writer_start = start.clone();
            scope.spawn(move || {
                writer_start.wait();
                for n in 0..500 {
                    incoming
                        .apply(Data::Text(format!("peer update {n}")))
                        .unwrap();
                    incoming
                        .apply(Data::ImageStart {
                            id: n,
                            bytes: png.len() as u64,
                        })
                        .unwrap();
                    incoming
                        .apply(Data::ImageChunk {
                            id: n,
                            bytes: png.clone(),
                        })
                        .unwrap();
                    incoming.apply(Data::ImageEnd { id: n }).unwrap();
                }
            });
            start.wait();
            for _ in 0..5000 {
                handler.capture().unwrap();
            }
        });
        handler.capture().unwrap();
        assert_no_echo(handler.outgoing);
    }

    pub(super) fn assert_native_recopy_is_delivered(
        source: NativeClipboard,
        target: NativeClipboard,
    ) {
        let (mut handler, mut incoming) = native_handlers(source, target);
        for own_readback in [true, false] {
            handler.outgoing = Arc::new(Outgoing::new());
            incoming.apply(Data::Text("peer copy".into())).unwrap();
            handler.capture().unwrap();
            if !own_readback {
                handler.ctx.set_text("local intervening copy").unwrap();
                handler.capture().unwrap();
                handler.ctx.set_text("peer copy").unwrap();
                handler.capture().unwrap();
            }
            if own_readback {
                assert_no_echo(handler.outgoing.clone());
            } else {
                match delivered_pending(handler.outgoing.clone()).unwrap() {
                    Data::Text(value) => assert_eq!(
                        value, "peer copy",
                        "a new native copy must be delivered even when its content was received earlier"
                    ),
                    other => panic!("expected copied text, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn hostile_paths_are_rejected() {
        for value in [
            "../evil",
            "/absolute",
            "C:/drive",
            "a/../../b",
            "a\\b",
            "a//b",
        ] {
            assert!(safe_relative(value).is_err(), "{value}");
        }
        assert_eq!(
            safe_relative("folder/a.txt").unwrap(),
            PathBuf::from("folder").join("a.txt")
        );
    }
}
