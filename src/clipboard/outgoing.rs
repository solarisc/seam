//! Pending clipboard content between native capture and the data writer.
use super::{Entry, Job, list_files, offer::Promise, received};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

pub(super) struct Outgoing {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    pending: Option<Job>,
    replies: VecDeque<Job>,
    /// Offered files a paste asked for, sent ahead of newer copies.
    upload: Option<Job>,
    /// The files last offered to the peer, until something newer is copied.
    offered: Option<(u64, Offered)>,
    /// An upload the receiver turned down, until the writer stops it.
    refused: Option<u64>,
    closed: bool,
}

/// Files offered to the peer.
#[derive(Clone)]
pub(super) enum Offered {
    /// Copied here, or received complete from another computer, as listed
    /// when they were offered.
    Files {
        entries: Arc<Vec<Entry>>,
        total: u64,
        received: Option<Arc<received::Item>>,
    },
    /// Offered by another computer and not fetched from it yet.
    Promised(Arc<Promise>),
}

/// Why an upload in progress should stop.
pub(super) enum Stop {
    /// The receiver turned it down and shows why.
    Refused,
    /// A newer copy is waiting and would replace it on arrival anyway.
    Superseded,
}

impl Outgoing {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        }
    }

    pub fn submit(&self, job: Job) {
        let replaced = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.pending.replace(job)
        };
        self.ready.notify_one();
        // Free a superseded image/text buffer without holding the writer lock.
        drop(replaced);
    }

    /// Queue a receipt or refusal for a transfer from the peer. Replies go
    /// out ahead of copies, which can take a long time to send.
    pub fn reply(&self, job: Job) {
        let mut state = self.state.lock().unwrap();
        if state.replies.len() < 64 {
            state.replies.push_back(job);
        }
        self.ready.notify_one();
    }

    /// Files `id` were offered to the peer; `None` once something else was
    /// copied instead.
    pub fn offer(&self, offer: Option<(u64, Offered)>) {
        let replaced = std::mem::replace(&mut self.state.lock().unwrap().offered, offer);
        drop(replaced);
    }

    /// A paste on the peer asked for offered files `id`: send them, or say
    /// they are gone.
    pub fn wanted(self: &Arc<Self>, id: u64) {
        let offered = {
            let state = self.state.lock().unwrap();
            state
                .offered
                .as_ref()
                .filter(|(offered, _)| *offered == id)
                .map(|(_, offered)| offered.clone())
        };
        match offered {
            None => self.reply(Job::Withdrawn(id)),
            Some(Offered::Files {
                entries,
                total,
                received,
            }) => self.upload(Job::Upload {
                id,
                entries,
                total,
                received,
            }),
            // Fetch them from the computer they were copied on first,
            // without holding up this connection's other messages.
            Some(Offered::Promised(promise)) => {
                let outgoing = self.clone();
                let fetched = std::thread::Builder::new()
                    .name("passing on files".into())
                    .spawn(move || {
                        let listed = promise.fetch().and_then(|(paths, item)| {
                            let (entries, total) =
                                list_files(paths, &crate::shutdown::Signal::default())?;
                            Ok((entries, total, item))
                        });
                        match listed {
                            Ok((entries, total, item)) => outgoing.upload(Job::Upload {
                                id,
                                entries: Arc::new(entries),
                                total,
                                received: Some(item),
                            }),
                            Err(error) => {
                                log::info!("cannot pass on copied files: {error:#}");
                                outgoing.reply(Job::Withdrawn(id));
                            }
                        }
                    });
                if fetched.is_err() {
                    self.reply(Job::Withdrawn(id));
                }
            }
        }
    }

    fn upload(&self, job: Job) {
        let replaced = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.upload.replace(job)
        };
        self.ready.notify_one();
        drop(replaced);
    }

    /// The peer turned down upload `id`.
    pub fn refused(&self, id: u64) {
        self.state.lock().unwrap().refused = Some(id);
    }

    /// Checked between the chunks of upload `id`.
    pub fn stop(&self, id: u64) -> Option<Stop> {
        let state = self.state.lock().unwrap();
        if state.refused == Some(id) {
            Some(Stop::Refused)
        } else if state.pending.is_some() {
            Some(Stop::Superseded)
        } else {
            None
        }
    }

    pub fn recv(&self) -> Option<Job> {
        let mut state = self
            .ready
            .wait_while(self.state.lock().unwrap(), |state| {
                state.pending.is_none()
                    && state.upload.is_none()
                    && state.replies.is_empty()
                    && !state.closed
            })
            .unwrap();
        state
            .replies
            .pop_front()
            .or_else(|| state.upload.take())
            .or_else(|| state.pending.take())
    }

    pub fn close(&self) {
        let discarded = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            (
                state.pending.take(),
                state.upload.take(),
                state.offered.take(),
            )
        };
        self.ready.notify_all();
        drop(discarded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_stop_when_refused_or_replaced_and_replies_go_first() {
        let outgoing = Outgoing::new();
        assert!(outgoing.stop(1).is_none());
        outgoing.refused(1);
        assert!(matches!(outgoing.stop(1), Some(Stop::Refused)));
        assert!(outgoing.stop(2).is_none(), "a refusal names one upload");
        outgoing.submit(Job::Text("newer".into()));
        assert!(matches!(outgoing.stop(2), Some(Stop::Superseded)));
        outgoing.reply(Job::Receipt(9));
        assert!(matches!(outgoing.recv(), Some(Job::Receipt(9))));
        assert!(matches!(outgoing.recv(), Some(Job::Text(text)) if text == "newer"));
    }

    #[test]
    fn offered_files_are_sent_only_when_asked_for() {
        let outgoing = Arc::new(Outgoing::new());
        let entries = Arc::new(vec![(
            std::path::PathBuf::from("copied.bin"),
            "copied.bin".to_string(),
            Some(3),
        )]);
        outgoing.offer(Some((
            5,
            Offered::Files {
                entries: entries.clone(),
                total: 3,
                received: None,
            },
        )));
        // An older offer, or one replaced by something newer, is gone.
        outgoing.wanted(4);
        assert!(matches!(outgoing.recv(), Some(Job::Withdrawn(4))));
        outgoing.submit(Job::Text("newer".into()));
        outgoing.wanted(5);
        assert!(
            matches!(outgoing.recv(), Some(Job::Upload { id: 5, entries: sent, .. }) if sent == entries),
            "a paste's files go ahead of newer copies"
        );
        assert!(matches!(outgoing.recv(), Some(Job::Text(_))));
        outgoing.offer(None);
        outgoing.wanted(5);
        assert!(matches!(outgoing.recv(), Some(Job::Withdrawn(5))));
    }
}
