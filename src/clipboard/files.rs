//! Receiving file batches, independent of the native clipboard.
use super::{MAX_TRANSFER_BYTES, MAX_TRANSFER_FILES, safe_relative};
use crate::protocol::{Data, FILE_CHUNK_SIZE};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Unfinished batches are staged in folders named with this prefix.
pub(super) const STAGING_PREFIX: &str = ".seam-incoming-";

/// Free space left beyond a batch, so receiving one never fills the disk.
const SPACE_MARGIN: u64 = 1024 * 1024 * 1024;

pub(super) struct Receiver {
    directory: PathBuf,
    batch: Option<Batch>,
    /// A batch this computer couldn't take; its remaining messages are ignored.
    refused: Option<u64>,
}

/// What a file-transfer message did.
pub(super) enum Received {
    Pending,
    Complete {
        id: u64,
        paths: Vec<PathBuf>,
    },
    /// This computer couldn't take batch `id`. Nothing of it is kept.
    Failed {
        id: u64,
        error: anyhow::Error,
    },
    /// The sender stopped the batch in progress.
    Aborted,
}

struct IncomingFile {
    file: File,
    expected: u64,
    received: u64,
}
struct Batch {
    id: u64,
    // Rust drops fields in declaration order: close an unfinished file before
    // TempDir removes its containing tree, including on Windows.
    current: Option<IncomingFile>,
    staging: tempfile::TempDir,
    destination: PathBuf,
    top: HashSet<String>,
    seen: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
    expected: u32,
    entries: u32,
    bytes: u64,
    expected_bytes: u64,
}
impl Receiver {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            batch: None,
            refused: None,
        }
    }

    /// Messages that don't fit the batch in progress are an error: the peer
    /// is broken or hostile. A batch this computer can't take, for any reason
    /// from a full disk to a name Windows doesn't allow, fails on its own.
    pub fn apply(&mut self, message: Data) -> Result<Received> {
        let id = match &message {
            Data::BatchStart { id, .. }
            | Data::Directory { id, .. }
            | Data::FileStart { id, .. }
            | Data::FileChunk { id, .. }
            | Data::FileEnd { id }
            | Data::BatchEnd { id }
            | Data::BatchAbort { id } => *id,
            _ => bail!("unexpected file-transfer message"),
        };
        let ends = matches!(message, Data::BatchEnd { .. } | Data::BatchAbort { .. });
        if self.refused == Some(id) {
            if ends {
                self.refused = None;
            }
            return Ok(Received::Pending);
        }
        let current = self.batch.as_ref().map(|batch| batch.id);
        match message {
            Data::BatchAbort { .. } if current == Some(id) => {
                self.batch = None;
                return Ok(Received::Aborted);
            }
            // The receiver refused it, or it ended before it started.
            Data::BatchAbort { .. } => return Ok(Received::Pending),
            Data::BatchStart { .. } if current.is_some() => bail!("overlapping file batches"),
            Data::BatchStart { .. } => {}
            _ if current != Some(id) => bail!("file transfer without its batch"),
            _ => {}
        }
        match self.receive(message) {
            Ok(Some(paths)) => Ok(Received::Complete { id, paths }),
            Ok(None) => Ok(Received::Pending),
            Err(error) => {
                // Dropping the batch removes its staging folder.
                self.batch = None;
                if !ends {
                    self.refused = Some(id);
                }
                Ok(Received::Failed { id, error })
            }
        }
    }

    fn receive(&mut self, message: Data) -> Result<Option<Vec<PathBuf>>> {
        match message {
            Data::BatchStart { id, count, bytes } => {
                if count == 0 || count as usize > MAX_TRANSFER_FILES || bytes > MAX_TRANSFER_BYTES {
                    bail!("invalid transfer entry count");
                }
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                fs::create_dir_all(&self.directory)?;
                if let Some(free) = free_space(&self.directory)
                    && bytes.saturating_add(SPACE_MARGIN) > free
                {
                    bail!(
                        "Not enough free disk space for {}",
                        crate::transfers::size(bytes)
                    );
                }
                // Keep staging on the destination filesystem for atomic rename.
                // Peer-supplied names never alias this batch's temporary files.
                let staging = tempfile::Builder::new()
                    .prefix(STAGING_PREFIX)
                    .tempdir_in(&self.directory)?;
                let destination = self
                    .directory
                    .join(format!("{now}-{:016x}", super::new_id()));
                self.batch = Some(Batch {
                    id,
                    staging,
                    destination,
                    top: HashSet::new(),
                    seen: HashSet::new(),
                    directories: HashSet::new(),
                    expected: count,
                    entries: 0,
                    bytes: 0,
                    expected_bytes: bytes,
                    current: None,
                });
            }
            Data::Directory { id, path } => {
                let batch = self.batch_mut(id)?;
                let relative = safe_relative(&path)?;
                record_entry(batch, &relative)?;
                ensure_directory(batch, &relative)?;
            }
            Data::FileStart { id, path, bytes } => {
                let batch = self.batch_mut(id)?;
                if batch.current.is_some() {
                    bail!("overlapping files");
                }
                let relative = safe_relative(&path)?;
                record_entry(batch, &relative)?;
                batch.bytes = batch
                    .bytes
                    .checked_add(bytes)
                    .context("transfer size overflow")?;
                if batch.bytes > batch.expected_bytes {
                    bail!("transfer too large");
                }
                ensure_directory(batch, relative.parent().unwrap())?;
                let target = batch.staging.path().join(relative);
                // create_new also refuses filesystem aliases of an existing
                // entry (for example case-insensitive names on Windows).
                let file = File::options().write(true).create_new(true).open(&target)?;
                batch.current = Some(IncomingFile {
                    file,
                    expected: bytes,
                    received: 0,
                });
            }
            Data::FileChunk { id, bytes } => {
                let file = self
                    .batch_mut(id)?
                    .current
                    .as_mut()
                    .context("file chunk without file")?;
                if bytes.len() > FILE_CHUNK_SIZE
                    || file.received + bytes.len() as u64 > file.expected
                {
                    bail!("invalid file chunk");
                }
                file.file.write_all(&bytes)?;
                file.received += bytes.len() as u64;
            }
            Data::FileEnd { id } => {
                let batch = self.batch_mut(id)?;
                let mut file = batch.current.take().context("file end without file")?;
                if file.received != file.expected {
                    bail!("incomplete file");
                }
                // No fsync: the cache is disposable, and a crash before
                // BatchEnd discards the batch anyway.
                file.file.flush()?;
            }
            Data::BatchEnd { id } => {
                let batch = self.batch.take().context("batch end without start")?;
                if batch.id != id
                    || batch.current.is_some()
                    || batch.entries != batch.expected
                    || batch.bytes != batch.expected_bytes
                {
                    bail!("incomplete transfer batch");
                }
                ensure!(
                    !batch.destination.exists(),
                    "completed transfer directory already exists"
                );
                rename(batch.staging.path(), &batch.destination)?;
                // The files now belong to the completed directory. Do not let
                // cleanup revisit the vacated temporary pathname.
                let _ = batch.staging.keep();
                let mut paths: Vec<PathBuf> = batch
                    .top
                    .iter()
                    .map(|name| batch.destination.join(name))
                    .collect();
                paths.sort();
                return Ok(Some(paths));
            }
            _ => bail!("unexpected file-transfer message"),
        }
        Ok(None)
    }
    fn batch_mut(&mut self, id: u64) -> Result<&mut Batch> {
        let batch = self.batch.as_mut().context("transfer without batch")?;
        if batch.id != id {
            bail!("transfer ID mismatch");
        }
        Ok(batch)
    }
}

