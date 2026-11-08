//! The home page: what is connected and where each screen sits.
use super::{App, arrangement, theme, widgets};
use crate::{
    config::Side,
    protocol::Monitor,
    status::{LinkStatus, NearbyPeer},
    transfers::size,
};
use eframe::egui::{self, Align, Layout, RichText, Ui};
use widgets::{Kind, Tone};

impl App {
    pub(super) fn screens_page(&mut self, ui: &mut Ui) {
        let controlled = self.config.peers.is_empty() && !self.config.controller_name.is_empty();
        self.status_banners(ui);

        if self.config.peers.is_empty() && !controlled {
            return self.get_started(ui);
        }

        let links: Vec<LinkStatus> = self
            .snapshot
            .as_ref()
            .map(|s| s.links.clone())
            .unwrap_or_default();
        let computers: Vec<arrangement::Computer> = if self.config.peers.is_empty() {
            let link = links.iter().find(|l| l.peer.is_none());
            vec![arrangement::Computer {
                key: None,
                name: link
                    .map(|l| l.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| self.config.controller_name.clone()),
                monitors: link.map(|l| l.remote_monitors.clone()).unwrap_or_default(),
                arrangement: self.config.arrangement.clone(),
                connected: link.is_some_and(|l| l.connected),
                // A controlled computer's keyboard is usually on the
                // controller, so the pointer is there unless it is here.
                active: link.is_some_and(|l| l.connected && !l.controlled),
            }]
        } else {
            self.config
                .peers
                .iter()
                .enumerate()
                .map(|(index, peer)| {
                    let link = links.iter().find(|l| l.peer == Some(index));
                    let hint = crate::discovery::pairing_id(&peer.code);
                    let discovered = self.snapshot.as_ref().and_then(|s| {
                        s.nearby
                            .iter()
                            .find(|n| n.pairing_id == hint)
                            .map(|n| n.name.clone())
                    });
                    let name = match (peer.name.trim().is_empty(), link, discovered) {
                        (false, _, _) => peer.name.clone(),
                        (true, Some(l), _) if !l.name.is_empty() => l.name.clone(),
                        (true, _, Some(found)) => found,
                        _ => peer.address.clone(),
                    };
                    arrangement::Computer {
                        key: Some(index),
                        name,
                        monitors: link.map(|l| l.remote_monitors.clone()).unwrap_or_default(),
                        arrangement: peer.arrangement.clone(),
                        connected: link.is_some_and(|l| l.connected),
                        active: link.is_some_and(|l| l.connected && l.controlling),
                    }
                })
                .collect()
        };
        let editable = !self.busy() && !self.remote_active();
        let local = self.local.clone();
        let name = self.config.name.clone();
        let local_active = self.running() && !computers.iter().any(|c| c.active);
        if let Some(moved) = arrangement::show(
            ui,
            &name,
            &local,
            &computers,
            &mut self.drag,
            editable,
            local_active,
        ) {
            self.rearrange(moved.key, |arrangement| {
                arrangement.side = moved.side;
                arrangement.edge_offset = moved.offset;
                arrangement.local_monitor = moved.local_monitor;
            });
        }
        #[cfg(windows)]
        self.shortcut_hint(ui, &computers);
        ui.add_space(24.0);

        widgets::section(ui, "Computers", None);
        let p = theme::palette(ui);
        let mut change: Option<(Option<usize>, Choice)> = None;
        widgets::list(ui, |list| {
            for computer in &computers {
                let link = links.iter().find(|l| l.peer == computer.key);
                list.custom(|ui| {
                    ui.horizontal(|ui| {
                        let connected = link.is_some_and(|l| l.connected);
                        let ready = link.is_some_and(LinkStatus::ready);
                        widgets::dot(ui, widgets::state_color(&p, connected, ready));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(
                                RichText::new(&computer.name)
                                    .font(theme::strong(13.5))
                                    .color(p.text),
                            );
                            if arrangement::edge_blocked(&local, &computer.arrangement) {
                                ui.label(
                                    RichText::new(
                                        "Another display covers this edge, so the pointer can't cross. Drag it to an outer edge.",
                                    )
                                    .font(theme::small())
                                    .color(p.danger),
                                );
                            } else {
                                widgets::muted(
                                    ui,
                                    describe(
                                        link,
                                        &computer.name,
                                        computer.key.is_none(),
                                        &self.running_text(),
                                    ),
                                );
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(link) = link.filter(|l| l.connected) {
                                widgets::muted(ui, widgets::latency(link.rtt_us));
                                widgets::muted(ui, "·");
                            }
                            widgets::muted(ui, side_phrase(computer.arrangement.side));
                        });
                    });
                    let gone = gone_displays(link, &local);
                    if local.len() > 1 || computer.monitors.len() > 1 || gone.local || gone.remote
                    {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(18.0);
                            if let Some(choice) =
                                display_pickers(ui, &local, computer, gone, editable)
                            {
                                change = Some((computer.key, choice));
                            }
                        });
                    }
                });
            }
        });
        if let Some((key, choice)) = change {
            self.rearrange(key, |arrangement| match choice {
                Choice::Local(id) => arrangement.local_monitor = id,
                Choice::Remote(id) => arrangement.remote_monitor = id,
            });
        }

        let transfers = self
            .snapshot
            .as_ref()
            .map(|s| s.transfers.clone())
            .unwrap_or_default();
        if !transfers.is_empty() {
            ui.add_space(24.0);
            widgets::section(ui, "Clipboard transfers", None);
            widgets::list(ui, |list| {
                for transfer in &transfers {
                    list.custom(|ui| transfer_row(ui, transfer));
                }
            });
        }

        if self.remote_active() {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if widgets::button_enabled(ui, "Return control here", Kind::Secondary, !self.busy())
                    .clicked()
                {
                    self.return_control();
                }
                widgets::muted(
                    ui,
                    "Pointer stuck? Press Ctrl+Alt+Shift+Esc on the keyboard you're using.",
                );
            });
        }
    }

    /// The keys that switch computers, and the one that stops switching.
    #[cfg(windows)]
    fn shortcut_hint(&self, ui: &mut Ui, computers: &[arrangement::Computer]) {
        if self.config.peers.is_empty() || !self.running() {
            return;
        }
        let mut sides: Vec<Side> = computers
            .iter()
            .filter(|computer| computer.connected)
            .map(|computer| computer.arrangement.side)
            .collect();
        if sides.is_empty() {
            return;
        }
        sides.sort_by_key(|side| *side as u8);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            if self.config.switch_shortcut {
                widgets::muted(ui, "Switch with");
                ui.add_space(3.0);
                for key in ["Ctrl", "Alt", "Shift"] {
                    widgets::keycap(ui, key);
                }
                for side in sides {
                    widgets::arrow_keycap(ui, side);
                }
                ui.add_space(14.0);
            }
            widgets::keycap(ui, "Scroll Lock");
            ui.add_space(3.0);
            widgets::muted(ui, "keeps the pointer where it is");
        });
    }

    /// Each computer's name and state, worded as on this page, for the
    /// status in the title bar.
    pub(super) fn connection_details(&self) -> String {
        let Some(snapshot) = &self.snapshot else {
            return String::new();
        };
        let paused = self.running_text();
        snapshot
            .links
            .iter()
            .filter_map(|link| {
                let name = match link.peer {
                    Some(index) => self
                        .config
                        .peers
                        .get(index)
                        .map_or_else(|| link.name.clone(), |peer| peer.label().to_string()),
                    None if !link.name.is_empty() => link.name.clone(),
                    None => self.config.controller_name.clone(),
                };
                // Nobody has paired to control this computer yet.
                if name.is_empty() {
                    return None;
                }
                let state = describe(Some(link), &name, link.peer.is_none(), &paused);
                Some(format!("{name}: {state}"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn running_text(&self) -> String {
        if self.running() {
            String::new()
        } else {
            "Sharing is paused".into()
        }
    }

    pub(super) fn status_banners(&mut self, ui: &mut Ui) {
        if !self.polled {
            return;
        }
        let paused = self.snapshot.is_none() && !self.busy();
        let needs_permission = self
            .snapshot
            .as_ref()
            .is_some_and(|s| !s.input_permission_ready);
        if paused && let Some(error) = self.start_error.clone() {
            if widgets::notice(
                ui,
                Tone::Error,
                &format!("Seam couldn't start sharing. {error}"),
                Some("Try again"),
            ) {
                self.start();
            }
        } else if paused
            && widgets::notice(
                ui,
                Tone::Warning,
                "Sharing is paused. Other computers can't connect.",
                Some("Start sharing"),
            )
        {
            self.start();
        }
        if needs_permission
            && widgets::notice(
                ui,
                Tone::Warning,
                "Allow seam-agent under Privacy & Security → Accessibility to share this Mac.",
                Some("Open Settings"),
            )
        {
            open_accessibility_settings();
        }
        let problems = self
            .snapshot
            .as_ref()
            .map(|s| s.problems.clone())
            .unwrap_or_default();
        for problem in problems {
            widgets::notice(ui, Tone::Error, &problem, None);
        }
    }

    /// First run: this computer's screens, and the computers it could control.
    fn get_started(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let local = self.local.clone();
        let name = self.config.name.clone();
        arrangement::show(ui, &name, &local, &[], &mut self.drag, false, false);
        ui.add_space(28.0);
        widgets::section(
            ui,
            "Nearby computers",
            Some(
                "Computers running Seam on this network. Connect one to use it with this keyboard and mouse.",
            ),
        );
        self.nearby_list(ui);
        ui.add_space(20.0);
        let address = local_addresses().into_iter().next().unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "To control this computer from another one instead, choose {name}{} there.",
                if address.is_empty() {
                    String::new()
                } else {
                    format!(" ({address})")
                }
            ))
            .font(theme::small())
            .color(p.muted),
        );
    }

    /// Discovered computers that can be connected, plus entry by address.
    pub(super) fn nearby_list(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let paired: Vec<String> = self
            .config
            .peers
            .iter()
            .map(|peer| crate::discovery::pairing_id(&peer.code))
            .collect();
        let nearby: Vec<NearbyPeer> = self
            .snapshot
            .as_ref()
            .map(|s| {
                s.nearby
                    .iter()
                    .filter(|n| !paired.contains(&n.pairing_id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let full = self.config.free_side().is_none();
        let mut connect = None;
        widgets::list(ui, |list| {
            if nearby.is_empty() {
                list.custom(|ui| {
                    ui.horizontal(|ui| {
                        // Computers announce themselves every few seconds; after
                        // a while, say what to check instead of spinning on.
                        let searching = self.opened.elapsed() < std::time::Duration::from_secs(10);
                        if self.running() && searching {
                            widgets::spinner(ui);
                            widgets::muted(
                                ui,
                                "Looking for computers running Seam on this network…",
                            );
                        } else if self.running() {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "No other computers found. Open Seam on the other computer and check that both are on the same network, or enter its address below.",
                                    )
                                    .font(theme::small())
                                    .color(theme::palette(ui).muted),
                                )
                                .wrap(),
                            );
                        } else {
                            widgets::muted(
                                ui,
                                "Start sharing to look for computers on this network.",
                            );
                        }
                    });
                });
            }
            for found in &nearby {
                let detail = if !found.accepts_pairing {
                    format!(
                        "{} · {} · shares its own keyboard and mouse",
                        widgets::os_name(&found.os),
                        found.address
                    )
                } else if !found.controller.is_empty() {
                    format!(
                        "{} · {} · controlled by {}",
                        widgets::os_name(&found.os),
                        found.address,
                        found.controller
                    )
                } else {
                    format!("{} · {}", widgets::os_name(&found.os), found.address)
                };
                let enabled = found.accepts_pairing && !full && !self.busy();
                list.row(
                    RichText::new(&found.name)
                        .font(theme::strong(13.5))
                        .color(p.text),
                    Some(&detail),
                    |ui| {
                        if widgets::button_enabled(ui, "Connect", Kind::Secondary, enabled)
                            .clicked()
                        {
                            connect = Some((found.name.clone(), found.address.clone(), found.port));
                        }
                    },
                );
            }
            list.custom(|ui| {
                ui.horizontal(|ui| {
                    widgets::muted(ui, "Not listed? Enter its address:");
                    let width = (ui.available_width() - 110.0).clamp(140.0, 260.0);
                    let response =
                        widgets::text_input(ui, &mut self.manual_address, "192.168.1.20", width);
                    let submit =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let valid = !self.manual_address.trim().is_empty() && !full && !self.busy();
                    if (widgets::button_enabled(ui, "Connect", Kind::Secondary, valid).clicked()
                        || submit)
                        && valid
                    {
                        let (host, port) =
                            split_port(&self.manual_address, self.config.listen_port);
                        connect = Some((String::new(), host, port));
                    }
                });
            });
        });
        if full {
            ui.add_space(6.0);
            widgets::muted(
                ui,
                "Every screen edge already has a computer. Remove one to add another.",
            );
        }
        if let Some((name, address, port)) = connect {
            self.pair(&name, &address, port);
        }
    }
}

fn transfer_row(ui: &mut Ui, transfer: &crate::transfers::Snapshot) {
    let p = theme::palette(ui);
    let direction = if transfer.uploading { "to" } else { "from" };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(&transfer.name)
                .font(theme::strong(13.0))
                .color(p.text),
        );
        widgets::muted(ui, format!("{direction} {}", transfer.peer));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let detail = match transfer.state.as_str() {
                "failed" | "skipped" => transfer.error.clone(),
                "complete" | "sent" => "Done".into(),
                _ => {
                    let mut text = format!("{} of {}", size(transfer.bytes), size(transfer.total));
                    if transfer.bytes_per_second > 0.0 {
                        text += &format!(" · {}/s", size(transfer.bytes_per_second as u64));
                    }
                    text
                }
            };
            widgets::muted(ui, detail);
        });
    });
    let fraction = if transfer.total == 0 {
        1.0
    } else {
        (transfer.bytes as f32 / transfer.total as f32).clamp(0.0, 1.0)
    };
    ui.add_space(6.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(2), p.sunken);
    let color = match transfer.state.as_str() {
        "failed" => p.danger,
        "skipped" => p.muted,
        _ => p.accent,
    };
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * fraction, 4.0)),
        egui::CornerRadius::same(2),
        color,
    );
}

