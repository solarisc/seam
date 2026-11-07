//! The settings window. All agent and network calls run on worker threads;
//! the render loop only reads their latest results.
mod arrangement;
mod computers;
mod glyphs;
mod screens;
mod settings;
mod theme;
mod widgets;
mod window_frame;

use crate::{
    config::{Config, config_dir},
    layout::Layout,
    pairing,
    protocol::{Monitor, monitors},
    service,
    shutdown::Signal,
    status::{self, Snapshot},
};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use eframe::egui::{
    self, Align, CornerRadius, Frame, Layout as Flow, Margin, RichText, Sense, Stroke, Vec2,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use widgets::{Kind, Tone};

/// Label of the task that starts sharing; its failure is shown as a banner.
const STARTING: &str = "Starting…";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Screens,
    Computers,
    Settings,
}

impl Page {
    const ALL: [Page; 3] = [Page::Screens, Page::Computers, Page::Settings];

    fn label(self) -> &'static str {
        match self {
            Page::Screens => "Screens",
            Page::Computers => "Computers",
            Page::Settings => "Settings",
        }
    }
}

/// Polls the agent once a second on its own thread.
struct Poller {
    requests: Sender<(u64, Config)>,
    results: Receiver<(u64, Option<Snapshot>)>,
    generation: u64,
    pending: bool,
    last: Instant,
}

impl Poller {
    fn new() -> Self {
        let (request_tx, request_rx) = crossbeam_channel::bounded::<(u64, Config)>(1);
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            while let Ok((generation, config)) = request_rx.recv() {
                let snapshot = status::agent(&config).status().ok();
                if result_tx.send((generation, snapshot)).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: request_tx,
            results: result_rx,
            generation: 0,
            pending: false,
            last: Instant::now() - Duration::from_secs(5),
        }
    }

    /// Discard any answer started before a configuration change.
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.last = Instant::now() - Duration::from_secs(5);
    }
}

/// What a finished background action changes in the window.
enum Outcome {
    Saved(Box<Config>, String),
    Done(String),
    Layout,
}

struct Task {
    label: String,
    reply: Receiver<std::result::Result<Outcome, String>>,
}

enum PairingState {
    Connecting,
    Confirm(String),
    Failed(String),
}

struct Outgoing {
    name: String,
    address: String,
    port: u16,
    state: PairingState,
    events: Receiver<pairing::Event>,
    cancel: Arc<Signal>,
}

struct Toast {
    text: String,
    tone: Tone,
    shown: Instant,
}

