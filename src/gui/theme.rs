//! Colors, type and spacing. The window follows the system light/dark setting.
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    Theme, Vec2, style::WidgetVisuals,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub window: Color32,
    /// The toolbar across the top of the window.
    pub chrome: Color32,
    pub surface: Color32,
    pub sunken: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    /// Display frames in the arrangement.
    pub bezel: Color32,
    /// This computer's screens in the arrangement.
    pub screen: Color32,
}

const LIGHT: Palette = Palette {
    window: Color32::from_rgb(0xF7, 0xF7, 0xF6),
    chrome: Color32::from_rgb(0xEF, 0xEF, 0xEE),
    surface: Color32::WHITE,
    sunken: Color32::from_rgb(0xEC, 0xEC, 0xEA),
    border: Color32::from_rgb(0xDD, 0xDD, 0xDA),
    text: Color32::from_rgb(0x1F, 0x1F, 0x1E),
    muted: Color32::from_rgb(0x6B, 0x6B, 0x68),
    faint: Color32::from_rgb(0x9C, 0x9C, 0x98),
    accent: Color32::from_rgb(0x2F, 0x6B, 0xDE),
    accent_text: Color32::WHITE,
    accent_soft: Color32::from_rgb(0xDD, 0xE7, 0xF8),
    success: Color32::from_rgb(0x2E, 0x9A, 0x5C),
    warning: Color32::from_rgb(0xB0, 0x6E, 0x0C),
    danger: Color32::from_rgb(0xC9, 0x3C, 0x37),
    danger_soft: Color32::from_rgb(0xF8, 0xE3, 0xE1),
    bezel: Color32::from_rgb(0x3A, 0x3A, 0x39),
    screen: Color32::from_rgb(0xF4, 0xF3, 0xF0),
};

const DARK: Palette = Palette {
    window: Color32::from_rgb(0x1C, 0x1C, 0x1C),
    chrome: Color32::from_rgb(0x24, 0x24, 0x24),
    surface: Color32::from_rgb(0x26, 0x26, 0x26),
    sunken: Color32::from_rgb(0x15, 0x15, 0x15),
    border: Color32::from_rgb(0x36, 0x36, 0x36),
    text: Color32::from_rgb(0xEE, 0xEE, 0xEC),
    muted: Color32::from_rgb(0xA3, 0xA3, 0xA0),
    faint: Color32::from_rgb(0x6E, 0x6E, 0x6B),
    accent: Color32::from_rgb(0x5B, 0x8F, 0xF0),
    accent_text: Color32::WHITE,
    accent_soft: Color32::from_rgb(0x24, 0x31, 0x4A),
    success: Color32::from_rgb(0x4C, 0xC0, 0x7F),
    warning: Color32::from_rgb(0xE8, 0xA8, 0x45),
    danger: Color32::from_rgb(0xF0, 0x6B, 0x63),
    danger_soft: Color32::from_rgb(0x40, 0x23, 0x22),
    bezel: Color32::from_rgb(0x0B, 0x0B, 0x0B),
    screen: Color32::from_rgb(0x34, 0x34, 0x33),
};

pub fn palette(ui: &egui::Ui) -> Palette {
    palette_for(ui.ctx())
}

pub fn palette_for(ctx: &egui::Context) -> Palette {
    if ctx.style().visuals.dark_mode {
        DARK
    } else {
        LIGHT
    }
}

pub const RADIUS: u8 = 8;
/// Content stays readable on wide windows.
pub const CONTENT_WIDTH: f32 = 780.0;

pub fn body() -> FontId {
    FontId::new(13.5, FontFamily::Proportional)
}

pub fn small() -> FontId {
    FontId::new(12.0, FontFamily::Proportional)
}

pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("strong".into()))
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    // egui's bundled fonts stay as fallbacks for scripts Inter does not cover.
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .unwrap()
        .insert(0, "inter".into());
    let mut strong = vec!["inter-semibold".to_string()];
    strong.extend(fallback);
    fonts
        .families
        .insert(FontFamily::Name("strong".into()), strong);
    ctx.set_fonts(fonts);
    ctx.options_mut(|options| options.theme_preference = egui::ThemePreference::System);
    ctx.set_style_of(Theme::Light, style(Theme::Light));
    ctx.set_style_of(Theme::Dark, style(Theme::Dark));
}

fn style(theme: Theme) -> egui::Style {
    let p = if theme == Theme::Dark { DARK } else { LIGHT };
    let mut style = egui::Style {
        visuals: if theme == Theme::Dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        },
        ..Default::default()
    };
    style.text_styles = [
        (egui::TextStyle::Body, body()),
        (egui::TextStyle::Button, body()),
        (egui::TextStyle::Small, small()),
        (egui::TextStyle::Heading, strong(20.0)),
        (
            egui::TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
    ]
    .into();
    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(8.0, 8.0);
    spacing.button_padding = Vec2::new(14.0, 7.0);
    spacing.interact_size = Vec2::new(32.0, 32.0);
    spacing.slider_width = 220.0;
    spacing.combo_width = 200.0;
    spacing.window_margin = Margin::same(20);
    spacing.menu_margin = Margin::same(6);

    let v = &mut style.visuals;
    v.override_text_color = None;
    v.panel_fill = p.window;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if theme == Theme::Dark { 120 } else { 40 }),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if theme == Theme::Dark { 100 } else { 28 }),
    };
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.sunken;
    v.code_bg_color = p.sunken;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.text_cursor.stroke = Stroke::new(1.5_f32, p.accent);
    let widget = |fill: Color32, stroke: Color32, text: Color32| WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: Stroke::new(1.0_f32, stroke),
        fg_stroke: Stroke::new(1.0_f32, text),
        corner_radius: CornerRadius::same(RADIUS),
        expansion: 0.0,
    };
    let hover = if theme == Theme::Dark {
        Color32::from_rgb(0x2C, 0x2E, 0x33)
    } else {
        Color32::from_rgb(0xF3, 0xF4, 0xF6)
    };
    v.widgets.noninteractive = widget(p.surface, p.border, p.text);
    v.widgets.inactive = widget(p.surface, p.border, p.text);
    v.widgets.hovered = widget(hover, p.border, p.text);
    v.widgets.active = widget(p.sunken, p.accent, p.text);
    v.widgets.open = widget(p.surface, p.accent, p.text);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;
    v.indent_has_left_vline = false;
    style
}
