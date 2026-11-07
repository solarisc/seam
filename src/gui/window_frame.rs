//! Seam draws its own title bar, as the toolbar across the top of the window.
//!
//! On Windows the native frame is turned off. Windows still provides the drop
//! shadow and, on Windows 11, rounded corners; Seam draws the minimize,
//! maximize and close buttons and handles moving and resizing. On macOS the
//! window keeps its own buttons over a transparent title bar.
use eframe::egui::{self, Rect, Sense, Ui, ViewportBuilder, ViewportCommand};
#[cfg(windows)]
use {
    super::theme,
    eframe::egui::{
        Color32, CornerRadius, CursorIcon, Pos2, ResizeDirection, Stroke, Vec2, pos2, vec2,
    },
};

/// Space the toolbar leaves on the left for the system's window buttons.
pub const LEADING: f32 = if cfg!(target_os = "macos") { 76.0 } else { 0.0 };

/// Width of one window button drawn by Seam (Windows' own size).
#[cfg(windows)]
const BUTTON_WIDTH: f32 = 46.0;

/// How far from the window's edge the pointer can start a resize.
#[cfg(windows)]
const RESIZE_BAND: f32 = 5.0;

pub fn configure(builder: ViewportBuilder) -> ViewportBuilder {
    #[cfg(windows)]
    let builder = builder.with_decorations(false);
    #[cfg(target_os = "macos")]
    let builder = builder
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    builder
}

/// Ask Windows for the shadow and rounded corners a framed window gets.
#[cfg(windows)]
pub fn native_outline(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        Graphics::Dwm::{
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmExtendFrameIntoClientArea,
            DwmSetWindowAttribute,
        },
        UI::Controls::MARGINS,
    };
    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(window) = handle.as_raw() else {
        return;
    };
    let hwnd = window.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    // A one-pixel frame is enough for the compositor to draw the shadow.
    let margins = MARGINS {
        cxLeftWidth: 1,
        cxRightWidth: 1,
        cyTopHeight: 1,
        cyBottomHeight: 1,
    };
    let corners = DWMWCP_ROUND;
    let rounded = unsafe {
        DwmExtendFrameIntoClientArea(hwnd, &margins);
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&raw const corners).cast(),
            size_of_val(&corners) as u32,
        )
    } >= 0;
    // Windows 11 draws a border around rounded windows; earlier versions
    // refuse the setting and leave the edge to Seam.
    OUTLINE.store(!rounded, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(windows)]
static OUTLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// A hairline around the window where Windows draws none, so a dark window
/// doesn't melt into a dark desktop.
#[cfg(windows)]
pub fn outline(ctx: &egui::Context) {
    let maximized = ctx.input(|input| input.viewport().maximized.unwrap_or(false));
    if maximized || !OUTLINE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let p = theme::palette_for(ctx);
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("window-outline"),
    ))
    .rect_stroke(
        ctx.viewport_rect(),
        CornerRadius::ZERO,
        Stroke::new(1.0_f32, p.border),
        egui::StrokeKind::Inside,
    );
}

/// Lets the pointer resize the window from its edges, which a window without
/// a native frame does not do on its own.
#[cfg(windows)]
pub fn resize_edges(ctx: &egui::Context) {
    let (maximized, screen, pointer, pressed, down) = ctx.input(|input| {
        (
            input.viewport().maximized.unwrap_or(false),
            input.viewport_rect(),
            input.pointer.hover_pos(),
            input.pointer.primary_pressed(),
            input.pointer.any_down(),
        )
    });
    // Runs after the widgets, which have seen the flag for this release.
    if !down {
        ctx.data_mut(|data| data.remove::<bool>(resize_id()));
    }
    if maximized {
        return;
    }
    let Some(pointer) = pointer else {
        return;
    };
    let Some(direction) = edge(screen, pointer) else {
        return;
    };
    ctx.set_cursor_icon(match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
    });
    if pressed {
        ctx.data_mut(|data| data.insert_temp(resize_id(), true));
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
    }
}

fn resize_id() -> egui::Id {
    egui::Id::new("seam-window-resize")
}

/// Whether the press now held or ending resizes the window from its edge.
/// Windows' resize loop keeps the pointer's movement from egui, which would
/// take the release for a click on the button or title bar under the edge.
fn resizing(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(resize_id()).unwrap_or(false))
}

