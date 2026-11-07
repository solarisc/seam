//! Pending clipboard content between native capture and the data writer.
use super::Job;
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};

pub(super) struct Outgoing {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    pending: Option<Job>,
    replies: VecDeque<Job>,
    /// An upload the receiver turned down, until the writer stops it.
    refused: Option<u64>,
    closed: bool,
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
                state.pending.is_none() && state.replies.is_empty() && !state.closed
            })
            .unwrap();
        state.replies.pop_front().or_else(|| state.pending.take())
    }

    pub fn close(&self) {
        let discarded = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.pending.take()
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
}
