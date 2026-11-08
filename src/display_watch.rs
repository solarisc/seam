//! When the display watcher reads the displays in full.
//!
//! Each second the watcher takes a cheap look at the display layout: where
//! each display is, how big, and which is the main one. Only a change there,
//! or time, calls for a full read, which also asks for every display's
//! persistent identity.
use std::time::{Duration, Instant};

/// Even with the layout unchanged, read the displays in full this often: a
/// display swapped for another of the same size on the same port changes
/// only its identity.
const FULL_READ: Duration = Duration::from_secs(30);

pub(crate) struct LayoutWatch<L> {
    /// The layout at the last successful full read, and when.
    read: Option<(L, Instant)>,
}

impl<L> Default for LayoutWatch<L> {
    fn default() -> Self {
        Self { read: None }
    }
}

impl<L: PartialEq> LayoutWatch<L> {
    /// `layout` is `None` when the system would not describe it; a full read
    /// is the only way to learn more then.
    pub(crate) fn needs_read(&self, layout: Option<&L>, now: Instant) -> bool {
        match (&self.read, layout) {
            (Some((seen, at)), Some(layout)) => {
                seen != layout || now.duration_since(*at) >= FULL_READ
            }
            _ => true,
        }
    }

    /// A full read succeeded with `layout` in place.
    pub(crate) fn read(&mut self, layout: Option<L>, now: Instant) {
        self.read = layout.map(|layout| (layout, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_read_follows_a_layout_change_or_time() {
        let start = Instant::now();
        let mut watch = LayoutWatch::default();
        let two = vec![(0, 1920), (1920, 1920)];
        assert!(watch.needs_read(Some(&two), start), "nothing read yet");
        watch.read(Some(two.clone()), start);
        assert!(!watch.needs_read(Some(&two), start + Duration::from_secs(1)));
        // A display unplugged or moved, or the layout unreadable: read now.
        assert!(watch.needs_read(Some(&vec![(0, 1920)]), start + Duration::from_secs(1)));
        assert!(watch.needs_read(Some(&vec![(0, 1920), (-1920, 1920)]), start));
        assert!(watch.needs_read(None, start));
        // Unchanged, but a display may have been swapped for an identical one.
        assert!(watch.needs_read(Some(&two), start + FULL_READ));
        // A failed full read records nothing, so the next look reads again.
        watch.read(None, start);
        assert!(watch.needs_read(Some(&two), start + Duration::from_secs(1)));
    }
}
