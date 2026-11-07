use crate::{
    config::{Arrangement, Side},
    protocol::Monitor,
};

// Trigger just before the desktop wall. Low-level Windows hooks can stop
// reporting useful outward motion once the hardware cursor is clamped there.
const EDGE_ACTIVATION_PX: f64 = 12.0;

/// The part of an edge, at each end, where the pointer does not switch
/// computers when corners are kept: a share of the edge, at least this much.
const CORNER_SHARE: f64 = 0.04;
const CORNER_MIN_PX: f64 = 24.0;

#[derive(Clone, Debug)]
pub struct Geometry {
    pub local: Monitor,
    pub remote: Monitor,
    pub all_local: Vec<Monitor>,
    pub all_remote: Vec<Monitor>,
    pub side: Side,
    pub offset: f64,
    /// Screen corners don't switch computers, so a flick to a window's close
    /// button stays on this computer.
    pub keep_corners: bool,
}

impl Geometry {
    pub fn new(arrangement: &Arrangement, local: &[Monitor], remote: &[Monitor]) -> Option<Self> {
        let select = |monitors: &[Monitor], selected: Option<u32>| {
            monitors
                .iter()
                .find(|m| Some(m.id) == selected)
                .or_else(|| monitors.iter().find(|m| m.primary))
                .or_else(|| monitors.first())
                .cloned()
        };
        Some(Self {
            local: select(local, arrangement.local_monitor)?,
            remote: select(remote, arrangement.remote_monitor)?,
            all_local: local.to_vec(),
            all_remote: remote.to_vec(),
            side: arrangement.side,
            offset: arrangement.edge_offset,
            keep_corners: false,
        })
    }

    /// Only this computer's side of a seam, for a computer that isn't
    /// connected: `enter` tells whether the pointer leaves across the edge,
    /// and the position it gives means nothing.
    pub fn local_edge(arrangement: &Arrangement, local: &[Monitor]) -> Option<Self> {
        Self::new(arrangement, local, local)
    }

    pub fn with_kept_corners(mut self, keep: bool) -> Self {
        self.keep_corners = keep;
        self
    }

    /// A desktop-wide outer corner for parking a hidden source cursor. A
    /// monitor immediately to the right or below a selected seam display
    /// must not turn the parked cursor into a visible cursor on that monitor.
    pub fn local_park_corner(&self) -> (f64, f64) {
        self.all_local
            .iter()
            .max_by_key(|m| {
                (
                    i64::from(m.x) + i64::from(m.width),
                    i64::from(m.y) + i64::from(m.height),
                )
            })
            .map(|m| (m.right() - 1.0, m.bottom() - 1.0))
            .unwrap_or((self.local.right() - 1.0, self.local.bottom() - 1.0))
    }

    pub fn enter(&self, x: f64, y: f64, dx: f64, dy: f64) -> Option<(f64, f64)> {
        let l = &self.local;
        if !l.contains(x, y) {
            return None;
        }
        let crossing = match self.side {
            Side::Right => x >= l.right() - EDGE_ACTIVATION_PX && dx > 0.0,
            Side::Left => x < l.x as f64 + EDGE_ACTIVATION_PX && dx < 0.0,
            Side::Top => y < l.y as f64 + EDGE_ACTIVATION_PX && dy < 0.0,
            Side::Bottom => y >= l.bottom() - EDGE_ACTIVATION_PX && dy > 0.0,
        };
        if !crossing || (self.keep_corners && at_corner(l, self.side, x, y)) {
            return None;
        }
        let blocked_by_local = match self.side {
            Side::Right => self
                .all_local
                .iter()
                .any(|m| m.id != l.id && m.contains(l.right(), y)),
            Side::Left => self
                .all_local
                .iter()
                .any(|m| m.id != l.id && m.contains(l.x as f64 - 1.0, y)),
            Side::Top => self
                .all_local
                .iter()
                .any(|m| m.id != l.id && m.contains(x, l.y as f64 - 1.0)),
            Side::Bottom => self
                .all_local
                .iter()
                .any(|m| m.id != l.id && m.contains(x, l.bottom())),
        };
        if blocked_by_local {
            return None;
        }
        let r = &self.remote;
        let tx = (x - l.x as f64) / l.width as f64
            - if matches!(self.side, Side::Top | Side::Bottom) {
                self.offset
            } else {
                0.0
            };
        let ty = (y - l.y as f64) / l.height as f64
            - if matches!(self.side, Side::Right | Side::Left) {
                self.offset
            } else {
                0.0
            };
        if !(0.0..=1.0).contains(&tx) || !(0.0..=1.0).contains(&ty) {
            return None;
        }
        Some(match self.side {
            Side::Right => (r.x as f64 + 2.0, r.y as f64 + ty * (r.height - 1) as f64),
            Side::Left => (r.right() - 3.0, r.y as f64 + ty * (r.height - 1) as f64),
            Side::Top => (r.x as f64 + tx * (r.width - 1) as f64, r.bottom() - 3.0),
            Side::Bottom => (r.x as f64 + tx * (r.width - 1) as f64, r.y as f64 + 2.0),
        })
    }

