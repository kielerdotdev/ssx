//! Small custom widgets shared by the bars: icon buttons with tooltips and accessibility labels,
//! dropdown buttons, separators, segmented controls.

use egui::{
    Align2, Color32, CornerRadius, FontId, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
    WidgetInfo, WidgetType, pos2, vec2,
};

use super::theme;
use crate::icons::{self, Icon, IconColors};

/// Toolbar button size.
pub const BUTTON: Vec2 = vec2(30.0, 28.0);
/// Icon size inside a toolbar button.
pub const ICON: f32 = 20.0;

/// Colours for icons in the current widget state.
fn icon_colors(ui: &Ui, enabled: bool, hovered: bool, selected: bool) -> IconColors {
    let ink = if !enabled {
        theme::TEXT_DIM.gamma_multiply(0.55)
    } else if selected || hovered {
        Color32::WHITE
    } else {
        theme::TEXT
    };
    let _ = ui;
    IconColors::with_ink(ink)
}

fn tooltip(ui: &mut Ui, label: &str, shortcut: Option<&str>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).strong());
        if let Some(s) = shortcut {
            ui.label(egui::RichText::new(s).color(theme::TEXT_DIM).monospace());
        }
    });
}

/// An icon button. `label` is the tooltip and the accessible name; `shortcut` is shown dimmed.
pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    shortcut: Option<&str>,
    selected: bool,
    enabled: bool,
) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(BUTTON, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, selected, label));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered() && enabled;
        paint_button_bg(ui, rect, hovered, resp.is_pointer_button_down_on() && enabled, selected);
        let ir = egui::Rect::from_center_size(rect.center(), Vec2::splat(ICON));
        icons::paint(ui.painter(), icon, ir, icon_colors(ui, enabled, hovered, selected));
    }
    if enabled { resp.on_hover_ui(|ui| tooltip(ui, label, shortcut)) } else { resp }
}

/// Background of a toolbar button.
pub fn paint_button_bg(ui: &Ui, rect: egui::Rect, hovered: bool, pressed: bool, selected: bool) {
    let r = CornerRadius::same(5);
    if selected {
        ui.painter().rect_filled(rect, r, theme::ACTIVE_BG);
        ui.painter().rect_stroke(rect, r, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
    } else if pressed {
        ui.painter().rect_filled(rect, r, theme::PRESSED_BG);
    } else if hovered {
        ui.painter().rect_filled(rect, r, theme::HOVER_BG);
    }
}

/// A tool button that may hold several variants: a small triangle marks the corner. Returns
/// `(button, caret_clicked)`; the caller decides what a click on the caret (or a right-click)
/// means, typically opening a popup.
pub fn tool_slot_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    shortcut: Option<&str>,
    selected: bool,
    has_variants: bool,
) -> (Response, bool) {
    let resp = icon_button(ui, icon, label, shortcut, selected, true);
    let mut caret_clicked = false;
    if has_variants {
        let r = resp.rect;
        let tip = pos2(r.right() - 3.0, r.bottom() - 3.0);
        let tri = vec![tip, pos2(tip.x - 5.0, tip.y), pos2(tip.x, tip.y - 5.0)];
        ui.painter().add(egui::Shape::convex_polygon(tri, theme::TEXT_DIM, Stroke::NONE));
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                caret_clicked = p.x > r.right() - 10.0 && p.y > r.bottom() - 10.0;
            }
        }
    }
    (resp, caret_clicked)
}

/// A button showing an icon and a chevron, for dropdown menus (Effects, Canvas...).
pub fn dropdown_button(ui: &mut Ui, icon: Icon, label: &str, open: bool) -> Response {
    let size = vec2(BUTTON.x + 14.0, BUTTON.y);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        paint_button_bg(ui, rect, hovered || open, resp.is_pointer_button_down_on(), false);
        let ir = egui::Rect::from_center_size(
            pos2(rect.left() + 16.0, rect.center().y),
            Vec2::splat(ICON),
        );
        let colors = icon_colors(ui, true, hovered || open, false);
        icons::paint(ui.painter(), icon, ir, colors);
        let cr = egui::Rect::from_center_size(
            pos2(rect.right() - 9.0, rect.center().y),
            Vec2::splat(12.0),
        );
        icons::paint(ui.painter(), Icon::ChevronDown, cr, colors);
    }
    resp.on_hover_text(label)
}

/// A text-and-optional-icon button in the accent colour (Done, the primary action of a dialog).
pub fn accent_button(ui: &mut Ui, text: &str, icon: Option<Icon>) -> Response {
    accent_button_ex(ui, text, icon, true)
}

