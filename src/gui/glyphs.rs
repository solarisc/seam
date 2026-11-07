//! Vector drawings: the logo and the small glyphs used across the window.
//! Painted rather than loaded as images, so they stay sharp at any scale.
use eframe::egui::{
    Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, Vec2, epaint::PathShape, vec2,
};

const TILE: Color32 = Color32::from_rgb(0x46, 0x72, 0xE0);
const POINTER: Color32 = Color32::from_rgb(0x10, 0x20, 0x4A);

/// The app icon, drawn from the same geometry as `assets/icon/seam.svg`
/// (a 256-unit square). Small sizes use the simplified drawing, as the icon
/// files do, because the pointer turns into noise below about 32 pixels.
pub fn logo(painter: &Painter, rect: Rect) {
    let s = rect.width() / 256.0;
    let at = |x: f32, y: f32| rect.min + vec2(x, y) * s;
    let area = |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(at(x, y), vec2(w, h) * s);
    let radius = |r: f32| CornerRadius::same((r * s).round().clamp(0.0, 255.0) as u8);
    let white = Color32::WHITE;
    let dim = Color32::from_white_alpha(140);
    if rect.width() <= 32.0 {
        painter.rect_filled(area(0.0, 0.0, 256.0, 256.0), radius(56.0), TILE);
        for (x, color) in [(20.0, white), (134.0, dim)] {
            painter.rect_filled(area(x, 64.0, 102.0, 80.0), radius(14.0), color);
            painter.rect_filled(area(x + 30.0, 160.0, 42.0, 18.0), radius(6.0), color);
        }
        return;
    }
    painter.rect_filled(area(8.0, 8.0, 240.0, 240.0), radius(56.0), TILE);
    for (x, color) in [(30.0, white), (136.0, dim)] {
        painter.rect_filled(area(x, 80.0, 90.0, 62.0), radius(10.0), color);
        painter.rect_filled(area(x + 40.0, 140.0, 10.0, 20.0), radius(0.0), color);
        painter.rect_filled(area(x + 22.0, 158.0, 46.0, 8.0), radius(4.0), color);
    }
    pointer(painter, at(121.0, 92.0), 51.0 * s, POINTER, white);
}

/// The arrow pointer with its tip at `tip`, `height` tall, outlined so it
/// reads on any background.
pub fn pointer(painter: &Painter, tip: Pos2, height: f32, fill: Color32, outline: Color32) {
    // The icon's arrow, relative to its tip, in units of its 51-unit height.
    const OUTLINE: [(f32, f32); 7] = [
        (0.0, 0.0),
        (0.0, 44.0),
        (10.0, 35.0),
        (17.0, 51.0),
        (26.0, 47.0),
        (19.0, 31.0),
        (33.0, 31.0),
    ];
    let scale = height / 51.0;
    let point = |(x, y): (f32, f32)| tip + vec2(x, y) * scale;
    let p: Vec<Pos2> = OUTLINE.iter().copied().map(point).collect();
    // Fill as convex pieces: the head as a fan from the tip, then the tail.
    for piece in [
        vec![p[0], p[1], p[2]],
        vec![p[0], p[2], p[5]],
        vec![p[0], p[5], p[6]],
        vec![p[2], p[3], p[4], p[5]],
    ] {
        painter.add(Shape::convex_polygon(piece, fill, Stroke::NONE));
    }
    painter.add(PathShape::closed_line(
        p,
        Stroke::new((height * 0.12).max(1.0), outline),
    ));
}

/// A filled circle with a single character, for notices.
pub fn badge(painter: &Painter, center: Pos2, radius: f32, color: Color32, mark: &str) {
    painter.circle_filled(center, radius, color);
    painter.text(
        center + vec2(0.0, 0.5),
        eframe::egui::Align2::CENTER_CENTER,
        mark,
        super::theme::strong(radius * 1.4),
        Color32::WHITE,
    );
}

/// A rotating arc. Callers repaint at a modest rate while it is shown.
pub fn spinner(painter: &Painter, center: Pos2, radius: f32, time: f64, color: Color32) {
    let start = (time * 4.0) as f32;
    let points: Vec<Pos2> = (0..=24)
        .map(|i| {
            let angle = start + i as f32 / 24.0 * std::f32::consts::PI * 1.5;
            center + radius * Vec2::angled(angle)
        })
        .collect();
    painter.add(PathShape::line(points, Stroke::new(radius * 0.28, color)));
}

/// An arrow key cap's arrow, pointing toward `direction`.
#[cfg(windows)]
pub fn arrow(painter: &Painter, center: Pos2, size: f32, direction: Vec2, color: Color32) {
    let forward = direction.normalized() * size * 0.5;
    let side = forward.rot90() * 0.75;
    let tip = center + forward;
    let back = center - forward * 0.9;
    let stroke = Stroke::new((size * 0.14).max(1.2), color);
    painter.line_segment([back, tip], stroke);
    painter.add(PathShape::line(
        vec![tip - forward * 0.7 + side, tip, tip - forward * 0.7 - side],
        stroke,
    ));
}
