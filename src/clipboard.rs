use crate::transfers::{self, Transfer};
use crate::{
    config::Config,
    network::{SecureReader, SecureWriter},
    protocol::{Data, FILE_CHUNK_SIZE, OfferedEntry},
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
    sync::{Arc, Mutex, Weak},
};
use walkdir::WalkDir;

mod files;
mod image;
mod native;
mod offer;
mod outgoing;
mod pacing;
mod received;
mod relay;
use image::PreparedImage;
use native::NativeClipboard;
use offer::Promise;
use outgoing::{Offered, Outgoing};
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
    /// Files to offer. Nothing of them is sent until a paste asks.
    Files {
        paths: Vec<PathBuf>,
        /// A relayed copy's folder stays until it has been sent on.
        received: Option<Arc<received::Item>>,
    },
    /// Files another computer offered, offered on to this link's peer.
    Promised(Arc<Promise>),
    /// Ask the peer for the files it offered as `id`: a paste wants them.
    Want(u64),
    /// Send offered files `id`, which a paste on the peer asked for, as
    /// they were listed when offered.
    Upload {
        id: u64,
        entries: Arc<Vec<Entry>>,
        total: u64,
        received: Option<Arc<received::Item>>,
    },
    /// Offered files `id` can't be sent anymore.
    Withdrawn(u64),
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
    let mut incoming = Incoming::new(applied, config, outgoing.clone())?;
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
            Data::FilesWanted { id } => replies.wanted(id),
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
    incoming.disconnected();
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
        // Anything newer copied here replaces the files offered before.
        if matches!(
            job,
            Job::Text(_) | Job::Image { .. } | Job::Files { .. } | Job::Promised(_)
        ) {
            outgoing.offer(None);
        }
        match job {
            Job::Receipt(id) => writer.write(&Data::TransferReceipt { id })?,
            Job::Refusal { id, reason } => writer.write(&Data::TransferFailed { id, reason })?,
            Job::Want(id) => writer.write(&Data::FilesWanted { id })?,
            Job::Withdrawn(id) => writer.write(&Data::BatchAbort { id })?,
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
            // Listed now, so the other computer knows their names and
            // sizes, but not read until a paste there asks for them.
            Job::Files { paths, received } => match list_files(paths, cancel) {
                Ok((entries, total)) => {
                    let listing = entries
                        .iter()
                        .map(|(_, path, bytes)| OfferedEntry {
                            path: path.clone(),
                            bytes: *bytes,
                        })
                        .collect();
                    let offered = Offered::Files {
                        entries: Arc::new(entries),
                        total,
                        received,
                    };
                    offer_files(writer, &outgoing, listing, offered)?;
                }
                Err(error) => log::info!("not offering copied files: {error:#}"),
            },
            Job::Promised(promise) => {
                let listing = promise.entries().to_vec();
                offer_files(writer, &outgoing, listing, Offered::Promised(promise))?;
            }
            Job::Upload {
                id,
                entries,
                total,
                received: _received,
            } => send_files(
                writer, id, &entries, total, &outgoing, cancel, &mut pacer, &context,
            )?,
        }
    }
    Ok(())
}

/// Room for paths in one offer message, well inside `MAX_WIRE_SIZE`.
const OFFER_MESSAGE_BYTES: usize = 48 * 1024;