pub struct App {
    /// The configuration the agent runs with, as saved on disk.
    config: Config,
    /// Unsaved edits on the Settings page.
    draft: Config,
    local: Vec<Monitor>,
    last_display_scan: Instant,
    /// When the window opened, to stop searching for computers after a while.
    opened: Instant,
    snapshot: Option<Snapshot>,
    /// At least one status poll has finished, so `snapshot` is meaningful.
    polled: bool,
    poller: Poller,
    page: Page,
    task: Option<Task>,
    outgoing: Option<Outgoing>,
    answered: Option<u64>,
    toast: Option<Toast>,
    drag: arrangement::Drag,
    manual_address: String,
    confirm_remove: Option<usize>,
    started_agent: bool,
    /// Why the last attempt to start sharing failed, shown until it works.
    start_error: Option<String>,
    /// Whether Seam starts at sign-in; checking runs a program, so once.
    starts_at_login: Option<bool>,
    /// Modification time of the saved configuration last loaded.
    config_stamp: Option<std::time::SystemTime>,
    last_config_check: Instant,
    queued_save: Option<(Box<Config>, String)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config) -> Self {
        theme::install(&cc.egui_ctx);
        #[cfg(windows)]
        window_frame::native_outline(cc);
        if let Err(error) = service::ensure_login_item() {
            log::warn!("cannot register login item: {error:#}");
        }
        Self {
            draft: config.clone(),
            config,
            local: Vec::new(),
            last_display_scan: Instant::now() - Duration::from_secs(10),
            opened: Instant::now(),
            snapshot: None,
            polled: false,
            poller: Poller::new(),
            page: Page::Screens,
            task: None,
            outgoing: None,
            answered: None,
            toast: None,
            drag: arrangement::Drag::default(),
            manual_address: String::new(),
            confirm_remove: None,
            started_agent: false,
            start_error: None,
            starts_at_login: None,
            config_stamp: config_stamp(),
            last_config_check: Instant::now(),
            queued_save: None,
        }
    }

    fn running(&self) -> bool {
        self.snapshot.is_some()
    }

    fn busy(&self) -> bool {
        self.task.is_some()
    }

    fn remote_active(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(Snapshot::has_remote_control)
    }

    fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.toast = Some(Toast {
            text: text.into(),
            tone,
            shown: Instant::now(),
        });
    }

    fn spawn(&mut self, label: &str, work: impl FnOnce() -> Result<Outcome> + Send + 'static) {
        if self.task.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(work().map_err(|e| format!("{e:#}")));
        });
        self.task = Some(Task {
            label: label.into(),
            reply: rx,
        });
        self.poller.invalidate();
    }

    /// Save a new configuration and restart sharing with it. A save that
    /// arrives while another action runs waits for it to finish.
    fn save(&mut self, config: Config, message: &str) {
        if self.task.is_some() {
            self.queued_save = Some((Box::new(config), message.to_string()));
            return;
        }
        let previous = self.config.clone();
        let message = message.to_string();
        self.spawn("Applying changes…", move || {
            service::apply(&previous, &config)?;
            Ok(Outcome::Saved(Box::new(config), message))
        });
    }

    fn start(&mut self) {
        let config = self.config.clone();
        self.spawn(STARTING, move || {
            crate::config::edit(|saved| {
                saved.paused = false;
                Ok(())
            })?;
            service::start(&config)?;
            Ok(Outcome::Done("Sharing is on".into()))
        });
    }

    /// Stays paused when the window is closed and opened again; signing in
    /// again resumes sharing, so a computer without a keyboard is never
    /// left unreachable.
    fn pause(&mut self) {
        let config = self.config.clone();
        self.spawn("Pausing…", move || {
            service::stop(&config)?;
            crate::config::edit(|saved| {
                saved.paused = true;
                Ok(())
            })?;
            Ok(Outcome::Done("Sharing paused".into()))
        });
    }

    fn return_control(&mut self) {
        let config = self.config.clone();
        self.spawn("Returning control…", move || {
            status::agent(&config).reset_input()?;
            Ok(Outcome::Done("The pointer is back on this computer".into()))
        });
    }

    /// Change one link's arrangement: live through the agent when connected,
    /// otherwise by saving the configuration.
    fn rearrange(
        &mut self,
        key: Option<usize>,
        change: impl FnOnce(&mut crate::config::Arrangement),
    ) {
        let mut config = self.config.clone();
        let arrangement = match key {
            Some(index) => match config.peers.get_mut(index) {
                Some(peer) => &mut peer.arrangement,
                None => return,
            },
            None => &mut config.arrangement,
        };
        change(arrangement);
        let layout = Layout::of(arrangement);
        let live = self
            .snapshot
            .as_ref()
            .and_then(|s| s.link(key))
            .filter(|link| link.connected)
            .and_then(|link| link.layout.as_ref())
            .filter(|report| report.revision != 0 && !report.pending)
            .map(|report| report.revision);
        match live {
            Some(revision) => {
                let current = self.config.clone();
                self.spawn("Updating arrangement…", move || {
                    status::agent(&current).apply_layout(key, layout, revision)?;
                    Ok(Outcome::Layout)
                });
            }
            None => self.save(config, "Arrangement saved"),
        }
    }

    fn pair(&mut self, name: &str, address: &str, port: u16) {
        let (events_tx, events) = crossbeam_channel::unbounded();
        let cancel = Arc::new(Signal::default());
        let (me, target, stop) = (
            self.config.clone(),
            address.trim().to_string(),
            cancel.clone(),
        );
        std::thread::spawn(move || {
            let result = pairing::request(&target, port, &me, &events_tx, &stop);
            let _ = events_tx.send(pairing::Event::Finished(
                result.map_err(|e| format!("{e:#}")),
            ));
        });
        self.outgoing = Some(Outgoing {
            name: if name.is_empty() {
                address.trim().to_string()
            } else {
                name.to_string()
            },
            address: address.trim().to_string(),
            port,
            state: PairingState::Connecting,
            events,
            cancel,
        });
    }

    fn paired(&mut self, paired: pairing::Paired) {
        let mut config = self.config.clone();
        let name = paired.name.clone();
        if let Some(existing) = config.peers.iter_mut().find(|peer| {
            peer.code == paired.code || peer.address.eq_ignore_ascii_case(&paired.address)
        }) {
            existing.name = paired.name;
            existing.address = paired.address;
            existing.port = paired.port;
            existing.code = paired.code;
        } else {
            match paired.into_peer(&config) {
                Ok(peer) => config.peers.push(peer),
                Err(error) => return self.notify(format!("{error:#}"), Tone::Error),
            }
        }
        // Controlling other computers replaces being controlled.
        config.controller_name.clear();
        self.save(config, &format!("Paired with {name}. Connecting…"));
    }

    fn remove_peer(&mut self, index: usize) {
        let mut config = self.config.clone();
        if index < config.peers.len() {
            let peer = config.peers.remove(index);
            self.save(config, &format!("Removed {}", peer.label()));
        }
    }

    /// Forget the controlling computer by replacing this computer's secret.
    fn unpair_controller(&mut self) {
        let mut config = self.config.clone();
        config.pair_code = crate::config::random_secret();
        config.controller_name.clear();
        self.save(config, "This computer is no longer paired");
    }

    fn answer(&mut self, id: u64, accept: bool, name: String) {
        self.answered = Some(id);
        let config = self.config.clone();
        self.spawn(
            if accept { "Pairing…" } else { "Declining…" },
            move || {
                status::agent(&config).answer_pairing(id, accept)?;
                Ok(Outcome::Done(if accept {
                    format!("Paired. {name} can now control this computer.")
                } else {
                    "Pairing request declined".into()
                }))
            },
        );
    }

    fn poll(&mut self) {
        if let Some(task) = &self.task {
            match task.reply.try_recv() {
                Ok(result) => {
                    let starting = task.label == STARTING;
                    self.task = None;
                    self.poller.invalidate();
                    if starting {
                        self.start_error = result.as_ref().err().cloned();
                    }
                    match result {
                        Ok(Outcome::Saved(config, message)) => {
                            self.config = (*config).clone();
                            self.draft = *config;
                            self.config_stamp = config_stamp();
                            self.notify(message, Tone::Info);
                        }
                        Ok(Outcome::Done(message)) => self.notify(message, Tone::Info),
                        Ok(Outcome::Layout) => {
                            if let Ok(config) = Config::load_or_create() {
                                self.config = config;
                            }
                            self.notify("Arrangement updated on both computers", Tone::Info);
                        }
                        // Shown as a lasting banner rather than a toast.
                        Err(error) if starting => log::error!("starting sharing failed: {error}"),
                        Err(error) => self.notify(error, Tone::Error),
                    }
                }
                Err(TryRecvError::Disconnected) => self.task = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(outgoing) = &mut self.outgoing {
            let mut finished = None;
            while let Ok(event) = outgoing.events.try_recv() {
                match event {
                    pairing::Event::Confirm(code) => outgoing.state = PairingState::Confirm(code),
                    pairing::Event::Finished(Ok(paired)) => finished = Some(paired),
                    pairing::Event::Finished(Err(error)) => {
                        if outgoing.cancel.is_stopped() {
                            self.outgoing = None;
                            break;
                        }
                        outgoing.state = PairingState::Failed(error);
                    }
                }
            }
            if let Some(paired) = finished {
                self.outgoing = None;
                self.paired(paired);
            }
        }
        if let Ok((generation, snapshot)) = self.poller.results.try_recv() {
            self.poller.pending = false;
            if generation == self.poller.generation {
                self.snapshot = snapshot;
                self.polled = true;
            }
        }
        if !self.poller.pending
            && self.task.is_none()
            && self.poller.last.elapsed() >= Duration::from_secs(1)
            && self
                .poller
                .requests
                .try_send((self.poller.generation, self.config.clone()))
                .is_ok()
        {
            self.poller.pending = true;
            self.poller.last = Instant::now();
        }
        // The window starts sharing when opened, so a fresh install works
        // without a separate step, unless the person paused it.
        if self.polled
            && !self.started_agent
            && !self.config.paused
            && self.snapshot.is_none()
            && self.task.is_none()
        {
            self.started_agent = true;
            self.start();
        }
        if self.task.is_none()
            && let Some((config, message)) = self.queued_save.take()
        {
            self.save(*config, &message);
        }
        self.reload_if_changed();
        if self.last_display_scan.elapsed() >= Duration::from_secs(3) {
            // display-info uses AppKit on macOS and must stay on this thread.
            if let Ok(displays) = monitors() {
                self.local = displays;
            }
            self.last_display_scan = Instant::now();
        }
    }

    /// The agent saves arrangement and pairing changes itself; follow them
    /// unless the person has unsaved edits.
    fn reload_if_changed(&mut self) {
        if self.task.is_some() || self.last_config_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_config_check = Instant::now();
        let stamp = config_stamp();
        if stamp == self.config_stamp {
            return;
        }
        self.config_stamp = stamp;
        if let Ok(config) = Config::load_or_create() {
            if self.draft == self.config {
                self.draft = config.clone();
            } else {
                self.draft.peers.clone_from(&config.peers);
                self.draft.arrangement.clone_from(&config.arrangement);
                self.draft
                    .controller_name
                    .clone_from(&config.controller_name);
                self.draft.paused = config.paused;
            }
            self.config = config;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let width = ui.available_width();
        // The whole bar, margins included, moves the window.
        window_frame::title_bar(ui, ui.max_rect().expand2(Vec2::new(16.0, 0.0)));
        ui.horizontal_centered(|ui| {
            ui.set_width(width);
            ui.add_space(window_frame::LEADING);
            let (logo, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            glyphs::logo(ui.painter(), logo);
            ui.label(
                RichText::new("Seam")
                    .font(theme::strong(14.0))
                    .color(p.text),
            );

            // Center the page switcher in the window when there is room, and
            // otherwise keep it clear of the controls on the right.
            let labels: Vec<&str> = Page::ALL.iter().map(|page| page.label()).collect();
            let switcher = widgets::SEGMENT_WIDTH * labels.len() as f32 + 4.0;
            let window = ui.ctx().viewport_rect();
            let compact = window.width() < 900.0;
            let window_buttons = if cfg!(windows) { 3.0 * 46.0 + 8.0 } else { 0.0 };
            let trailing = window_buttons + 48.0 + if compact { 24.0 } else { 116.0 };
            let start = (window.center().x - switcher / 2.0)
                .min(window.right() - trailing - switcher - 16.0)
                .max(ui.cursor().min.x + 16.0);
            ui.add_space(start - ui.cursor().min.x);
            let selected = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0);
            if let Some(index) = widgets::segmented(ui, &labels, selected) {
                self.page = Page::ALL[index];
            }

            ui.with_layout(Flow::right_to_left(Align::Center), |ui| {
                #[cfg(windows)]
                {
                    window_frame::buttons(ui);
                    ui.add_space(8.0);
                }
                let running = self.running();
                let mut sharing = running;
                let remote = running && self.remote_active();
                let enabled = self.polled && !self.busy() && !remote;
                if widgets::toggle_enabled(ui, &mut sharing, enabled)
                    .on_hover_text(if remote {
                        "Move the pointer back to this computer to change this"
                    } else if running {
                        "Pause sharing"
                    } else {
                        "Start sharing"
                    })
                    .changed()
                {
                    if sharing {
                        self.start();
                    } else {
                        self.pause();
                    }
                }
                ui.add_space(4.0);
                let (color, text) = match &self.snapshot {
                    _ if !self.polled => (p.faint, "Starting".to_string()),
                    None if self.start_error.is_some() => (p.danger, "Not running".to_string()),
                    None => (p.faint, "Paused".to_string()),
                    Some(_)
                        if self.config.peers.is_empty()
                            && self.config.controller_name.is_empty() =>
                    {
                        (p.faint, "Ready".to_string())
                    }
                    Some(s) => match s.connection_count() {
                        0 => (p.warning, "Not connected".to_string()),
                        1 => (p.success, "1 connected".to_string()),
                        n => (p.success, format!("{n} connected")),
                    },
                };
                // The short label says how many; hovering says why for each
                // computer, so a failing connection explains itself here.
                let details = self.connection_details();
                if compact {
                    // Narrow windows show the state on hover only.
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, color);
                    response.on_hover_text(if details.is_empty() {
                        text
                    } else {
                        format!("{text}\n\n{details}")
                    });
                } else {
                    let label = ui.add(
                        egui::Label::new(RichText::new(text).font(theme::small()).color(p.muted))
                            .sense(Sense::hover()),
                    );
                    widgets::dot(ui, color);
                    if !details.is_empty() {
                        label.on_hover_text(details);
                    }
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let p = theme::palette_for(ctx);
        if let Some(prompt) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.pairing.clone())
            .filter(|prompt| self.answered != Some(prompt.id))
        {
            let mut answer = None;
            egui::Modal::new("incoming-pairing".into())
                .frame(dialog_frame(&p))
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(
                        RichText::new(format!("Allow {} to control this computer?", prompt.name))
                            .font(theme::strong(16.0))
                            .color(p.text),
                    );
                    ui.add_space(6.0);
                    widgets::muted(
                        ui,
                        format!(
                            "{} at {} wants to use its keyboard and mouse here. Only allow it if the code below matches the one shown on that computer.",
                            widgets::os_name(&prompt.os),
                            prompt.address
                        ),
                    );
                    ui.add_space(14.0);
                    code_box(ui, &prompt.code);
                    ui.add_space(18.0);
                    ui.with_layout(Flow::right_to_left(Align::Center), |ui| {
                        if widgets::button(ui, "Allow", Kind::Primary).clicked() {
                            answer = Some(true);
                        }
                        if widgets::button(ui, "Decline", Kind::Secondary).clicked() {
                            answer = Some(false);
                        }
                    });
                });
            if let Some(accept) = answer {
                self.answer(prompt.id, accept, prompt.name.clone());
            }
        }

        if let Some(outgoing) = &self.outgoing {
            let mut close = false;
            let mut retry = false;
            egui::Modal::new("outgoing-pairing".into())
                .frame(dialog_frame(&p))
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    let title = match &outgoing.state {
                        PairingState::Failed(error) if error.contains("declined") => {
                            format!("{} declined the request", outgoing.name)
                        }
                        PairingState::Failed(_) => format!("Couldn't connect to {}", outgoing.name),
                        _ => format!("Connect to {}", outgoing.name),
                    };
                    ui.label(RichText::new(title).font(theme::strong(16.0)).color(p.text));
                    ui.add_space(8.0);
                    match &outgoing.state {
                        PairingState::Connecting => {
                            ui.horizontal(|ui| {
                                widgets::spinner(ui);
                                widgets::muted(ui, "Reaching the other computer…");
                            });
                        }
                        PairingState::Confirm(code) => {
                            widgets::muted(
                                ui,
                                format!(
                                    "On {}, choose Allow if it shows this code. If the codes differ, choose Cancel.",
                                    outgoing.name
                                ),
                            );
                            ui.add_space(14.0);
                            code_box(ui, code);
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                widgets::spinner(ui);
                                widgets::muted(
                                    ui,
                                    format!(
                                        "Waiting for {}… If nothing appears there, open Seam on it.",
                                        outgoing.name
                                    ),
                                );
                            });
                        }
                        PairingState::Failed(error) => {
                            ui.label(RichText::new(error).color(p.muted));
                        }
                    }
                    ui.add_space(16.0);
                    ui.with_layout(Flow::right_to_left(Align::Center), |ui| {
                        if let PairingState::Failed(_) = outgoing.state {
                            if widgets::button(ui, "Try again", Kind::Primary).clicked() {
                                retry = true;
                            }
                            if widgets::button(ui, "Close", Kind::Secondary).clicked() {
                                close = true;
                            }
                        } else if widgets::button(ui, "Cancel", Kind::Secondary).clicked() {
                            outgoing.cancel.stop();
                            close = true;
                        }
                    });
                });
            if close {
                self.outgoing = None;
            } else if retry {
                let (name, address, port) = (
                    outgoing.name.clone(),
                    outgoing.address.clone(),
                    outgoing.port,
                );
                self.pair(&name, &address, port);
            }
        }

        if let Some(index) = self.confirm_remove {
            let name = self
                .config
                .peers
                .get(index)
                .map(|p| p.label().to_string())
                .unwrap_or_default();
            let mut decision = None;
            egui::Modal::new("confirm-remove".into())
                .frame(dialog_frame(&p))
                .show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.label(
                        RichText::new(format!("Remove {name}?"))
                            .font(theme::strong(16.0))
                            .color(p.text),
                    );
                    ui.add_space(6.0);
                    widgets::muted(
                        ui,
                        format!(
                            "This computer will stop controlling it. To have {name} forget this computer too, choose Unpair on its Computers page."
                        ),
                    );
                    ui.add_space(18.0);
                    ui.with_layout(Flow::right_to_left(Align::Center), |ui| {
                        if widgets::button(ui, "Remove", Kind::Danger).clicked() {
                            decision = Some(true);
                        }
                        if widgets::button(ui, "Cancel", Kind::Secondary).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            if let Some(remove) = decision {
                self.confirm_remove = None;
                if remove {
                    self.remove_peer(index);
                }
            }
        }
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let p = theme::palette_for(ctx);
        if let Some(task) = &self.task {
            toast_frame(ctx, &p, p.text, &task.label, true, 1.0);
            return;
        }
        let Some(toast) = &self.toast else { return };
        let ttl = match toast.tone {
            Tone::Error => 8.0,
            _ => 4.0,
        };
        let age = toast.shown.elapsed().as_secs_f32();
        if age > ttl {
            self.toast = None;
            return;
        }
        // Fade in over 0.15 s and out over the last 0.4 s, redrawing only then.
        const FADE_IN: f32 = 0.15;
        const FADE_OUT: f32 = 0.4;
        let opacity = (age / FADE_IN).min((ttl - age) / FADE_OUT).clamp(0.0, 1.0);
        if age < FADE_IN || age > ttl - FADE_OUT {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_secs_f32(ttl - FADE_OUT - age));
        }
        let color = match toast.tone {
            Tone::Error => p.danger,
            Tone::Warning => p.warning,
            Tone::Info => p.text,
        };
        if toast_frame(ctx, &p, color, &toast.text, false, opacity) {
            self.toast = None;
        }
    }
}