    /// Returns the next remote position, or the local re-entry position.
    /// While a drag owns the destination, motion still traverses its displays
    /// and clamps to their edges, but cannot return to the source computer.
    pub fn advance(&self, x: f64, y: f64, dx: f64, dy: f64, allow_return: bool) -> Advance {
        let nx = x + dx;
        let ny = y + dy;
        let r = &self.remote;
        let return_to_local = match self.side {
            Side::Right => nx < r.x as f64 && dx < 0.0 && ny >= r.y as f64 && ny < r.bottom(),
            Side::Left => nx >= r.right() && dx > 0.0 && ny >= r.y as f64 && ny < r.bottom(),
            Side::Top => ny >= r.bottom() && dy > 0.0 && nx >= r.x as f64 && nx < r.right(),
            Side::Bottom => ny < r.y as f64 && dy < 0.0 && nx >= r.x as f64 && nx < r.right(),
        };
        // The other computer's corners on the shared edge stay there too.
        let corner = self.keep_corners && at_corner(r, self.side, nx, ny);
        if allow_return && return_to_local && !corner {
            let l = &self.local;
            let tx = ((nx - r.x as f64) / r.width as f64).clamp(0.0, 1.0)
                + if matches!(self.side, Side::Top | Side::Bottom) {
                    self.offset
                } else {
                    0.0
                };
            let ty = ((ny - r.y as f64) / r.height as f64).clamp(0.0, 1.0)
                + if matches!(self.side, Side::Right | Side::Left) {
                    self.offset
                } else {
                    0.0
                };
            return Advance::Return(match self.side {
                Side::Right => (l.right() - 3.0, l.y as f64 + ty * (l.height - 1) as f64),
                Side::Left => (l.x as f64 + 2.0, l.y as f64 + ty * (l.height - 1) as f64),
                Side::Top => (l.x as f64 + tx * (l.width - 1) as f64, l.y as f64 + 2.0),
                Side::Bottom => (l.x as f64 + tx * (l.width - 1) as f64, l.bottom() - 3.0),
            });
        }
        Advance::Remote(clamp_to_displays(nx, ny, &self.all_remote))
    }
}

impl Geometry {
    /// The nearest point on the destination's displays.
    pub fn clamp_remote(&self, x: f64, y: f64) -> (f64, f64) {
        clamp_to_displays(x, y, &self.all_remote)
    }
}

#[derive(Debug, PartialEq)]
pub enum Advance {
    Remote((f64, f64)),
    Return((f64, f64)),
}

/// Whether (`x`, `y`) lies at either end of the edge of `screen` facing
/// `side`, where screen corners are.
fn at_corner(screen: &Monitor, side: Side, x: f64, y: f64) -> bool {
    let (along, start, length) = match side {
        Side::Left | Side::Right => (y, screen.y as f64, screen.height as f64),
        Side::Top | Side::Bottom => (x, screen.x as f64, screen.width as f64),
    };
    let size = (length * CORNER_SHARE).max(CORNER_MIN_PX);
    along < start + size || along >= start + length - size
}