#[derive(Clone, Copy)]
enum Choice {
    Local(Option<u32>),
    Remote(Option<u32>),
}

/// No display has this number. A picker showing it reads "Disconnected
/// display", and choosing any display replaces it.
const GONE: u32 = 0;

/// Whether the display a computer's edge belongs to is gone, on this
/// computer (`local`) or the other one (`remote`), as the agents found when
/// they connected. Seam never moves an edge to a display nobody chose, so the
/// person picks another, even when only one is left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Gone {
    local: bool,
    remote: bool,
}

fn gone_displays(link: Option<&LinkStatus>, local: &[Monitor]) -> Gone {
    let Some((link, report)) = link
        .filter(|link| link.connected)
        .and_then(|link| Some((link, link.layout.as_ref()?)))
        // Until the computers settle a layout, its displays aren't checked.
        .filter(|(_, report)| report.revision != 0)
    else {
        return Gone::default();
    };
    let gone = |selected: Option<u32>, displays: &[Monitor]| {
        !displays.is_empty() && selected.is_some_and(|id| displays.iter().all(|m| m.id != id))
    };
    Gone {
        local: gone(report.layout.local_monitor, local),
        remote: gone(report.layout.remote_monitor, &link.remote_monitors),
    }
}

fn display_pickers(
    ui: &mut Ui,
    local: &[Monitor],
    computer: &arrangement::Computer,
    gone: Gone,
    enabled: bool,
) -> Option<Choice> {
    let selected = |gone: bool, id: Option<u32>| if gone { Some(GONE) } else { id };
    let mut choice = None;
    ui.add_enabled_ui(enabled, |ui| {
        if local.len() > 1 || gone.local {
            widgets::muted(ui, "This display");
            if let Some(id) = picker(
                ui,
                ("local", computer.key),
                local,
                selected(gone.local, computer.arrangement.local_monitor),
            ) {
                choice = Some(Choice::Local(id));
            }
            ui.add_space(12.0);
        }
        if computer.monitors.len() > 1 || gone.remote {
            widgets::muted(ui, format!("{} display", computer.name));
            if let Some(id) = picker(
                ui,
                ("remote", computer.key),
                &computer.monitors,
                selected(gone.remote, computer.arrangement.remote_monitor),
            ) {
                choice = Some(Choice::Remote(id));
            }
        }
    });
    choice
}

