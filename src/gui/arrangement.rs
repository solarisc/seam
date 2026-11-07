//! The screen arrangement: this computer's displays with every other
//! computer placed against one edge. Drag a computer to move it.
use super::theme;
use crate::{
    config::{Arrangement, Side},
    protocol::Monitor,
};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
};

pub struct Computer {
    /// Link key: index into `Config::peers`, or `None` for a controlling computer.
    pub key: Option<usize>,
    pub name: String,
    pub monitors: Vec<Monitor>,
    pub arrangement: Arrangement,
    pub connected: bool,
    /// The pointer is on this computer now.
    pub active: bool,
}

#[derive(Default)]
pub struct Drag {
    key: Option<Option<usize>>,
    delta: Vec2,
}

pub struct Moved {
    pub key: Option<usize>,
    pub side: Side,
    pub offset: f64,
    /// The display of this computer whose edge the other computer now meets.
    pub local_monitor: Option<u32>,
}

/// `true` when another display of this computer covers the chosen edge, so
/// the pointer can never cross there.
pub fn edge_blocked(local: &[Monitor], arrangement: &Arrangement) -> bool {
    select(local, arrangement.local_monitor)
        .is_some_and(|anchor| blocked(local, anchor, arrangement.side))
}

fn blocked(local: &[Monitor], anchor: &Monitor, side: Side) -> bool {
    let edge = rect_of(anchor);
    let strip = match side {
        Side::Left => Rect::from_min_max(
            Pos2::new(edge.left() - 2.0, edge.top()),
            Pos2::new(edge.left(), edge.bottom()),
        ),
        Side::Right => Rect::from_min_max(
            Pos2::new(edge.right(), edge.top()),
            Pos2::new(edge.right() + 2.0, edge.bottom()),
        ),
        Side::Top => Rect::from_min_max(
            Pos2::new(edge.left(), edge.top() - 2.0),
            Pos2::new(edge.right(), edge.top()),
        ),
        Side::Bottom => Rect::from_min_max(
            Pos2::new(edge.left(), edge.bottom()),
            Pos2::new(edge.right(), edge.bottom() + 2.0),
        ),
    };
    local.iter().any(|m| {
        let overlap = rect_of(m).intersect(strip);
        m.id != anchor.id && overlap.width() > 0.5 && overlap.height() > 0.5
    })
}

const PLACEHOLDER: (u32, u32) = (1920, 1080);

fn rect_of(monitor: &Monitor) -> Rect {
    Rect::from_min_size(
        Pos2::new(monitor.x as f32, monitor.y as f32),
        Vec2::new(monitor.width.max(1) as f32, monitor.height.max(1) as f32),
    )
}

fn select(monitors: &[Monitor], id: Option<u32>) -> Option<&Monitor> {
    monitors
        .iter()
        .find(|m| Some(m.id) == id)
        .or_else(|| monitors.iter().find(|m| m.primary))
        .or_else(|| monitors.first())
}

fn placed(anchor: Rect, size: Vec2, side: Side, offset: f64, gap: f32) -> Rect {
    let offset = offset as f32;
    let origin = match side {
        Side::Left => Pos2::new(
            anchor.left() - gap - size.x,
            anchor.top() + anchor.height() * offset,
        ),
        Side::Right => Pos2::new(
            anchor.right() + gap,
            anchor.top() + anchor.height() * offset,
        ),
        Side::Top => Pos2::new(
            anchor.left() + anchor.width() * offset,
            anchor.top() - gap - size.y,
        ),
        Side::Bottom => Pos2::new(
            anchor.left() + anchor.width() * offset,
            anchor.bottom() + gap,
        ),
    };
    Rect::from_min_size(origin, size)
}

/// The edge and alignment closest to where a dragged screen was dropped.
fn nearest(anchor: Rect, moved: Rect) -> (Side, f64) {
    let relative = (moved.center() - anchor.center()) / (anchor.size() + moved.size());
    if relative.x.abs() >= relative.y.abs() {
        (
            if relative.x < 0.0 {
                Side::Left
            } else {
                Side::Right
            },
            ((moved.top() - anchor.top()) / anchor.height()).clamp(-0.9, 0.9) as f64,
        )
    } else {
        (
            if relative.y < 0.0 {
                Side::Top
            } else {
                Side::Bottom
            },
            ((moved.left() - anchor.left()) / anchor.width()).clamp(-0.9, 0.9) as f64,
        )
    }
}

