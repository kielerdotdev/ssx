//! The optional object list on the right: select, hide, lock, reorder and delete annotations.

use egui::{Color32, RichText, Sense, Ui, vec2};
use ssx_editor::{Object, ObjectKind, Tool};

use super::{
    theme,
    widgets::{self, icon_button},
};
use crate::{
    action::Action,
    document::EditorDoc,
    icons::{self, Icon, IconColors},
    state::AppState,
    tools::ToolId,
};

/// The label of an object row: kind plus a hint of its content.
pub fn object_label(doc: &EditorDoc, o: &Object) -> String {
    let base = match &o.kind {
        ObjectKind::Text(t) => {
            let first = t.content.text.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                "Text".to_owned()
            } else {
                format!("Text: {}", shorten(first, 22))
            }
        }
        ObjectKind::Balloon(b) => {
            let first = b.content.text.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                "Balloon".to_owned()
            } else {
                format!("Balloon: {}", shorten(first, 18))
            }
        }
        ObjectKind::Step(_) => format!("Step {}", doc.doc().step_number(o.id).unwrap_or(0)),
        ObjectKind::Freehand(f) if f.arrow.is_some() => "Freehand arrow".to_owned(),
        ObjectKind::Highlight(h) if !h.points.is_empty() => "Highlighter pen".to_owned(),
        ObjectKind::Highlight(_) => "Highlight".to_owned(),
        other => {
            let n = other.name().replace('_', " ");
            let mut c = n.chars();
            c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
        }
    };
    base
}

fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// The icon representing an object kind.
pub fn object_icon(k: &ObjectKind) -> Icon {
    Tool::for_kind(k).map_or(Icon::Select, |t| ToolId::from_engine(t).icon())
}

/// Draws the panel contents.
pub fn show(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Objects").strong());
        ui.label(RichText::new(format!("({})", doc.doc().objects().len())).color(theme::TEXT_DIM));
    });
    ui.separator();
    let selection = doc.session.selection().to_vec();
    let has_sel = !selection.is_empty();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (icon, label, action) in [
            (Icon::ToFront, "Bring to front", Action::BringToFront),
            (Icon::ChevronUp, "Bring forward", Action::Raise),
            (Icon::ChevronDown, "Send backward", Action::Lower),
            (Icon::ToBack, "Send to back", Action::SendToBack),
            (Icon::Copy, "Duplicate", Action::Duplicate),
            (Icon::Trash, "Delete", Action::DeleteSelection),
        ] {
            let chord = crate::shortcuts::primary_for(&action).map(|c| c.display());
            if icon_button(ui, icon, label, chord.as_deref(), false, has_sel).clicked() {
                state.push(action);
            }
        }
    });
    ui.separator();
    if doc.doc().objects().is_empty() {
        ui.add_space(8.0);
        ui.label(
            RichText::new("No annotations yet.\nPick a tool and draw on the picture.")
                .color(theme::TEXT_DIM)
                .size(12.0),
        );
        return;
    }
    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.shift);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        // Top of the z-order first.
        for o in doc.doc().objects().iter().rev() {
            let selected = selection.contains(&o.id);
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
            resp.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    true,
                    selected,
                    object_label(doc, o),
                )
            });
            if selected {
                ui.painter().rect_filled(rect, 4.0, theme::ACTIVE_BG);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 4.0, theme::HOVER_BG);
            }
            let colors = IconColors::with_ink(if o.visible {
                theme::TEXT
            } else {
                theme::TEXT_DIM.gamma_multiply(0.6)
            });
            let icon_rect = egui::Rect::from_center_size(
                rect.left_center() + vec2(14.0, 0.0),
                vec2(16.0, 16.0),
            );
            icons::paint(ui.painter(), object_icon(&o.kind), icon_rect, colors);
            ui.painter().text(
                rect.left_center() + vec2(30.0, 0.0),
                egui::Align2::LEFT_CENTER,
                object_label(doc, o),
                egui::FontId::proportional(12.5),
                if o.visible { Color32::WHITE.gamma_multiply(0.92) } else { theme::TEXT_DIM },
            );
            // Eye and lock toggles on the right.
            let eye = egui::Rect::from_center_size(
                rect.right_center() - vec2(14.0, 0.0),
                vec2(20.0, 20.0),
            );
            let lock = egui::Rect::from_center_size(
                rect.right_center() - vec2(38.0, 0.0),
                vec2(20.0, 20.0),
            );
            let eye_resp = ui.interact(eye, resp.id.with("eye"), Sense::click());
            let lock_resp = ui.interact(lock, resp.id.with("lock"), Sense::click());
            let dim = IconColors::with_ink(theme::TEXT_DIM);
            icons::paint(
                ui.painter(),
                if o.visible { Icon::Eye } else { Icon::EyeOff },
                eye.shrink(2.0),
                if eye_resp.hovered() { IconColors::with_ink(Color32::WHITE) } else { dim },
            );
            if o.locked || lock_resp.hovered() || resp.hovered() {
                icons::paint(
                    ui.painter(),
                    if o.locked { Icon::Lock } else { Icon::Unlock },
                    lock.shrink(2.0),
                    if o.locked { IconColors::with_ink(theme::TEXT) } else { dim },
                );
            }
            if eye_resp.on_hover_text(if o.visible { "Hide" } else { "Show" }).clicked() {
                state.push(Action::SetVisible(o.id, !o.visible));
            } else if lock_resp.on_hover_text(if o.locked { "Unlock" } else { "Lock" }).clicked() {
                state.push(Action::SetLocked(o.id, !o.locked));
            } else if resp.clicked() {
                state.push(Action::SelectObjects { ids: vec![o.id], additive: ctrl });
            }
        }
    });
    let _ = widgets::BUTTON;
}