fn picker(
    ui: &mut Ui,
    salt: impl std::hash::Hash,
    monitors: &[Monitor],
    selected: Option<u32>,
) -> Option<Option<u32>> {
    let name = |id: Option<u32>| match id.and_then(|id| monitors.iter().find(|m| m.id == id)) {
        Some(m) => display_name(m),
        None if id.is_some() => "Disconnected display".into(),
        None => "Main display".into(),
    };
    let mut value = selected;
    egui::ComboBox::from_id_salt(salt)
        .width(170.0)
        .selected_text(name(selected))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut value, None, "Main display");
            for monitor in monitors {
                ui.selectable_value(&mut value, Some(monitor.id), display_name(monitor));
            }
        });
    (value != selected).then_some(value)
}

fn display_name(monitor: &Monitor) -> String {
    let name = if monitor.name.trim().is_empty() {
        format!("Display {}", monitor.id)
    } else {
        monitor.name.clone()
    };
    format!("{name} ({}×{})", monitor.width, monitor.height)
}

fn describe(link: Option<&LinkStatus>, name: &str, controlling_us: bool, paused: &str) -> String {
    if !paused.is_empty() {
        return paused.into();
    }
    let Some(link) = link else {
        return "Starting connection…".into();
    };
    if link.controlled {
        return "Controlling this computer now".into();
    }
    if link.controlling {
        return "In use with this keyboard and mouse".into();
    }
    if link.connected {
        let os = widgets::os_name(&link.os);
        return match link.layout.as_ref() {
            Some(report) if !report.error.is_empty() => report.error.clone(),
            Some(report) if report.ready => format!(
                "Connected · {os} · {}",
                link.address.split(':').next().unwrap_or("")
            ),
            _ => "Connected · finishing setup".into(),
        };
    }
    if controlling_us {
        format!("Waiting for {name} to connect")
    } else if link.last_error.is_empty() {
        "Connecting…".into()
    } else {
        friendly_error(&link.last_error, name)
    }
}