/// Tells the peer about copied files, which a paste there can ask for.
fn offer_files(
    writer: &mut SecureWriter,
    outgoing: &Outgoing,
    listing: Vec<OfferedEntry>,
    offered: Offered,
) -> Result<()> {
    let id = new_id();
    // Known before the peer can ask for them.
    outgoing.offer(Some((id, offered)));
    let mut entries = Vec::new();
    let mut size = 0;
    let count = listing.len();
    for (index, entry) in listing.into_iter().enumerate() {
        // A path, its length and its size.
        size += entry.path.len() + 24;
        entries.push(entry);
        let last = index + 1 == count;
        if size >= OFFER_MESSAGE_BYTES || last {
            writer.write(&Data::FilesOffer {
                id,
                entries: std::mem::take(&mut entries),
                last,
            })?;
            size = 0;
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

/// Sends offered files `id`, as they were listed when offered.
#[allow(clippy::too_many_arguments)]
fn send_files(
    writer: &mut SecureWriter,
    id: u64,
    entries: &[Entry],
    total: u64,
    outgoing: &Outgoing,
    cancel: &crate::shutdown::Signal,
    pacer: &mut BulkPacer,
    context: &transfers::Context,
) -> Result<()> {
    let mut progress = Transfer::start(context, id, true, "Preparing files…".into(), 0);
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

/// A file to send, or a folder when it has no size: where it is here, and
/// its path within the copy.
pub(super) type Entry = (PathBuf, String, Option<u64>);

/// Everything under `roots`, checked before any of it is announced.
pub(super) fn list_files(
    roots: Vec<PathBuf>,
    cancel: &crate::shutdown::Signal,
) -> Result<(Vec<Entry>, u64)> {
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
    entries: &[Entry],
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
        progress.name(wire_path);
        let Some(size) = *size else {
            writer.write(&Data::Directory {
                id,
                path: wire_path.clone(),
            })?;
            continue;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let unreadable = |error: std::io::Error| {
            Stopped::Local(anyhow::Error::new(error).context(format!("Cannot read “{name}”")))
        };
        let changed =
            || Stopped::Local(anyhow::anyhow!("“{name}” changed while it was being sent"));
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) => return Ok(Some(unreadable(error))),
        };
        writer.write(&Data::FileStart {
            id,
            path: wire_path.clone(),
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
    /// The files the peer offered last, while they can still be pasted:
    /// the clipboard, a paste in progress or another computer holds them.
    promise: Weak<Promise>,
    /// An offer whose listing is still arriving.
    offering: Option<(u64, Vec<OfferedEntry>)>,
    /// The connection back to the peer, to ask for offered files.
    peer: Arc<Outgoing>,
}

fn refusal(id: u64, reason: &str) -> Job {
    Job::Refusal {
        id,
        reason: reason.into(),
    }
}

impl Incoming {
    fn new(
        applied: Arc<Mutex<Option<Applied>>>,
        config: Config,
        peer: Arc<Outgoing>,
    ) -> Result<Self> {
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
            promise: Weak::new(),
            offering: None,
            peer,
        })
    }

    /// The connection ended: offered files can't be fetched anymore.
    fn disconnected(&mut self) {
        if let Some(promise) = self.promise.upgrade() {
            promise.fail("The other computer disconnected");
        }
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
            Data::FilesOffer { id, entries, last } => {
                if self.config.files {
                    let mut listing = match self.offering.take() {
                        Some((offering, listing)) if offering == id => listing,
                        _ => Vec::new(),
                    };
                    listing.extend(entries);
                    ensure!(listing.len() <= MAX_TRANSFER_FILES, "invalid file offer");
                    if !last {
                        self.offering = Some((id, listing));
                    } else if let Err(error) = check_listing(&listing) {
                        // Such as a name this system doesn't allow.
                        log::warn!("cannot offer files from {}: {error:#}", self.context.peer);
                        if let Some(previous) = self.promise.upgrade() {
                            previous.fail("Something newer was copied");
                        }
                    } else {
                        self.offered(id, listing);
                    }
                }
            }
            // Answers about this computer's own transfers, which the session
            // loop handles.
            Data::TransferReceipt { .. }
            | Data::TransferFailed { .. }
            | Data::FilesWanted { .. } => {}
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

    /// The peer copied files. They go on the clipboard as a promise that a
    /// paste fetches, and on to this computer's other destinations.
    fn offered(&mut self, id: u64, listing: Vec<OfferedEntry>) {
        let promise = Promise::new(id, listing, &self.peer);
        let previous = std::mem::replace(&mut self.promise, Arc::downgrade(&promise));
        if let Some(previous) = previous.upgrade() {
            previous.fail("Something newer was copied");
        }
        let placeholder = Arc::new(Placeholder {
            promise: promise.clone(),
            digest: *blake3::hash(&id.to_le_bytes()).as_bytes(),
        });
        match self.apply_native(placeholder.digest, None, |clipboard| {
            clipboard.set_promised_files(&placeholder)
        }) {
            Ok(()) => relay::publish(self.context.session, Job::Promised(promise)),
            Err(error) => log::warn!(
                "cannot offer files from {} on the clipboard: {error:#}",
                self.context.peer
            ),
        }
    }

    /// The offered files a batch brings, when a paste asked for them.
    fn promised(&self, id: u64) -> Option<Arc<Promise>> {
        self.promise.upgrade().filter(|promise| promise.id() == id)
    }

    fn receive_files(&mut self, message: Data) -> Result<Option<Job>> {
        if let Data::BatchAbort { id } = message
            && let Some(promise) = self.promised(id)
        {
            promise.fail("Stopped by the sending computer");
        }
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
            files::Received::Failed { id, error } => {
                if let Some(promise) = self.promised(id) {
                    promise.fail(&format!("{error:#}"));
                }
                Some(self.finish(id, Err(error)))
            }
            files::Received::Complete { id, paths } => {
                let delivered = Self::deliver(self.promised(id), paths);
                Some(self.finish(id, delivered))
            }
        })
    }

    /// Hands files a paste asked for to the promise it waits on: `Ok(false)`
    /// when nothing can paste them anymore, as after something newer was
    /// copied.
    fn deliver(promise: Option<Arc<Promise>>, paths: Vec<PathBuf>) -> Result<bool> {
        let item = received::Item::new(
            paths[0]
                .parent()
                .context("received files have no folder")?
                .to_path_buf(),
        );
        match promise {
            Some(promise) => {
                promise.arrived(paths, item);
                Ok(true)
            }
            // Dropping the copy deletes it.
            None => Ok(false),
        }
    }

    /// A received image is also offered as a file, as a cut, so pasting it
    /// into a folder moves it there. Once it has gone there is no file left
    /// to paste: take it off the clipboard, as Windows does after a cut and
    /// paste, keeping the picture, and delete its emptied folder.
    #[cfg(windows)]
    fn when_moved(&self, item: &Arc<received::Item>, paths: Vec<PathBuf>, image: bool) {
        when_moved(&self.applied, item, paths, image);
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

#[cfg(windows)]
fn when_moved(
    applied: &Arc<Mutex<Option<Applied>>>,
    item: &Arc<received::Item>,
    paths: Vec<PathBuf>,
    image: bool,
) {
    let applied = applied.clone();
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

/// Files another computer offered, on this computer's clipboard until a
/// paste asks for them.
pub(super) struct Placeholder {
    promise: Arc<Promise>,
    /// Marks the clipboard content that is this placeholder.
    digest: [u8; 32],
}

impl Placeholder {
    /// How many items were copied.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn count(&self) -> usize {
        self.promise.count()
    }

    /// Every offered file and folder, each folder before what it holds.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn entries(&self) -> &[OfferedEntry] {
        self.promise.entries()
    }

    /// Waits for the files a paste asked for: their paths, or `None` when
    /// they can't come.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn files(&self) -> Option<Vec<String>> {
        match self.promise.fetch() {
            Ok((paths, _)) => Some(paths_to_strings(&paths)),
            Err(error) => {
                log::warn!("cannot paste offered files: {error:#}");
                None
            }
        }
    }

    /// Asks for the files a paste wants without waiting for them: the folder
    /// holding them once they have arrived, `None` while they are on their way.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn try_folder(&self) -> Option<Result<PathBuf>> {
        self.promise
            .try_fetch()
            .map(|fetched| fetched.map(|(_, item)| item.path().to_path_buf()))
    }
}

fn paths_to_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// An offer's listing must describe files this computer could receive,
/// each folder before what it holds. Anything else is a broken peer.
fn check_listing(listing: &[OfferedEntry]) -> Result<()> {
    ensure!(!listing.is_empty(), "empty file offer");
    let mut seen = HashSet::new();
    let mut folders = HashSet::new();
    let mut total = 0u64;
    for entry in listing {
        safe_relative(&entry.path)?;
        ensure!(seen.insert(entry.path.as_str()), "duplicate offered path");
        if let Some((folder, _)) = entry.path.rsplit_once('/') {
            ensure!(folders.contains(folder), "offered path outside its folder");
        }
        match entry.bytes {
            None => {
                folders.insert(entry.path.as_str());
            }
            Some(bytes) => {
                total = total.checked_add(bytes).context("offer size overflow")?;
                ensure!(total <= MAX_TRANSFER_BYTES, "offered files too large");
            }
        }
    }
    Ok(())
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
    pub(super) fn assert_pasted_images_leave_the_clipboard() {
        use clipboard_rs::common::RustImage;
        use std::fs;
        let (mut handler, incoming) = native_handlers(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
        handler.config.files = true;
        handler.config.images = true;
        let folder = tempfile::tempdir().unwrap();

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
            let cancel = crate::shutdown::Signal::default();
            let (entries, total) = list_files(vec![source], &cancel)?;
            send_files(
                &mut writer,
                new_id(),
                &entries,
                total,
                &Outgoing::new(),
                &cancel,
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
    /// up to and including a text message sent after them. `meanwhile` runs
    /// between listing the files and sending them.
    fn sent_files(roots: Vec<PathBuf>, outgoing: Outgoing, meanwhile: impl FnOnce()) -> Vec<Data> {
        use std::{thread, time::Duration};

        let (mut reader, mut writer) = encrypted_pipe();
        reader.set_timeout(Duration::from_secs(5)).unwrap();
        writer.set_timeout(Duration::from_secs(5)).unwrap();
        let cancel = crate::shutdown::Signal::default();
        let (entries, total) = list_files(roots, &cancel).unwrap();
        meanwhile();
        let sender = thread::spawn(move || {
            let mut pacer = BulkPacer::new(test_signals());
            send_files(
                &mut writer,
                new_id(),
                &entries,
                total,
                &outgoing,
                &cancel,
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
    fn files_gone_since_they_were_offered_are_called_off_alone() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("deleted.txt");
        std::fs::write(&file, b"offered").unwrap();
        let arrived = sent_files(vec![file.clone()], Outgoing::new(), || {
            std::fs::remove_file(&file).unwrap()
        });
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
        assert!(
            list_files(vec![file], &crate::shutdown::Signal::default()).is_err(),
            "missing files aren't offered"
        );
    }

    #[test]
    fn copied_files_are_listed_and_sent_only_when_asked_for() {
        use std::{fs, thread, time::Duration};

        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("copied");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.txt"), b"first").unwrap();
        let (mut reader, mut writer) = encrypted_pipe();
        reader.set_timeout(Duration::from_secs(5)).unwrap();
        writer.set_timeout(Duration::from_secs(5)).unwrap();
        let outgoing = Arc::new(Outgoing::new());
        let cancel = Arc::new(crate::shutdown::Signal::default());
        let sender = {
            let (outgoing, cancel) = (outgoing.clone(), cancel.clone());
            thread::spawn(move || {
                send_jobs(
                    &mut writer,
                    outgoing,
                    &cancel,
                    test_signals(),
                    transfers::Context::default(),
                )
            })
        };
        outgoing.submit(Job::Files {
            paths: vec![source],
            received: None,
        });
        let Data::FilesOffer { id, entries, last } = reader.read::<Data>().unwrap() else {
            panic!("a copy is offered")
        };
        assert!(last);
        assert_eq!(
            entries,
            vec![
                OfferedEntry {
                    path: "copied".into(),
                    bytes: None
                },
                OfferedEntry {
                    path: "copied/a.txt".into(),
                    bytes: Some(5)
                },
            ]
        );
        reader.set_timeout(Duration::from_millis(300)).unwrap();
        assert!(reader.read::<Data>().is_err(), "nothing more until asked");
        reader.set_timeout(Duration::from_secs(5)).unwrap();

        outgoing.wanted(id);
        let mut receiver = files::Receiver::new(folder.path().join("received"));
        let paths = loop {
            match receiver.apply(reader.read::<Data>().unwrap()).unwrap() {
                files::Received::Complete { id: sent, paths } => {
                    assert_eq!(sent, id, "the batch answers the offer");
                    break paths;
                }
                files::Received::Pending => {}
                _ => panic!("the files arrive"),
            }
        };
        assert_eq!(fs::read(paths[0].join("a.txt")).unwrap(), b"first");
        // Something newer copied here withdraws the offer.
        outgoing.submit(Job::Text("newer".into()));
        assert!(matches!(reader.read::<Data>().unwrap(), Data::Text(_)));
        outgoing.wanted(id);
        assert!(matches!(
            reader.read::<Data>().unwrap(),
            Data::BatchAbort { id: called_off } if called_off == id
        ));
        cancel.stop();
        outgoing.close();
        sender.join().unwrap().unwrap();
    }

    #[test]
    fn a_newer_copy_stops_an_upload_in_progress() {
        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("large");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.bin"), vec![1; FILE_CHUNK_SIZE * 3]).unwrap();
        let outgoing = Outgoing::new();
        outgoing.submit(Job::Text("newer".into()));
        let arrived = sent_files(vec![source], outgoing, || {});
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
            promise: Weak::new(),
            offering: None,
            peer: outgoing.clone(),
        };
        (handler, incoming)
    }

    /// Offered files reach the clipboard as a list without their contents.
    /// Only a paste reads contents, and that fetches the files and waits.
    #[cfg(windows)]
    pub(super) fn assert_offered_files_arrive_when_pasted() {
        use native::paste::Clipboard;
        use std::{sync::mpsc, time::Duration};
        let (mut handler, mut incoming) = native_handlers(
            NativeClipboard::new().unwrap(),
            NativeClipboard::new().unwrap(),
        );
        handler.config.files = true;
        incoming.config.files = true;
        let cache = tempfile::tempdir().unwrap();
        incoming.files = Some(files::Receiver::new(cache.path().to_path_buf()));
        // A pasting app, on a thread of its own as File Explorer's.
        let (paste, pasting) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::spawn(move || {
            unsafe { windows::Win32::System::Ole::OleInitialize(None) }.unwrap();
            for task in pasting {
                task();
            }
        });
        let pasted = |task: fn(&Clipboard) -> Box<dyn std::any::Any + Send>| {
            let (done, result) = mpsc::channel();
            paste
                .send(Box::new(move || {
                    let _ = done.send(task(&Clipboard::open()));
                }))
                .unwrap();
            result
        };

        let listing = vec![
            OfferedEntry {
                path: "folder".into(),
                bytes: None,
            },
            OfferedEntry {
                path: "folder/offered.txt".into(),
                bytes: Some(5),
            },
        ];
        assert!(
            incoming
                .apply(Data::FilesOffer {
                    id: 9,
                    entries: listing,
                    last: true,
                })
                .unwrap()
                .is_none()
        );
        // The offer on the clipboard is not a copy made here.
        handler.capture().unwrap();
        // File Explorer reads the list whenever the clipboard changes.
        let listed = pasted(|clipboard| Box::new(clipboard.listed()))
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .downcast::<Option<Vec<String>>>()
            .unwrap();
        assert_eq!(
            *listed,
            Some(vec![
                "folder".to_string(),
                r"folder\offered.txt".to_string()
            ])
        );
        let peer = handler.outgoing.clone();
        let (asked, wanted) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = asked.send(peer.recv());
        });
        assert!(
            wanted.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing is fetched before a paste reads contents"
        );

        let contents = pasted(|clipboard| Box::new(clipboard.contents(1).map_err(|e| e.code())));
        assert!(matches!(
            wanted.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(Job::Want(9))
        ));
        assert!(
            contents.recv_timeout(Duration::from_millis(200)).is_err(),
            "the paste waits for the files"
        );
        for message in [
            Data::BatchStart {
                id: 9,
                count: 2,
                bytes: 5,
            },
            Data::Directory {
                id: 9,
                path: "folder".into(),
            },
            Data::FileStart {
                id: 9,
                path: "folder/offered.txt".into(),
                bytes: 5,
            },
            Data::FileChunk {
                id: 9,
                bytes: b"hello".to_vec(),
            },
            Data::FileEnd { id: 9 },
        ] {
            assert!(incoming.apply(message).unwrap().is_none());
        }
        assert!(matches!(
            incoming.apply(Data::BatchEnd { id: 9 }).unwrap(),
            Some(Job::Receipt(9))
        ));
        let contents = contents
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .downcast::<std::result::Result<Vec<u8>, windows_core::HRESULT>>()
            .unwrap();
        assert_eq!(*contents, Ok(b"hello".to_vec()));

        // Nothing about the paste is a copy made here either.
        handler.outgoing = Arc::new(Outgoing::new());
        handler.capture().unwrap();
        assert_no_echo(handler.outgoing.clone());

        // Newer files replace these, and a paste of ones the sender called
        // off fails instead of waiting.
        let newer = vec![OfferedEntry {
            path: "newer.txt".into(),
            bytes: Some(1),
        }];
        incoming
            .apply(Data::FilesOffer {
                id: 10,
                entries: newer,
                last: true,
            })
            .unwrap();
        incoming.apply(Data::BatchAbort { id: 10 }).unwrap();
        let failed = pasted(|clipboard| Box::new(clipboard.contents(0).is_err()))
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .downcast::<bool>()
            .unwrap();
        assert!(*failed);
        incoming.disconnected();
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