struct Group {
    key: Option<usize>,
    blocked: bool,
    name: String,
    connected: bool,
    active: bool,
    side: Side,
    offset: f64,
    anchor: Rect,
    selected: Rect,
    monitors: Vec<(Rect, String)>,
}

/// Draws this computer's displays and the other computers around them, and
/// returns a computer the person dragged to a new place. `local_active` marks
/// this computer as the one with the pointer.
pub fn show(
    ui: &mut Ui,
    this_name: &str,
    local: &[Monitor],
    computers: &[Computer],
    drag: &mut Drag,
    editable: bool,
    local_active: bool,
) -> Option<Moved> {
    let p = theme::palette(ui);
    let height = 280.0;
    let (canvas, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(canvas);
    painter.rect_filled(canvas, CornerRadius::same(12), p.chrome);
    let fallback = [Monitor {
        id: 0,
        identity: None,
        name: String::new(),
        x: 0,
        y: 0,
        width: PLACEHOLDER.0,
        height: PLACEHOLDER.1,
        primary: true,
    }];
    let local = if local.is_empty() {
        &fallback[..]
    } else {
        local
    };

    // Lay out every computer in this computer's desktop coordinates.
    let groups: Vec<Group> = computers
        .iter()
        .map(|computer| {
            let anchor_monitor = select(local, computer.arrangement.local_monitor).unwrap();
            let anchor = rect_of(anchor_monitor);
            let monitors = if computer.monitors.is_empty() {
                &fallback[..]
            } else {
                &computer.monitors[..]
            };
            let selected = select(monitors, computer.arrangement.remote_monitor).unwrap();
            let gap = anchor.width().min(anchor.height()) * 0.06;
            let target = placed(
                anchor,
                rect_of(selected).size(),
                computer.arrangement.side,
                computer.arrangement.edge_offset,
                gap,
            );
            let shift = target.min - rect_of(selected).min;
            Group {
                key: computer.key,
                blocked: blocked(local, anchor_monitor, computer.arrangement.side),
                name: computer.name.clone(),
                connected: computer.connected,
                active: computer.active,
                side: computer.arrangement.side,
                offset: computer.arrangement.edge_offset,
                anchor,
                selected: target,
                monitors: monitors
                    .iter()
                    .map(|m| (rect_of(m).translate(shift), m.name.clone()))
                    .collect(),
            }
        })
        .collect();
    let bounds = local
        .iter()
        .map(rect_of)
        .chain(
            groups
                .iter()
                .flat_map(|g| g.monitors.iter().map(|(r, _)| *r)),
        )
        .reduce(Rect::union)
        .unwrap();
    let area = canvas.shrink2(Vec2::new(40.0, 40.0));
    let scale = (area.width() / bounds.width())
        .min(area.height() / bounds.height())
        .min(0.18);
    let project = |rect: Rect| {
        Rect::from_center_size(
            area.center() + (rect.center() - bounds.center()) * scale,
            rect.size() * scale,
        )
    };

    let mut moved = None;
    let mut preview = None;
    for group in &groups {
        let hit = group
            .monitors
            .iter()
            .map(|(r, _)| project(*r))
            .reduce(Rect::union)
            .unwrap();
        let id = ui.id().with(("arrangement", group.key));
        let response = ui.interact(
            hit,
            id,
            if editable {
                Sense::drag()
            } else {
                Sense::hover()
            },
        );
        if editable && response.hovered() && drag.key.is_none() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if response.drag_started() {
            *drag = Drag {
                key: Some(group.key),
                delta: Vec2::ZERO,
            };
        }
        if drag.key == Some(group.key) {
            if response.dragged() {
                drag.delta += response.drag_delta();
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            let dropped = project(group.selected).translate(drag.delta);
            // Attach to the nearest outer edge of any of this computer's
            // displays; an edge shared with another display cannot be crossed.
            let (anchor, side, offset) = local
                .iter()
                .filter_map(|monitor| {
                    let area = project(rect_of(monitor));
                    let (side, offset) = nearest(area, dropped);
                    let gap = area.width().min(area.height()) * 0.06;
                    let target = placed(area, dropped.size(), side, offset, gap);
                    (!blocked(local, monitor, side)).then(|| {
                        let distance = (target.center() - dropped.center()).length();
                        (distance, monitor, side, offset)
                    })
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, monitor, side, offset)| (monitor, side, offset))
                .unwrap_or_else(|| {
                    let (side, offset) = nearest(project(group.anchor), dropped);
                    (select(local, None).unwrap(), side, offset)
                });
            let free = !groups.iter().any(|g| g.key != group.key && g.side == side);
            preview = Some((project(rect_of(anchor)), group, side, offset, free));
            if response.drag_stopped() {
                let anchor_changed = rect_of(anchor) != group.anchor;
                if free
                    && (anchor_changed
                        || side != group.side
                        || (offset - group.offset).abs() > 0.01)
                {
                    moved = Some(Moved {
                        key: group.key,
                        side,
                        offset,
                        local_monitor: (anchor.id != 0).then_some(anchor.id),
                    });
                }
                *drag = Drag::default();
            }
        }
    }

    // A display: a dark bezel around the screen area, outlined when it is
    // the screen another computer meets.
    let screen = |rect: Rect, fill: Color32, outline: Option<Color32>| -> Rect {
        let bezel = (rect.height() * 0.025).clamp(1.5, 3.5);
        painter.rect_filled(rect, CornerRadius::same(5), p.bezel);
        let inner = rect.shrink(bezel);
        painter.rect_filled(inner, CornerRadius::same(2), fill);
        if let Some(color) = outline {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(7),
                Stroke::new(1.5_f32, color),
                StrokeKind::Outside,
            );
        }
        inner
    };
    let label = |rect: Rect, title: &str, detail: &str, color: Color32, muted: Color32| {
        if rect.width() < 44.0 || rect.height() < 26.0 {
            return;
        }
        let two_lines = rect.height() >= 58.0 && !detail.is_empty();
        let center = rect.center() - Vec2::new(0.0, if two_lines { 8.0 } else { 0.0 });
        text(&painter, rect, center, title, theme::strong(12.5), color);
        if two_lines {
            text(
                &painter,
                rect,
                rect.center() + Vec2::new(0.0, 10.0),
                detail,
                theme::small(),
                muted,
            );
        }
    };

    // Where each computer's named screen landed, for the pointer highlight.
    let mut highlights: Vec<(egui::Id, Rect, bool)> = Vec::new();

    // This computer's displays.
    for (index, monitor) in local.iter().enumerate() {
        let world = rect_of(monitor);
        let inner = screen(project(world), p.screen, None);
        let named =
            local.len() == 1 || monitor.primary || (index == 0 && !local.iter().any(|m| m.primary));
        let title = if named { this_name } else { "" };
        label(inner, title, &monitor.name, p.text, p.muted);
        if named {
            highlights.push((ui.id().with("pointer-local"), inner, local_active));
        }
    }

    // Other computers, offset while dragging.
    for group in &groups {
        let delta = if drag.key == Some(group.key) {
            drag.delta
        } else {
            Vec2::ZERO
        };
        let (fill, text_color, muted) = if group.blocked {
            (p.danger_soft, p.text, p.muted)
        } else if group.connected {
            (p.accent_soft, p.text, p.muted)
        } else {
            (p.screen, p.muted, p.faint)
        };
        for (world, name) in &group.monitors {
            let rect = project(*world).translate(delta);
            let selected = *world == group.selected;
            let inner = screen(rect, fill, None);
            if selected {
                label(inner, &group.name, name, text_color, muted);
                highlights.push((
                    ui.id().with(("pointer", group.key)),
                    inner,
                    group.active && delta == Vec2::ZERO,
                ));
            }
        }
        if delta == Vec2::ZERO {
            seam(
                &painter,
                project(group.anchor),
                project(group.selected),
                group.side,
                group.offset,
                if group.blocked {
                    p.danger
                } else if group.connected {
                    p.accent
                } else {
                    p.faint
                },
            );
        }
    }

    // The screen the pointer is on: an accent outline and a pointer in the
    // corner, fading across when control moves to another computer.
    for (id, inner, active) in highlights {
        let t = ui.ctx().animate_bool_with_time(id, active, 0.25);
        if t <= 0.0 {
            continue;
        }
        painter.rect_stroke(
            inner.expand(4.0),
            CornerRadius::same(6),
            Stroke::new(2.0_f32, p.accent.gamma_multiply(t)),
            StrokeKind::Outside,
        );
        let height = (inner.height() * 0.28).clamp(10.0, 22.0);
        if inner.width() > height * 2.5 {
            let tip = inner.right_bottom() - Vec2::new(height * 1.1, height * 1.25);
            super::glyphs::pointer(
                &painter,
                tip,
                height,
                p.accent.gamma_multiply(t),
                p.surface.gamma_multiply(t),
            );
        }
    }

    if let Some((anchor, group, side, offset, free)) = preview {
        let target = placed(
            anchor,
            project(group.selected).size(),
            side,
            offset,
            anchor.width().min(anchor.height()) * 0.06,
        );
        let color = if free { p.accent } else { p.danger };
        painter.rect(
            target.expand(3.0),
            CornerRadius::same(8),
            color.gamma_multiply(0.10),
            Stroke::new(1.5_f32, color),
            StrokeKind::Outside,
        );
        if !free {
            painter.text(
                canvas.center_bottom() - Vec2::new(0.0, 14.0),
                Align2::CENTER_BOTTOM,
                "Another computer already uses that edge",
                theme::small(),
                p.danger,
            );
        }
    } else if editable && !groups.is_empty() {
        painter.text(
            canvas.right_bottom() - Vec2::new(14.0, 12.0),
            Align2::RIGHT_BOTTOM,
            "Drag a computer to the edge you move the pointer across",
            theme::small(),
            p.faint,
        );
    }
    moved
}

fn seam(
    painter: &egui::Painter,
    local: Rect,
    remote: Rect,
    side: Side,
    offset: f64,
    color: Color32,
) {
    let offset = offset as f32;
    let local_range = [offset.max(0.0), (1.0 + offset).min(1.0)];
    let edge = |rect: Rect, range: [f32; 2]| {
        range.map(|t| match side {
            Side::Left => Pos2::new(rect.left(), rect.top() + rect.height() * t),
            Side::Right => Pos2::new(rect.right(), rect.top() + rect.height() * t),
            Side::Top => Pos2::new(rect.left() + rect.width() * t, rect.top()),
            Side::Bottom => Pos2::new(rect.left() + rect.width() * t, rect.bottom()),
        })
    };
    let [a, b] = edge(local, local_range);
    // Clip the shared stretch to the part the two screens actually overlap.
    let (a, b) = match side {
        Side::Left | Side::Right => (
            Pos2::new(a.x, a.y.max(remote.top())),
            Pos2::new(b.x, b.y.min(remote.bottom())),
        ),
        Side::Top | Side::Bottom => (
            Pos2::new(a.x.max(remote.left()), a.y),
            Pos2::new(b.x.min(remote.right()), b.y),
        ),
    };
    if (b - a).length() > 2.0 {
        painter.line_segment([a, b], Stroke::new(3.0_f32, color));
    }
}

fn text(
    painter: &egui::Painter,
    clip: Rect,
    center: Pos2,
    value: &str,
    font: egui::FontId,
    color: Color32,
) {
    let mut job = egui::text::LayoutJob::simple_singleline(value.into(), font, color);
    job.wrap.max_width = clip.width() - 12.0;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    painter
        .with_clip_rect(clip.shrink(4.0))
        .galley(center - galley.size() * 0.5, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_shared_with_another_display_is_blocked() {
        let display = |id, x| Monitor {
            id,
            identity: None,
            name: String::new(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            primary: id == 1,
        };
        let local = [display(1, 0), display(2, 1920)];
        assert!(blocked(&local, &local[0], Side::Right));
        assert!(!blocked(&local, &local[0], Side::Left));
        assert!(blocked(&local, &local[1], Side::Left));
        assert!(!blocked(&local, &local[1], Side::Right));
        assert!(!blocked(&local, &local[1], Side::Top));
        assert!(edge_blocked(
            &local,
            &Arrangement {
                local_monitor: Some(1),
                ..Arrangement::on(Side::Right)
            }
        ));
    }

    #[test]
    fn drop_position_maps_to_the_nearest_edge_and_alignment() {
        let anchor = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 60.0));
        for side in Side::ALL {
            let rect = placed(anchor, Vec2::new(80.0, 50.0), side, 0.25, 6.0);
            let (found, offset) = nearest(anchor, rect);
            assert_eq!(found, side);
            assert!((offset - 0.25).abs() < 1e-4);
        }
    }
}
