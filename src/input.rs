use crate::{
    config::{Config, Link, Side},
    geometry::{Advance, Geometry},
    protocol::{Input, Monitor, MouseButton, ScrollUnit},
};
use rdev::{Button, Event, EventType, Key};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const QUEUE_CAPACITY: usize = 512;
/// A sleeping computer is woken once the pointer has pushed against its edge
/// this long, so a flick to a scroll bar at that edge wakes nothing.
const WAKE_PUSH: Duration = Duration::from_millis(250);
/// A longer pause between motion reports ends a push.
const WAKE_PUSH_GAP: Duration = Duration::from_millis(150);

pub(crate) const MODIFIER_KEYS: [Key; 9] = [
    Key::ControlLeft,
    Key::ControlRight,
    Key::ShiftLeft,
    Key::ShiftRight,
    Key::Alt,
    Key::AltGr,
    Key::MetaLeft,
    Key::MetaRight,
    Key::Function,
];

/// The order to release held keys in: ordinary keys first, then Shift, Alt
/// and Ctrl last, so the moment between releases never looks like Alt+Shift
/// alone (Windows' keyboard layout shortcut) or a lone Alt that opens a menu.
pub(crate) fn release_rank(key: &Key) -> u8 {
    match key {
        Key::ShiftLeft | Key::ShiftRight => 1,
        Key::Alt | Key::AltGr => 2,
        Key::MetaLeft | Key::MetaRight => 3,
        Key::ControlLeft | Key::ControlRight => 4,
        _ => 0,
    }
}

#[cfg(not(target_os = "windows"))]
fn synchronize_local_modifiers(keys: &HashSet<Key>, down: bool) {
    #[cfg(target_os = "macos")]
    crate::mac_capture::set_local_modifiers(keys, down);
    #[cfg(not(target_os = "macos"))]
    let _ = (keys, down);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueEnd {
    Closed,
    Overloaded,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<Input>,
    end: Option<QueueEnd>,
}

pub struct InputQueue {
    inner: Mutex<QueueState>,
    wake: Condvar,
}

impl InputQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QueueState::default()),
            wake: Condvar::new(),
        }
    }

    pub fn push(&self, event: Input) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.end.is_some() {
            return false;
        }
        let queue = &mut inner.events;
        if matches!(event, Input::Pointer { .. })
            && matches!(queue.back(), Some(Input::Pointer { .. }))
        {
            queue.pop_back();
        }
        // Raw motion is relative: merge it with motion still waiting.
        if let Input::Motion { dx, dy } = event
            && let Some(Input::Motion {
                dx: waiting_x,
                dy: waiting_y,
            }) = queue.back_mut()
        {
            *waiting_x = waiting_x.saturating_add(dx);
            *waiting_y = waiting_y.saturating_add(dy);
            return true;
        }
        if queue.len() >= QUEUE_CAPACITY {
            // Ordered input cannot be dropped safely: an old position can own
            // the next click, and a rejected release can leave a modifier held.
            // Stop this stream so session teardown releases peer-owned input.
            queue.clear();
            inner.end = Some(QueueEnd::Overloaded);
            self.wake.notify_all();
            return false;
        }
        queue.push_back(event);
        self.wake.notify_one();
        true
    }

    pub fn pop(&self, timeout: Duration) -> Option<Input> {
        let mut inner = self.inner.lock().unwrap();
        if inner.events.is_empty() && inner.end.is_none() {
            inner = self.wake.wait_timeout(inner, timeout).unwrap().0;
        }
        let mut event = inner.events.pop_front()?;
        // Coalesce work already pending, but never add a timer to live input.
        // A fixed wait is noticeable even on a fast LAN and also delays keys
        // queued just after the pointer.
        if matches!(event, Input::Pointer { .. }) {
            while matches!(inner.events.front(), Some(Input::Pointer { .. })) {
                event = inner.events.pop_front().unwrap();
            }
        }
        Some(event)
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.end.get_or_insert(QueueEnd::Closed);
        self.wake.notify_all();
    }

    pub fn discard_and_close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.events.clear();
        inner.end.get_or_insert(QueueEnd::Closed);
        self.wake.notify_all();
    }

    pub(crate) fn overloaded(&self) -> bool {
        self.inner.lock().unwrap().end == Some(QueueEnd::Overloaded)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().end.is_some()
    }
}

impl Default for InputQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) enum CursorCommand {
    Hide(
        (f64, f64),
        bool,
        Option<crossbeam_channel::Sender<anyhow::Result<()>>>,
    ),
    #[cfg(not(target_os = "macos"))]
    Show(Option<crossbeam_channel::Sender<()>>),
    Quit,
    #[cfg(not(target_os = "windows"))]
    ShowAndWarp((f64, f64)),
    #[cfg(target_os = "windows")]
    RestoreSource((f64, f64)),
    #[cfg(target_os = "windows")]
    ReleaseKeys(HashSet<Key>),
    Barrier(crossbeam_channel::Sender<()>),
}

struct CursorSender {
    queue: crossbeam_channel::Sender<CursorCommand>,
    #[cfg(target_os = "windows")]
    thread: u32,
}
impl CursorSender {
    fn send(&self, command: CursorCommand) -> anyhow::Result<()> {
        self.queue.send(command)?;
        #[cfg(target_os = "windows")]
        crate::windows_cursor::wake(self.thread)?;
        Ok(())
    }
}
impl Drop for CursorSender {
    fn drop(&mut self) {
        let _ = self.send(CursorCommand::Quit);
    }
}

struct PeerBinding {
    geometry: Option<Arc<Geometry>>,
    queue: Arc<InputQueue>,
    os: String,
    command_mapping: bool,
    app_switch_mapping: bool,
    ready: bool,
    /// An app on that computer has taken the mouse, e.g. a game's mouse-look.
    pointer_locked: bool,
}

/// The edge a controlled computer sits on, for waking it while it is not
/// connected. Only the local side of the geometry counts.
struct WakeEdge {
    geometry: Geometry,
    alarm: Arc<crate::wake::Alarm>,
}

/// The pointer pushing against a sleeping computer's edge.
#[derive(Clone, Copy)]
struct WakePush {
    peer: usize,
    since: Instant,
    last: Instant,
}

struct ControllerState {
    peers: BTreeMap<usize, PeerBinding>,
    selected_peer: usize,
    geometry: Option<Arc<Geometry>>,
    layout_frozen: bool,
    keyboard: crate::keymap::KeyboardMap,
    peer_cursor_hidden: bool,
    peer_cursor_active: bool,
    remote: bool,
    edge_armed: bool,
    last_local: Option<(f64, f64)>,
    remote_pos: (f64, f64),
    warp_target: Option<(f64, f64)>,
    #[cfg(target_os = "windows")]
    source_anchor: Option<(f64, f64)>,
    keys: HashSet<Key>,
    /// Keys that fired the switch shortcut, swallowed until released.
    #[cfg(target_os = "windows")]
    shortcut_keys: HashSet<Key>,
    // Physical holds outlive connections and emergency returns. Only captured
    // button releases retire them, so a reset cannot split a drag across PCs.
    buttons: HashSet<MouseButton>,
    #[cfg(target_os = "macos")]
    scroll_remainder: (f64, f64),
    #[cfg(target_os = "windows")]
    pointer_lock: crate::windows_pointer_lock::PointerLock,
    /// Controlled computers that can be woken from sleep, by peer id.
    wake_edges: BTreeMap<usize, WakeEdge>,
    wake_push: Option<WakePush>,
}

impl ControllerState {
    /// One crossing decision shared by native motion and Windows' raw-edge fallback.
    fn entry_position(
        &mut self,
        geometry: &Geometry,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    ) -> Option<(f64, f64)> {
        #[cfg(target_os = "windows")]
        self.pointer_lock.moved();
        // The geometry first: it rules out almost every motion event cheaply.
        let position = geometry.enter(x, y, dx, dy)?;
        #[cfg(target_os = "windows")]
        self.forget_missed_releases();
        if !self.buttons.is_empty() {
            return None;
        }
        // Scroll Lock keeps the pointer on this computer, as in other
        // keyboard-and-mouse sharing software. (Tests ignore the real key.)
        // Read first: it is one cheap call, and the game check below would
        // otherwise run for every report while the pointer rests on the edge.
        #[cfg(all(target_os = "windows", not(test)))]
        if crate::windows_capture::scroll_lock_on() {
            return None;
        }
        // A game using the mouse for looking around keeps it until it lets go.
        #[cfg(target_os = "windows")]
        if self.pointer_lock.blocks_crossing() {
            return None;
        }
        Some(position)
    }

    /// A release can happen where the hooks never see it: on the secure
    /// desktop (Ctrl+Alt+Del, UAC, Win+L) or after Windows removes a hook that
    /// took too long. Carried into a crossing, a stale key would be held down
    /// on the other computer and a stale button would block every crossing.
    /// While this computer has the pointer, Windows' own state is the truth.
    #[cfg(target_os = "windows")]
    fn forget_missed_releases(&mut self) {
        // Unit tests drive the hooks with simulated input that Windows never saw.
        #[cfg(not(test))]
        self.forget_released(
            |key| crate::windows_capture::key_down(key).unwrap_or(true),
            crate::windows_capture::button_down,
        );
    }

    #[cfg(any(target_os = "windows", test))]
    fn forget_released(
        &mut self,
        key_down: impl Fn(Key) -> bool,
        button_down: impl Fn(MouseButton) -> bool,
    ) {
        if self.remote {
            return;
        }
        self.keys.retain(|&key| key_down(key));
        self.buttons.retain(|&button| button_down(button));
    }

    /// Motion then goes to that app as raw movement and never returns at an edge.
    fn remote_pointer_locked(&self) -> bool {
        self.remote
            && self
                .peers
                .get(&self.selected_peer)
                .is_some_and(|peer| peer.pointer_locked)
    }

    fn advance_remote(&self, geometry: &Geometry, dx: f64, dy: f64) -> Advance {
        // Scroll Lock keeps the pointer on the other computer too.
        #[cfg(all(target_os = "windows", not(test)))]
        let locked = crate::windows_capture::scroll_lock_on();
        #[cfg(not(all(target_os = "windows", not(test))))]
        let locked = false;
        geometry.advance(
            self.remote_pos.0,
            self.remote_pos.1,
            dx,
            dy,
            self.buttons.is_empty() && !locked,
        )
    }

    /// Keys held now, including those that fired the switch shortcut.
    fn held_keys(&self) -> HashSet<Key> {
        #[cfg(target_os = "windows")]
        return self.keys.union(&self.shortcut_keys).copied().collect();
        #[cfg(not(target_os = "windows"))]
        self.keys.clone()
    }
}

pub struct Controller {
    core: Arc<ControllerCore>,
    peer_id: usize,
}

