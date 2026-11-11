use crate::{
    config::{Arrangement, Link, Side},
    geometry::Geometry,
    input::{Controller, InputQueue},
    protocol::{Input, Monitor},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub side: Side,
    pub edge_offset: f64,
    pub local_monitor: Option<u32>,
    pub remote_monitor: Option<u32>,
}

impl Layout {
    pub fn of(arrangement: &Arrangement) -> Self {
        Self {
            side: arrangement.side,
            edge_offset: arrangement.edge_offset,
            local_monitor: arrangement.local_monitor,
            remote_monitor: arrangement.remote_monitor,
        }
    }

    pub fn write_to(&self, arrangement: &mut Arrangement) {
        if arrangement.local_monitor != self.local_monitor {
            arrangement.local_monitor_uuid = None;
        }
        if arrangement.remote_monitor != self.remote_monitor {
            arrangement.remote_monitor_uuid = None;
        }
        arrangement.side = self.side;
        arrangement.edge_offset = self.edge_offset;
        arrangement.local_monitor = self.local_monitor;
        arrangement.remote_monitor = self.remote_monitor;
    }

    fn mirrored(&self) -> Self {
        Self {
            side: self.side.opposite(),
            edge_offset: -self.edge_offset,
            local_monitor: self.remote_monitor,
            remote_monitor: self.local_monitor,
        }
    }

