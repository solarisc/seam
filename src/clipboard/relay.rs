//! The controlling computer explicitly fans out received copies to its other
//! destinations. Native generation suppression keeps them from being echoed.
use super::{Job, Outgoing};
use crate::config::Config;
use std::sync::{Arc, Mutex, Weak};

struct Link {
    id: u64,
    outgoing: Weak<Outgoing>,
    text: bool,
    images: bool,
    files: bool,
}
static LINKS: Mutex<Vec<Link>> = Mutex::new(Vec::new());
pub(super) struct Registration(u64);
impl Registration {
    /// Only links from the controlling computer to its destinations relay.
    pub fn new(id: u64, outgoing: &Arc<Outgoing>, config: &Config, controls_peer: bool) -> Self {
        if controls_peer {
            LINKS.lock().unwrap().push(Link {
                id,
                outgoing: Arc::downgrade(outgoing),
                text: config.clipboard,
                images: config.images,
                files: config.files,
            });
        }
        Self(id)
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        LINKS.lock().unwrap().retain(|link| link.id != self.0);
    }
}
pub(super) fn publish(source: u64, job: Job) {
    let links = LINKS.lock().unwrap();
    // Only a controller's outgoing links participate. Receivers never relay
    // back to their controller, including during simultaneous native updates.
    if !links.iter().any(|link| link.id == source) {
        return;
    }
    for link in links.iter().filter(|link| link.id != source) {
        let enabled = match job {
            Job::Text(_) => link.text,
            Job::Image { .. } => link.images,
            Job::Files { .. } => link.files,
            Job::Receipt(_) | Job::Refusal { .. } => false,
        };
        if enabled && let Some(outgoing) = link.outgoing.upgrade() {
            outgoing.submit(job.clone());
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn received_copy_reaches_other_destinations_once_without_echoing_to_source() {
        let config = Config::default();
        let first = Arc::new(Outgoing::new());
        let second = Arc::new(Outgoing::new());
        let ids: [u64; 2] = rand::random();
        let _a = Registration::new(ids[0], &first, &config, true);
        let _b = Registration::new(ids[1], &second, &config, true);
        publish(ids[0], Job::Text("shared copy".into()));
        assert!(matches!(second.recv(), Some(Job::Text(text)) if text == "shared copy"));
        first.close();
        second.close();
        assert!(first.recv().is_none());
        assert!(second.recv().is_none());
    }
}
