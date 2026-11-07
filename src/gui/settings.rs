//! Preferences that apply to every connection.
use super::{App, theme, widgets};
use crate::config::Config;
use eframe::egui::{Align, Layout, RichText, Ui};
use widgets::Kind;

impl App {
    pub(super) fn settings_page(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let controlled = self.config.peers.is_empty();

        let controlled_with_own_mouse = controlled && !self.config.controller_name.is_empty();
        let controls_others = !self.config.peers.is_empty();
        if cfg!(windows) || controlled_with_own_mouse || controls_others {
            widgets::section(ui, "Pointer", None);
            widgets::list(ui, |list| {
                if controls_others || controlled_with_own_mouse {
                    list.row(
                        "Keep screen corners on this computer",
                        Some("The pointer doesn't switch computers at the ends of a shared edge, so reaching for a window's close button stays here."),
                        |ui| widgets::toggle(ui, &mut self.draft.keep_corners),
                    );
                }
                if cfg!(windows) {
                    list.row(
                        "Switch with the keyboard",
                        Some("Ctrl + Alt + Shift and an arrow key move the pointer to the computer on that side, and back. Turn this off if another app uses those keys, such as column selection in VS Code."),
                        |ui| widgets::toggle(ui, &mut self.draft.switch_shortcut),
                    );
                    list.row(
                        "Show the pointer without a mouse",
                        Some("Windows hides the pointer on a PC with no mouse plugged in. Keep it visible while it's controlled."),
                        |ui| widgets::toggle(ui, &mut self.draft.cursor_without_mouse),
                    );
                }
                if controlled_with_own_mouse {
                    list.row(
                        "Use this computer's own mouse here only",
                        Some("Never move this computer's pointer onto the computer that controls it."),
                        |ui| widgets::toggle(ui, &mut self.draft.receive_only),
                    );
                }
            });
            ui.add_space(24.0);
        }

        if self.mac_involved() {
            widgets::section(ui, "Keyboard", Some("Shortcuts between Mac and Windows."));
            widgets::list(ui, |list| {
                list.row(
                    "Swap Command and Control",
                    Some("Command shortcuts on a Mac keyboard work as Control shortcuts on Windows, and the other way around."),
                    |ui| widgets::toggle(ui, &mut self.draft.map_mac_command_to_ctrl),
                );
                list.row(
                    "Match app switching",
                    Some("Command-Tab and Alt-Tab switch apps on either system."),
                    |ui| widgets::toggle(ui, &mut self.draft.map_app_switch_shortcut),
                );
            });
            ui.add_space(24.0);
        }

        widgets::section(
            ui,
            "Clipboard",
            Some("Copy on one computer and paste on another, into any folder or app."),
        );
        widgets::list(ui, |list| {
            list.row("Text", None, |ui| {
                widgets::toggle(ui, &mut self.draft.clipboard)
            });
            list.row("Images", None, |ui| {
                widgets::toggle(ui, &mut self.draft.images)
            });
            list.row("Files and folders", None, |ui| {
                widgets::toggle(ui, &mut self.draft.files)
            });
        });
        ui.add_space(24.0);

        widgets::section(ui, "Sleep", None);
        widgets::list(ui, |list| {
            list.row(
                "Keep this computer awake",
                Some(if cfg!(target_os = "macos") {
                    "While sharing is on, this Mac doesn't go to sleep on its own, so the pointer can always reach it. A laptop still sleeps when you close its lid."
                } else {
                    "While sharing is on, this computer doesn't go to sleep on its own, so the pointer can always reach it."
                }),
                |ui| widgets::toggle(ui, &mut self.draft.keep_awake),
            );
            if self.draft.keep_awake {
                list.row(
                    "Keep the screen on",
                    Some(if cfg!(target_os = "macos") {
                        "The screen stays on too, so it doesn't lock when it would have turned off. Otherwise it turns off as usual and wakes when the pointer arrives."
                    } else {
                        "The screen stays on too, so it doesn't lock when it would have turned off. Otherwise it turns off as usual."
                    }),
                    |ui| widgets::toggle(ui, &mut self.draft.keep_screen_on),
                );
            }
            if controls_others {
                list.row(
                    "Wake computers from the edge",
                    Some("Push the pointer against the edge of a computer that's asleep to wake it over the network. That computer must be set up for Wake-on-LAN."),
                    |ui| widgets::toggle(ui, &mut self.draft.wake_at_edge),
                );
            }
        });
        ui.add_space(24.0);

        let changed = preferences_changed(&self.config, &self.draft);
        if changed {
            ui.horizontal(|ui| {
                let enabled = !self.busy() && !self.remote_active();
                if widgets::button_enabled(ui, "Save changes", Kind::Primary, enabled).clicked() {
                    let mut config = self.config.clone();
                    copy_preferences(&self.draft, &mut config);
                    self.save(config, "Settings saved");
                }
                if widgets::button(ui, "Discard", Kind::Secondary).clicked() {
                    copy_preferences(&self.config.clone(), &mut self.draft);
                }
                widgets::muted(ui, "Sharing restarts briefly to apply.");
            });
            ui.add_space(24.0);
        }

        widgets::section(ui, "About", None);
        widgets::list(ui, |list| {
            list.row(
                "Start automatically",
                Some(if cfg!(windows) {
                    "Set up by the installer to start when you sign in."
                } else {
                    "Seam starts in the background when you log in."
                }),
                |ui| {
                    let on = *self
                        .starts_at_login
                        .get_or_insert_with(crate::service::starts_at_login);
                    ui.label(RichText::new(if on { "On" } else { "Off" }).color(if on {
                        p.text
                    } else {
                        p.muted
                    }));
                },
            );
            list.row(
                "Troubleshooting",
                Some("The log records connections and errors on this computer."),
                |ui| {
                    if widgets::button(ui, "Open log folder", Kind::Secondary).clicked() {
                        open_folder();
                    }
                },
            );
            list.custom(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Seam")
                            .font(theme::strong(13.0))
                            .color(p.text),
                    );
                    widgets::muted(ui, format!("Version {}", env!("CARGO_PKG_VERSION")));
                    let repository = env!("CARGO_PKG_REPOSITORY");
                    if !repository.is_empty() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.hyperlink_to(
                                RichText::new("Source code").font(theme::small()),
                                repository,
                            );
                        });
                    }
                });
            });
        });
    }
}

impl App {
    /// Mac keyboard options matter only when this computer or one it works
    /// with is a Mac.
    fn mac_involved(&self) -> bool {
        cfg!(target_os = "macos")
            || self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.links.iter().any(|link| link.os == "macos")
                    || snapshot.nearby.iter().any(|peer| peer.os == "macos")
            })
    }
}

fn preferences_changed(saved: &Config, draft: &Config) -> bool {
    let mut copy = saved.clone();
    copy_preferences(draft, &mut copy);
    copy != *saved
}

fn copy_preferences(from: &Config, to: &mut Config) {
    to.switch_shortcut = from.switch_shortcut;
    to.keep_corners = from.keep_corners;
    to.cursor_without_mouse = from.cursor_without_mouse;
    to.receive_only = from.receive_only;
    to.map_mac_command_to_ctrl = from.map_mac_command_to_ctrl;
    to.map_app_switch_shortcut = from.map_app_switch_shortcut;
    to.clipboard = from.clipboard;
    to.images = from.images;
    to.files = from.files;
    to.keep_awake = from.keep_awake;
    to.keep_screen_on = from.keep_screen_on;
    to.wake_at_edge = from.wake_at_edge;
}

fn open_folder() {
    let Ok(path) = crate::config::config_dir() else {
        return;
    };
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(path)
        .spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
