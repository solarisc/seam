//! This computer's identity and its pairings.
use super::{App, screens::local_addresses, theme, widgets};
use eframe::egui::{RichText, Ui};
use widgets::Kind;

impl App {
    pub(super) fn computers_page(&mut self, ui: &mut Ui) {
        self.status_banners(ui);
        let p = theme::palette(ui);

        widgets::section(ui, "This computer", None);
        let addresses = local_addresses();
        let mut rename = None;
        widgets::list(ui, |list| {
            list.row("Name", Some("Shown on your other computers."), |ui| {
                let response =
                    widgets::text_input(ui, &mut self.draft.name, "Computer name", 220.0);
                let changed = self.draft.name.trim() != self.config.name;
                if changed
                    && !self.draft.name.trim().is_empty()
                    && (response.lost_focus()
                        || ui.input(|i| i.key_pressed(eframe::egui::Key::Enter)))
                {
                    rename = Some(self.draft.name.trim().to_string());
                }
            });
            list.row(
                "Network address",
                Some("Use this when a computer isn't found automatically."),
                |ui| {
                    let response = ui.label(
                        RichText::new(addresses.first().map_or("Not connected", String::as_str))
                            .color(p.text),
                    );
                    if addresses.len() > 1 {
                        response.on_hover_text(format!("Also: {}", addresses[1..].join(", ")));
                    }
                },
            );
        });
        if let Some(name) = rename {
            let mut config = self.config.clone();
            config.name = name;
            self.save(config, "Name updated");
        }
        ui.add_space(24.0);

        if !self.config.controller_name.is_empty() && self.config.peers.is_empty() {
            widgets::section(
                ui,
                "Controlled by",
                Some("This computer accepts the keyboard and mouse of the computer below."),
            );
            let mut unpair = false;
            let controller = self.config.controller_name.clone();
            widgets::list(ui, |list| {
                list.row(
                    RichText::new(&controller)
                        .font(theme::strong(13.5))
                        .color(p.text),
                    Some("Unpairing stops it from connecting until it's paired again."),
                    |ui| {
                        unpair = widgets::button_enabled(ui, "Unpair", Kind::Danger, !self.busy())
                            .clicked();
                    },
                );
            });
            if unpair {
                self.unpair_controller();
            }
            ui.add_space(24.0);
        }

        if !self.config.peers.is_empty() {
            widgets::section(
                ui,
                "Controlled from this computer",
                Some("Up to four computers, one on each screen edge."),
            );
            let mut remove = None;
            widgets::list(ui, |list| {
                for (index, peer) in self.config.peers.iter().enumerate() {
                    let detail = format!(
                        "{} · {}",
                        peer.address,
                        match peer.arrangement.side {
                            crate::config::Side::Left => "left of this screen",
                            crate::config::Side::Right => "right of this screen",
                            crate::config::Side::Top => "above this screen",
                            crate::config::Side::Bottom => "below this screen",
                        }
                    );
                    list.row(
                        RichText::new(peer.label())
                            .font(theme::strong(13.5))
                            .color(p.text),
                        Some(&detail),
                        |ui| {
                            if widgets::button_enabled(ui, "Remove", Kind::Danger, !self.busy())
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        },
                    );
                }
            });
            if remove.is_some() {
                self.confirm_remove = remove;
            }
            ui.add_space(24.0);
        }

        widgets::section(
            ui,
            "Add a computer",
            Some(
                "Choose a computer to control with this keyboard and mouse. You'll confirm a code on both screens.",
            ),
        );
        self.nearby_list(ui);
    }
}
