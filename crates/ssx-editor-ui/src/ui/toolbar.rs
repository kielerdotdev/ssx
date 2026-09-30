//! The tool bar, laid out like the reference screenshot: region shapes, select, shapes, text
//! tools, callouts, insert, obscure tools, then the Effects and Canvas dropdowns and the gear.

use egui::{Popup, PopupCloseBehavior, Ui, containers::SetOpenCommand};

use super::{
    menubar,
    widgets::{self, dropdown_button, icon_button, tool_slot_button},
};
use crate::{
    action::{Action, DialogKind},
    document::EditorDoc,
    icons::Icon,
    shortcuts::primary_for,
    state::AppState,
    tools::{TOOLBAR, ToolId},
};

fn shortcut_of(tool: ToolId) -> Option<String> {
    primary_for(&Action::SetTool(tool)).map(|c| c.display())
}

/// Draws the tool bar.
pub fn show(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (gi, group) in TOOLBAR.iter().enumerate() {
            if gi > 0 {
                widgets::separator(ui);
            }
            for slot in *group {
                let tool = state.slot_tool(slot);
                let selected = slot.contains(state.tool);
                let has_variants = slot.variants.len() > 1;
                let is_image = tool == ToolId::Image;
                let (resp, caret) = tool_slot_button(
                    ui,
                    tool.icon(),
                    tool.label(),
                    shortcut_of(tool).as_deref(),
                    selected,
                    has_variants || is_image,
                );
                let popup_id = egui::Id::new(("slot", slot.variants[0]));
                let toggle = if caret || resp.secondary_clicked() {
                    Some(SetOpenCommand::Toggle)
                } else {
                    None
                };
                if resp.clicked() && !caret {
                    state.push(Action::SetTool(tool));
                }
                if has_variants || is_image {
                    Popup::from_response(&resp)
                        .id(popup_id)
                        .open_memory(toggle)
                        .close_behavior(PopupCloseBehavior::CloseOnClick)
                        .show(|ui| {
                            ui.set_min_width(180.0);
                            if is_image {
                                if ui.button("Insert from file...").clicked() {
                                    state.push(Action::SetTool(ToolId::Image));
                                    state.push(Action::PickImageFile);
                                }
                                if ui.button("Insert from clipboard").clicked() {
                                    state.push(Action::PickImageClipboard);
                                    state.push(Action::SetTool(ToolId::Image));
                                }
                            } else {
                                for v in slot.variants {
                                    if ui.selectable_label(state.tool == *v, v.label()).clicked() {
                                        state.push(Action::SetTool(*v));
                                    }
                                }
                            }
                        });
                }
            }
        }
        widgets::separator(ui);

        let fx = dropdown_button(ui, Icon::Effects, "Image effects", false);
        Popup::menu(&fx).show(|ui| {
            ui.set_min_width(190.0);
            menubar::effects_menu(ui, state);
        });
        let canvas = dropdown_button(ui, Icon::Canvas, "Canvas and crop", false);
        Popup::menu(&canvas).show(|ui| {
            ui.set_min_width(260.0);
            menubar::canvas_menu(ui, state, doc);
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon_button(
                ui,
                Icon::Gear,
                "Settings",
                primary_for(&Action::OpenDialog(DialogKind::Settings))
                    .map(|c| c.display())
                    .as_deref(),
                false,
                true,
            )
            .clicked()
            {
                state.push(Action::OpenDialog(DialogKind::Settings));
            }
            if icon_button(
                ui,
                Icon::Layers,
                "Object list",
                primary_for(&Action::ToggleLayers).map(|c| c.display()).as_deref(),
                state.prefs.show_layers,
                true,
            )
            .clicked()
            {
                state.push(Action::ToggleLayers);
            }
            if icon_button(ui, Icon::Keyboard, "Keyboard shortcuts", Some("F1"), false, true)
                .clicked()
            {
                state.push(Action::OpenDialog(DialogKind::Shortcuts));
            }
        });
    });
}
