//! Small building blocks shared by every page, so spacing and states match.
use super::theme::{self, Palette, RADIUS};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Frame, Layout, Margin, Response, RichText, Sense, Stroke,
    Ui, Vec2, WidgetText,
};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Quiet,
}

pub fn button(ui: &mut Ui, label: &str, kind: Kind) -> Response {
    button_enabled(ui, label, kind, true)
}

pub fn button_enabled(ui: &mut Ui, label: &str, kind: Kind, enabled: bool) -> Response {
    let p = theme::palette(ui);
    let (fill, stroke, text) = match kind {
        Kind::Primary => (p.accent, Stroke::NONE, p.accent_text),
        Kind::Secondary => (p.surface, Stroke::new(1.0_f32, p.border), p.text),
        Kind::Danger => (p.surface, Stroke::new(1.0_f32, p.border), p.danger),
        Kind::Quiet => (Color32::TRANSPARENT, Stroke::NONE, p.accent),
    };
    let label = RichText::new(label)
        .font(theme::strong(13.0))
        .color(if enabled { text } else { p.faint });
    ui.add_enabled(
        enabled,
        egui::Button::new(label)
            .fill(if enabled || kind == Kind::Quiet {
                fill
            } else {
                p.sunken
            })
            .stroke(stroke)
            .corner_radius(RADIUS)
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// A card made of rows separated by hairlines, like a settings list.
pub fn list<R>(ui: &mut Ui, add: impl FnOnce(&mut List) -> R) -> R {
    let p = theme::palette(ui);
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(18, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(&mut List { ui, rows: 0 })
        })
        .inner
}

pub struct List<'a> {
    pub ui: &'a mut Ui,
    rows: usize,
}

impl List<'_> {
    /// A row with a title and optional description on the left and a control
    /// aligned to the right edge.
    pub fn row<R>(
        &mut self,
        title: impl Into<WidgetText>,
        description: Option<&str>,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.separator();
        let p = theme::palette(self.ui);
        let mut result = None;
        Frame::new()
            .inner_margin(Margin::symmetric(0, 12))
            .show(self.ui, |ui| {
                ui.horizontal(|ui| {
                    let right = 260.0_f32.min(ui.available_width() * 0.45);
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width() - right - 12.0, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            ui.label(title);
                            if let Some(description) = description {
                                ui.label(
                                    RichText::new(description)
                                        .font(theme::small())
                                        .color(p.muted),
                                );
                            }
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        result = Some(control(ui));
                    });
                });
            });
        result.unwrap()
    }

    /// A free-form row spanning the full width.
    pub fn custom<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> R {
        self.separator();
        Frame::new()
            .inner_margin(Margin::symmetric(0, 12))
            .show(self.ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui)
            })
            .inner
    }

    fn separator(&mut self) {
        if self.rows > 0 {
            let p = theme::palette(self.ui);
            let width = self.ui.available_width();
            let (rect, _) = self
                .ui
                .allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
            self.ui.painter().hline(
                rect.x_range(),
                rect.center().y,
                Stroke::new(1.0_f32, p.border),
            );
        }
        self.rows += 1;
    }
}

/// A heading above a card, with optional supporting text.
pub fn section(ui: &mut Ui, title: &str, detail: Option<&str>) {
    let p = theme::palette(ui);
    ui.add_space(4.0);
    ui.label(RichText::new(title).font(theme::strong(14.0)).color(p.text));
    if let Some(detail) = detail {
        ui.add_space(2.0);
        ui.label(RichText::new(detail).font(theme::small()).color(p.muted));
    }
    ui.add_space(8.0);
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = theme::palette(ui);
    ui.label(RichText::new(text).font(theme::small()).color(p.muted))
}

/// A keyboard key, as shown in a shortcut hint.
#[cfg(windows)]
pub fn keycap(ui: &mut Ui, label: &str) {
    let p = theme::palette(ui);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::small(), p.text);
    let width = (galley.size().x + 14.0).max(24.0);
    let rect = keycap_frame(ui, width);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, p.text);
}

/// An arrow key pointing toward `side`.
#[cfg(windows)]
pub fn arrow_keycap(ui: &mut Ui, side: crate::config::Side) {
    use crate::config::Side;
    let p = theme::palette(ui);
    let rect = keycap_frame(ui, 24.0);
    let direction = match side {
        Side::Left => Vec2::new(-1.0, 0.0),
        Side::Right => Vec2::new(1.0, 0.0),
        Side::Top => Vec2::new(0.0, -1.0),
        Side::Bottom => Vec2::new(0.0, 1.0),
    };
    super::glyphs::arrow(ui.painter(), rect.center(), 11.0, direction, p.text);
}