/// What a connection error means for the person, without internal detail.
fn friendly_error(error: &str, name: &str) -> String {
    if error.contains(crate::network::KEY_MISMATCH) {
        return format!(
            "{name} no longer recognizes this computer. Remove it here, then connect again."
        );
    }
    if error.starts_with(crate::agent::CLOSED_WHILE_CONNECTING) {
        return format!(
            "{name} closed the connection while connecting. If this keeps happening, restart Seam on {name}."
        );
    }
    let first = error.split(": ").next().unwrap_or(error);
    if first.starts_with("Cannot reach") {
        format!(
            "Can't reach {name}. Check that it's on, Seam is running there, and both are on the same network."
        )
    } else {
        first.to_string()
    }
}

fn side_phrase(side: Side) -> &'static str {
    match side {
        Side::Left => "On the left",
        Side::Right => "On the right",
        Side::Top => "Above",
        Side::Bottom => "Below",
    }
}

pub(super) fn split_port(value: &str, default: u16) -> (String, u16) {
    let value = value.trim();
    if let Ok(socket) = value.parse::<std::net::SocketAddr>() {
        return (socket.ip().to_string(), socket.port());
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.contains(':')
        && let Ok(port) = port.parse()
    {
        return (host.to_string(), port);
    }
    (value.trim_matches(['[', ']']).to_string(), default)
}

/// This computer's LAN addresses, the one used for the default route first.
pub(super) fn local_addresses() -> Vec<String> {
    // Connecting a UDP socket selects the outgoing interface without sending.
    let primary = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .ok()
        .map(|address| address.ip().to_string());
    let mut found: Vec<String> = primary.into_iter().collect();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = &interface.addr
            && !interface.is_loopback()
            && !v4.ip.is_link_local()
        {
            let ip = v4.ip.to_string();
            if !found.contains(&ip) {
                found.push(ip);
            }
        }
    }
    found
}

fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_addresses_accept_optional_ports() {
        assert_eq!(
            split_port(" 192.168.1.20 ", 47843),
            ("192.168.1.20".into(), 47843)
        );
        assert_eq!(
            split_port("192.168.1.20:5000", 47843),
            ("192.168.1.20".into(), 5000)
        );
        assert_eq!(
            split_port("desk.local", 47843),
            ("desk.local".into(), 47843)
        );
        assert_eq!(
            split_port("[fe80::1]:6000", 47843),
            ("fe80::1".into(), 6000)
        );
        assert_eq!(split_port("fe80::1", 47843), ("fe80::1".into(), 47843));
    }

    #[test]
    fn a_hang_up_names_the_computer_instead_of_blaming_versions() {
        let message = friendly_error(crate::agent::CLOSED_WHILE_CONNECTING, "Right PC");
        assert_eq!(
            message,
            "Right PC closed the connection while connecting. If this keeps happening, restart Seam on Right PC."
        );
        assert!(!message.contains("version"));
    }

    #[test]
    fn a_display_that_is_gone_offers_a_choice_even_when_one_is_left() {
        let display = |id| Monitor {
            id,
            identity: None,
            name: format!("Display {id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        // A Mac whose built-in display (1) is closed, leaving only an
        // external one (3); the agent marks the edge's display as gone (0).
        let link = |local_monitor, remote_monitor, revision| LinkStatus {
            peer: Some(0),
            connected: true,
            name: "Mac".into(),
            address: "192.0.2.5:47843".into(),
            os: "macos".into(),
            remote_monitors: vec![display(3)],
            layout: Some(crate::layout::Report {
                layout: crate::layout::Layout {
                    side: Side::Left,
                    edge_offset: 0.0,
                    local_monitor,
                    remote_monitor,
                },
                revision,
                ready: false,
                pending: false,
                error: String::new(),
            }),
            controlling: false,
            controlled: false,
            rtt_us: None,
            sent_events: 0,
            received_events: 0,
            last_error: String::new(),
        };
        let local = [display(7)];
        let gone = |link: &LinkStatus| gone_displays(Some(link), &local);
        assert_eq!(
            gone(&link(Some(7), Some(0), 9)),
            Gone {
                local: false,
                remote: true
            }
        );
        assert_eq!(
            gone(&link(Some(1), Some(3), 9)),
            Gone {
                local: true,
                remote: false
            }
        );
        // The main display, or the one chosen, is still there.
        assert_eq!(gone(&link(None, Some(3), 9)), Gone::default());
        // Not settled yet, or not connected: nothing to check.
        assert_eq!(gone(&link(Some(7), Some(0), 0)), Gone::default());
        let offline = LinkStatus {
            connected: false,
            ..link(Some(7), Some(0), 9)
        };
        assert_eq!(gone(&offline), Gone::default());
        assert_eq!(gone_displays(None, &local), Gone::default());
    }
}