/// Antivirus and search indexers briefly open files just written, and
/// Windows refuses to move their folder until they let go.
fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match fs::rename(from, to) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => return result,
        }
    }
}

/// Bytes available to this user on the volume holding `path`.
fn free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats = unsafe { std::mem::zeroed::<libc::statvfs>() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        Some(u64::from(stats.f_bavail).saturating_mul(stats.f_frsize))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        None
    }
}

fn ensure_directory(batch: &mut Batch, path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    for directory in path.ancestors() {
        if directory.as_os_str().is_empty() || batch.directories.contains(directory) {
            break;
        }
        missing.push(directory);
    }
    for directory in missing.into_iter().rev() {
        // Reuse only the exact spelling we created. create_dir rejects an
        // existing filesystem alias, including case or Unicode equivalence,
        // without imposing either platform's naming rules on other volumes.
        fs::create_dir(batch.staging.path().join(directory))
            .context("cannot create distinct transfer directory")?;
        batch.directories.insert(directory.to_path_buf());
    }
    Ok(())
}

fn record_entry(batch: &mut Batch, path: &Path) -> Result<()> {
    if !batch.seen.insert(path.to_path_buf()) {
        bail!("duplicate transfer path");
    }
    batch.entries += 1;
    if batch.entries > batch.expected {
        bail!("too many transfer entries");
    }
    let first = path
        .components()
        .next()
        .context("empty path")?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    batch.top.insert(first);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receive_file(receiver: &mut Receiver, id: u64, name: &str, bytes: &[u8]) {
        for message in [
            Data::FileStart {
                id,
                path: name.into(),
                bytes: bytes.len() as u64,
            },
            Data::FileChunk {
                id,
                bytes: bytes.to_vec(),
            },
            Data::FileEnd { id },
        ] {
            assert!(
                matches!(receiver.apply(message).unwrap(), Received::Pending),
                "files must not be published before BatchEnd"
            );
        }
    }

    fn complete(received: Result<Received>) -> Vec<PathBuf> {
        match received.unwrap() {
            Received::Complete { paths, .. } => paths,
            _ => panic!("expected a completed batch"),
        }
    }

    /// Failed on its own, or an error that ends the connection.
    fn rejected(received: Result<Received>) -> bool {
        matches!(received, Err(_) | Ok(Received::Failed { .. }))
    }

    #[test]
    fn a_failed_or_aborted_batch_ends_alone_and_the_next_one_arrives() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("received");
        let mut receiver = Receiver::new(directory.clone());
        let start = |id, bytes| Data::BatchStart {
            id,
            count: 1,
            bytes,
        };
        receiver.apply(start(1, 3)).unwrap();
        // Larger than announced: this batch fails, the connection doesn't.
        let failed = receiver.apply(Data::FileStart {
            id: 1,
            path: "big.bin".into(),
            bytes: 4,
        });
        assert!(matches!(failed, Ok(Received::Failed { id: 1, .. })));
        // The sender's remaining messages for it are ignored.
        for message in [
            Data::FileChunk {
                id: 1,
                bytes: vec![0; 4],
            },
            Data::FileEnd { id: 1 },
            Data::BatchAbort { id: 1 },
        ] {
            assert!(matches!(
                receiver.apply(message).unwrap(),
                Received::Pending
            ));
        }
        assert!(fs::read_dir(&directory).unwrap().next().is_none());

        receiver.apply(start(2, 3)).unwrap();
        receive_file(&mut receiver, 2, "partial.bin", b"abc");
        assert!(matches!(
            receiver.apply(Data::BatchAbort { id: 2 }).unwrap(),
            Received::Aborted
        ));
        assert!(fs::read_dir(&directory).unwrap().next().is_none());

        receiver.apply(start(3, 2)).unwrap();
        receive_file(&mut receiver, 3, "next.txt", b"ok");
        let paths = complete(receiver.apply(Data::BatchEnd { id: 3 }));
        assert_eq!(fs::read(&paths[0]).unwrap(), b"ok");
        // Messages for a batch that isn't in progress still end the connection.
        assert!(
            receiver
                .apply(Data::FileChunk {
                    id: 9,
                    bytes: vec![1],
                })
                .is_err()
        );
    }

    #[test]
    fn completed_batches_preserve_user_filenames_and_bytes() {
        let folder = tempfile::tempdir().unwrap();
        let mut receiver = Receiver::new(folder.path().join("received"));
        let literal_names = [".hidden", "COM0.txt", "COM10.txt", "NULL", "report .txt"];
        receiver
            .apply(Data::BatchStart {
                id: 7,
                count: 5 + literal_names.len() as u32,
                bytes: 92,
            })
            .unwrap();
        // The old per-file temporary suffix aliases this legitimate filename.
        receive_file(&mut receiver, 7, "report.seam-part", b"first file");
        receive_file(&mut receiver, 7, "report", b"second file");
        // An implicit parent may later arrive as an explicit directory entry.
        receive_file(&mut receiver, 7, "字 space/hash#%.txt", b"nested file");
        for name in ["empty folder", "字 space"] {
            assert!(matches!(
                receiver
                    .apply(Data::Directory {
                        id: 7,
                        path: name.into()
                    })
                    .unwrap(),
                Received::Pending
            ));
        }
        for name in literal_names {
            receive_file(&mut receiver, 7, name, b"literal name");
        }
        let paths = complete(receiver.apply(Data::BatchEnd { id: 7 }));
        assert_eq!(paths.len(), 4 + literal_names.len());
        let root = paths[0].parent().unwrap();
        for name in literal_names {
            let path = paths
                .iter()
                .find(|p| p.file_name().unwrap() == name)
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), b"literal name");
            assert!(
                fs::read_dir(root)
                    .unwrap()
                    .any(|entry| entry.unwrap().file_name() == name)
            );
        }
        assert_eq!(fs::read_dir(root.join("empty folder")).unwrap().count(), 0);
        assert_eq!(
            fs::read(root.join("字 space/hash#%.txt")).unwrap(),
            b"nested file"
        );
        for (name, bytes) in [
            ("report", b"second file".as_slice()),
            ("report.seam-part", b"first file".as_slice()),
        ] {
            let path = paths
                .iter()
                .find(|p| p.file_name().unwrap() == name)
                .unwrap();
            assert_eq!(
                fs::read(path).expect("a completed file must survive the whole batch"),
                bytes
            );
        }
        // A later batch may reuse a wire ID and the same filenames. It must
        // receive its own directory without replacing the earlier result.
        receiver
            .apply(Data::BatchStart {
                id: 7,
                count: 1,
                bytes: 10,
            })
            .unwrap();
        receive_file(&mut receiver, 7, "report", b"later file");
        let later = complete(receiver.apply(Data::BatchEnd { id: 7 }));
        drop(receiver);
        assert_eq!(later.len(), 1);
        assert_eq!(fs::read(&later[0]).unwrap(), b"later file");
        assert_ne!(later[0].parent().unwrap(), root);
    }

    #[test]
    fn distinct_directory_names_never_merge_on_the_destination_filesystem() {
        let mut merged = Vec::new();
        for (first, second) in [("Folder", "folder"), ("café", "cafe\u{301}")] {
            for explicit in [true, false] {
                let folder = tempfile::tempdir().unwrap();
                // Ask the actual destination filesystem about name equivalence.
                // Case-sensitive volumes must still accept distinct names.
                let probe = folder.path().join("probe");
                fs::create_dir(&probe).unwrap();
                fs::create_dir(probe.join(first)).unwrap();
                let aliases = match fs::create_dir(probe.join(second)) {
                    Ok(()) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => true,
                    Err(error) => panic!("filesystem name probe failed: {error}"),
                };
                let destination = folder.path().join("received");
                let mut receiver = Receiver::new(destination.clone());
                receiver
                    .apply(Data::BatchStart {
                        id: 7,
                        count: if explicit { 3 } else { 2 },
                        bytes: 5,
                    })
                    .unwrap();
                if explicit {
                    receiver
                        .apply(Data::Directory {
                            id: 7,
                            path: format!("container/{first}"),
                        })
                        .unwrap();
                }
                receive_file(
                    &mut receiver,
                    7,
                    &format!("container/{first}/first.txt"),
                    b"first",
                );
                let entry = if explicit {
                    Data::Directory {
                        id: 7,
                        path: format!("container/{second}"),
                    }
                } else {
                    Data::FileStart {
                        id: 7,
                        path: format!("container/{second}/second.txt"),
                        bytes: 0,
                    }
                };
                let result = receiver.apply(entry).unwrap();
                if aliases {
                    if !matches!(result, Received::Failed { .. }) {
                        merged.push((first, second, explicit));
                    }
                    // A failed batch removes everything it staged.
                    assert!(fs::read_dir(destination).unwrap().next().is_none());
                } else {
                    assert!(matches!(result, Received::Pending));
                    if !explicit {
                        receiver.apply(Data::FileEnd { id: 7 }).unwrap();
                    }
                    let paths = complete(receiver.apply(Data::BatchEnd { id: 7 }));
                    assert_eq!(paths.len(), 1);
                    let container = &paths[0];
                    assert_eq!(fs::read_dir(container).unwrap().count(), 2);
                    assert_eq!(
                        fs::read(container.join(first).join("first.txt")).unwrap(),
                        b"first"
                    );
                    assert!(container.join(second).is_dir());
                    if !explicit {
                        assert_eq!(
                            fs::metadata(container.join(second).join("second.txt"))
                                .unwrap()
                                .len(),
                            0
                        );
                    }
                }
            }
        }
        assert!(
            merged.is_empty(),
            "receiver merged distinct directory names: {merged:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_unrepresentable_names_before_creating_entries() {
        for (directory, name) in [
            (false, "report."),
            (false, "report "),
            (false, "folder./report.txt"),
            (true, "folder "),
            (false, "NUL.txt"),
            (false, "cOm¹.log"),
            (false, "COM1 .txt"),
            (true, "lpt9"),
            (false, "a?b"),
            (false, "a\u{1}b"),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let destination = folder.path().join("received");
            let mut receiver = Receiver::new(destination.clone());
            receiver
                .apply(Data::BatchStart {
                    id: 7,
                    count: 1,
                    bytes: 0,
                })
                .unwrap();
            let message = if directory {
                Data::Directory {
                    id: 7,
                    path: name.into(),
                }
            } else {
                Data::FileStart {
                    id: 7,
                    path: name.into(),
                    bytes: 0,
                }
            };
            assert!(
                rejected(receiver.apply(message)),
                "must reject unsupported Windows name {name:?}"
            );
            // A rejection must precede filesystem entry creation, not just fail
            // later when trying to publish the paths to the native clipboard.
            for stage in fs::read_dir(&destination).unwrap() {
                assert_eq!(fs::read_dir(stage.unwrap().path()).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn cancelled_and_rejected_batches_remove_unpublished_files() {
        let mut leaked = Vec::new();
        for case in [
            "cancel-open",
            "wrong-id",
            "missing-entry",
            "open-file",
            "truncated-file",
        ] {
            let folder = tempfile::tempdir().unwrap();
            let directory = folder.path().join("received");
            let mut receiver = Receiver::new(directory.clone());
            receiver
                .apply(Data::BatchStart {
                    id: 7,
                    count: if case == "missing-entry" { 2 } else { 1 },
                    bytes: 3,
                })
                .unwrap();
            receiver
                .apply(Data::FileStart {
                    id: 7,
                    path: "nested/file.txt".into(),
                    bytes: 3,
                })
                .unwrap();
            receiver
                .apply(Data::FileChunk {
                    id: 7,
                    bytes: if case == "truncated-file" {
                        vec![1, 2]
                    } else {
                        vec![1, 2, 3]
                    },
                })
                .unwrap();
            match case {
                "cancel-open" => {}
                "open-file" => assert!(rejected(receiver.apply(Data::BatchEnd { id: 7 }))),
                "truncated-file" => assert!(rejected(receiver.apply(Data::FileEnd { id: 7 }))),
                _ => {
                    receiver.apply(Data::FileEnd { id: 7 }).unwrap();
                    assert!(rejected(receiver.apply(Data::BatchEnd {
                        id: if case == "wrong-id" { 8 } else { 7 }
                    })));
                }
            }
            drop(receiver);
            if fs::read_dir(&directory).unwrap().next().is_some() {
                leaked.push(case);
            }
        }
        assert!(
            leaked.is_empty(),
            "unpublished transfers leaked on {leaked:?}"
        );
    }
}
