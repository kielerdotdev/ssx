//! Drag-to-reorder rows for egui, on top of the arithmetic in [`crate::reorder`].
//!
//! Each row gets a grip handle on the left. Pressing and dragging the handle shows a marker
//! line where the row would land, dims the dragged row, and reports a [`Moved`] when the
//! pointer is released. The same handle works from the keyboard: focus it (Tab) and press
//! Alt+Up / Alt+Down; the focus follows the row. Screen readers get a name for the handle
//! that says what it moves and how.
//!
//! Where the row would land is decided *after* all rows are laid out in the frame, from the
//! rows' real rectangles, so rows of different heights (a step with an open editor) work.

use egui::{Color32, CursorIcon, Id, Rect, Sense, Stroke, Ui, WidgetInfo, WidgetType, pos2, vec2};
use ssx_editor_ui::ui::theme;

use crate::reorder::{Move, gap_at, marker_y};

/// A row that was dropped (or nudged with the keyboard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    /// The move to apply to the list.
    pub mv: Move,
    /// It came from the keyboard, so the focus should follow the row.
    pub keyboard: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Drag {
    from: Option<usize>,
}

fn handle_id(list: Id, i: usize) -> Id {
    list.with(("handle", i))
}

fn paint_grip(ui: &Ui, rect: Rect, hot: bool) {
    let c = if hot { Color32::WHITE } else { theme::TEXT_DIM };
    let centre = rect.center();
    for dx in [-2.5, 2.5] {
        for dy in [-5.0, 0.0, 5.0] {
            ui.painter().circle_filled(pos2(centre.x + dx, centre.y + dy), 1.3, c);
        }
    }
}

/// Draws `count` rows. `label(i)` names row `i` for screen readers; `content(ui, i)` draws
/// the row's content next to the grip. Returns a [`Moved`] when the user finished a move.
pub fn list(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    count: usize,
    label: impl Fn(usize) -> String,
    mut content: impl FnMut(&mut Ui, usize),
) -> Option<Moved> {
    let id = ui.make_persistent_id(id_salt);
    let mut drag: Drag = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let width = ui.available_width();
    let mut rects: Vec<Rect> = Vec::with_capacity(count);
    let mut keyboard: Option<Move> = None;
    let mut stopped: Option<usize> = None;

    for i in 0..count {
        let dragging_this = drag.from == Some(i);
        let row = ui.horizontal_top(|ui| {
            let (_, rect) = ui.allocate_space(vec2(18.0, 26.0));
            let resp = ui.interact(rect, handle_id(id, i), Sense::click_and_drag());
            let name = format!("Reorder {}: drag, or press Alt+Up or Alt+Down", label(i));
            resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name.clone()));
            let hot = resp.hovered() || resp.has_focus() || dragging_this;
            paint_grip(ui, rect, hot);
            if resp.has_focus() {
                ui.painter().rect_stroke(
                    rect.expand(1.0),
                    4.0,
                    Stroke::new(1.5, Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
            }
            if resp.hovered() && drag.from.is_none() {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
            ui.vertical(|ui| content(ui, i));
            resp
        });
        let handle = row.inner;
        rects.push(row.response.rect);
        if handle.drag_started() {
            drag.from = Some(i);
        }
        if handle.drag_stopped() && drag.from == Some(i) {
            stopped = Some(i);
        }
        if handle.has_focus() {
            let (up, down) = ui.input_mut(|inp| {
                (
                    inp.consume_key(egui::Modifiers::ALT, egui::Key::ArrowUp),
                    inp.consume_key(egui::Modifiers::ALT, egui::Key::ArrowDown),
                )
            });
            if up {
                keyboard = Some(Move::step(i, -1));
            } else if down {
                keyboard = Some(Move::step(i, 1));
            }
        }
        ui.add_space(2.0);
    }

    let mut result = keyboard.map(|mv| Moved { mv, keyboard: true });
    if let Some(from) = drag.from {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let pointer = ui.input(|i| i.pointer.interact_pos().or_else(|| i.pointer.latest_pos()));
        let centers: Vec<f32> = rects.iter().map(|r| r.center().y).collect();
        if let (Some(p), true) = (pointer, !rects.is_empty()) {
            let gap = gap_at(&centers, p.y);
            let spans: Vec<(f32, f32)> = rects.iter().map(|r| (r.top(), r.bottom())).collect();
            if let (Some(y), Some(first)) = (marker_y(&spans, gap), rects.first()) {
                let x0 = first.left();
                ui.painter().line_segment(
                    [pos2(x0, y), pos2(x0 + width, y)],
                    Stroke::new(2.0, theme::ACCENT),
                );
                ui.painter().circle_filled(pos2(x0, y), 3.5, theme::ACCENT);
            }
            if let Some(r) = rects.get(from) {
                ui.painter().rect_filled(*r, 4.0, Color32::from_black_alpha(90));
            }
            if stopped == Some(from) {
                result = Some(Moved { mv: Move { from, gap }, keyboard: false });
            }
        }
        ui.ctx().request_repaint();
    }
    if stopped.is_some() || (drag.from.is_some() && !ui.input(|i| i.pointer.any_down())) {
        drag.from = None;
    }
    ui.data_mut(|d| d.insert_temp(id, drag));
    result
}

/// Applies `moved` to `items` and, for a keyboard move, makes the focus follow the row.
/// Returns the row's new position.
pub fn apply<T>(
    ui: &Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    items: &mut Vec<T>,
    moved: Moved,
) -> Option<usize> {
    let pos = moved.mv.apply(items)?;
    if moved.keyboard {
        let id = ui.make_persistent_id(id_salt);
        ui.memory_mut(|m| m.request_focus(handle_id(id, pos)));
    }
    Some(pos)
}