/// Gives inputs (drag values, combo boxes, toggles) a visible box, for dialogs and the
/// properties bar; the toolbar's flat look is kept elsewhere.
pub fn input_style(ui: &mut Ui) {
    let w = &mut ui.visuals_mut().widgets;
    w.inactive.weak_bg_fill = Color32::from_rgb(58, 60, 67);
    w.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(78, 81, 90));
    w.hovered.weak_bg_fill = Color32::from_rgb(72, 75, 84);
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(98, 102, 114));
}

/// [`accent_button`] that can be disabled.
pub fn accent_button_ex(ui: &mut Ui, text: &str, icon: Option<Icon>, enabled: bool) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::WHITE);
    let icon_w = if icon.is_some() { 20.0 } else { 0.0 };
    let size = vec2(galley.size().x + icon_w + 20.0, 26.0);
    let (rect, resp) =
        ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let fill = if !enabled {
            Color32::from_rgb(58, 62, 72)
        } else if resp.is_pointer_button_down_on() {
            theme::ACTIVE_BG
        } else if resp.hovered() {
            Color32::from_rgb(88, 158, 236)
        } else {
            theme::ACCENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
        let mut x = rect.left() + 10.0;
        if let Some(i) = icon {
            let ir =
                egui::Rect::from_center_size(pos2(x + 8.0, rect.center().y), Vec2::splat(16.0));
            icons::paint(ui.painter(), i, ir, IconColors::with_ink(Color32::WHITE));
            x += icon_w;
        }
        ui.painter().galley(
            pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            Color32::WHITE,
        );
    }
    resp
}

/// A thin vertical separator between toolbar groups.
pub fn separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, BUTTON.y), Sense::hover());
    let x = rect.center().x;
    ui.painter().line_segment(
        [pos2(x, rect.top() + 5.0), pos2(x, rect.bottom() - 5.0)],
        Stroke::new(1.0, Color32::from_rgb(70, 72, 80)),
    );
}

/// A small dimmed caption.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(theme::TEXT_DIM).size(11.5));
}

/// A segmented control over `options`; returns the newly chosen index.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: T, options: &[(T, &str)]) -> Option<T> {
    let mut chosen = None;
    ui.spacing_mut().item_spacing.x = 1.0;
    for (i, (value, label)) in options.iter().enumerate() {
        let selected = *value == current;
        let galley = ui.painter().layout_no_wrap(
            (*label).to_owned(),
            FontId::proportional(12.5),
            theme::TEXT,
        );
        let size = vec2(galley.size().x + 16.0, 22.0);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        resp.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, selected, *label));
        let r = CornerRadius {
            nw: if i == 0 { 5 } else { 0 },
            sw: if i == 0 { 5 } else { 0 },
            ne: if i + 1 == options.len() { 5 } else { 0 },
            se: if i + 1 == options.len() { 5 } else { 0 },
        };
        let fill = if selected {
            theme::ACTIVE_BG
        } else if resp.hovered() {
            theme::HOVER_BG
        } else {
            Color32::from_rgb(56, 58, 64)
        };
        ui.painter().rect_filled(rect, r, fill);
        if selected {
            ui.painter().rect_stroke(rect, r, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(12.5),
            if selected { Color32::WHITE } else { theme::TEXT },
        );
        if resp.clicked() && !selected {
            chosen = Some(*value);
        }
    }
    ui.spacing_mut().item_spacing.x = 6.0;
    chosen
}

/// A labelled slider row; returns `true` when the value changed.
pub fn slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
) -> bool {
    let mut changed = false;
    caption(ui, label);
    let r = ui.add(
        egui::Slider::new(value, range)
            .suffix(suffix.to_owned())
            .trailing_fill(true)
            .max_decimals(1),
    );
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Slider, true, label));
    changed |= r.changed();
    changed
}

/// Draws a chequerboard in `rect` (behind swatches and previews that may be transparent).
pub fn paint_checker(ui: &Ui, rect: egui::Rect, cell: f32) {
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, Color32::from_gray(200));
    let mut y = rect.top();
    let mut row = 0;
    while y < rect.bottom() {
        let mut x = rect.left() + if row % 2 == 0 { 0.0 } else { cell };
        while x < rect.right() {
            p.rect_filled(
                egui::Rect::from_min_size(pos2(x, y), Vec2::splat(cell)),
                0.0,
                Color32::from_gray(150),
            );
            x += cell * 2.0;
        }
        y += cell;
        row += 1;
    }
}