/// The key's face with a deeper bottom edge, like a physical key.
#[cfg(windows)]
fn keycap_frame(ui: &mut Ui, width: f32) -> egui::Rect {
    let p = theme::palette(ui);
    let (outer, _) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(outer, CornerRadius::same(6), p.border);
    let face = egui::Rect::from_min_max(outer.min, outer.max - Vec2::new(0.0, 2.0));
    painter.rect(
        face,
        CornerRadius::same(6),
        p.surface,
        Stroke::new(1.0_f32, p.border),
        egui::StrokeKind::Inside,
    );
    face
}

/// A busy indicator redrawn about 30 times a second. egui's own spinner
/// redraws at the display's full refresh rate for as long as it is shown.
pub fn spinner(ui: &mut Ui) {
    let p = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
    let time = ui.input(|input| input.time);
    super::glyphs::spinner(ui.painter(), rect.center(), 6.0, time, p.muted);
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
}

pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    toggle_enabled(ui, on, true)
}

pub fn toggle_enabled(ui: &mut Ui, on: &mut bool, enabled: bool) -> Response {
    let p = theme::palette(ui);
    let size = Vec2::new(36.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let track = if enabled {
        lerp(p.border, p.accent, t)
    } else {
        p.sunken
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), track);
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    painter.circle_filled(egui::pos2(x, rect.center().y), 8.0, Color32::WHITE);
    if response.hovered() && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn text_input(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    let p = theme::palette(ui);
    Frame::new()
        .fill(p.sunken)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(10, 0))
        .show(ui, |ui| {
            ui.add_sized(
                [width - 22.0, 30.0],
                egui::TextEdit::singleline(value)
                    .hint_text(RichText::new(hint).color(p.faint))
                    .frame(false)
                    .margin(Margin::symmetric(0, 6))
                    .font(theme::body()),
            )
        })
        .inner
}

#[derive(Clone, Copy)]
pub enum Tone {
    Info,
    Warning,
    Error,
}

/// A one-line notice above the page content. Returns `true` when its action
/// was clicked.
pub fn notice(ui: &mut Ui, tone: Tone, text: &str, action: Option<&str>) -> bool {
    let p = theme::palette(ui);
    let (color, mark) = match tone {
        Tone::Info => (p.accent, "i"),
        Tone::Warning => (p.warning, "!"),
        Tone::Error => (p.danger, "!"),
    };
    let mut clicked = false;
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin {
            left: 12,
            right: 6,
            top: 4,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                let (badge, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                super::glyphs::badge(ui.painter(), badge.center(), 8.0, color, mark);
                ui.add(egui::Label::new(RichText::new(text).color(p.text)).wrap());
                if let Some(action) = action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clicked = button(ui, action, Kind::Quiet).clicked();
                    });
                }
            });
        });
    ui.add_space(16.0);
    clicked
}

/// Width of one choice in a segmented control.
pub const SEGMENT_WIDTH: f32 = 96.0;

/// A segmented control; returns the newly selected index when it changes.
pub fn segmented(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let p = theme::palette(ui);
    let segment = Vec2::new(SEGMENT_WIDTH, 26.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(segment.x * labels.len() as f32 + 4.0, segment.y + 4.0),
        Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(7), p.sunken);
    let mut changed = None;
    for (index, label) in labels.iter().enumerate() {
        let cell = egui::Rect::from_min_size(
            rect.min + Vec2::new(2.0 + segment.x * index as f32, 2.0),
            segment,
        );
        let response = ui.interact(cell, ui.id().with(("segment", index)), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                true,
                index == selected,
                *label,
            )
        });
        if index == selected {
            ui.painter().rect(
                cell,
                CornerRadius::same(5),
                p.surface,
                Stroke::new(1.0_f32, p.border),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        ui.painter().text(
            cell.center(),
            egui::Align2::CENTER_CENTER,
            *label,
            if index == selected {
                theme::strong(12.5)
            } else {
                theme::small()
            },
            if index == selected { p.text } else { p.muted },
        );
        if response.clicked() && index != selected {
            changed = Some(index);
        }
    }
    changed
}

pub fn os_name(os: &str) -> &'static str {
    match os {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "Computer",
    }
}

pub fn latency(rtt_us: Option<u64>) -> String {
    match rtt_us {
        Some(us) if us < 1000 => "<1 ms".into(),
        Some(us) => format!("{} ms", us / 1000),
        None => "—".into(),
    }
}

pub fn state_color(p: &Palette, connected: bool, ready: bool) -> Color32 {
    match (connected, ready) {
        (true, true) => p.success,
        (true, false) => p.warning,
        _ => p.faint,
    }
}
