//! The top row: text menus on the left, undo/redo and the finish buttons on the right.

use egui::{Popup, PopupCloseBehavior, Ui};
use ssx_editor::object::Orient;

use super::widgets::{self, accent_button, icon_button};
use crate::{
    action::{Action, DialogKind, Finish},
    document::EditorDoc,
    effects::EffectKind,
    icons::Icon,
    shortcuts::primary_for,
    state::AppState,
};

/// A menu entry with its shortcut on the right; queues `action` when clicked.
pub fn item(ui: &mut Ui, state: &mut AppState, label: &str, action: Action, enabled: bool) {
    let mut b = egui::Button::new(label);
    if let Some(c) = primary_for(&action) {
        b = b.shortcut_text(c.display());
    }
    if ui.add_enabled(enabled, b).clicked() {
        state.push(action);
        ui.close();
    }
}

/// The list of image effects, grouped, for the Effects dropdown and the Image menu.
pub fn effects_menu(ui: &mut Ui, state: &mut AppState) {
    let mut last = "";
    for k in EffectKind::ALL {
        if k.category() != last {
            if !last.is_empty() {
                ui.separator();
            }
            widgets::caption(ui, k.category());
            last = k.category();
        }
        if ui.button(format!("{}...", k.label())).clicked() {
            state.push(Action::OpenEffect(k));
            ui.close();
        }
    }
}

/// Rotate/flip entries.
pub fn orient_items(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    let _ = doc;
    for (o, l) in [
        (Orient::Rotate90, "Rotate 90 degrees clockwise"),
        (Orient::Rotate270, "Rotate 90 degrees counter-clockwise"),
        (Orient::Rotate180, "Rotate 180 degrees"),
        (Orient::FlipH, "Flip horizontally"),
        (Orient::FlipV, "Flip vertically"),
    ] {
        item(ui, state, l, Action::Orient(o), true);
    }
}

/// The Canvas dropdown / Image menu body.
pub fn canvas_menu(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    item(ui, state, "Crop numerically...", Action::OpenDialog(DialogKind::Crop), true);
    item(ui, state, "Auto-crop borders", Action::AutoCrop(8), true);
    item(ui, state, "Cut out a strip...", Action::OpenDialog(DialogKind::CutOut), true);
    ui.separator();
    item(ui, state, "Canvas size...", Action::OpenDialog(DialogKind::Canvas), true);
    item(ui, state, "Resize image...", Action::OpenDialog(DialogKind::Resize), true);
    ui.separator();
    orient_items(ui, state, doc);
    ui.separator();
    item(
        ui,
        state,
        "Flatten annotations into the image",
        Action::Flatten,
        !doc.doc().objects().is_empty(),
    );
}

fn file_menu(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    let _ = doc;
    item(ui, state, "New from clipboard", Action::NewFromClipboard, true);
    item(ui, state, "Open...", Action::Open, true);
    ui.separator();
    item(ui, state, "Save", Action::Save, true);
    item(ui, state, "Save as...", Action::SaveAs, true);
    item(ui, state, "Save editable project (.ssxe)", Action::SaveProject, true);
    ui.separator();
    item(ui, state, "Copy image to clipboard", Action::CopyImage, true);
    item(ui, state, "Upload", Action::Upload, true);
    ui.separator();
    item(ui, state, "Done (save and close)", Action::Done(Finish::Save), true);
    item(ui, state, "Close editor", Action::RequestClose, true);
}

fn edit_menu(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    let has_sel = !doc.session.selection().is_empty();
    item(ui, state, "Undo", Action::Undo, doc.session.can_undo());
    item(ui, state, "Redo", Action::Redo, doc.session.can_redo());
    ui.separator();
    item(ui, state, "Cut", Action::Cut, has_sel);
    item(ui, state, "Copy", Action::Copy, true);
    item(ui, state, "Paste", Action::Paste, true);
    item(ui, state, "Paste image from clipboard", Action::PasteImage, true);
    item(ui, state, "Duplicate", Action::Duplicate, has_sel);
    item(ui, state, "Delete", Action::DeleteSelection, has_sel);
    item(ui, state, "Select all", Action::SelectAll, !doc.doc().objects().is_empty());
    item(ui, state, "Deselect", Action::Deselect, has_sel);
    ui.separator();
    item(ui, state, "Group", Action::Group, doc.session.selection().len() > 1);
    item(ui, state, "Ungroup", Action::Ungroup, has_sel);
    ui.separator();
    item(ui, state, "Bring to front", Action::BringToFront, has_sel);
    item(ui, state, "Bring forward", Action::Raise, has_sel);
    item(ui, state, "Send backward", Action::Lower, has_sel);
    item(ui, state, "Send to back", Action::SendToBack, has_sel);
}

fn view_menu(ui: &mut Ui, state: &mut AppState) {
    item(ui, state, "Zoom in", Action::ZoomIn, true);
    item(ui, state, "Zoom out", Action::ZoomOut, true);
    item(ui, state, "Fit to window", Action::ZoomFit, true);
    item(ui, state, "Actual size (100 %)", Action::ZoomActual, true);
    ui.separator();
    let mut grid = state.prefs.pixel_grid;
    if ui.checkbox(&mut grid, "Pixel grid at high zoom").changed() {
        state.push(Action::TogglePixelGrid);
    }
    let mut layers = state.prefs.show_layers;
    if ui.checkbox(&mut layers, "Object list").changed() {
        state.push(Action::ToggleLayers);
    }
    ui.separator();
    item(ui, state, "Keyboard shortcuts", Action::OpenDialog(DialogKind::Shortcuts), true);
    item(ui, state, "Settings...", Action::OpenDialog(DialogKind::Settings), true);
}