    /// The layout both computers install: concrete displays, each moved to
    /// the outside of its desktop when another display covers the chosen
    /// one's edge (see `geometry::seam_display`), so the saved arrangement
    /// follows the displays and the pointer can always cross.
    fn resolve(&self, local: &[Monitor], remote: &[Monitor]) -> Result<Self> {
        ensure!(
            (-0.9..=0.9).contains(&self.edge_offset),
            "Invalid display alignment"
        );
        fn select(monitors: &[Monitor], selected: Option<u32>, side: Side) -> Result<u32> {
            let monitor = match selected {
                Some(id) => monitors.iter().find(|m| m.id == id),
                None => monitors
                    .iter()
                    .find(|m| m.primary)
                    .or_else(|| monitors.first()),
            }
            .ok_or_else(|| {
                anyhow::anyhow!("Selected display is no longer available; choose another")
            })?;
            ensure!(
                monitor.width > 0 && monitor.height > 0,
                "Invalid display size"
            );
            Ok(crate::geometry::seam_display(monitors, monitor, side).id)
        }
        Ok(Self {
            local_monitor: Some(select(local, self.local_monitor, self.side)?),
            remote_monitor: Some(select(remote, self.remote_monitor, self.side.opposite())?),
            ..self.clone()
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub layout: Layout,
    pub revision: u64,
    pub ready: bool,
    pub pending: bool,
    pub error: String,
}

/// One coordinator (the controlling computer) serializes changes from either GUI.
/// Neither peer permits new handoffs until both have installed the same layout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Control {
    Propose {
        id: u64,
        expected: u64,
        layout: Layout,
    },
    Prepare {
        id: u64,
        layout: Layout,
    },
    Ready {
        id: u64,
    },
    Commit {
        id: u64,
    },
    Applied {
        id: u64,
    },
    Finish {
        id: u64,
    },
    Reject {
        id: u64,
        reason: String,
    },
    Abort {
        id: u64,
        reason: String,
    },
    Unavailable {
        id: u64,
        layout: Layout,
        reason: String,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Preparing,
    Prepared,
    Committing,
    Applied,
}

struct Transaction {
    id: u64,
    layout: Layout,
    stage: Stage,
    started: Instant,
}

type Reply = crossbeam_channel::Sender<std::result::Result<Report, String>>;

struct Session {
    queue: Arc<InputQueue>,
    remote: Vec<Monitor>,
    transaction: Option<Transaction>,
    waiter: Option<(u64, Reply, Instant)>,
    revision: u64,
    ready: bool,
}

struct State {
    link: Link,
    local: Vec<Monitor>,
    session: Option<Session>,
    error: String,
}

pub struct Service {
    state: Mutex<State>,
    controller: Arc<Controller>,
    path: PathBuf,
    coordinator: bool,
}

impl Service {
    /// The computer sharing its keyboard and mouse coordinates the layout,
    /// so the arrangement chosen there is applied when a link connects.
    pub fn new(
        mut link: Link,
        local: Vec<Monitor>,
        controller: Arc<Controller>,
        path: PathBuf,
    ) -> Arc<Self> {
        link.arrangement.restore_local(&local);
        Arc::new(Self {
            coordinator: link.connects(),
            controller,
            path,
            state: Mutex::new(State {
                link,
                local,
                session: None,
                error: String::new(),
            }),
        })
    }

    pub fn report(&self) -> Report {
        Self::report_locked(&self.state.lock().unwrap())
    }

    fn report_locked(state: &State) -> Report {
        Report {
            layout: Layout::of(&state.link.arrangement),
            revision: state.session.as_ref().map_or(0, |s| s.revision),
            ready: state.session.as_ref().is_some_and(|s| s.ready),
            pending: state
                .session
                .as_ref()
                .is_some_and(|s| s.transaction.is_some() || s.waiter.is_some()),
            error: state.error.clone(),
        }
    }

    pub fn local_monitors(&self) -> Vec<Monitor> {
        self.state.lock().unwrap().local.clone()
    }

    /// A desktop change invalidates the current coordinate space. End the input
    /// session so its receiver releases held input, then exchange fresh displays
    /// on reconnect. A seam display that is gone is stood in for by the main
    /// display until it returns; a reused display number never decides
    /// (see `Arrangement::restore_local`).
    pub fn update_monitors(&self, mut local: Vec<Monitor>) {
        local.sort_by_key(|m| m.id);
        let mut state = self.state.lock().unwrap();
        state.local.sort_by_key(|m| m.id);
        if state.local == local {
            return;
        }
        state.link.arrangement.restore_local(&local);
        state.local = local;
        Self::disconnect_locked(
            &mut state,
            &self.controller,
            "Displays changed; reconnecting input sharing",
        );
    }

    pub fn attach(
        &self,
        local: &[Monitor],
        remote: Vec<Monitor>,
        os: &str,
        queue: Arc<InputQueue>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        ensure!(state.session.is_none(), "Duplicate layout session");
        ensure!(
            state.local == local,
            "Displays changed during connection setup"
        );
        state.link.arrangement.restore_remote(&remote);
        self.controller
            .attach(&state.link, &state.local, &remote, os, queue.clone());
        state.error.clear();
        state.session = Some(Session {
            queue,
            remote,
            transaction: None,
            waiter: None,
            revision: 0,
            ready: false,
        });
        if self.coordinator {
            let layout = Layout::of(&state.link.arrangement);
            let id = rand::random();
            match layout.resolve(&state.local, &state.session.as_ref().unwrap().remote) {
                Ok(layout) => self.begin(&mut state, id, layout)?,
                Err(error) => {
                    state.error = error.to_string();
                    state.session.as_mut().unwrap().revision = id;
                    Self::send(
                        &state,
                        Control::Unavailable {
                            id,
                            layout,
                            reason: state.error.clone(),
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn detach(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(mut session) = state.session.take() {
            if let Some((_, reply, _)) = session.waiter.take() {
                let _ = reply.send(Err(
                    "Connection ended before both computers confirmed the layout".into(),
                ));
            }
            session.queue.close();
        }
        self.controller.detach();
    }

    /// The same shutdown path is used for failed transactions and normal teardown.
    pub fn disconnect(&self, reason: &str) {
        let mut state = self.state.lock().unwrap();
        Self::disconnect_locked(&mut state, &self.controller, reason);
    }

    fn disconnect_locked(state: &mut State, controller: &Controller, reason: &str) {
        state.error = reason.into();
        // Detaching removes the geometry as well as restoring local capture.
        // Otherwise a physical event could re-enter while shutdown is pending.
        controller.detach();
        if let Some(session) = &mut state.session {
            session.ready = false;
            session.queue.discard_and_close();
        }
    }

    pub fn request(
        &self,
        layout: Layout,
        expected: u64,
    ) -> Result<crossbeam_channel::Receiver<std::result::Result<Report, String>>> {
        let mut state = self.state.lock().unwrap();
        let session = state.session.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Connect the other computer before applying its layout")
        })?;
        ensure!(
            !session.queue.is_closed()
                && session.revision != 0
                && session.transaction.is_none()
                && session.waiter.is_none(),
            "A layout change is already being applied"
        );
        ensure!(
            session.revision == expected,
            "The layout changed on the other computer; review the latest arrangement first"
        );
        let layout = layout.resolve(&state.local, &session.remote)?;
        let id = rand::random();
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.coordinator {
            self.begin(&mut state, id, layout)?;
        } else {
            Self::send(
                &state,
                Control::Propose {
                    id,
                    expected,
                    layout: layout.mirrored(),
                },
            )?;
        }
        state.session.as_mut().unwrap().waiter = Some((id, tx, Instant::now()));
        Ok(rx)
    }

    fn send(state: &State, control: Control) -> Result<()> {
        let session = state
            .session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No layout session"))?;
        ensure!(
            session.queue.push(Input::Layout(control)),
            "Layout connection closed"
        );
        Ok(())
    }

    fn begin(&self, state: &mut State, id: u64, layout: Layout) -> Result<()> {
        let session = state.session.as_ref().unwrap();
        ensure!(
            session.transaction.is_none(),
            "Another layout change is in progress"
        );
        let layout = layout.resolve(&state.local, &session.remote)?;
        self.controller.freeze_layout()?;
        state.session.as_mut().unwrap().transaction = Some(Transaction {
            id,
            layout: layout.clone(),
            stage: Stage::Preparing,
            started: Instant::now(),
        });
        Self::send(state, Control::Prepare { id, layout })
    }

    fn transaction(state: &State, id: u64, stage: Stage) -> Result<&Transaction> {
        let transaction = state
            .session
            .as_ref()
            .and_then(|s| s.transaction.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Unexpected layout message"))?;
        ensure!(
            transaction.id == id && transaction.stage == stage,
            "Out-of-order layout message"
        );
        Ok(transaction)
    }

    fn persist(&self, state: &mut State, layout: &Layout) -> Result<()> {
        let mut arrangement = state.link.arrangement.clone();
        layout.write_to(&mut arrangement);
        arrangement.remember(&state.local, &state.session.as_ref().unwrap().remote);
        // Several links may negotiate simultaneously. Merge only this link's
        // arrangement into the latest file.
        let peer = state
            .link
            .peer
            .map(|index| state.link.config.peers[index].code.clone());
        if !self.path.exists() {
            state.link.config.save_to(&self.path)?;
        }
        crate::config::edit_at(&self.path, |saved| {
            let target = match &peer {
                Some(code) => {
                    &mut saved
                        .peers
                        .iter_mut()
                        .find(|peer| &peer.code == code)
                        .ok_or_else(|| anyhow::anyhow!("Computer was removed from settings"))?
                        .arrangement
                }
                None => &mut saved.arrangement,
            };
            *target = arrangement.clone();
            Ok(())
        })?;
        let geometry = Geometry::new(
            &arrangement,
            &state.local,
            &state.session.as_ref().unwrap().remote,
        )
        .ok_or_else(|| anyhow::anyhow!("Selected display unavailable"))?
        .with_kept_corners(state.link.config.keep_corners);
        self.controller.install_geometry(geometry);
        state.link.arrangement = arrangement;
        Ok(())
    }

    fn finish(&self, state: &mut State, id: u64) {
        let session = state.session.as_mut().unwrap();
        session.transaction = None;
        session.revision = id;
        session.ready = true;
        state.error.clear();
        self.controller.unfreeze_layout();
        let reply = if session
            .waiter
            .as_ref()
            .is_some_and(|(request, _, _)| *request == id)
        {
            session.waiter.take()
        } else {
            None
        };
        if let Some((_, reply, _)) = reply {
            let _ = reply.send(Ok(Self::report_locked(state)));
        }
    }

    fn reject(&self, state: &mut State, id: u64, reason: String) -> Result<()> {
        let session = state.session.as_mut().unwrap();
        if let Some(transaction) = &session.transaction
            && transaction.id == id
        {
            ensure!(
                matches!(transaction.stage, Stage::Preparing | Stage::Prepared),
                "Peer rejected a committed layout"
            );
            session.transaction = None;
            if session.ready {
                self.controller.unfreeze_layout();
            }
        }
        if session
            .waiter
            .as_ref()
            .is_some_and(|(request, _, _)| *request == id)
        {
            let (_, reply, _) = session.waiter.take().unwrap();
            let _ = reply.send(Err(reason.clone()));
        }
        state.error = reason;
        Ok(())
    }

    pub fn receive(&self, control: Control) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        ensure!(
            state.session.as_ref().is_some_and(|s| !s.queue.is_closed()),
            "Layout message arrived outside an active session"
        );
        match control {
            Control::Propose {
                id,
                expected,
                layout,
            } => {
                ensure!(self.coordinator, "Only the coordinator accepts proposals");
                let session = state.session.as_ref().unwrap();
                let result = if session.revision != 0 && session.revision == expected {
                    self.begin(&mut state, id, layout)
                } else {
                    Err(anyhow::anyhow!(
                        "Layout changed; refresh the arrangement and try again"
                    ))
                };
                if let Err(error) = result {
                    Self::send(
                        &state,
                        Control::Reject {
                            id,
                            reason: error.to_string(),
                        },
                    )?;
                }
            }
            Control::Prepare { id, layout } => {
                ensure!(!self.coordinator, "Only the coordinator prepares layouts");
                ensure!(
                    state.session.as_ref().unwrap().transaction.is_none(),
                    "Overlapping layout transaction"
                );
                let layout = layout
                    .mirrored()
                    .resolve(&state.local, &state.session.as_ref().unwrap().remote)?;
                if let Err(error) = self.controller.freeze_layout() {
                    Self::send(
                        &state,
                        Control::Reject {
                            id,
                            reason: error.to_string(),
                        },
                    )?;
                    return Ok(());
                }
                state.session.as_mut().unwrap().transaction = Some(Transaction {
                    id,
                    layout,
                    stage: Stage::Prepared,
                    started: Instant::now(),
                });
                Self::send(&state, Control::Ready { id })?;
            }
            Control::Ready { id } => {
                ensure!(self.coordinator, "Unexpected layout acknowledgement");
                let layout = Self::transaction(&state, id, Stage::Preparing)?
                    .layout
                    .clone();
                self.persist(&mut state, &layout)?;
                state
                    .session
                    .as_mut()
                    .unwrap()
                    .transaction
                    .as_mut()
                    .unwrap()
                    .stage = Stage::Committing;
                Self::send(&state, Control::Commit { id })?;
            }
            Control::Commit { id } => {
                ensure!(!self.coordinator, "Unexpected layout commit");
                let layout = Self::transaction(&state, id, Stage::Prepared)?
                    .layout
                    .clone();
                self.persist(&mut state, &layout)?;
                state
                    .session
                    .as_mut()
                    .unwrap()
                    .transaction
                    .as_mut()
                    .unwrap()
                    .stage = Stage::Applied;
                Self::send(&state, Control::Applied { id })?;
            }
            Control::Applied { id } => {
                ensure!(
                    self.coordinator,
                    "Unexpected layout installation acknowledgement"
                );
                Self::transaction(&state, id, Stage::Committing)?;
                Self::send(&state, Control::Finish { id })?;
                self.finish(&mut state, id);
            }
            Control::Finish { id } => {
                ensure!(!self.coordinator, "Unexpected layout completion");
                Self::transaction(&state, id, Stage::Applied)?;
                self.finish(&mut state, id);
            }
            Control::Reject { id, reason } => {
                if self.coordinator {
                    Self::send(
                        &state,
                        Control::Abort {
                            id,
                            reason: reason.clone(),
                        },
                    )?;
                }
                self.reject(&mut state, id, reason)?;
            }
            Control::Abort { id, reason } => {
                ensure!(!self.coordinator, "Only the coordinator aborts layouts");
                self.reject(&mut state, id, reason)?;
            }
            Control::Unavailable { id, layout, reason } => {
                ensure!(
                    !self.coordinator,
                    "Only the coordinator reports unavailable layouts"
                );
                let session = state.session.as_mut().unwrap();
                ensure!(
                    session.revision == 0 && session.transaction.is_none(),
                    "Unexpected unavailable layout"
                );
                session.revision = id;
                // This report may contain the missing-display sentinel. Keep
                // identities so a later reconnect can find the same screens.
                let arrangement = &mut state.link.arrangement;
                let local_identity = arrangement.local_monitor_uuid.clone();
                let remote_identity = arrangement.remote_monitor_uuid.clone();
                layout.mirrored().write_to(arrangement);
                arrangement.local_monitor_uuid = local_identity;
                arrangement.remote_monitor_uuid = remote_identity;
                state.error = reason;
            }
        }
        Ok(())
    }

    pub fn tick(&self) {
        let state = self.state.lock().unwrap();
        let expired = state.session.as_ref().is_some_and(|session| {
            session
                .transaction
                .as_ref()
                .is_some_and(|t| t.started.elapsed() >= Duration::from_secs(4))
                || session
                    .waiter
                    .as_ref()
                    .is_some_and(|(_, _, at)| at.elapsed() >= Duration::from_secs(4))
        });
        drop(state);
        if expired {
            self.disconnect("Layout synchronization timed out");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, x: i32, width: u32, primary: bool) -> Monitor {
        Monitor {
            id,
            identity: None,
            name: String::new(),
            x,
            y: 0,
            width,
            height: 1080,
            primary,
        }
    }

    #[test]
    fn the_settled_layout_meets_the_outside_of_both_desktops() {
        let local = [display(1, 0, 1920, true), display(2, 1920, 1920, false)];
        let remote = [display(7, 0, 1440, true), display(8, 1440, 1920, false)];
        // On the left of the right-hand display, meeting the other
        // computer's left-hand display: both edges are covered.
        let layout = Layout {
            side: Side::Left,
            edge_offset: 0.1,
            local_monitor: Some(2),
            remote_monitor: Some(7),
        };
        let settled = layout.resolve(&local, &remote).unwrap();
        assert_eq!(settled.local_monitor, Some(1));
        assert_eq!(settled.remote_monitor, Some(8));
        assert_eq!((settled.side, settled.edge_offset), (Side::Left, 0.1));
        // Settling again changes nothing, and the other computer settles
        // the mirrored layout to the same displays.
        assert_eq!(settled.resolve(&local, &remote).unwrap(), settled);
        assert_eq!(
            layout.mirrored().resolve(&remote, &local).unwrap(),
            settled.mirrored()
        );
        // The main display stands in when none is chosen, moved outward too.
        let unchosen = Layout {
            local_monitor: None,
            remote_monitor: None,
            side: Side::Right,
            ..layout
        };
        let settled = unchosen.resolve(&local, &remote).unwrap();
        assert_eq!(
            (settled.local_monitor, settled.remote_monitor),
            (Some(2), Some(7))
        );
    }
}
