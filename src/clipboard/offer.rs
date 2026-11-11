//! Copied files are offered, not sent. The computer they were copied on
//! sends them only once a paste on the other computer asks for them, so
//! copying files while working on one computer moves nothing over the
//! network.
use super::{Job, outgoing::Outgoing, received};
use crate::protocol::OfferedEntry;
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak},
};

/// Files another computer offered, fetched from it the first time a paste
/// asks for them, here or on a computer this one passes the offer on to.
pub(super) struct Promise {
    id: u64,
    /// Every file and folder offered, each folder before what it holds.
    entries: Vec<OfferedEntry>,
    /// The connection to the computer that offered them.
    peer: Weak<Outgoing>,
    state: Mutex<State>,
    changed: Condvar,
}

enum State {
    Offered,
    /// Asked for; the files are on their way.
    Wanted,
    Arrived {
        paths: Vec<PathBuf>,
        item: Arc<received::Item>,
    },
    Failed(String),
}

impl Promise {
    pub fn new(id: u64, entries: Vec<OfferedEntry>, peer: &Arc<Outgoing>) -> Arc<Self> {
        Arc::new(Self {
            id,
            entries,
            peer: Arc::downgrade(peer),
            state: Mutex::new(State::Offered),
            changed: Condvar::new(),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn entries(&self) -> &[OfferedEntry] {
        &self.entries
    }

    /// How many items were copied: the entries outside any offered folder.
    pub fn count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !entry.path.contains('/'))
            .count()
    }

    /// Asks the computer that offered the files for them, the first time,
    /// and waits until they have arrived or can't come.
    pub fn fetch(&self) -> Result<(Vec<PathBuf>, Arc<received::Item>)> {
        let state = self.request(self.state.lock().unwrap());
        let state = self
            .changed
            .wait_while(state, |state| matches!(state, State::Wanted))
            .unwrap();
        Self::outcome_of(&state).expect("fetch waits for an outcome")
    }

    /// Asks for the files, the first time, without waiting: `None` while
    /// they are on their way.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn try_fetch(&self) -> Option<Result<(Vec<PathBuf>, Arc<received::Item>)>> {
        let state = self.request(self.state.lock().unwrap());
        Self::outcome_of(&state)
    }

    fn request<'a>(
        &self,
        mut state: std::sync::MutexGuard<'a, State>,
    ) -> std::sync::MutexGuard<'a, State> {
        if matches!(*state, State::Offered) {
            *state = match self.peer.upgrade() {
                Some(peer) => {
                    peer.reply(Job::Want(self.id));
                    State::Wanted
                }
                None => State::Failed("The other computer disconnected".into()),
            };
        }
        state
    }

    fn outcome_of(state: &State) -> Option<Result<(Vec<PathBuf>, Arc<received::Item>)>> {
        match state {
            State::Arrived { paths, item } => Some(Ok((paths.clone(), item.clone()))),
            State::Failed(reason) => Some(Err(anyhow::anyhow!("{reason}"))),
            State::Offered | State::Wanted => None,
        }
    }

    /// Whether these files were asked for and are still on their way.
    #[cfg(test)]
    pub fn wanted(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Wanted)
    }

    pub fn arrived(&self, paths: Vec<PathBuf>, item: Arc<received::Item>) {
        self.settle(State::Arrived { paths, item });
    }

    /// The files can't come, for `reason`. Files that already arrived stay.
    pub fn fail(&self, reason: &str) {
        self.settle(State::Failed(reason.into()));
    }

    fn settle(&self, outcome: State) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Offered | State::Wanted) {
            *state = outcome;
            self.changed.notify_all();
        }
    }
}

impl Drop for Promise {
    fn drop(&mut self) {
        let state = std::mem::replace(self.state.get_mut().unwrap(), State::Offered);
        if let State::Arrived { item, .. } = state {
            // A paste may still be reading them.
            received::retire(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(paths: &[&str]) -> Vec<OfferedEntry> {
        paths
            .iter()
            .map(|path| OfferedEntry {
                path: (*path).into(),
                bytes: path.contains('.').then_some(1),
            })
            .collect()
    }

    #[test]
    fn files_are_asked_for_once_and_shared_by_every_paste() {
        let peer = Arc::new(Outgoing::new());
        let promise = Promise::new(7, listing(&["a.txt"]), &peer);
        assert!(!promise.wanted());
        let waiting = {
            let promise = promise.clone();
            std::thread::spawn(move || promise.fetch().map(|(paths, _)| paths))
        };
        assert!(matches!(peer.recv(), Some(Job::Want(7))));
        while !promise.wanted() {
            std::thread::yield_now();
        }
        let folder = tempfile::tempdir().unwrap();
        let copy = folder.path().join("copy");
        std::fs::create_dir(&copy).unwrap();
        let file = copy.join("a.txt");
        promise.arrived(vec![file.clone()], received::Item::new(copy));
        assert_eq!(waiting.join().unwrap().unwrap(), vec![file.clone()]);
        // A second paste gets the same files without asking again.
        assert_eq!(promise.fetch().unwrap().0, vec![file]);
        peer.close();
        assert!(peer.recv().is_none());
    }

    #[test]
    fn a_paste_learns_why_files_cannot_come() {
        let peer = Arc::new(Outgoing::new());
        let promise = Promise::new(3, listing(&["a", "a/b.txt", "c.txt"]), &peer);
        assert_eq!(promise.count(), 2, "items, not what folders hold");
        promise.fail("Something newer was copied");
        let Err(error) = promise.fetch() else {
            panic!("failed files must not arrive")
        };
        assert_eq!(error.to_string(), "Something newer was copied");
        drop(peer);
        let orphan = Promise::new(4, listing(&["a.txt"]), &Arc::new(Outgoing::new()));
        assert!(orphan.fetch().is_err(), "nobody is left to ask");
    }
}
