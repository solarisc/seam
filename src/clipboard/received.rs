//! Received files and images wait in a private cache, never in the person's
//! own folders. On Windows, pasting them into a folder moves them there, so
//! that is the only copy left; apps that read them instead leave them here.
//! A copy is deleted once a paste has moved it all away, or once something
//! newer has replaced it on the clipboard and a grace period has passed, so
//! a paste or relay still reading it can finish.
use super::files::STAGING_PREFIX;
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Once},
    time::{Duration, Instant, SystemTime},
};

const GRACE: Duration = Duration::from_secs(10 * 60);

/// One received copy's folder, deleted when the last reference is dropped.
pub(super) struct Item {
    path: PathBuf,
    /// Waits for a paste to move the files away while they are on the
    /// clipboard.
    #[cfg(windows)]
    watch: Mutex<Option<moved::Watch>>,
}

impl Item {
    pub fn new(directory: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path: directory,
            #[cfg(windows)]
            watch: Mutex::new(None),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Calls `moved` once a paste has moved every one of `entries` out of
    /// this copy's folder, unless the copy is retired first.
    #[cfg(windows)]
    pub fn watch(
        &self,
        entries: Vec<PathBuf>,
        moved: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let watch = moved::Watch::start(&self.path, entries, moved)?;
        *self.watch.lock().unwrap() = Some(watch);
        Ok(())
    }

    fn unwatch(&self) {
        #[cfg(windows)]
        self.watch.lock().unwrap().take();
    }
}

impl Drop for Item {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                log::warn!("removing received copy {}: {error}", self.path.display());
            }
            _ => {}
        }
    }
}

/// The cache folder. Its first use in a process clears what earlier runs
/// left behind, before any transfer of this run can start staging there.
pub(super) fn directory() -> Result<PathBuf> {
    let directory = dirs::cache_dir()
        .context("no cache directory")?
        .join("Seam")
        .join("Clipboard");
    static LEFTOVERS: Once = Once::new();
    LEFTOVERS.call_once(|| remove_leftovers(&directory));
    Ok(directory)
}

/// Unfinished transfers are abandoned. The newest finished copy may still be
/// on the clipboard, so it gets the same grace as a replaced one.
fn remove_leftovers(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut finished = Vec::new();
    for entry in entries.flatten() {
        let item = Item::new(entry.path());
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let modified = entry.metadata().and_then(|m| m.modified());
            finished.push((modified.unwrap_or(SystemTime::UNIX_EPOCH), item));
        }
    }
    finished.sort_by_key(|(modified, _)| *modified);
    if let Some((_, newest)) = finished.pop() {
        retire(newest);
    }
}

static RETIRED: Mutex<Retired> = Mutex::new(Retired {
    items: Vec::new(),
    sweeping: false,
});

/// Called when `item` is no longer the clipboard's content. A sweeper
/// deletes it later, so no caller waits for a large folder to be removed.
pub(super) fn retire(item: Arc<Item>) {
    item.unwatch();
    // A paste moved everything away: nothing is left for a reader to finish.
    if fs::read_dir(item.path()).is_ok_and(|mut entries| entries.next().is_none()) {
        return;
    }
    let start = {
        let mut retired = RETIRED.lock().unwrap();
        retired.items.push((Instant::now(), item));
        !std::mem::replace(&mut retired.sweeping, true)
    };
    if start {
        std::thread::spawn(sweep);
    }
}

/// Runs only while copies wait to be deleted.
fn sweep() {
    loop {
        std::thread::sleep(GRACE / 10);
        let (expired, more) = {
            let mut retired = RETIRED.lock().unwrap();
            let expired = retired.expire(Instant::now());
            retired.sweeping = !retired.items.is_empty();
            (expired, retired.sweeping)
        };
        // Delete outside the lock.
        drop(expired);
        if !more {
            return;
        }
    }
}

struct Retired {
    items: Vec<(Instant, Arc<Item>)>,
    sweeping: bool,
}

impl Retired {
    /// Hand back the copies whose grace period has ended.
    fn expire(&mut self, now: Instant) -> Vec<Arc<Item>> {
        let expired = self
            .items
            .iter()
            .take_while(|(retired, _)| now.duration_since(*retired) >= GRACE)
            .count();
        self.items.drain(..expired).map(|(_, item)| item).collect()
    }
}

/// Windows tells a waiting thread when a folder's entries are renamed or
/// deleted, so nothing runs while the copy waits to be pasted.
#[cfg(windows)]
mod moved {
    use anyhow::Result;
    use std::{
        io,
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr,
        sync::Arc,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        Storage::FileSystem::{
            FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FindCloseChangeNotification,
            FindFirstChangeNotificationW, FindNextChangeNotification,
        },
        System::Threading::{
            CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
        },
    };

    /// Stops waiting when dropped.
    pub(super) struct Watch(Arc<Event>);