fn clamp_to_displays(x: f64, y: f64, monitors: &[Monitor]) -> (f64, f64) {
    monitors
        .iter()
        .map(|m| {
            let px = x.clamp(m.x as f64, m.right() - 1.0);
            let py = y.clamp(m.y as f64, m.bottom() - 1.0);
            let distance = (px - x).powi(2) + (py - y).powi(2);
            (distance, (px, py))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|v| v.1)
        .unwrap_or((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn monitor(id: u32, x: i32, y: i32, w: u32, h: u32) -> Monitor {
        Monitor {
            id,
            identity: None,
            name: id.to_string(),
            x,
            y,
            width: w,
            height: h,
            primary: id == 1,
        }
    }
    #[test]
    fn kept_corners_stay_on_their_computer_in_both_directions() {
        let g = Geometry::new(
            &Arrangement::default(),
            &[monitor(1, 0, 0, 1920, 1080)],
            &[monitor(2, 0, 0, 2560, 1440)],
        )
        .unwrap()
        .with_kept_corners(true);
        // A flick to the top-right close button.
        assert_eq!(g.enter(1919.0, 10.0, 5.0, 0.0), None);
        assert_eq!(g.enter(1919.0, 1070.0, 5.0, 0.0), None);
        assert!(g.enter(1919.0, 540.0, 5.0, 0.0).is_some());
        // The other computer's top-left corner on the shared edge.
        assert!(matches!(
            g.advance(2.0, 12.0, -4.0, 0.0, true),
            Advance::Remote(_)
        ));
        assert!(matches!(
            g.advance(2.0, 720.0, -4.0, 0.0, true),
            Advance::Return(_)
        ));
        // Without kept corners, corners switch as before.
        let g = g.with_kept_corners(false);
        assert!(g.enter(1919.0, 10.0, 5.0, 0.0).is_some());
    }

    #[test]
    fn enters_and_returns_at_matching_edge() {
        let g = Geometry::new(
            &Arrangement::default(),
            &[monitor(1, 0, 0, 1920, 1080)],
            &[monitor(2, 0, 0, 2560, 1440)],
        )
        .unwrap();
        let start = g.enter(1919.0, 540.0, 5.0, 0.0).unwrap();
        assert_eq!(start.0, 2.0);
        assert!((start.1 - 720.0).abs() < 2.0);
        assert!(matches!(
            g.advance(start.0, start.1, -4.0, 0.0, true),
            Advance::Return(_)
        ));
    }
    #[test]
    fn local_neighbor_blocks_transition() {
        let g = Geometry::new(
            &Arrangement::default(),
            &[
                monitor(1, 0, 0, 1920, 1080),
                monitor(3, 1920, 0, 1920, 1080),
            ],
            &[monitor(2, 0, 0, 1920, 1080)],
        )
        .unwrap();
        assert_eq!(g.enter(1919.0, 500.0, 6.0, 0.0), None);
    }

    #[test]
    fn parks_at_outer_corner_across_local_displays() {
        let g = Geometry::new(
            &Arrangement::default(),
            &[
                monitor(1, 0, 0, 1920, 1080),
                monitor(3, 1920, 0, 2560, 1440),
            ],
            &[monitor(2, 0, 0, 1920, 1080)],
        )
        .unwrap();
        assert_eq!(g.local_park_corner(), (4479.0, 1439.0));
    }

    #[test]
    fn hands_off_before_cursor_hits_the_wall() {
        let g = Geometry::new(
            &Arrangement::on(Side::Left),
            &[monitor(1, 0, 0, 1920, 1080)],
            &[monitor(2, 0, 0, 1920, 1080)],
        )
        .unwrap();
        assert!(g.enter(8.0, 540.0, -3.0, 0.0).is_some());
        assert!(g.enter(16.0, 540.0, -3.0, 0.0).is_none());
        assert!(g.enter(8.0, 540.0, 3.0, 0.0).is_none());
    }
}
