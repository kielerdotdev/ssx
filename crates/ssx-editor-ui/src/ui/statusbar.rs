//! The status bar: tool hint or message on the left; pointer position, pixel colour, image
//! size and zoom controls on the right.

use egui::{Color32, Popup, PopupCloseBehavior, RichText, Sense, Ui, vec2};

use super::{
    color::paint_swatch,
    theme,
    widgets::{self, icon_button},
};
use crate::{action::Action, icons::Icon, state::AppState};

const PRESETS: [f32; 9] = [0.125, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

/// Draws the bar.
pub fn show(ui: &mut Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        // Left: message or hint.
        let msg = if let Some(t) = &state.toast {
            RichText::new(&t.text).color(if t.error { theme::DANGER } else { theme::TEXT })
        } else if state.eyedropper.is_some() {
            RichText::new("Click on the picture to pick a colour, Esc to cancel")
                .color(theme::ACCENT)
        } else if state.crop_pending {
            RichText::new("Enter applies the crop, Esc cancels").color(theme::TEXT_DIM)
        } else {
            RichText::new(state.tool.hint()).color(theme::TEXT_DIM)
        };
        ui.label(msg.size(12.0));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // Zoom controls (right-most).
            if icon_button(ui, Icon::Fit, "Fit to window", Some("Ctrl+0"), false, true).clicked() {
                state.push(Action::ZoomFit);
            }
            if icon_button(ui, Icon::ZoomIn, "Zoom in", Some("Ctrl++"), false, true).clicked() {
                state.push(Action::ZoomIn);
            }
            let zoom_text = format!("{:.0}%", state.zoom_percent);
            let zb = ui.add(
                egui::Button::new(RichText::new(&zoom_text).monospace().size(12.5))
                    .min_size(vec2(58.0, 22.0)),
            );
            zb.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Zoom level")
            });
            Popup::from_toggle_button_response(&zb)
                .close_behavior(PopupCloseBehavior::CloseOnClick)
                .show(|ui| {
                    for z in PRESETS {
                        if ui.button(format!("{:.1}%", z * 100.0).replace(".0%", "%")).clicked() {
                            state.push(Action::ZoomTo(z));
                        }
                    }
                    ui.separator();
                    if ui.button("Fit to window").clicked() {
                        state.push(Action::ZoomFit);
                    }
                });
            if icon_button(ui, Icon::ZoomOut, "Zoom out", Some("Ctrl+-"), false, true).clicked() {
                state.push(Action::ZoomOut);
            }
            widgets::separator(ui);
            let (w, h) = state.image_size;
            ui.label(RichText::new(format!("{w} x {h}")).monospace().size(12.0).color(theme::TEXT));
            widgets::separator(ui);
            // Pixel colour.
            if let (Some(_), Some([r, g, b, a])) = (state.hover.pixel, state.hover.color) {
                ui.label(
                    RichText::new(format!("#{r:02x}{g:02x}{b:02x}  rgba({r},{g},{b},{a})"))
                        .monospace()
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
                let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                paint_swatch(ui, rect, ssx_editor::Color::rgba(r, g, b, a), false);
            } else {
                let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                ui.painter().rect_stroke(
                    rect,
                    3.0,
                    egui::Stroke::new(1.0, Color32::from_gray(70)),
                    egui::StrokeKind::Inside,
                );
            }
            widgets::separator(ui);
            // Pointer position.
            let pos =
                state.hover.pixel.map_or_else(|| "-, -".to_owned(), |(x, y)| format!("{x}, {y}"));
            ui.label(RichText::new(pos).monospace().size(12.0).color(theme::TEXT));
        });
    });
}
