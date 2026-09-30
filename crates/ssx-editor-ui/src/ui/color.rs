//! The colour swatch button and its popup: palette, recent colours, HSV picker, alpha, hex
//! field and the canvas eyedropper.

use egui::{
    Color32, CornerRadius, Id, Popup, PopupCloseBehavior, Sense, Stroke, StrokeKind, Ui, vec2,
};
use ssx_editor::Color;

use super::{theme, widgets};
use crate::{icons::Icon, props::ColorField, state::AppState};

/// The fixed palette: two rows of 12, dark and saturated on top, light tints below.
pub const PALETTE: [[Color; 12]; 2] = [
    [
        Color::rgb(0, 0, 0),
        Color::rgb(90, 90, 90),
        Color::rgb(160, 160, 160),
        Color::rgb(255, 255, 255),
        Color::rgb(230, 30, 30),
        Color::rgb(255, 128, 0),
        Color::rgb(255, 220, 0),
        Color::rgb(140, 210, 30),
        Color::rgb(0, 170, 60),
        Color::rgb(0, 190, 190),
        Color::rgb(30, 110, 240),
        Color::rgb(150, 60, 220),
    ],
    [
        Color::rgb(45, 45, 45),
        Color::rgb(120, 120, 120),
        Color::rgb(210, 210, 210),
        Color::rgb(255, 238, 238),
        Color::rgb(255, 150, 150),
        Color::rgb(255, 190, 120),
        Color::rgb(255, 245, 150),
        Color::rgb(200, 240, 140),
        Color::rgb(150, 225, 170),
        Color::rgb(150, 230, 230),
        Color::rgb(150, 190, 255),
        Color::rgb(215, 170, 250),
    ],
];

fn to32(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

fn same_rgb(a: Color, b: Color) -> bool {
    (a.r, a.g, a.b) == (b.r, b.g, b.b)
}

fn from_hsva(h: egui::ecolor::Hsva) -> Color {
    let [r, g, b, a] = h.to_srgba_unmultiplied();
    Color::rgba(r, g, b, a)
}

/// Paints a swatch (with a chequerboard behind transparency and a slash for "none").
pub fn paint_swatch(ui: &Ui, rect: egui::Rect, c: Color, none_slash: bool) {
    let r = CornerRadius::same(4);
    if c.a < 255 {
        widgets::paint_checker(ui, rect.shrink(1.0), 4.0);
    }
    if c.a > 0 {
        ui.painter().rect_filled(rect.shrink(1.0), r, to32(c));
    } else if none_slash {
        ui.painter().rect_filled(rect.shrink(1.0), r, Color32::from_gray(52));
        ui.painter().line_segment(
            [rect.left_bottom() + vec2(3.0, -3.0), rect.right_top() + vec2(-3.0, 3.0)],
            Stroke::new(1.6, theme::DANGER),
        );
    }
    ui.painter().rect_stroke(rect, r, Stroke::new(1.0, Color32::from_gray(96)), StrokeKind::Inside);
}

/// The swatch button. Returns the new colour when the user picks one.
///
/// `allow_none` adds a "no colour" choice (transparent), used for fills and backgrounds.
pub fn color_button(
    ui: &mut Ui,
    label: &str,
    color: Color,
    field: ColorField,
    state: &mut AppState,
    allow_none: bool,
) -> Option<Color> {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 22.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(rect.expand(2.0), CornerRadius::same(5), theme::HOVER_BG);
        }
        paint_swatch(ui, rect, color, allow_none);
    }
    let resp = resp.on_hover_text(label);
    let popup_id = Id::new(("ssx-color-popup", label));
    let mut result = None;
    let hsva_id = popup_id.with("hsva");
    let hex_id = popup_id.with("hex");
    let open_before = Popup::is_id_open(ui.ctx(), popup_id);
    let popup = Popup::from_toggle_button_response(&resp)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .id(popup_id);
    let shown = popup.show(|ui| {
        ui.set_min_width(252.0);
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        let mut out: Option<Color> = None;
        // Palette.
        for row in &PALETTE {
            ui.horizontal(|ui| {
                for c in row {
                    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    paint_swatch(ui, r, *c, false);
                    if resp.hovered() {
                        ui.painter().rect_stroke(
                            r.expand(1.0),
                            3.0,
                            Stroke::new(1.5, Color32::WHITE),
                            StrokeKind::Outside,
                        );
                    }
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, c.to_hex())
                    });
                    if resp.on_hover_text(c.to_hex()).clicked() {
                        out = Some(c.with_alpha(color.a.max(1)));
                    }
                }
            });
        }
        // Recent colours.
        if !state.prefs.recent_colors.is_empty() {
            widgets::caption(ui, "Recent");
            ui.horizontal_wrapped(|ui| {
                for c in state.prefs.recent_colors.clone() {
                    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    paint_swatch(ui, r, c, false);
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("Recent {}", c.to_hex()),
                        )
                    });
                    if resp.on_hover_text(c.to_hex()).clicked() {
                        out = Some(c);
                    }
                }
            });
        }
        ui.separator();
        // HSV picker, kept in memory while the popup is open so hue survives grey colours.
        let stored: Option<egui::ecolor::Hsva> = ui.data(|d| d.get_temp(hsva_id));
        let mut hsva = match stored {
            Some(h) if same_rgb(from_hsva(h), color) => h,
            _ => egui::ecolor::Hsva::from_srgba_unmultiplied([color.r, color.g, color.b, 255]),
        };
        if egui::color_picker::color_picker_hsva_2d(
            ui,
            &mut hsva,
            egui::color_picker::Alpha::Opaque,
        ) {
            out = Some(from_hsva(hsva).with_alpha(color.a.max(1)));
        }
        ui.data_mut(|d| d.insert_temp(hsva_id, hsva));
        // Alpha.
        ui.horizontal(|ui| {
            widgets::caption(ui, "Opacity");
            let mut pct = f32::from(color.a) / 255.0 * 100.0;
            if ui
                .add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix("%").max_decimals(0))
                .changed()
            {
                out = Some(color.with_alpha((pct / 100.0 * 255.0).round() as u8));
            }
        });
        // Hex.
        ui.horizontal(|ui| {
            widgets::caption(ui, "Hex");
            let mut text: String =
                ui.data(|d| d.get_temp::<String>(hex_id)).unwrap_or_else(|| color.to_hex());
            let r = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(86.0)
                    .font(egui::TextStyle::Monospace),
            );
            if r.changed() {
                ui.data_mut(|d| d.insert_temp(hex_id, text.clone()));
                if let Some(c) = Color::from_hex(&text) {
                    out = Some(c);
                }
            }
            if !r.has_focus() {
                ui.data_mut(|d| d.remove::<String>(hex_id));
            }
            if widgets::icon_button(
                ui,
                Icon::Eyedropper,
                "Pick a colour from the image",
                None,
                false,
                true,
            )
            .clicked()
            {
                state.eyedropper = Some(field);
                Popup::close_all(ui.ctx());
            }
            if allow_none && ui.button("None").on_hover_text("No colour (transparent)").clicked() {
                out = Some(Color::TRANSPARENT);
            }
        });
        out
    });
    if let Some(inner) = shown
        && let Some(c) = inner.inner
    {
        result = Some(c);
    }
    // Remember the final colour when the popup closes after a change.
    let open_now = Popup::is_id_open(ui.ctx(), popup_id);
    if open_before && !open_now {
        if !color.is_transparent() {
            state.note_color(color);
        }
        ui.data_mut(|d| d.remove::<egui::ecolor::Hsva>(hsva_id));
    }
    result
}
