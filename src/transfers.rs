//! Bounded transfer telemetry. No timer or worker runs when transfers are idle.
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u64,
    pub uploading: bool,
    pub peer: String,
    pub name: String,
    pub bytes: u64,
    pub total: u64,
    pub bytes_per_second: f64,
    pub eta_seconds: Option<f64>,
    pub state: String,
    pub error: String,
}
struct Entry {
    session: u64,
    updated: Instant,
    snapshot: Snapshot,
}
static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

#[derive(Clone, Default)]
pub(crate) struct Context {
    pub session: u64,
    pub peer: String,
}
pub(crate) fn disconnect(session: u64) {
    for entry in ENTRIES.lock().unwrap().iter_mut() {
        if entry.session == session && active(&entry.snapshot) {
            entry.snapshot.state = "failed".into();
            entry.snapshot.error = "Connection interrupted. Copy again to retry.".into();
            entry.updated = Instant::now();
        }
    }
}
fn active(snapshot: &Snapshot) -> bool {
    matches!(
        snapshot.state.as_str(),
        "preparing" | "transferring" | "finishing"
    )
}
pub fn snapshots() -> Vec<Snapshot> {
    let mut entries = ENTRIES.lock().unwrap();
    entries.retain(|e| active(&e.snapshot) || e.updated.elapsed() < Duration::from_secs(8));
    entries
        .iter()
        .map(|e| {
            let mut s = e.snapshot.clone();
            if active(&s) && e.updated.elapsed() > Duration::from_secs(2) {
                s.bytes_per_second = 0.0;
                s.eta_seconds = None;
            }
            s
        })
        .collect()
}
/// The receiver turned down upload `id`, for `reason`.
pub(crate) fn refused(session: u64, id: u64, reason: &str) {
    let mut entries = ENTRIES.lock().unwrap();
    if let Some(entry) = entries
        .iter_mut()
        .find(|e| e.session == session && e.snapshot.id == id && e.snapshot.uploading)
    {
        entry.snapshot.state = "failed".into();
        entry.snapshot.error = reason.chars().take(240).collect();
        entry.snapshot.eta_seconds = None;
        entry.updated = Instant::now();
    }
}

/// A byte count the way the transfer list shows it.
pub fn size(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub(crate) fn receipt(session: u64, id: u64) {
    let mut entries = ENTRIES.lock().unwrap();
    if let Some(entry) = entries
        .iter_mut()
        .find(|e| e.session == session && e.snapshot.id == id && e.snapshot.uploading)
    {
        entry.snapshot.bytes = entry.snapshot.total;
        entry.snapshot.state = "complete".into();
        entry.updated = Instant::now();
    }
}

pub(crate) struct Transfer {
    session: u64,
    snapshot: Snapshot,
    sampled: Instant,
    sampled_bytes: u64,
    released: bool,
}
impl Transfer {
    pub fn start(context: &Context, id: u64, uploading: bool, name: String, total: u64) -> Self {
        let mut result = Self {
            session: context.session,
            snapshot: Snapshot {
                id,
                uploading,
                peer: context.peer.clone(),
                name,
                bytes: 0,
                total,
                bytes_per_second: 0.0,
                eta_seconds: None,
                state: "preparing".into(),
                error: String::new(),
            },
            sampled: Instant::now(),
            sampled_bytes: 0,
            released: false,
        };
        result.publish();
        result
    }
    pub fn total(&mut self, total: u64) {
        self.snapshot.total = total;
        self.snapshot.state = "transferring".into();
        self.publish();
    }
    pub fn name(&mut self, name: &str) {
        self.snapshot.name = name.chars().take(256).collect();
    }
    pub fn advance(&mut self, bytes: usize) {
        self.snapshot.bytes += bytes as u64;
        if self.sampled.elapsed() >= Duration::from_millis(150) {
            let rate = (self.snapshot.bytes - self.sampled_bytes) as f64
                / self.sampled.elapsed().as_secs_f64();
            self.snapshot.bytes_per_second = if self.snapshot.bytes_per_second == 0.0 {
                rate
            } else {
                self.snapshot.bytes_per_second * 0.65 + rate * 0.35
            };
            self.snapshot.eta_seconds = (self.snapshot.bytes_per_second > 0.0).then(|| {
                self.snapshot.total.saturating_sub(self.snapshot.bytes) as f64
                    / self.snapshot.bytes_per_second
            });
            self.sampled = Instant::now();
            self.sampled_bytes = self.snapshot.bytes;
            self.publish();
        }
    }
    pub fn finishing(&mut self) {
        self.snapshot.state = "finishing".into();
        self.snapshot.eta_seconds = None;
        self.publish();
    }
    pub fn sent(mut self) {
        self.finishing();
        self.released = true;
    }
    pub fn complete(mut self) {
        self.snapshot.state = "complete".into();
        self.publish();
        self.released = true;
    }
    pub fn fail(&mut self, error: &str) {
        self.end("failed", error);
    }
    /// Ended without being an error, such as a copy replaced by a newer one.
    pub fn skip(mut self, why: &str) {
        self.end("skipped", why);
    }
    /// Ended by the peer, whose reason the entry already shows.
    pub fn release(mut self) {
        self.released = true;
    }
    fn end(&mut self, state: &str, why: &str) {
        self.snapshot.state = state.into();
        self.snapshot.error = why.chars().take(240).collect();
        self.snapshot.eta_seconds = None;
        self.publish();
        self.released = true;
    }
    fn publish(&mut self) {
        let mut entries = ENTRIES.lock().unwrap();
        if let Some(entry) = entries.iter_mut().find(|e| {
            e.session == self.session
                && e.snapshot.id == self.snapshot.id
                && e.snapshot.uploading == self.snapshot.uploading
        }) {
            // A receipt or refusal can arrive before the final socket write
            // returns; it decides how the transfer ended.
            if active(&entry.snapshot) {
                entry.snapshot = self.snapshot.clone();
                entry.updated = Instant::now();
            }
        } else {
            if entries.len() >= 32 {
                if let Some(at) = entries.iter().position(|e| !active(&e.snapshot)) {
                    entries.remove(at);
                } else {
                    return;
                }
            }
            entries.push(Entry {
                session: self.session,
                updated: Instant::now(),
                snapshot: self.snapshot.clone(),
            });
        }
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.released {
            self.fail("Transfer interrupted. Copy again to retry.");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_requires_receipt_and_disconnect_fails_pending_uploads() {
        let ctx = Context {
            session: rand::random(),
            peer: "Test peer".into(),
        };
        let id = rand::random();
        let mut transfer =
            Transfer::start(&ctx, id, true, "large.iso".into(), 5 * 1024 * 1024 * 1024);
        transfer.total(5 * 1024 * 1024 * 1024);
        transfer.advance(4096);
        transfer.sent();
        assert_eq!(
            snapshots().into_iter().find(|s| s.id == id).unwrap().state,
            "finishing"
        );
        receipt(ctx.session, id);
        let result = snapshots().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(result.state, "complete");
        assert_eq!(result.bytes, 5 * 1024 * 1024 * 1024);
        let failed = rand::random();
        Transfer::start(&ctx, failed, true, "next.iso".into(), 100).sent();
        disconnect(ctx.session);
        assert_eq!(
            snapshots()
                .into_iter()
                .find(|s| s.id == failed)
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            snapshots().into_iter().find(|s| s.id == id).unwrap().state,
            "complete"
        );
    }
}