fn config_stamp() -> Option<std::time::SystemTime> {
    crate::config::config_path()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
}

fn dialog_frame(p: &theme::Palette) -> Frame {
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(24))
}

/// A bottom-centered status message. Returns `true` when dismissed.
fn toast_frame(
    ctx: &egui::Context,
    p: &theme::Palette,
    color: egui::Color32,
    text: &str,
    spinner: bool,
    opacity: f32,
) -> bool {
    let mut dismissed = false;
    egui::Area::new("toast".into())
        // Rises a few pixels as it fades in.
        .anchor(
            egui::Align2::CENTER_BOTTOM,
            Vec2::new(0.0, -20.0 + 6.0 * (1.0 - opacity)),
        )
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_opacity(opacity);
            let response = Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0_f32, p.border))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(16, 10))
                .shadow(ctx.style().visuals.popup_shadow)
                .show(ui, |ui| {
                    ui.set_max_width(520.0);
                    ui.horizontal(|ui| {
                        if spinner {
                            widgets::spinner(ui);
                        }
                        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
                    });
                })
                .response;
            dismissed = response.interact(Sense::click()).clicked();
        });
    dismissed
}

fn code_box(ui: &mut egui::Ui, code: &str) {
    let p = theme::palette(ui);
    Frame::new()
        .fill(p.sunken)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(0, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(code)
                        .font(theme::strong(30.0))
                        .color(p.text)
                        .extra_letter_spacing(3.0),
                );
            });
        });
}

