//! The dark look of the editor, modelled on the reference toolbar: near-black canvas surround,
//! slightly lighter charcoal bars, light-grey icons, and one blue accent for the active tool.

use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals,
    style::Selection,
};

/// Area around the picture.
pub const CANVAS_BG: Color32 = Color32::from_rgb(28, 29, 32);
/// Menu / properties / status bars.
pub const BAR_BG: Color32 = Color32::from_rgb(38, 39, 43);
/// The tool bar (a touch lighter, like the reference).
pub const TOOLBAR_BG: Color32 = Color32::from_rgb(46, 47, 52);
/// Hairlines between bars.
pub const HAIRLINE: Color32 = Color32::from_rgb(24, 25, 28);
/// Hovered button background.
pub const HOVER_BG: Color32 = Color32::from_rgb(70, 72, 80);
/// Pressed button background.
pub const PRESSED_BG: Color32 = Color32::from_rgb(84, 87, 96);
/// The accent (active tool, focus, selection handles).
pub const ACCENT: Color32 = Color32::from_rgb(74, 144, 226);
/// Active tool background: the accent, dimmed.
pub const ACTIVE_BG: Color32 = Color32::from_rgb(45, 84, 138);
/// Primary text and icon colour.
pub const TEXT: Color32 = Color32::from_rgb(218, 220, 226);
/// Secondary text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(140, 144, 154);
/// Errors.
pub const DANGER: Color32 = Color32::from_rgb(232, 96, 96);
/// Snap guides.
pub const GUIDE: Color32 = Color32::from_rgb(255, 64, 200);

/// Installs fonts sizes, colours and spacing.
pub fn apply(ctx: &Context) {
    ctx.set_visuals(visuals());
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(16.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.spacing.interact_size = egui::vec2(28.0, 24.0);
        style.spacing.slider_width = 96.0;
        style.spacing.combo_width = 90.0;
        style.spacing.menu_margin = egui::Margin::same(6);
        style.spacing.window_margin = egui::Margin::same(12);
        style.spacing.tooltip_width = 320.0;
        style.interaction.tooltip_delay = 0.35;
        style.animation_time = 0.08;
    });
}

/// The colour scheme.
pub fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BAR_BG;
    v.window_fill = Color32::from_rgb(44, 45, 50);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(66, 68, 76));
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.extreme_bg_color = Color32::from_rgb(26, 27, 30);
    v.faint_bg_color = Color32::from_rgb(42, 43, 48);
    v.hyperlink_color = ACCENT;
    v.selection =
        Selection { bg_fill: ACCENT.gamma_multiply(0.55), stroke: Stroke::new(1.0, ACCENT) };
    v.popup_shadow =
        egui::Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(110) };
    v.window_shadow =
        egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(140) };

    let r = CornerRadius::same(5);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = Color32::from_rgb(58, 60, 66);
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = HOVER_BG;
    w.hovered.weak_bg_fill = HOVER_BG;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.hovered.corner_radius = r;
    w.active.bg_fill = PRESSED_BG;
    w.active.weak_bg_fill = PRESSED_BG;
    w.active.bg_stroke = Stroke::NONE;
    w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.corner_radius = r;
    w.open.bg_fill = HOVER_BG;
    w.open.weak_bg_fill = HOVER_BG;
    w.open.corner_radius = r;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_installs_without_panicking_and_is_dark() {
        let ctx = Context::default();
        apply(&ctx);
        assert!(ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.global_style().visuals.panel_fill, BAR_BG);
    }

    #[test]
    fn text_is_readable_on_the_bars() {
        // WCAG-ish contrast: primary text on the bars must exceed 7:1, dim text 3:1.
        fn lum(c: Color32) -> f32 {
            let f = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        fn contrast(a: Color32, b: Color32) -> f32 {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        }
        for bg in [BAR_BG, TOOLBAR_BG, CANVAS_BG] {
            assert!(contrast(TEXT, bg) > 7.0, "{bg:?}");
            assert!(contrast(TEXT_DIM, bg) > 3.0, "{bg:?}");
        }
        assert!(contrast(TEXT, ACTIVE_BG) > 4.5);
    }
}