impl std::ops::Deref for Controller {
    type Target = ControllerCore;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

/// Shared by every connection; only one native input hook owns this state.
pub struct ControllerCore {
    source_only: bool,
    receive_only: bool,
    /// The controlling computer drives several others; this one must never
    /// move its own pointer back into it.
    controller_is_hub: AtomicBool,
    state: Mutex<ControllerState>,
    queue: RwLock<Option<Arc<InputQueue>>>,
    cursor_tx: CursorSender,
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    injected: Mutex<VecDeque<(EventType, std::time::Instant)>>,
    #[cfg(target_os = "windows")]
    pub(crate) cursor_without_mouse: bool,
    #[cfg(target_os = "windows")]
    switch_shortcut: bool,
    pub remote_active: AtomicBool,
    /// The other computer currently owns this desktop's input session.
    pub peer_control_active: AtomicBool,
    pub sent_events: AtomicU64,
}

impl Controller {
    #[cfg(target_os = "macos")]
    pub(crate) fn observes_local_input_only(&self) -> bool {
        self.receives_only()
    }

    /// Fixed for the agent's lifetime, so a capture tap can be created passive.
    #[cfg(target_os = "macos")]
    pub(crate) fn configured_receive_only(&self) -> bool {
        self.receive_only
    }

    pub fn new(config: &Config) -> Arc<Self> {
        let (cursor_tx, cursor_rx) = crossbeam_channel::unbounded();
        #[cfg(target_os = "windows")]
        let cursor_thread = crate::windows_cursor::start(cursor_rx).unwrap_or_else(|error| {
            log::error!("cannot initialize cursor ownership: {error:#}");
            0
        });
        #[cfg(not(target_os = "windows"))]
        std::thread::spawn(move || cursor_worker(cursor_rx));
        let cursor_tx = CursorSender {
            queue: cursor_tx,
            #[cfg(target_os = "windows")]
            thread: cursor_thread,
        };
        Arc::new(Self {
            peer_id: 0,
            core: Arc::new(ControllerCore {
                source_only: config.peers.len() > 1,
                receive_only: config.receive_only,
                controller_is_hub: AtomicBool::new(false),
                state: Mutex::new(ControllerState {
                    peers: BTreeMap::new(),
                    selected_peer: 0,
                    geometry: None,
                    layout_frozen: false,
                    keyboard: crate::keymap::KeyboardMap::new(
                        std::env::consts::OS,
                        "",
                        config.map_mac_command_to_ctrl,
                        config.map_app_switch_shortcut,
                    ),
                    peer_cursor_hidden: false,
                    peer_cursor_active: false,
                    remote: false,
                    edge_armed: true,
                    last_local: None,
                    remote_pos: (0.0, 0.0),
                    warp_target: None,
                    #[cfg(target_os = "windows")]
                    source_anchor: None,
                    keys: HashSet::new(),
                    #[cfg(target_os = "windows")]
                    shortcut_keys: HashSet::new(),
                    buttons: HashSet::new(),
                    #[cfg(target_os = "macos")]
                    scroll_remainder: (0.0, 0.0),
                    #[cfg(target_os = "windows")]
                    pointer_lock: Default::default(),
                    wake_edges: BTreeMap::new(),
                    wake_push: None,
                }),
                queue: RwLock::new(None),
                cursor_tx,
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                injected: Mutex::new(VecDeque::new()),
                #[cfg(target_os = "windows")]
                cursor_without_mouse: config.cursor_without_mouse,
                #[cfg(target_os = "windows")]
                switch_shortcut: config.switch_shortcut,
                remote_active: AtomicBool::new(false),
                peer_control_active: AtomicBool::new(false),
                sent_events: AtomicU64::new(0),
            }),
        })
    }

    pub(crate) fn for_peer(self: &Arc<Self>, peer_id: usize) -> Arc<Self> {
        Arc::new(Self {
            core: self.core.clone(),
            peer_id,
        })
    }

    pub fn is_controlling_peer(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.remote && state.selected_peer == self.peer_id
    }

    /// Never cross into the controlling computer, by preference or because
    /// it controls several computers and owns the keyboard and mouse.
    pub(crate) fn receives_only(&self) -> bool {
        self.receive_only || self.controller_is_hub.load(Ordering::Relaxed)
    }

    pub(crate) fn set_controller_is_hub(&self, hub: bool) {
        self.controller_is_hub.store(hub, Ordering::Relaxed);
    }

    pub fn attach(
        &self,
        link: &Link,
        local: &[Monitor],
        remote: &[Monitor],
        remote_os: &str,
        queue: Arc<InputQueue>,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.selected_peer == self.peer_id {
            self.force_local_locked(&mut state, false);
        }
        state.peers.insert(
            self.peer_id,
            PeerBinding {
                geometry: Geometry::new(&link.arrangement, local, remote)
                    .map(|geometry| Arc::new(geometry.with_kept_corners(link.config.keep_corners))),
                queue,
                os: remote_os.into(),
                command_mapping: link.config.map_mac_command_to_ctrl,
                app_switch_mapping: link.config.map_app_switch_shortcut,
                ready: false,
                pointer_locked: false,
            },
        );
        if state.selected_peer == self.peer_id {
            self.select_peer(&mut state, self.peer_id);
        }
    }

    pub fn detach(&self) {
        let mut state = self.state.lock().unwrap();
        if state.selected_peer == self.peer_id {
            self.force_local_locked(&mut state, false);
            self.queue.write().unwrap().take();
            state.geometry = None;
        }
        if let Some(peer) = state.peers.remove(&self.peer_id) {
            peer.queue.close();
        }
    }

    pub(crate) fn freeze_layout(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(
            !state.remote || state.selected_peer != self.peer_id,
            "Return control to the source computer before changing its layout"
        );
        if let Some(peer) = state.peers.get_mut(&self.peer_id) {
            peer.ready = false;
        }
        if state.selected_peer == self.peer_id {
            state.layout_frozen = true;
        }
        Ok(())
    }

    pub(crate) fn install_geometry(&self, geometry: Geometry) {
        let mut state = self.state.lock().unwrap();
        let geometry = Arc::new(geometry);
        if let Some(peer) = state.peers.get_mut(&self.peer_id) {
            peer.geometry = Some(geometry.clone());
        }
        if state.selected_peer == self.peer_id {
            debug_assert!(state.layout_frozen && !state.remote);
            state.geometry = Some(geometry);
            state.last_local = None;
        }
    }

    pub(crate) fn unfreeze_layout(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(peer) = state.peers.get_mut(&self.peer_id) {
            peer.ready = true;
        }
        if state.selected_peer == self.peer_id {
            state.layout_frozen = false;
        }
    }

    fn select_peer(&self, state: &mut ControllerState, id: usize) {
        let peer = &state.peers[&id];
        state.geometry = peer.geometry.clone();
        state.layout_frozen = !peer.ready;
        state.keyboard = crate::keymap::KeyboardMap::new(
            std::env::consts::OS,
            &peer.os,
            peer.command_mapping,
            peer.app_switch_mapping,
        );
        *self.queue.write().unwrap() = Some(peer.queue.clone());
        state.selected_peer = id;
    }

    /// Choose only at a local edge. Once remote, all events stay on that peer
    /// until its return seam is crossed or its connection is retired.
    fn select_at_edge(&self, state: &mut ControllerState, x: f64, y: f64, dx: f64, dy: f64) {
        if state.remote || self.receives_only() || !state.edge_armed || !state.buttons.is_empty() {
            return;
        }
        let destination = state.peers.iter().find_map(|(&id, peer)| {
            (peer.ready
                && !peer.queue.is_closed()
                && peer
                    .geometry
                    .as_ref()
                    .is_some_and(|g| g.enter(x, y, dx, dy).is_some()))
            .then_some(id)
        });
        if let Some(id) = destination
            && id != state.selected_peer
        {
            self.select_peer(state, id);
        }
    }

    /// Remember the edge this link's computer sits on, so pushing against it
    /// while the computer isn't connected wakes it. `None` forgets the edge.
    pub(crate) fn watch_for_sleep(
        &self,
        geometry: Option<Geometry>,
        alarm: &Arc<crate::wake::Alarm>,
    ) {
        let mut state = self.state.lock().unwrap();
        match geometry {
            Some(geometry) => {
                let alarm = alarm.clone();
                state
                    .wake_edges
                    .insert(self.peer_id, WakeEdge { geometry, alarm });
            }
            None => {
                state.wake_edges.remove(&self.peer_id);
            }
        }
    }

    /// Pushing the pointer against the edge of a computer that isn't
    /// connected, for a moment, asks for it to be woken. Never waits for the
    /// network: that computer's connection thread sends the packet.
    fn wake_at_edge(&self, state: &mut ControllerState, x: f64, y: f64, dx: f64, dy: f64) {
        if state.wake_edges.is_empty() {
            state.wake_push = None;
            return;
        }
        let pushing = if state.remote || self.receives_only() || !state.buttons.is_empty() {
            None
        } else {
            state
                .wake_edges
                .iter()
                .find(|(id, edge)| {
                    !state.peers.contains_key(id) && edge.geometry.enter(x, y, dx, dy).is_some()
                })
                .map(|(&id, _)| id)
        };
        let now = Instant::now();
        state.wake_push = pushing.map(|peer| WakePush {
            peer,
            since: state
                .wake_push
                .filter(|push| push.peer == peer && now - push.last < WAKE_PUSH_GAP)
                .map_or(now, |push| push.since),
            last: now,
        });
        if let Some(push) = state.wake_push
            && now - push.since >= WAKE_PUSH
        {
            state.wake_edges[&push.peer].alarm.ring();
        }
    }

    /// The destination reports that an app there took the mouse or let go.
    pub(crate) fn set_peer_pointer_lock(&self, locked: bool, x: f64, y: f64) {
        let mut state = self.state.lock().unwrap();
        let Some(peer) = state.peers.get_mut(&self.peer_id) else {
            return;
        };
        if peer.pointer_locked == locked {
            return;
        }
        peer.pointer_locked = locked;
        log::info!(
            "an app on the other computer {} the mouse",
            if locked { "took" } else { "released" }
        );
        // Continue from wherever the app left the cursor.
        if !locked
            && state.remote
            && state.selected_peer == self.peer_id
            && x.is_finite()
            && y.is_finite()
            && let Some(geometry) = state.geometry.clone()
        {
            state.remote_pos = geometry.clamp_remote(x, y);
        }
    }

    pub(crate) fn local_pointer_activity(&self) {
        let mut state = self.state.lock().unwrap();
        state.peer_cursor_active = false;
        if state.peer_cursor_hidden && !state.remote {
            self.show_peer_cursor(&mut state);
        }
    }

    fn peer_pointer_activity(&self) {
        let mut state = self.state.lock().unwrap();
        state.peer_cursor_active = true;
        self.show_peer_cursor(&mut state);
    }