    struct Event(HANDLE);
    // Kernel handles can be signalled and closed from any thread.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}
    impl Drop for Event {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    struct Changes(HANDLE);
    unsafe impl Send for Changes {}
    impl Drop for Changes {
        fn drop(&mut self) {
            unsafe { FindCloseChangeNotification(self.0) };
        }
    }

    impl Watch {
        pub fn start(
            folder: &Path,
            entries: Vec<PathBuf>,
            moved: impl FnOnce() + Send + 'static,
        ) -> Result<Self> {
            let wide: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
            let changes = unsafe {
                FindFirstChangeNotificationW(
                    wide.as_ptr(),
                    0,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                )
            };
            if changes == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error().into());
            }
            let changes = Changes(changes);
            let stop = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if stop.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            let stop = Arc::new(Event(stop));
            let stopped = stop.clone();
            std::thread::Builder::new()
                .name("received copy".into())
                .spawn(move || {
                    // Listed first, a retired copy stops the wait even when
                    // its folder changed at the same moment.
                    let waits = [stopped.0, changes.0];
                    loop {
                        if entries.iter().all(|entry| gone(entry)) {
                            // Let go of the folder so it can be deleted.
                            drop(changes);
                            if unsafe { WaitForSingleObject(stopped.0, 0) } != WAIT_OBJECT_0 {
                                moved();
                            }
                            return;
                        }
                        let woke =
                            unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
                        if woke != WAIT_OBJECT_0 + 1
                            || unsafe { FindNextChangeNotification(changes.0) } == 0
                        {
                            return;
                        }
                    }
                })?;
            Ok(Self(stop))
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            unsafe { SetEvent(self.0.0) };
        }
    }

    fn gone(entry: &Path) -> bool {
        std::fs::symlink_metadata(entry).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(root: &Path, name: &str) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("file.txt"), b"copy").unwrap();
        directory
    }

    fn item(root: &Path, name: &str) -> Arc<Item> {
        Item::new(copy(root, name))
    }

    #[test]
    fn replaced_copies_outlive_the_grace_period_and_any_relay() {
        let folder = tempfile::tempdir().unwrap();
        let mut retired = Retired {
            items: Vec::new(),
            sweeping: false,
        };
        let start = Instant::now();
        let first = item(folder.path(), "first");
        let second = item(folder.path(), "second");
        let relay = second.clone();
        retired.items.push((start, first));
        retired.items.push((start, second));
        let later = start + GRACE / 2;
        retired.items.push((later, item(folder.path(), "third")));
        assert!(retired.expire(later).is_empty());
        for name in ["first", "second", "third"] {
            assert!(folder.path().join(name).is_dir(), "{name} deleted early");
        }

        drop(retired.expire(start + GRACE));
        assert!(!folder.path().join("first").exists());
        assert!(
            folder.path().join("second").is_dir(),
            "a copy being relayed must survive until the relay finishes"
        );
        assert!(folder.path().join("third").is_dir());
        drop(relay);
        assert!(!folder.path().join("second").exists());
    }

    #[test]
    fn a_copy_a_paste_moved_away_is_deleted_at_once() {
        let folder = tempfile::tempdir().unwrap();
        let directory = folder.path().join("emptied");
        fs::create_dir(&directory).unwrap();
        retire(Item::new(directory.clone()));
        assert!(!directory.exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_paste_that_moves_everything_away_is_noticed() {
        let folder = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let directory = copy(folder.path(), "copy");
        fs::create_dir(directory.join("photos")).unwrap();
        let item = Item::new(directory.clone());
        let (moved, noticed) = crossbeam_channel::bounded(1);
        item.watch(
            vec![directory.join("file.txt"), directory.join("photos")],
            move || moved.send(()).unwrap(),
        )
        .unwrap();
        fs::rename(
            directory.join("file.txt"),
            destination.path().join("file.txt"),
        )
        .unwrap();
        assert!(
            noticed.recv_timeout(Duration::from_millis(300)).is_err(),
            "part of the copy is still there"
        );
        fs::rename(directory.join("photos"), destination.path().join("photos")).unwrap();
        noticed
            .recv_timeout(Duration::from_secs(5))
            .expect("the paste is noticed");
        drop(item);
        assert!(!directory.exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_retired_copy_is_no_longer_watched() {
        let folder = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let item = item(folder.path(), "copy");
        let (moved, noticed) = crossbeam_channel::bounded(1);
        item.watch(vec![item.path().join("file.txt")], move || {
            moved.send(()).unwrap()
        })
        .unwrap();
        item.unwatch();
        fs::rename(
            item.path().join("file.txt"),
            destination.path().join("file.txt"),
        )
        .unwrap();
        assert!(noticed.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn leftovers_keep_only_the_newest_finished_copy() {
        let folder = tempfile::tempdir().unwrap();
        for name in ["older", "newest", &format!("{STAGING_PREFIX}abc")] {
            copy(folder.path(), name);
            std::thread::sleep(Duration::from_millis(20));
        }
        remove_leftovers(folder.path());
        let mut remaining: Vec<_> = fs::read_dir(folder.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        remaining.sort();
        assert_eq!(remaining, ["newest"]);
    }
}