#[cfg(windows)]
fn edge(screen: Rect, pointer: Pos2) -> Option<ResizeDirection> {
    let left = pointer.x <= screen.left() + RESIZE_BAND;
    let right = pointer.x >= screen.right() - RESIZE_BAND;
    let top = pointer.y <= screen.top() + RESIZE_BAND;
    let bottom = pointer.y >= screen.bottom() - RESIZE_BAND;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => ResizeDirection::NorthWest,
        (_, true, true, _) => ResizeDirection::NorthEast,
        (true, _, _, true) => ResizeDirection::SouthWest,
        (_, true, _, true) => ResizeDirection::SouthEast,
        (true, ..) => ResizeDirection::West,
        (_, true, ..) => ResizeDirection::East,
        (_, _, true, _) => ResizeDirection::North,
        (.., true) => ResizeDirection::South,
        _ => return None,
    })
}

/// Makes the toolbar's empty space behave like a title bar: drag to move,
/// double-click to maximize. Call before adding the toolbar's widgets, which
/// then take the clicks meant for them.
pub fn title_bar(ui: &Ui, rect: Rect) {
    let response = ui.interact(rect, ui.id().with("title-bar"), Sense::click_and_drag());
    if resizing(ui.ctx()) {
        return;
    }
    if response.double_clicked() {
        let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

/// Minimize, maximize or restore, and close, flush with the window's top
/// right corner. Adds them right to left, so call first in a right-to-left
/// layout.
#[cfg(windows)]
pub fn buttons(ui: &mut Ui) {
    let p = theme::palette(ui);
    let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
    let height = ui.max_rect().height();
    let button = |ui: &mut Ui, label: &str, close: bool| {
        let (rect, response) = ui.allocate_exact_size(vec2(BUTTON_WIDTH, height), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
        let hovered = response.hovered();
        let held = response.is_pointer_button_down_on();
        let (fill, ink) = match (close, hovered, held) {
            (true, true, false) => (Color32::from_rgb(0xC4, 0x2B, 0x1C), Color32::WHITE),
            (true, true, true) => (Color32::from_rgb(0xA3, 0x24, 0x18), Color32::WHITE),
            (false, true, false) => (p.text.gamma_multiply(0.08), p.text),
            (false, true, true) => (p.text.gamma_multiply(0.14), p.text),
            _ => (Color32::TRANSPARENT, p.text),
        };
        ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
        (
            rect.center(),
            ink,
            response.clicked() && !resizing(ui.ctx()),
        )
    };
    let stroke = |ink| Stroke::new(1.0_f32, ink);

    let (center, ink, clicked) = button(ui, "Close", true);
    let d = 5.0;
    ui.painter()
        .line_segment([center + vec2(-d, -d), center + vec2(d, d)], stroke(ink));
    ui.painter()
        .line_segment([center + vec2(-d, d), center + vec2(d, -d)], stroke(ink));
    if clicked {
        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
    }

    let (center, ink, clicked) = button(ui, if maximized { "Restore" } else { "Maximize" }, false);
    let square = |c: Pos2| Rect::from_center_size(c, Vec2::splat(9.0));
    if maximized {
        let back = square(center + vec2(1.5, -1.5));
        let front = square(center + vec2(-1.0, 1.0));
        ui.painter()
            .line_segment([back.left_top(), back.right_top()], stroke(ink));
        ui.painter()
            .line_segment([back.right_top(), back.right_bottom()], stroke(ink));
        ui.painter()
            .rect_filled(front, CornerRadius::ZERO, ui.visuals().panel_fill);
        ui.painter().rect_stroke(
            front,
            CornerRadius::ZERO,
            stroke(ink),
            egui::StrokeKind::Inside,
        );
    } else {
        ui.painter().rect_stroke(
            square(center),
            CornerRadius::ZERO,
            stroke(ink),
            egui::StrokeKind::Inside,
        );
    }
    if clicked {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }

    let (center, ink, clicked) = button(ui, "Minimize", false);
    ui.painter().line_segment(
        [pos2(center.x - d, center.y), pos2(center.x + d, center.y)],
        stroke(ink),
    );
    if clicked {
        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn corners_and_sides_map_to_resize_directions() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        assert_eq!(
            edge(screen, pos2(1.0, 1.0)),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            edge(screen, pos2(799.0, 599.0)),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(edge(screen, pos2(400.0, 1.0)), Some(ResizeDirection::North));
        assert_eq!(edge(screen, pos2(1.0, 300.0)), Some(ResizeDirection::West));
        assert_eq!(edge(screen, pos2(400.0, 300.0)), None);
    }
}