/// Undo/redo button with the history popup on its chevron.
fn history_chevron(ui: &mut Ui, enabled: bool, label: &str) -> egui::Response {
    let chev = ui
        .add_enabled(enabled, egui::Button::new("").min_size(egui::vec2(14.0, 28.0)).frame(false));
    chev.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let colors = crate::icons::IconColors::with_ink(if enabled {
        super::theme::TEXT_DIM
    } else {
        super::theme::TEXT_DIM.gamma_multiply(0.4)
    });
    crate::icons::paint(
        ui.painter(),
        Icon::ChevronDown,
        egui::Rect::from_center_size(chev.rect.center(), egui::vec2(11.0, 11.0)),
        colors,
    );
    chev
}

fn history_button(
    ui: &mut Ui,
    state: &mut AppState,
    doc: &EditorDoc,
    undo: bool,
    chevron_first: bool,
) {
    let (icon, label, enabled) = if undo {
        (Icon::Undo, "Undo", doc.session.can_undo())
    } else {
        (Icon::Redo, "Redo", doc.session.can_redo())
    };
    let action = if undo { Action::Undo } else { Action::Redo };
    let chord = primary_for(&action).map(|c| c.display());
    let next: Option<String> = if undo {
        doc.log.undo_labels().next().map(str::to_owned)
    } else {
        doc.log.redo_labels().next().map(str::to_owned)
    };
    let tip = match (&next, enabled) {
        (Some(n), true) => format!("{label} {n}"),
        _ => label.to_owned(),
    };
    let mut chev = if chevron_first {
        Some(history_chevron(ui, enabled, if undo { "Undo history" } else { "Redo history" }))
    } else {
        None
    };
    let r = icon_button(ui, icon, &tip, chord.as_deref(), false, enabled);
    if r.clicked() {
        state.push(action);
    }
    if chev.is_none() {
        chev =
            Some(history_chevron(ui, enabled, if undo { "Undo history" } else { "Redo history" }));
    }
    let Some(chev) = chev else { return };
    // History list, opened from the chevron next to the button.
    let id = egui::Id::new(("history", undo));
    Popup::from_toggle_button_response(&chev)
        .id(id)
        .close_behavior(PopupCloseBehavior::CloseOnClick)
        .show(|ui| {
            ui.set_min_width(190.0);
            let labels: Vec<String> = if undo {
                doc.log.undo_labels().map(str::to_owned).collect()
            } else {
                doc.log.redo_labels().map(str::to_owned).collect()
            };
            widgets::caption(ui, if undo { "Undo up to..." } else { "Redo up to..." });
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (i, l) in labels.iter().enumerate() {
                    if ui.selectable_label(false, format!("{}. {l}", i + 1)).clicked() {
                        state.push(if undo {
                            Action::UndoSteps(i + 1)
                        } else {
                            Action::RedoSteps(i + 1)
                        });
                    }
                }
            });
        });
}

/// Draws the row.
pub fn show(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        egui::containers::menu::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| file_menu(ui, state, doc));
            ui.menu_button("Edit", |ui| edit_menu(ui, state, doc));
            ui.menu_button("View", |ui| view_menu(ui, state));
            ui.menu_button("Image", |ui| {
                canvas_menu(ui, state, doc);
                ui.separator();
                ui.menu_button("Effects", |ui| effects_menu(ui, state));
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Done split button.
            let chev = ui.add(egui::Button::new("").min_size(egui::vec2(16.0, 26.0)).frame(false));
            crate::icons::paint(
                ui.painter(),
                Icon::ChevronDown,
                egui::Rect::from_center_size(chev.rect.center(), egui::vec2(12.0, 12.0)),
                crate::icons::IconColors::with_ink(super::theme::TEXT),
            );
            Popup::menu(&chev).show(|ui| {
                item(ui, state, "Save and close", Action::Done(Finish::Save), true);
                item(ui, state, "Copy and close", Action::Done(Finish::Copy), true);
                item(ui, state, "Upload and close", Action::Done(Finish::Upload), true);
                ui.separator();
                item(ui, state, "Cancel (discard)", Action::Done(Finish::Cancel), true);
            });
            let done = accent_button(ui, "Done", Some(Icon::Done))
                .on_hover_text("Save and close (Ctrl+Enter)");
            if done.clicked() {
                state.push(Action::Done(Finish::Save));
            }
            widgets::separator(ui);
            if icon_button(
                ui,
                Icon::Upload,
                "Upload",
                primary_for(&Action::Upload).map(|c| c.display()).as_deref(),
                false,
                true,
            )
            .clicked()
            {
                state.push(Action::Upload);
            }
            if icon_button(
                ui,
                Icon::Copy,
                "Copy image to clipboard",
                primary_for(&Action::CopyImage).map(|c| c.display()).as_deref(),
                false,
                true,
            )
            .clicked()
            {
                state.push(Action::CopyImage);
            }
            if icon_button(
                ui,
                Icon::Save,
                "Save",
                primary_for(&Action::Save).map(|c| c.display()).as_deref(),
                false,
                true,
            )
            .clicked()
            {
                state.push(Action::Save);
            }
            if icon_button(
                ui,
                Icon::Open,
                "Open",
                primary_for(&Action::Open).map(|c| c.display()).as_deref(),
                false,
                true,
            )
            .clicked()
            {
                state.push(Action::Open);
            }
            widgets::separator(ui);
            // Right-to-left: add redo first so undo ends up on its left.
            // Right-to-left layout: the first widget added is the rightmost, so each pair adds
            // its chevron first to read "icon, chevron" on screen, and redo before undo.
            history_button(ui, state, doc, false, true);
            history_button(ui, state, doc, true, true);
        });
    });
}