    fn show_peer_cursor(&self, state: &mut ControllerState) {
        if state.peer_cursor_hidden {
            state.peer_cursor_hidden = false;
            #[cfg(target_os = "macos")]
            crate::mac_capture::show_cursor();
            #[cfg(target_os = "windows")]
            {
                let (done, wait) = crossbeam_channel::bounded(1);
                let result = self
                    .cursor_tx
                    .send(CursorCommand::Show(Some(done)))
                    .and_then(|_| {
                        wait.recv_timeout(Duration::from_millis(100))
                            .map_err(Into::into)
                    });
                if let Err(error) = result {
                    state.peer_cursor_hidden = true;
                    log::error!("cannot restore local cursor ownership: {error:#}");
                }
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            let _ = self.cursor_tx.send(CursorCommand::Show(None));
        }
    }

    fn peer_left(&self) {
        let mut state = self.state.lock().unwrap();
        if state.peer_cursor_active && !state.remote && !state.peer_cursor_hidden {
            state.peer_cursor_active = false;
            #[cfg(target_os = "macos")]
            {
                state.peer_cursor_hidden = crate::mac_capture::hide_cursor();
            }
            #[cfg(target_os = "windows")]
            if let Some(point) = crate::windows_raw::cursor_position() {
                state.peer_cursor_hidden = self.hide_windows_cursor(point, true);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn hide_windows_cursor(&self, point: (f64, f64), idle_receiver: bool) -> bool {
        let (done, wait) = crossbeam_channel::bounded(1);
        let result = (|| -> anyhow::Result<()> {
            self.cursor_tx
                .send(CursorCommand::Hide(point, idle_receiver, Some(done)))?;
            // Acquisition runs once per crossing, never per motion event.
            // Stay well below Windows' low-level hook removal timeout.
            wait.recv_timeout(Duration::from_millis(100))??;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.cursor_tx.send(CursorCommand::Show(None));
            log::error!("cannot acquire cursor ownership: {error:#}");
            return false;
        }
        true
    }

    pub fn force_local(&self) {
        self.force_local_inner(false);
    }

    /// Windows' secure screen (Ctrl+Alt+Del) came up while another computer
    /// had the keyboard and mouse: take them back. The cursor can't be moved
    /// off the edge it crossed while that screen is up, so, as after the
    /// emergency keys, the pointer must move inward before it crosses again.
    pub fn take_back_from_secure_screen(&self) {
        self.force_local_inner(true);
    }

    pub(crate) fn settle_cursor(&self) -> anyhow::Result<()> {
        let (done, wait) = crossbeam_channel::bounded(1);
        self.cursor_tx.send(CursorCommand::Barrier(done))?;
        wait.recv_timeout(Duration::from_secs(1))?;
        Ok(())
    }

    fn restore_source_cursor(&self, point: (f64, f64), keys: &HashSet<Key>) {
        // Windows never presses held keys again; see release_local_keys.
        #[cfg(target_os = "windows")]
        {
            let _ = keys;
            let _ = self.cursor_tx.send(CursorCommand::RestoreSource(point));
        }
        #[cfg(not(target_os = "windows"))]
        {
            synchronize_local_modifiers(keys, true);
            let _ = self.cursor_tx.send(CursorCommand::ShowAndWarp(point));
        }
    }

    /// An incoming pointer wins a simultaneous handoff. The losing side must
    /// move inward before it can cross the seam again.
    pub fn yield_to_peer(&self) {
        self.force_local_inner(true);
    }

    fn force_local_inner(&self, wait_for_inward_motion: bool) {
        let mut state = self.state.lock().unwrap();
        self.force_local_locked(&mut state, wait_for_inward_motion);
    }

    fn force_local_locked(&self, state: &mut ControllerState, wait_for_inward_motion: bool) {
        state.peer_cursor_active = false;
        self.show_peer_cursor(state);
        if state.remote {
            self.send(Input::ReleaseAll);
            state.remote = false;
            self.remote_active.store(false, Ordering::Release);
            let point = state.last_local.unwrap_or((
                state
                    .geometry
                    .as_ref()
                    .map_or(100.0, |g| g.local.right() - 3.0),
                100.0,
            ));
            state.warp_target = Some(point);
            #[cfg(target_os = "macos")]
            {
                synchronize_local_modifiers(&state.keys, true);
                crate::mac_capture::restore_local_cursor(point.0, point.1);
            }
            #[cfg(not(target_os = "macos"))]
            self.restore_source_cursor(point, &state.keys);
        }
        #[cfg(target_os = "windows")]
        {
            state.source_anchor = None;
        }
        #[cfg(target_os = "macos")]
        {
            state.scroll_remainder = (0.0, 0.0);
        }
        state.edge_armed = !wait_for_inward_motion;
    }

    fn begin_remote(&self, state: &mut ControllerState, position: (f64, f64)) {
        // A modifier can have been pressed on the source before crossing.
        // Transfer it before the first remote key.
        let held = state.keys.clone();
        self.enter_remote(state, position, &held);
    }

    /// Hand the pointer to the selected computer at `position`, with `carried`
    /// keys pressed there. Held keys are released here because their physical
    /// releases will now be suppressed.
    fn enter_remote(
        &self,
        state: &mut ControllerState,
        position: (f64, f64),
        carried: &HashSet<Key>,
    ) {
        state.remote = true;
        state.remote_pos = position;
        self.remote_active.store(true, Ordering::Release);
        self.send(Input::Pointer {
            x: position.0,
            y: position.1,
        });
        state.keyboard.begin(carried, |event| self.send(event));
        #[cfg(target_os = "windows")]
        self.release_keys_elsewhere(&state.keys);
        #[cfg(not(target_os = "windows"))]
        synchronize_local_modifiers(&state.keys, false);
    }

    /// Release held keys from the cursor thread. The caller owns the hooks and
    /// holds the state lock, and Windows can run a hook for the mouse moving
    /// on while its thread is inside `SendInput`: that hook would wait for the
    /// lock forever. Crossing with a key held, such as Ctrl just after Ctrl+C,
    /// froze the agent this way.
    #[cfg(target_os = "windows")]
    fn release_keys_elsewhere(&self, keys: &HashSet<Key>) {
        if keys.is_empty() {
            return;
        }
        if let Err(error) = self
            .cursor_tx
            .send(CursorCommand::ReleaseKeys(keys.clone()))
        {
            log::error!("cannot release keys held while crossing: {error:#}");
        }
    }

    /// The connected computer on `side` of this screen and the middle of its
    /// screen, if the pointer may go there now.
    #[cfg(target_os = "windows")]
    fn shortcut_target(
        &self,
        state: &mut ControllerState,
        side: crate::config::Side,
    ) -> Option<(usize, (f64, f64))> {
        if self.receives_only() || !state.buttons.is_empty() || state.pointer_lock.blocks_crossing()
        {
            return None;
        }
        state.peers.iter().find_map(|(&id, peer)| {
            let geometry = peer.geometry.as_ref().filter(|g| g.side == side)?;
            (peer.ready && !peer.queue.is_closed()).then(|| {
                let screen = &geometry.remote;
                (
                    id,
                    (
                        f64::from(screen.x) + f64::from(screen.width) / 2.0,
                        f64::from(screen.y) + f64::from(screen.height) / 2.0,
                    ),
                )
            })
        })
    }

    /// Ctrl+Alt+Shift with an arrow moves the pointer to the computer on that
    /// side of this screen, and the arrow pointing back returns it here.
    /// Returns `false` when the keys are not this shortcut and should be
    /// handled as usual.
    #[cfg(target_os = "windows")]
    fn switch_by_shortcut(&self, state: &mut ControllerState, key: Key) -> bool {
        let Some(side) = arrow_side(key) else {
            return false;
        };
        if !self.switch_shortcut {
            return false;
        }
        // Modifiers released on the secure desktop (Ctrl+Alt+Del) must not
        // turn a plain Shift+arrow into the shortcut. Windows records this
        // arrow's own press only after the hooks return, so keep it.
        state.forget_missed_releases();
        state.keys.insert(key);
        let held = state.held_keys();
        if !emergency_chord(&held) {
            return false;
        }
        if state.remote {
            if !state
                .geometry
                .as_ref()
                .is_some_and(|geometry| geometry.side.opposite() == side)
            {
                // Other arrows belong to the other computer's apps.
                return false;
            }
            self.force_local_locked(state, true);
        } else {
            let Some((id, center)) = self.shortcut_target(state, side) else {
                return false;
            };
            if id != state.selected_peer {
                self.select_peer(state, id);
            }
            let Some(anchor) = crate::windows_raw::cursor_position() else {
                return false;
            };
            state.source_anchor = Some(anchor);
            state.last_local = Some(anchor);
            self.hide_windows_cursor(anchor, false);
            // Windows never saw this arrow's press; release only the rest.
            state.keys.remove(&key);
            self.enter_remote(state, center, &HashSet::new());
        }
        // Held keys now belong to the shortcut until released. Leaving them
        // in `keys` would press them on the other computer with its next key.
        state.shortcut_keys = held;
        state.keys.clear();
        true
    }

    /// Windows dropped Seam's hooks and they were installed again. Keyboard
    /// events went unseen meanwhile: start over on this computer.
    #[cfg(target_os = "windows")]
    pub fn recover_lost_hooks(&self) {
        let mut state = self.state.lock().unwrap();
        state.shortcut_keys.clear();
        self.force_local_locked(&mut state, false);
    }

    fn send(&self, event: Input) {
        if let Some(queue) = self.queue.read().unwrap().as_ref()
            && queue.push(event)
        {
            self.sent_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn handle(&self, event: Event) -> Option<Event> {
        #[cfg(target_os = "macos")]
        if matches!(
            event.event_type,
            EventType::MouseMove { .. } | EventType::Wheel { .. }
        ) {
            // The native HID tap owns Mac motion and scrolling. If it ever
            // drops out, the keyboard tap still prevents local leakage.
            return if self.remote_active.load(Ordering::Acquire) {
                None
            } else {
                Some(event)
            };
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if self.was_injected(event.event_type) {
            return Some(event);
        }
        let mut state = self.state.lock().unwrap();
        // The keys that fired the switch shortcut reach neither computer until
        // released: a held arrow's repeats must not switch again, and releases
        // must not reach a computer that never saw the press.
        #[cfg(target_os = "windows")]
        if let EventType::KeyPress(key) | EventType::KeyRelease(key) = event.event_type
            && state.shortcut_keys.contains(&key)
        {
            if matches!(event.event_type, EventType::KeyRelease(_)) {
                state.shortcut_keys.remove(&key);
                return None;
            }
            if !MODIFIER_KEYS.contains(&key) {
                return None;
            }
            // A modifier doesn't repeat once another key is pressed, so this
            // is a new press: its release, and maybe others, went unseen (on
            // the secure desktop, say). Trust none of the shortcut's keys.
            state.shortcut_keys.clear();
        }
        // Physical key ownership outlives a network session. In particular a
        // key-up during reconnect must retire a modifier held before the loss.
        match event.event_type {
            EventType::KeyPress(key) => {
                state.keys.insert(key);
            }
            EventType::KeyRelease(key) => {
                state.keys.remove(&key);
            }
            _ => {}
        }
        // Before the check for a selected computer below: the shortcut
        // chooses one itself.
        #[cfg(target_os = "windows")]
        if let EventType::KeyPress(key) = event.event_type
            && self.switch_by_shortcut(&mut state, key)
        {
            return None;
        }
        if let EventType::MouseMove { x, y } = event.event_type {
            let (dx, dy) = state
                .last_local
                .map(|(px, py)| (x - px, y - py))
                .unwrap_or((0.0, 0.0));
            self.select_at_edge(&mut state, x, y, dx, dy);
        }
        let Some(geometry) = state.geometry.clone() else {
            if let EventType::MouseMove { x, y } = event.event_type {
                state.last_local = Some((x, y));
            }
            return Some(event);
        };
        match event.event_type {
            EventType::MouseMove { x, y } => {
                #[cfg(target_os = "windows")]
                if state.remote {
                    // The hook reports Windows' accelerated proposed cursor
                    // position even when we suppress the actual local move.
                    // Read the current source position BEFORE Windows applies
                    // this hook event. An application closing, releasing a
                    // cursor clip, or warping the pointer can move our parked
                    // cursor. Subtracting the old handoff anchor then biases
                    // every delta and can make the return edge unreachable.
                    let Some(anchor) = crate::windows_raw::cursor_position() else {
                        // Do not invent movement if the input desktop is gone.
                        drop(state);
                        self.force_local_inner(true);
                        return Some(event);
                    };
                    if state.source_anchor != Some(anchor) {
                        state.source_anchor = Some(anchor);
                        let _ = self
                            .cursor_tx
                            .send(CursorCommand::Hide(anchor, false, None));
                    }
                    if state.remote_pointer_locked() {
                        // Raw input carries the motion; see handle_windows_raw_remote.
                        return None;
                    }
                    let (dx, dy) = (x - anchor.0, y - anchor.1);
                    if dx != 0.0 || dy != 0.0 {
                        match state.advance_remote(&geometry, dx, dy) {
                            Advance::Remote(pos) => {
                                state.remote_pos = pos;
                                self.send(Input::Pointer { x: pos.0, y: pos.1 });
                            }
                            Advance::Return(pos) => {
                                self.send(Input::Leave);
                                state.remote = false;
                                state.source_anchor = None;
                                self.remote_active.store(false, Ordering::Release);
                                state.last_local = Some(pos);
                                state.warp_target = Some(pos);
                                self.restore_source_cursor(pos, &state.keys);
                            }
                        }
                    }
                    return None;
                }
                if let Some((wx, wy)) = state.warp_target
                    && (x - wx).abs() < 4.0
                    && (y - wy).abs() < 4.0
                {
                    state.warp_target = None;
                    state.last_local = Some((x, y));
                    return if state.remote { None } else { Some(event) };
                }
                if !state.remote && !state.edge_armed {
                    state.edge_armed = far_enough_inside(&geometry, x, y);
                    return Some(event);
                }
                let previous = state.last_local.replace((x, y));
                let (dx, dy) = previous
                    .map(|(px, py)| (x - px, y - py))
                    .unwrap_or((0.0, 0.0));
                if !state.remote {
                    if state.layout_frozen || self.receives_only() {
                        return Some(event);
                    }
                    if let Some(remote_pos) = state.entry_position(&geometry, x, y, dx, dy) {
                        #[cfg(target_os = "windows")]
                        {
                            state.source_anchor = crate::windows_raw::cursor_position();
                            let Some(anchor) = state.source_anchor else {
                                return Some(event);
                            };
                            // Cross even when Windows will not hand over the
                            // cursor: a visible parked pointer is better than
                            // one that cannot leave the screen.
                            self.hide_windows_cursor(anchor, false);
                        }
                        self.begin_remote(&mut state, remote_pos);
                        #[cfg(not(target_os = "windows"))]
                        let _ = self
                            .cursor_tx
                            .send(CursorCommand::Hide((x, y), false, None));
                        return None;
                    }
                    Some(event)
                } else {
                    if state.warp_target.is_some() && (dx.abs() > 200.0 || dy.abs() > 200.0) {
                        return None;
                    }
                    if dx != 0.0 || dy != 0.0 {
                        match state.advance_remote(&geometry, dx, dy) {
                            Advance::Remote(pos) => {
                                state.remote_pos = pos;
                                self.send(Input::Pointer { x: pos.0, y: pos.1 });
                            }
                            Advance::Return(pos) => {
                                self.send(Input::Leave);
                                state.remote = false;
                                self.remote_active.store(false, Ordering::Release);
                                state.warp_target = Some(pos);
                                self.restore_source_cursor(pos, &state.keys);
                            }
                        }
                    }
                    if state.remote && state.warp_target.is_none() {
                        let l = &geometry.local;
                        if x < l.x as f64 + 120.0
                            || x > l.right() - 120.0
                            || y < l.y as f64 + 120.0
                            || y > l.bottom() - 120.0
                        {
                            let park = (
                                l.x as f64 + l.width as f64 / 2.0,
                                l.y as f64 + l.height as f64 / 2.0,
                            );
                            state.warp_target = Some(park);
                            let _ = self.cursor_tx.send(CursorCommand::Hide(park, false, None));
                        }
                    }
                    None
                }
            }
            EventType::KeyPress(key) | EventType::KeyRelease(key) => {
                let down = matches!(event.event_type, EventType::KeyPress(_));
                if !state.remote {
                    return Some(event);
                }
                if matches!(key, Key::Unknown(_)) {
                    // Unknown carries a native OS key number, not a portable
                    // physical key. Forwarding it to the other OS can type an
                    // unrelated character or leave a modifier held.
                    return None;
                }
                // Scroll Lock keeps the pointer where it is, so it stays a key
                // of this computer and its light shows the lock.
                #[cfg(target_os = "windows")]
                if key == Key::ScrollLock {
                    return Some(event);
                }
                if down && key == Key::Escape && emergency_chord(&state.held_keys()) {
                    // The same return as any other, and the pointer must move
                    // inward before it can cross again: it comes back at the
                    // edge it left from.
                    self.force_local_locked(&mut state, true);
                    return None;
                }
                let ControllerState { keyboard, keys, .. } = &mut *state;
                keyboard.key(key, down, keys, |event| self.send(event));
                None
            }
            EventType::ButtonPress(button) | EventType::ButtonRelease(button) => {
                let down = matches!(event.event_type, EventType::ButtonPress(_));
                // Native Mac/Windows callbacks deliver semantic buttons through
                // handle_button. The generic rdev fallback supports common buttons;
                // its Unknown values are explicitly OS-specific, never wire IDs.
                let button = match button {
                    Button::Left => Some(MouseButton::Left),
                    Button::Right => Some(MouseButton::Right),
                    Button::Middle => Some(MouseButton::Middle),
                    Button::Unknown(_) => None,
                };
                let Some(button) = button else {
                    return if state.remote { None } else { Some(event) };
                };
                drop(state);
                self.handle_button(button, down).then_some(event)
            }
            EventType::Wheel { delta_x, delta_y } => {
                if state.remote {
                    // A touchpad's fraction of a notch is kept until it adds
                    // up to one; there is nothing to send until then.
                    if delta_x != 0 || delta_y != 0 {
                        self.send(Input::Wheel {
                            dx: delta_x,
                            dy: delta_y,
                            unit: ScrollUnit::Lines,
                        });
                    }
                    None
                } else {
                    Some(event)
                }
            }
        }
    }

    /// Called by native capture after decoding the platform's button identity.
    pub(crate) fn handle_button(&self, button: MouseButton, down: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        if down {
            state.buttons.insert(button);
        } else {
            state.buttons.remove(&button);
            #[cfg(target_os = "windows")]
            state.pointer_lock.button_released();
        }
        if state.remote {
            self.send(Input::Button { button, down });
            false
        } else {
            true
        }
    }

    /// Handle an unmodified macOS HID mouse event. Screen coordinates stop
    /// changing once the mouse is detached from the local cursor, so remote
    /// movement must use the event's physical delta instead.
    #[cfg(target_os = "macos")]
    pub fn handle_mac_motion(&self, x: f64, y: f64, raw_dx: f64, raw_dy: f64) -> bool {
        let mut state = self.state.lock().unwrap();
        let (dx, dy) = state
            .last_local
            .map(|(px, py)| (x - px, y - py))
            .unwrap_or((raw_dx, raw_dy));
        self.select_at_edge(&mut state, x, y, dx, dy);
        // The cursor stops at the screen's edge; the mouse keeps reporting.
        self.wake_at_edge(&mut state, x, y, raw_dx, raw_dy);
        let Some(geometry) = state.geometry.clone() else {
            state.last_local = Some((x, y));
            return true;
        };

        if !state.remote {
            if state.layout_frozen || self.receives_only() {
                state.last_local = Some((x, y));
                return true;
            }
            if !state.edge_armed {
                state.edge_armed = far_enough_inside(&geometry, x, y);
                state.last_local = Some((x, y));
                return true;
            }
            let (dx, dy) = state
                .last_local
                .map(|(px, py)| (x - px, y - py))
                .unwrap_or((raw_dx, raw_dy));
            state.last_local = Some((x, y));
            if let Some(pos) = state.entry_position(&geometry, x, y, dx, dy) {
                // Park the hidden source cursor at an outer corner. If macOS
                // or another app briefly reveals it, its shape is clipped by
                // the desktop edge instead of lingering at the handoff seam.
                let park = geometry.local_park_corner();
                if !crate::mac_capture::lock_local_cursor(park.0, park.1) {
                    return true;
                }
                state.scroll_remainder = (0.0, 0.0);
                self.begin_remote(&mut state, pos);
                return false;
            }
            return true;
        }

        if state.remote_pointer_locked() {
            if raw_dx != 0.0 || raw_dy != 0.0 {
                self.send(Input::Motion {
                    dx: raw_dx.round() as i32,
                    dy: raw_dy.round() as i32,
                });
            }
        } else if raw_dx != 0.0 || raw_dy != 0.0 {
            match state.advance_remote(&geometry, raw_dx, raw_dy) {
                Advance::Remote(pos) => {
                    state.remote_pos = pos;
                    self.send(Input::Pointer { x: pos.0, y: pos.1 });
                }
                Advance::Return(pos) => {
                    self.send(Input::Leave);
                    state.remote = false;
                    self.remote_active.store(false, Ordering::Release);
                    synchronize_local_modifiers(&state.keys, true);
                    state.last_local = Some(pos);
                    crate::mac_capture::restore_local_cursor(pos.0, pos.1);
                }
            }
        }
        let park = geometry.local_park_corner();
        if state.remote && ((x - park.0).abs() > 1.0 || (y - park.1).abs() > 1.0) {
            crate::mac_capture::pin_local_cursor(park.0, park.1);
        }
        false
    }

    #[cfg(target_os = "macos")]
    pub fn handle_mac_scroll(&self, dx: i64, dy: i64, unit: ScrollUnit) -> bool {
        if !self.remote_active.load(Ordering::Acquire) {
            return true;
        }
        let mut state = self.state.lock().unwrap();
        if !state.remote {
            return true;
        }
        let (dx, dy) = if unit == ScrollUnit::Pixels {
            (
                scale_pixel_scroll(dx, TRACKPAD_SCROLL_SCALE, &mut state.scroll_remainder.0),
                scale_pixel_scroll(dy, TRACKPAD_SCROLL_SCALE, &mut state.scroll_remainder.1),
            )
        } else {
            (dx, dy)
        };
        if dx != 0 || dy != 0 {
            self.send(Input::Wheel { dx, dy, unit });
        }
        false
    }

    #[cfg(target_os = "windows")]
    pub fn handle_windows_raw_motion(&self, dx: f64, dy: f64) {
        if (dx == 0.0 && dy == 0.0) || self.remote_active.load(Ordering::Acquire) {
            return;
        }
        self.local_pointer_activity();
        let mut state = self.state.lock().unwrap();
        if state.remote || self.receives_only() {
            return;
        }
        // Raw deltas keep detecting outward motion when Windows clamps the cursor.
        let Some((x, y)) = crate::windows_raw::cursor_position() else {
            return;
        };
        self.select_at_edge(&mut state, x, y, dx, dy);
        // Only here on Windows: the hooks stop reporting outward motion once
        // the cursor reaches the screen's edge, which would end every push.
        self.wake_at_edge(&mut state, x, y, dx, dy);
        if state.layout_frozen {
            return;
        }
        let Some(geometry) = state.geometry.clone() else {
            return;
        };
        state.last_local = Some((x, y));
        if !state.edge_armed {
            state.edge_armed = far_enough_inside(&geometry, x, y);
            return;
        }
        if let Some(pos) = state.entry_position(&geometry, x, y, dx, dy) {
            // See the hook path: crossing must not depend on hiding the cursor.
            self.hide_windows_cursor((x, y), false);
            state.source_anchor = Some((x, y));
            self.begin_remote(&mut state, pos);
        }
    }

    /// Raw, unaccelerated motion for an app holding the destination's mouse,
    /// the same counts a mouse plugged into that computer would report.
    #[cfg(target_os = "windows")]
    pub fn handle_windows_raw_remote(&self, dx: f64, dy: f64) {
        let state = self.state.lock().unwrap();
        if state.remote_pointer_locked() && (dx != 0.0 || dy != 0.0) {
            self.send(Input::Motion {
                dx: dx as i32,
                dy: dy as i32,
            });
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn mark_injected(&self, event: EventType) {
        #[cfg(target_os = "windows")]
        let _ = event;
        #[cfg(not(target_os = "windows"))]
        {
            let mut injected = self.injected.lock().unwrap();
            while injected.len() >= 64 {
                injected.pop_front();
            }
            injected.push_back((event, std::time::Instant::now()));
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn was_injected(&self, event: EventType) -> bool {
        let mut injected = self.injected.lock().unwrap();
        while injected
            .front()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_millis(100))
        {
            injected.pop_front();
        }
        if let Some(index) = injected.iter().position(|(expected, _)| *expected == event) {
            injected.remove(index);
            true
        } else {
            false
        }
    }
}

fn far_enough_inside(geometry: &Geometry, x: f64, y: f64) -> bool {
    let local = &geometry.local;
    if !local.contains(x, y) {
        return true;
    }
    const RESET_DISTANCE: f64 = 64.0;
    match geometry.side {
        Side::Left => x >= local.x as f64 + RESET_DISTANCE,
        Side::Right => x <= local.right() - RESET_DISTANCE,
        Side::Top => y >= local.y as f64 + RESET_DISTANCE,
        Side::Bottom => y <= local.bottom() - RESET_DISTANCE,
    }
}

/// Mac trackpad scrolling, in pixels, is scaled by this tuned factor before
/// it reaches the other computer.
#[cfg(target_os = "macos")]
const TRACKPAD_SCROLL_SCALE: f64 = 0.4;

#[cfg(target_os = "macos")]
fn scale_pixel_scroll(delta: i64, speed: f64, remainder: &mut f64) -> i64 {
    let scaled = delta as f64 * speed + *remainder;
    let whole = scaled.round();
    *remainder = scaled - whole;
    whole as i64
}

fn emergency_chord(keys: &HashSet<Key>) -> bool {
    (keys.contains(&Key::ControlLeft) || keys.contains(&Key::ControlRight))
        && (keys.contains(&Key::ShiftLeft) || keys.contains(&Key::ShiftRight))
        && (keys.contains(&Key::Alt) || keys.contains(&Key::AltGr))
}

/// The screen edge an arrow key points at.
#[cfg(target_os = "windows")]
fn arrow_side(key: Key) -> Option<crate::config::Side> {
    use crate::config::Side;
    match key {
        Key::LeftArrow => Some(Side::Left),
        Key::RightArrow => Some(Side::Right),
        Key::UpArrow => Some(Side::Top),
        Key::DownArrow => Some(Side::Bottom),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn simulate_event(event: &EventType) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    return crate::windows_input::simulate(event);
    #[cfg(not(target_os = "windows"))]
    rdev::simulate(event).map_err(|error| anyhow::anyhow!("input injection: {error:?}"))
}

#[cfg(not(target_os = "windows"))]
fn cursor_worker(rx: crossbeam_channel::Receiver<CursorCommand>) {
    let mut hidden = false;
    while let Ok(command) = rx.recv() {
        match command {
            CursorCommand::Barrier(done) => {
                let _ = done.send(());
            }
            CursorCommand::Hide((x, y), _idle_receiver, done) => {
                if !hidden {
                    #[cfg(target_os = "macos")]
                    set_mouse_cursor_associated(false);
                    set_cursor_visible(false);
                    hidden = true;
                }
                let _ = (x, y);
                if let Some(done) = done {
                    let _ = done.send(Ok(()));
                }
            }
            CursorCommand::Quit => break,
            #[cfg(not(target_os = "macos"))]
            CursorCommand::Show(done) => {
                if hidden {
                    set_cursor_visible(true);
                    hidden = false;
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            CursorCommand::ShowAndWarp((x, y)) => {
                #[cfg(target_os = "macos")]
                warp_mouse_cursor(x, y);
                #[cfg(not(target_os = "macos"))]
                let _ = simulate_event(&EventType::MouseMove { x, y });
                if hidden {
                    #[cfg(target_os = "macos")]
                    set_mouse_cursor_associated(true);
                    set_cursor_visible(true);
                    hidden = false;
                }
            }
        }
    }
    if hidden {
        #[cfg(target_os = "macos")]
        set_mouse_cursor_associated(true);
        set_cursor_visible(true);
    }
}

#[cfg(target_os = "macos")]
fn set_mouse_cursor_associated(associated: bool) {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGAssociateMouseAndMouseCursorPosition(associated: u32) -> i32;
    }
    let result = unsafe { CGAssociateMouseAndMouseCursorPosition(u32::from(associated)) };
    if result != 0 {
        log::error!("failed to change mouse/cursor association: {result}");
    }
}

#[cfg(target_os = "macos")]
fn warp_mouse_cursor(x: f64, y: f64) {
    #[repr(C)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGWarpMouseCursorPosition(point: Point) -> i32;
    }
    let result = unsafe { CGWarpMouseCursorPosition(Point { x, y }) };
    if result != 0 {
        log::error!("failed to restore local cursor position: {result}");
    }
}

#[cfg(target_os = "macos")]
fn set_cursor_visible(show: bool) {
    if show {
        crate::mac_capture::show_cursor();
    } else {
        crate::mac_capture::hide_cursor();
    }
}

pub(crate) fn run_capture(
    controller: Arc<Controller>,
    shutdown: Arc<crate::shutdown::Signal>,
) -> anyhow::Result<()> {
    if !accessibility_ready() {
        // Keep serving status so the settings window can guide the person,
        // and start capturing as soon as the permission is granted.
        #[cfg(target_os = "macos")]
        request_accessibility();
        log::warn!("waiting for Accessibility permission");
        while !accessibility_ready() {
            if shutdown.wait(std::time::Duration::from_secs(1)) {
                return Ok(());
            }
            // macOS can keep telling a running process it is not trusted
            // after the person allows it. A fresh process sees the change;
            // exit with an error so launchd starts one.
            #[cfg(target_os = "macos")]
            if accessibility_ready_in_new_process() {
                anyhow::bail!("Accessibility was allowed; restarting to use it");
            }
        }
        log::info!("Accessibility permission granted");
    }
    #[cfg(target_os = "macos")]
    {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        // One native tap owns all physical input and bypasses our tagged
        // injection before decoding modifiers. No global flag comparison.
        let result = crate::mac_capture::run(controller, ready_tx, shutdown);
        drop(ready_rx);
        result
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows_capture::run(controller, shutdown)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        rdev::grab(move |event| controller.handle(event))
            .map_err(|e| anyhow::anyhow!("input capture failed: {e:?}"))
    }
}

#[cfg(target_os = "macos")]
pub fn accessibility_ready() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// Ask a new copy of this program, which macOS judges with the same identity
/// but without this process's stale answer.
#[cfg(target_os = "macos")]
fn accessibility_ready_in_new_process() -> bool {
    std::env::current_exe()
        .and_then(|program| {
            std::process::Command::new(program)
                .arg("accessibility")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        })
        .is_ok_and(|status| status.success())
}

/// Add this program to the Accessibility list and show the system prompt.
#[cfg(target_os = "macos")]
fn request_accessibility() {
    use std::ffi::c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        static kAXTrustedCheckOptionPrompt: *const c_void;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: *const c_void;
        static kCFTypeDictionaryKeyCallBacks: [usize; 6];
        static kCFTypeDictionaryValueCallBacks: [usize; 5];
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const [usize; 6],
            value_callbacks: *const [usize; 5],
        ) -> *const c_void;
        fn CFRelease(value: *const c_void);
    }
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        if !options.is_null() {
            AXIsProcessTrustedWithOptions(options);
            CFRelease(options);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn accessibility_ready() -> bool {
    true
}

pub struct Receiver {
    keys: HashSet<Key>,
    buttons: HashSet<MouseButton>,
    controller: Arc<Controller>,
    /// Whether an app here steers the mouse itself, as told to the controller.
    #[cfg(target_os = "windows")]
    pointer_lock: crate::windows_pointer_lock::DestinationLock,
    report: Option<Input>,
    /// Releases Windows refused, for example while a UAC prompt had the
    /// input desktop. Retried until they succeed so no key stays down here.
    #[cfg(not(target_os = "macos"))]
    unreleased: HashSet<Key>,
    #[cfg(target_os = "macos")]
    keyboard: crate::mac_capture::PeerKeyboard,
    #[cfg(target_os = "macos")]
    pointer_position: Option<(f64, f64)>,
    #[cfg(target_os = "macos")]
    activity: crate::power::UserActivity,
}

impl Receiver {
    pub fn new(controller: Arc<Controller>) -> Self {
        Self {
            keys: HashSet::new(),
            buttons: HashSet::new(),
            controller,
            #[cfg(target_os = "windows")]
            pointer_lock: Default::default(),
            report: None,
            #[cfg(not(target_os = "macos"))]
            unreleased: HashSet::new(),
            #[cfg(target_os = "macos")]
            keyboard: crate::mac_capture::PeerKeyboard::new(),
            #[cfg(target_os = "macos")]
            pointer_position: None,
            #[cfg(target_os = "macos")]
            activity: crate::power::UserActivity::new(),
        }
    }
    pub fn apply(&mut self, input: Input) {
        // A multi-peer source owns the devices. Receiver desktops cannot inject
        // competing input into the source or interrupt a different destination.
        if self.controller.source_only {
            return;
        }
        let peer_input = matches!(
            input,
            Input::Pointer { .. }
                | Input::Motion { .. }
                | Input::Button { .. }
                | Input::Wheel { .. }
                | Input::Key { .. }
        );
        if peer_input && self.controller.remote_active.load(Ordering::Acquire) {
            self.controller.yield_to_peer();
        }
        if peer_input {
            // Wakes the screen when the pointer arrives, before it moves.
            #[cfg(target_os = "macos")]
            self.activity
                .declare(!self.controller.peer_control_active.load(Ordering::Acquire));
            #[cfg(target_os = "windows")]
            let arrived = !self.controller.peer_control_active.load(Ordering::Acquire);
            self.controller
                .peer_control_active
                .store(true, Ordering::Release);
            // Told once the flag is set: it keeps watching while the flag is.
            #[cfg(target_os = "windows")]
            if arrived {
                crate::windows_pointer_presence::pointer_arrived();
            }
            self.controller.peer_pointer_activity();
        }
        #[cfg(target_os = "windows")]
        if matches!(input, Input::Pointer { .. } | Input::Motion { .. }) {
            if matches!(input, Input::Pointer { .. }) && !self.pointer_lock.locked() {
                self.pointer_lock.notice_warp();
            }
            self.check_pointer_lock();
        }
        let event = match input {
            // Positions sent before the controller learned of the lock would
            // jerk the app's view; it reads raw motion now.
            #[cfg(target_os = "windows")]
            Input::Pointer { .. } if self.pointer_lock.locked() => None,
            Input::Pointer { x, y } => Some(EventType::MouseMove { x, y }),
            Input::Motion { dx, dy } => {
                #[cfg(target_os = "windows")]
                {
                    if let Err(error) = crate::windows_input::simulate_motion(dx, dy) {
                        log::warn!("cannot apply peer motion: {error:#}");
                    }
                    self.pointer_lock.moved_relatively();
                }
                // Only a Windows destination reports a lock, so only it is sent motion.
                #[cfg(not(target_os = "windows"))]
                let _ = (dx, dy);
                None
            }
            Input::Button { button, down } => {
                if let Err(error) = self.inject_button(button, down) {
                    log::warn!("cannot apply peer button: {error:#}");
                } else if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                    #[cfg(target_os = "windows")]
                    self.pointer_lock.button_released();
                }
                None
            }
            Input::Wheel { dx, dy, unit } => {
                #[cfg(target_os = "macos")]
                if let Err(error) = crate::mac_capture::simulate_peer_scroll(
                    dx,
                    dy,
                    unit,
                    self.keyboard.flags(&self.keys),
                ) {
                    log::warn!("cannot apply peer scroll: {error:#}");
                }
                #[cfg(target_os = "windows")]
                if let Err(error) = crate::windows_scroll::simulate_peer_scroll(dx, dy, unit) {
                    log::warn!("cannot apply peer scroll: {error:#}");
                }
                None
            }
            Input::Key { key, down } => {
                if matches!(key, Key::Unknown(_)) {
                    return;
                }
                let fresh_down = if down {
                    self.keys.insert(key)
                } else {
                    self.keys.remove(&key);
                    false
                };
                #[cfg(target_os = "macos")]
                {
                    if let Err(error) = self.keyboard.post(key, down, &self.keys, fresh_down) {
                        log::warn!("cannot apply peer key: {error:#}");
                    }
                    return;
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = fresh_down;
                    if down {
                        self.unreleased.remove(&key);
                    }
                    Some(if down {
                        EventType::KeyPress(key)
                    } else {
                        EventType::KeyRelease(key)
                    })
                }
            }
            Input::ReleaseAll => {
                self.release_all();
                self.controller
                    .peer_control_active
                    .store(false, Ordering::Release);
                None
            }
            Input::Leave => {
                self.release_all();
                self.controller
                    .peer_control_active
                    .store(false, Ordering::Release);
                self.controller.peer_left();
                None
            }
            _ => None,
        };
        #[cfg(target_os = "macos")]
        if let Some(EventType::MouseMove { x, y }) = event {
            let dragging = self.buttons.iter().min().copied();
            if let Err(error) = crate::mac_capture::simulate_peer_pointer(
                x,
                y,
                self.pointer_position,
                dragging,
                self.keyboard.flags(&self.keys),
            ) {
                log::error!("cannot move Mac pointer for peer: {error:#}");
            } else {
                self.pointer_position = Some((x, y));
            }
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(event) = event {
            // A key Windows refused to release must not modify this event.
            self.retry_releases();
            self.controller.mark_injected(event);
            if let Err(error) = simulate_event(&event) {
                log::warn!("cannot apply peer input: {error:?}");
                if let EventType::KeyRelease(key) = event {
                    self.unreleased.insert(key);
                }
            } else {
                #[cfg(target_os = "windows")]
                if let EventType::MouseMove { x, y } = event {
                    self.pointer_lock.placed_at(x, y);
                }
            }
        }
    }

    /// Input works again: send the releases Windows refused earlier.
    #[cfg(not(target_os = "macos"))]
    fn retry_releases(&mut self) {
        if self.unreleased.is_empty() {
            return;
        }
        let controller = self.controller.clone();
        self.unreleased.retain(|&key| {
            let event = EventType::KeyRelease(key);
            controller.mark_injected(event);
            simulate_event(&event).is_err()
        });
    }
    /// A message for the controller, produced while applying its input.
    pub fn take_report(&mut self) -> Option<Input> {
        self.report.take()
    }

    /// Tell the controller when an app here takes or releases the mouse.
    /// Checked only while the controller moves the pointer.
    #[cfg(target_os = "windows")]
    fn check_pointer_lock(&mut self) {
        if let Some(locked) = self.pointer_lock.update(!self.buttons.is_empty()) {
            let (x, y) = crate::windows_raw::cursor_position().unwrap_or((f64::NAN, f64::NAN));
            self.report = Some(Input::PointerLock { locked, x, y });
        }
    }

    pub fn release_all(&mut self) {
        let mut held: Vec<Key> = self.keys.iter().copied().collect();
        held.sort_by_key(release_rank);
        #[cfg(target_os = "windows")]
        if held.iter().any(|key| MODIFIER_KEYS.contains(key))
            && let Err(error) = crate::windows_input::mask_modifiers()
        {
            log::warn!("cannot mask held modifiers: {error:#}");
        }
        for key in held {
            self.keys.remove(&key);
            #[cfg(target_os = "macos")]
            {
                let _ = self.keyboard.post(key, false, &self.keys, false);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let event = EventType::KeyRelease(key);
                self.controller.mark_injected(event);
                if simulate_event(&event).is_err() {
                    self.unreleased.insert(key);
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        self.retry_releases();
        while let Some(button) = self.buttons.iter().next().copied() {
            let _ = self.inject_button(button, false);
            self.buttons.remove(&button);
        }
        #[cfg(target_os = "macos")]
        {
            self.pointer_position = None;
        }
    }

    fn inject_button(&self, button: MouseButton, down: bool) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        return crate::mac_capture::simulate_peer_button(
            button,
            down,
            self.pointer_position,
            self.keyboard.flags(&self.keys),
        );
        #[cfg(target_os = "windows")]
        return crate::windows_input::simulate_button(button, down);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let native = match button {
                MouseButton::Left => Button::Left,
                MouseButton::Right => Button::Right,
                MouseButton::Middle => Button::Middle,
                _ => anyhow::bail!("unsupported mouse button on this platform"),
            };
            let event = if down {
                EventType::ButtonPress(native)
            } else {
                EventType::ButtonRelease(native)
            };
            self.controller.mark_injected(event);
            simulate_event(&event)
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.release_all();
        self.controller
            .peer_control_active
            .store(false, Ordering::Release);
        self.controller.local_pointer_activity();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Monitor;

    fn two_destinations() -> (
        Arc<Controller>,
        Arc<Controller>,
        Arc<InputQueue>,
        Arc<InputQueue>,
    ) {
        let config = Config::default();
        let link = |side| Link {
            config: config.clone(),
            peer: None,
            arrangement: crate::config::Arrangement::on(side),
        };
        let left = Controller::new(&config);
        let right = left.for_peer(1);
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let left_queue = Arc::new(InputQueue::new());
        let right_queue = Arc::new(InputQueue::new());
        left.attach(
            &link(Side::Left),
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "macos",
            left_queue.clone(),
        );
        right.attach(
            &link(Side::Right),
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "windows",
            right_queue.clone(),
        );
        left.unfreeze_layout();
        right.unfreeze_layout();
        (left, right, left_queue, right_queue)
    }

    #[test]
    fn pushing_against_a_sleeping_computers_edge_asks_to_wake_it() {
        let (_left, right, _left_queue, _right_queue) = two_destinations();
        let display = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let mac = [0x04, 0x42, 0x1a, 0xf1, 0xc8, 0xe4];
        let alarm = Arc::new(crate::wake::Alarm::new(Some(mac), None));
        let edge =
            Geometry::local_edge(&crate::config::Arrangement::on(Side::Right), &[display]).unwrap();
        right.watch_for_sleep(Some(edge), &alarm);
        let push = |dx: f64| {
            let mut state = right.state.lock().unwrap();
            right.wake_at_edge(&mut state, 1915.0, 540.0, dx, 0.0);
        };
        // Pretend the push began a while ago, with reports arriving all along.
        let backdate = |by: Duration, pause: Duration| {
            let mut state = right.state.lock().unwrap();
            let push = state.wake_push.as_mut().unwrap();
            push.since = push.since.checked_sub(by).unwrap();
            push.last = push.last.checked_sub(pause).unwrap();
        };

        // While the computer is connected, that edge crosses instead.
        push(5.0);
        assert!(right.state.lock().unwrap().wake_push.is_none());

        right.detach();
        push(5.0);
        assert_eq!(alarm.wait(Duration::ZERO), None, "a flick wakes nothing");
        backdate(WAKE_PUSH, Duration::ZERO);
        push(5.0);
        assert_eq!(alarm.wait(Duration::ZERO), Some((mac, None)));

        // Moving inward ends the push, and so does a pause in the reports.
        push(-5.0);
        assert!(right.state.lock().unwrap().wake_push.is_none());
        push(5.0);
        backdate(WAKE_PUSH, WAKE_PUSH_GAP);
        push(5.0);
        let since = right.state.lock().unwrap().wake_push.unwrap().since;
        assert!(since.elapsed() < WAKE_PUSH);

        // A computer that can't be woken is left alone.
        right.watch_for_sleep(None, &alarm);
        push(5.0);
        assert!(right.state.lock().unwrap().wake_push.is_none());
    }

    #[test]
    fn a_game_holding_the_destination_mouse_keeps_the_pointer_there() {
        let (left, right, _left_queue, right_queue) = two_destinations();
        {
            let mut state = left.state.lock().unwrap();
            left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
            right.begin_remote(&mut state, (2.0, 540.0));
            assert!(!state.remote_pointer_locked());
        }
        while right_queue.pop(Duration::ZERO).is_some() {}
        // Only the computer being controlled can hold the pointer.
        left.set_peer_pointer_lock(true, f64::NAN, f64::NAN);
        assert!(!left.state.lock().unwrap().remote_pointer_locked());
        right.set_peer_pointer_lock(true, f64::NAN, f64::NAN);
        assert!(left.state.lock().unwrap().remote_pointer_locked());
        // Alt+Tab: resume from the game's cursor, kept on that computer's display.
        right.set_peer_pointer_lock(false, 5000.0, -20.0);
        let state = left.state.lock().unwrap();
        assert!(!state.remote_pointer_locked());
        assert_eq!(state.remote_pos, (1919.0, 0.0));
        drop(state);
        right.set_peer_pointer_lock(true, f64::NAN, f64::NAN);
        left.force_local();
        assert!(!left.state.lock().unwrap().remote_pointer_locked());
    }

    #[test]
    fn queued_raw_motion_is_summed_not_replaced() {
        let queue = InputQueue::new();
        for (dx, dy) in [(3, -1), (4, 2)] {
            assert!(queue.push(Input::Motion { dx, dy }));
        }
        queue.push(Input::Button {
            button: MouseButton::Left,
            down: true,
        });
        queue.push(Input::Motion {
            dx: i32::MAX,
            dy: 0,
        });
        queue.push(Input::Motion { dx: 1, dy: 0 });
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(Input::Motion { dx: 7, dy: 1 })
        ));
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(Input::Button { .. })
        ));
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(Input::Motion {
                dx: i32::MAX,
                dy: 0
            })
        ));
        assert!(queue.pop(Duration::ZERO).is_none());
    }

    #[test]
    fn three_computers_route_keyboard_to_only_the_selected_edge() {
        let (left, _right, left_queue, right_queue) = two_destinations();
        let mut state = left.state.lock().unwrap();
        left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
        assert_eq!(state.selected_peer, 1);
        left.send(Input::Key {
            key: Key::KeyA,
            down: true,
        });
        assert!(matches!(
            right_queue.pop(Duration::ZERO),
            Some(Input::Key {
                key: Key::KeyA,
                down: true
            })
        ));
        assert!(left_queue.pop(Duration::ZERO).is_none());
        // Crossing cannot pick a different connection while already remote.
        state.remote = true;
        left.select_at_edge(&mut state, 0.0, 540.0, -10.0, 0.0);
        assert_eq!(state.selected_peer, 1);
        state.remote = false;
        left.select_at_edge(&mut state, 0.0, 540.0, -10.0, 0.0);
        assert_eq!(state.selected_peer, 0);
        left.send(Input::Key {
            key: Key::KeyB,
            down: false,
        });
        assert!(matches!(
            left_queue.pop(Duration::ZERO),
            Some(Input::Key {
                key: Key::KeyB,
                down: false
            })
        ));
        assert!(right_queue.pop(Duration::ZERO).is_none());
    }

    #[test]
    fn disconnecting_inactive_peer_keeps_active_connection_and_physical_holds() {
        let (left, right, left_queue, right_queue) = two_destinations();
        {
            let mut state = left.state.lock().unwrap();
            left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
            state.remote = true;
            state.keys.insert(Key::ShiftLeft);
            state.buttons.insert(MouseButton::Left);
        }
        left.detach();
        assert!(left_queue.is_closed());
        assert!(!right_queue.is_closed());
        assert!(right.is_controlling_peer());
        let mut state = left.state.lock().unwrap();
        assert!(state.keys.contains(&Key::ShiftLeft));
        assert!(state.buttons.contains(&MouseButton::Left));
        state.remote = false; // This test never acquires the native cursor.
    }

    #[test]
    fn drag_frozen_and_disconnected_destinations_cannot_steal_routing() {
        let (left, right, _, right_queue) = two_destinations();
        {
            let mut state = left.state.lock().unwrap();
            state.buttons.insert(MouseButton::Left);
            left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
            assert_eq!(state.selected_peer, 0);
            state.buttons.clear();
        }
        right.freeze_layout().unwrap();
        {
            let mut state = left.state.lock().unwrap();
            left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
            assert_eq!(state.selected_peer, 0);
        }
        right.unfreeze_layout();
        right_queue.close();
        let mut state = left.state.lock().unwrap();
        left.select_at_edge(&mut state, 1919.0, 540.0, 10.0, 0.0);
        assert_eq!(state.selected_peer, 0);
    }

    #[test]
    fn held_buttons_keep_motion_on_its_computer_until_every_button_is_released() {
        // Exercise the source controller's crossing decisions without executing
        // cursor acquisition or posting clicks on the test runner's desktop.
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        for (side, entry, outward, remote, returning, clamped) in [
            (
                Side::Right,
                (1919.0, 540.0),
                (5.0, 0.0),
                (0.0, 540.0),
                (-20.0, 7.0),
                (0.0, 547.0),
            ),
            (
                Side::Left,
                (0.0, 540.0),
                (-5.0, 0.0),
                (1919.0, 540.0),
                (20.0, 7.0),
                (1919.0, 547.0),
            ),
            (
                Side::Top,
                (960.0, 0.0),
                (0.0, -5.0),
                (960.0, 1079.0),
                (7.0, 20.0),
                (967.0, 1079.0),
            ),
            (
                Side::Bottom,
                (960.0, 1079.0),
                (0.0, 5.0),
                (960.0, 0.0),
                (7.0, -20.0),
                (967.0, 0.0),
            ),
        ] {
            let config = Config::default();
            let link = Link {
                arrangement: crate::config::Arrangement::on(side),
                ..Link::incoming(&config)
            };
            let controller = Controller::new(&config);
            let attach = || {
                controller.attach(
                    &link,
                    std::slice::from_ref(&monitor),
                    std::slice::from_ref(&monitor),
                    "",
                    Arc::new(InputQueue::new()),
                )
            };
            attach();
            let decisions = || {
                let mut state = controller.state.lock().unwrap();
                state.remote_pos = remote;
                let geometry = state.geometry.clone().unwrap();
                (
                    state.entry_position(&geometry, entry.0, entry.1, outward.0, outward.1),
                    state.advance_remote(&geometry, returning.0, returning.1),
                )
            };
            let (entry_unlocked, return_unlocked) = decisions();
            assert!(entry_unlocked.is_some());
            assert!(matches!(return_unlocked, Advance::Return(_)));
            for button in [
                MouseButton::Left,
                MouseButton::Right,
                MouseButton::Middle,
                MouseButton::Back,
                MouseButton::Forward,
                MouseButton::Auxiliary(6),
            ] {
                assert!(controller.handle_button(button, true));
                assert_eq!(
                    decisions(),
                    (None, Advance::Remote(clamped)),
                    "{side:?} {button:?}"
                );
                // Reset/reconnection must not forget a still-physical hold.
                controller.detach();
                attach();
                assert_eq!(decisions(), (None, Advance::Remote(clamped)));
                controller.handle_button(MouseButton::Auxiliary(7), true);
                controller.handle_button(button, false);
                assert_eq!(decisions(), (None, Advance::Remote(clamped)));
                controller.handle_button(MouseButton::Auxiliary(7), false);
                let (entry_after, return_after) = decisions();
                assert!(entry_after.is_some());
                assert!(matches!(return_after, Advance::Return(_)));
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_switch_shortcut_targets_the_computer_on_the_arrows_side() {
        let local = Monitor {
            id: 1,
            identity: None,
            name: "local".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let remote = Monitor {
            id: 2,
            name: "remote".into(),
            width: 2560,
            height: 1440,
            ..local.clone()
        };
        let config = Config::default();
        let link = Link {
            arrangement: crate::config::Arrangement::on(Side::Right),
            ..Link::incoming(&config)
        };
        let controller = Controller::new(&config);
        controller.attach(
            &link,
            std::slice::from_ref(&local),
            std::slice::from_ref(&remote),
            "",
            Arc::new(InputQueue::new()),
        );
        let mut state = controller.state.lock().unwrap();
        for peer in state.peers.values_mut() {
            peer.ready = true;
        }
        assert_eq!(arrow_side(Key::RightArrow), Some(Side::Right));
        assert_eq!(arrow_side(Key::KeyA), None);
        let target = controller.shortcut_target(&mut state, Side::Right);
        assert_eq!(target.map(|(_, center)| center), Some((1280.0, 720.0)));
        assert_eq!(controller.shortcut_target(&mut state, Side::Left), None);
        // A drag in progress stays on this computer.
        state.buttons.insert(MouseButton::Left);
        assert_eq!(controller.shortcut_target(&mut state, Side::Right), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_switch_shortcut_leaves_no_key_held_on_either_computer() {
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let config = Config::default();
        let link = Link {
            arrangement: crate::config::Arrangement::on(Side::Right),
            ..Link::incoming(&config)
        };
        let controller = Controller::new(&config);
        let queue = Arc::new(InputQueue::new());
        controller.attach(
            &link,
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "",
            queue.clone(),
        );
        for peer in controller.state.lock().unwrap().peers.values_mut() {
            peer.ready = true;
        }
        let key = |key, down| {
            controller.handle(Event {
                event_type: if down {
                    EventType::KeyPress(key)
                } else {
                    EventType::KeyRelease(key)
                },
                time: std::time::SystemTime::now(),
                name: None,
            })
        };
        let remote = || controller.remote_active.load(Ordering::Acquire);
        let sent = || {
            let mut sent = Vec::new();
            while let Some(input) = queue.pop(Duration::ZERO) {
                sent.push(input);
            }
            sent
        };
        let chord = [Key::ControlLeft, Key::Alt, Key::ShiftLeft];

        for modifier in chord {
            assert!(
                key(modifier, true).is_some(),
                "this computer's apps see them"
            );
        }
        assert!(key(Key::RightArrow, true).is_none());
        assert!(remote());
        sent();
        // The arrow repeats while held and is usually released first.
        assert!(key(Key::RightArrow, true).is_none());
        assert!(key(Key::RightArrow, false).is_none());
        assert!(key(Key::KeyA, true).is_none());
        assert!(key(Key::KeyA, false).is_none());
        // Scroll Lock stays a key of this computer, where it locks the pointer.
        assert!(key(Key::ScrollLock, true).is_some());
        assert!(key(Key::ScrollLock, false).is_some());
        for modifier in chord {
            assert!(key(modifier, false).is_none());
        }
        let keys: Vec<_> = sent()
            .into_iter()
            .filter_map(|input| match input {
                Input::Key { key, down } => Some((key, down)),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            [(Key::KeyA, true), (Key::KeyA, false)],
            "the other computer gets the typed key, with no modifier held"
        );

        // Pressed on the other computer, the chord and the arrow back return,
        // even while an app there holds the mouse and with it the edge.
        controller.set_peer_pointer_lock(true, 960.0, 540.0);
        for modifier in chord {
            assert!(key(modifier, true).is_none());
        }
        assert!(key(Key::LeftArrow, true).is_none());
        assert!(!remote());
        assert!(
            sent()
                .iter()
                .any(|input| matches!(input, Input::ReleaseAll)),
            "the modifiers pressed there are released there"
        );
        assert!(key(Key::LeftArrow, false).is_none());
        // Still holding the chord, the arrow toward it leaves again.
        assert!(key(Key::RightArrow, true).is_none());
        assert!(remote());
        sent();
        for released in [Key::RightArrow].into_iter().chain(chord) {
            assert!(key(released, false).is_none());
        }
        assert!(
            !sent()
                .iter()
                .any(|input| matches!(input, Input::Key { .. })),
            "releases of the shortcut's keys reach neither computer"
        );
        // Ctrl+Alt+Del's way back leaves the cursor on the edge it crossed:
        // home, with the edge waiting for inward motion.
        assert!(remote());
        controller.take_back_from_secure_screen();
        assert!(!remote());
        assert!(!controller.state.lock().unwrap().edge_armed);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_modifier_pressed_again_after_a_missed_release_is_a_new_press() {
        let controller = Controller::new(&Config::default());
        let mut state = controller.state.lock().unwrap();
        state.shortcut_keys = [Key::ControlLeft, Key::Alt].into();
        drop(state);
        let press = |key| Event {
            event_type: EventType::KeyPress(key),
            time: std::time::SystemTime::now(),
            name: None,
        };
        // A repeat of the arrow that fired the shortcut would be swallowed,
        // but a modifier doesn't repeat: Ctrl+C after Ctrl+Alt+Del types a copy.
        assert!(controller.handle(press(Key::ControlLeft)).is_some());
        let state = controller.state.lock().unwrap();
        assert!(state.shortcut_keys.is_empty());
        assert!(state.keys.contains(&Key::ControlLeft));
    }

    #[test]
    fn releases_the_hooks_missed_do_not_block_or_cross_with_the_pointer() {
        // Ctrl+Alt+Del or a UAC prompt can swallow releases. The next crossing
        // must neither be refused for a "held" button nor carry the key along.
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let config = Config::default();
        let link = Link {
            arrangement: crate::config::Arrangement::on(Side::Right),
            ..Link::incoming(&config)
        };
        let controller = Controller::new(&config);
        controller.attach(
            &link,
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "",
            Arc::new(InputQueue::new()),
        );
        assert!(controller.handle_button(MouseButton::Left, true));
        let mut state = controller.state.lock().unwrap();
        state.keys.insert(Key::ControlLeft);
        let geometry = state.geometry.clone().unwrap();
        assert_eq!(
            state.entry_position(&geometry, 1919.0, 540.0, 5.0, 0.0),
            None
        );

        // What Windows reports once the secure desktop is gone: nothing held.
        state.forget_released(|_| false, |_| false);
        assert!(state.keys.is_empty());
        assert!(
            state
                .entry_position(&geometry, 1919.0, 540.0, 5.0, 0.0)
                .is_some()
        );

        // A genuinely held button still keeps the pointer here.
        state.buttons.insert(MouseButton::Left);
        state.forget_released(|_| true, |_| true);
        assert_eq!(
            state.entry_position(&geometry, 1919.0, 540.0, 5.0, 0.0),
            None
        );
    }

    #[test]
    fn peer_takeover_requires_inward_motion_before_recrossing() {
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "local".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let geometry = Geometry::new(
            &Default::default(),
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
        )
        .unwrap();
        assert!(!far_enough_inside(&geometry, 1919.0, 540.0));
        assert!(!far_enough_inside(&geometry, 1880.0, 540.0));
        assert!(far_enough_inside(&geometry, 1800.0, 540.0));
    }

    /// `SendInput` on the hook thread, under the state lock, lets a hook for
    /// the moving mouse run and wait for that same lock: the agent froze.
    #[cfg(target_os = "windows")]
    #[test]
    fn crossing_with_a_key_held_leaves_its_release_to_the_cursor_thread() {
        let mut controller = Controller::new(&Config::default());
        let (queue, commands) = crossbeam_channel::unbounded();
        let core = Arc::get_mut(&mut Arc::get_mut(&mut controller).unwrap().core).unwrap();
        core.cursor_tx = CursorSender { queue, thread: 0 };

        let mut state = controller.state.lock().unwrap();
        controller.begin_remote(&mut state, (10.0, 540.0));
        assert!(
            commands.try_recv().is_err(),
            "no key is held, so there is nothing to release"
        );
        state.remote = false;
        state.keys.insert(Key::ControlLeft);
        controller.begin_remote(&mut state, (10.0, 540.0));
        let held = HashSet::from([Key::ControlLeft]);
        assert!(matches!(
            commands.try_recv(),
            Ok(CursorCommand::ReleaseKeys(keys)) if keys == held
        ));
    }

    #[test]
    fn release_while_disconnected_is_not_transferred_as_a_held_modifier() {
        // The native Windows hook feeds Controller::handle; inspect its real
        // outbound queue across the same disconnect/reconnect boundary.
        let config = Config::default();
        let link = Link::incoming(&config);
        let controller = Controller::new(&config);
        let monitor = Monitor {
            id: 1,
            identity: None,
            name: "display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let queue = Arc::new(InputQueue::new());
        controller.attach(
            &link,
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "macos",
            queue,
        );
        let handle = |event_type| {
            controller.handle(Event {
                event_type,
                time: std::time::SystemTime::now(),
                name: None,
            })
        };
        handle(EventType::KeyPress(Key::ShiftLeft));
        controller.detach();
        handle(EventType::KeyRelease(Key::ShiftLeft));
        let reconnected = Arc::new(InputQueue::new());
        controller.attach(
            &link,
            std::slice::from_ref(&monitor),
            std::slice::from_ref(&monitor),
            "macos",
            reconnected.clone(),
        );
        // Exercise held-key transfer directly; synthesized coordinates cannot
        // acquire a real cursor outside an interactive desktop.
        controller.begin_remote(&mut controller.state.lock().unwrap(), (10.0, 540.0));
        assert!(controller.remote_active.load(Ordering::Acquire));
        assert!(matches!(
            reconnected.pop(Duration::ZERO),
            Some(Input::Pointer { .. })
        ));
        let stale = reconnected.pop(Duration::ZERO);
        // Always release the virtual modifier and restore local control before
        // asserting; CI and SSH tests run outside the user's interactive session.
        handle(EventType::KeyRelease(Key::ShiftLeft));
        controller.force_local();
        controller.settle_cursor().unwrap();
        assert!(
            stale.is_none(),
            "released Shift was sent again after reconnection: {stale:?}"
        );
    }

    #[test]
    fn queue_overload_ends_the_stream_instead_of_losing_ordered_input() {
        let mut failures = Vec::new();
        for protected_pointer in [false, true] {
            let q = InputQueue::new();
            // The writer consumed the modifier press before backing up.
            assert!(q.push(Input::Key {
                key: Key::ShiftLeft,
                down: true,
            }));
            assert!(matches!(
                q.pop(Duration::ZERO),
                Some(Input::Key { down: true, .. })
            ));
            if protected_pointer {
                assert!(q.push(Input::Pointer { x: 70.0, y: 90.0 }));
                assert!(q.push(Input::Button {
                    button: MouseButton::Left,
                    down: true,
                }));
            }
            let queued = q.inner.lock().unwrap().events.len();
            for _ in queued..QUEUE_CAPACITY {
                assert!(q.push(Input::Wheel {
                    dx: 0,
                    dy: 1,
                    unit: ScrollUnit::Lines,
                }));
            }
            let accepted = q.push(Input::Key {
                key: Key::ShiftLeft,
                down: false,
            });
            let next = q.pop(Duration::ZERO);
            let accepted_later = q.push(Input::Pointer { x: 1.0, y: 1.0 });
            if accepted || !q.is_closed() || next.is_some() || accepted_later {
                failures.push(format!(
                    "protected_pointer={protected_pointer}: accepted={accepted}, closed={}, next={next:?}, accepted_later={accepted_later}",
                    q.is_closed(),
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "overloaded input stayed live: {failures:?}"
        );
    }

    #[test]
    fn queue_preserves_click_after_coalesced_moves() {
        let q = InputQueue::new();
        for n in 0..10000 {
            assert!(q.push(Input::Pointer {
                x: n as f64,
                y: 1.0
            }));
        }
        q.push(Input::Button {
            button: MouseButton::Left,
            down: true,
        });
        assert!(matches!(
            q.pop(Duration::ZERO),
            Some(Input::Pointer { x: 9999.0, .. })
        ));
        assert!(matches!(
            q.pop(Duration::ZERO),
            Some(Input::Button { down: true, .. })
        ));
    }

    #[test]
    fn key_flushes_latest_pointer_without_waiting_for_coalesce_deadline() {
        let q = InputQueue::new();
        q.push(Input::Pointer { x: 1.0, y: 0.0 });
        q.push(Input::Pointer { x: 2.0, y: 0.0 });
        q.push(Input::Key {
            key: Key::F9,
            down: true,
        });
        assert!(matches!(
            q.pop(Duration::ZERO),
            Some(Input::Pointer { x: 2.0, .. })
        ));
        assert!(matches!(
            q.pop(Duration::ZERO),
            Some(Input::Key {
                key: Key::F9,
                down: true
            })
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trackpad_scroll_retains_small_deltas_and_wheel_notches_are_separate() {
        let mut remainder = 0.0;
        let total: i64 = (0..10)
            .map(|_| scale_pixel_scroll(1, 0.4, &mut remainder))
            .sum();
        assert_eq!(total, 4);
        assert_eq!(scale_pixel_scroll(16, 0.4, &mut remainder), 6);
        assert_eq!(scale_pixel_scroll(-16, 0.4, &mut remainder), -6);
    }
}