/// The app icon, decoded once for the sidebar.
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        let p = theme::palette_for(ctx);
        egui::TopBottomPanel::top("toolbar")
            .exact_height(48.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin {
                left: 16,
                // The window buttons sit flush with the corner on Windows.
                right: if cfg!(windows) { 0 } else { 16 },
                top: 0,
                bottom: 0,
            }))
            .show(ctx, |ui| {
                self.toolbar(ui);
                let bottom = ui.max_rect().bottom() - 0.5;
                ui.painter().hline(
                    ui.max_rect().x_range().expand(16.0),
                    bottom,
                    Stroke::new(1.0_f32, p.border),
                );
            });
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.window))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let side = ((ui.available_width() - theme::CONTENT_WIDTH) / 2.0)
                            .clamp(24.0, 120.0) as i8;
                        Frame::new()
                            .inner_margin(Margin {
                                left: side,
                                right: side,
                                top: 22,
                                bottom: 64,
                            })
                            .show(ui, |ui| match self.page {
                                Page::Screens => self.screens_page(ui),
                                Page::Computers => self.computers_page(ui),
                                Page::Settings => self.settings_page(ui),
                            });
                    });
            });
        self.dialogs(ctx);
        self.toast(ctx);
        // Last, so the resize cursor wins over whatever lies under the edge.
        #[cfg(windows)]
        window_frame::resize_edges(ctx);
        #[cfg(windows)]
        window_frame::outline(ctx);
        let fast = self.busy() || self.outgoing.is_some() || self.toast.is_some();
        ctx.request_repaint_after(Duration::from_millis(if fast { 100 } else { 500 }));
    }
}

pub fn run() -> Result<()> {
    let config = Config::load_or_create()?;
    std::fs::create_dir_all(config_dir()?)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config_dir()?.join("window.lock"))?;
    if lock.try_lock().is_err() {
        #[cfg(windows)]
        crate::windows_tray::focus_settings();
        return Ok(());
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png"))?;
    let options = eframe::NativeOptions {
        viewport: window_frame::configure(egui::ViewportBuilder::default())
            .with_title("Seam")
            .with_app_id("seam")
            .with_icon(icon)
            .with_inner_size([900.0, 660.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Seam",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, config)))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()));
    drop(lock);
    result
}
