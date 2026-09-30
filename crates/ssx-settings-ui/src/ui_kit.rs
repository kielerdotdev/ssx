//! The widgets every page is built from, in the editor's dark look.
//!
//! The colours and the accent button come from `ssx_editor_ui::ui::{theme, widgets}` so the
//! settings window and the editor read as one product; what is added here is what a settings
//! window needs and an editor does not: cards, labelled field rows with validation messages
//! underneath, switches, chips, code blocks, toasts and confirmation dialogs.
//!
//! Accessibility: a field's *label* is painted (not a separate widget) and attached to the
//! control with `widget_info`, so a screen reader announces "Quality, slider" once, not the
//! label and the control separately; every clickable thing is focusable and has a name.

use egui::{
    Align, Align2, Color32, Context, CornerRadius, FontId, Frame, Id, Layout, Margin, Modal,
    Response, RichText, Sense, Stroke, StrokeKind, Ui, WidgetInfo, WidgetType, pos2, vec2,
};
use ssx_core::settings::Severity;
use ssx_editor_ui::ui::{
    theme,
    widgets::{self, accent_button_ex},
};

use crate::validation::Issues;

/// Widest the page content grows.
pub const MAX_CONTENT_WIDTH: f32 = 900.0;
/// Width of the label column.
pub const LABEL_W: f32 = 178.0;

/// Secondary text. The editor's `TEXT_DIM` is a little too faint on the card background for
/// WCAG AA (4.5:1), so this window uses a slightly lighter grey.
pub const DIM_TEXT: Color32 = Color32::from_rgb(148, 152, 162);
/// The accent blue as text (links, badges): the editor's `ACCENT` is 4.2:1 on the cards.
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(104, 166, 240);
/// Card background.
pub const CARD_BG: Color32 = Color32::from_rgb(43, 44, 49);
/// Card outline.
pub const CARD_STROKE: Color32 = Color32::from_rgb(62, 64, 72);
/// Page background (the editor's canvas surround).
pub const PAGE_BG: Color32 = theme::CANVAS_BG;
/// Error text, light enough for 4.5:1 on the cards.
pub const ERROR_TEXT: Color32 = Color32::from_rgb(255, 128, 128);
/// Warning text.
pub const WARN_TEXT: Color32 = Color32::from_rgb(236, 190, 92);
/// Success text.
pub const OK_TEXT: Color32 = Color32::from_rgb(112, 208, 146);

/// Text colour for a severity.
pub const fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Error => ERROR_TEXT,
        Severity::Warning => WARN_TEXT,
    }
}

/// Paints the severity marker (a filled circle with `!`) at `centre`, without any font glyph.
pub fn paint_severity(painter: &egui::Painter, centre: egui::Pos2, s: Severity) {
    let c = severity_color(s);
    painter.circle_filled(centre, 6.0, c);
    let ink = Color32::from_rgb(30, 30, 34);
    painter.line_segment(
        [pos2(centre.x, centre.y - 3.0), pos2(centre.x, centre.y + 0.8)],
        Stroke::new(1.6, ink),
    );
    painter.circle_filled(pos2(centre.x, centre.y + 3.0), 0.95, ink);
}

/// Draws a small round severity marker in the layout.
pub fn severity_icon(ui: &mut Ui, s: Severity) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
    paint_severity(ui.painter(), pos2(rect.center().x, rect.center().y + 0.5), s);
}

/// A one-row text layout that ends in an ellipsis instead of wrapping.
pub fn truncated(text: &str, font: FontId, color: Color32, width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: width.max(8.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('\u{2026}'),
    };
    job
}

/// A two-line list entry (title over a dimmed subtitle) that highlights when selected.
/// `a11y` is the accessible name; `marker` shows a severity dot on the right.
pub fn list_item(
    ui: &mut Ui,
    selected: bool,
    title: &str,
    subtitle: &str,
    marker: Option<Severity>,
    a11y: &str,
) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width().max(160.0), 44.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, selected, a11y));
    if ui.is_rect_visible(rect) {
        let r = CornerRadius::same(5);
        if selected {
            ui.painter().rect_filled(rect, r, theme::ACTIVE_BG);
            ui.painter().rect_stroke(rect, r, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, r, theme::HOVER_BG.gamma_multiply(0.7));
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, r, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
        }
        let text_w = rect.width() - 22.0 - if marker.is_some() { 20.0 } else { 0.0 };
        let title_job = truncated(
            title,
            FontId::proportional(13.5),
            if selected { Color32::WHITE } else { theme::TEXT },
            text_w,
        );
        let g = ui.painter().layout_job(title_job);
        ui.painter().galley(pos2(rect.left() + 11.0, rect.top() + 6.0), g, theme::TEXT);
        let sub_job = truncated(
            subtitle,
            FontId::proportional(11.5),
            if selected { Color32::from_rgb(196, 208, 230) } else { DIM_TEXT },
            text_w,
        );
        let g = ui.painter().layout_job(sub_job);
        ui.painter().galley(pos2(rect.left() + 11.0, rect.top() + 25.0), g, DIM_TEXT);
        if let Some(sev) = marker {
            paint_severity(ui.painter(), pos2(rect.right() - 14.0, rect.center().y), sev);
        }
    }
    resp
}

/// A titled card with the page's standard padding.
pub fn card<R>(ui: &mut Ui, title: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    let out = Frame::new()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(t) = title {
                ui.label(RichText::new(t).strong().size(14.5).color(Color32::WHITE));
                ui.add_space(6.0);
            }
            add(ui)
        })
        .inner;
    ui.add_space(12.0);
    out
}

/// A dimmed, wrapping help line.
pub fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(DIM_TEXT)).wrap());
}

/// A dimmed, wrapping help line with a monospace part removed: plain text only.
pub fn hint_rich(ui: &mut Ui, text: impl Into<RichText>) {
    ui.add(egui::Label::new(text.into().size(12.0).color(DIM_TEXT)).wrap());
}

/// The validation messages of `path`, one line each with a severity marker.
pub fn issue_lines(ui: &mut Ui, issues: &Issues, path: &str) {
    for i in issues.at(path) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            severity_icon(ui, i.severity);
            ui.add(
                egui::Label::new(
                    RichText::new(&i.message).size(12.0).color(severity_color(i.severity)),
                )
                .wrap(),
            );
        });
    }
}

/// The validation messages that belong to `path` itself, not to something inside it.
pub fn issue_lines_exact(ui: &mut Ui, issues: &Issues, path: &str) {
    for i in issues.exactly_at(path) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            severity_icon(ui, i.severity);
            ui.add(
                egui::Label::new(
                    RichText::new(&i.message).size(12.0).color(severity_color(i.severity)),
                )
                .wrap(),
            );
        });
    }
}

/// A labelled field: label column, the control, then its validation messages and help.
#[derive(Debug)]
pub struct Field<'a> {
    label: &'a str,
    help: Option<&'a str>,
    issues: Option<(&'a Issues, &'a str)>,
    required: bool,
    label_width: f32,
}

impl<'a> Field<'a> {
    /// A field called `label`.
    pub fn new(label: &'a str) -> Self {
        Self { label, help: None, issues: None, required: false, label_width: LABEL_W }
    }

    /// A help line under the control.
    pub fn help(mut self, help: &'a str) -> Self {
        self.help = Some(help);
        self
    }

    /// Shows the validator's findings for `path` under the control.
    pub fn issues(mut self, issues: &'a Issues, path: &'a str) -> Self {
        self.issues = Some((issues, path));
        self
    }

    /// Marks the field as required.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// A wider (or narrower) label column, for long labels.
    pub fn label_width(mut self, w: f32) -> Self {
        self.label_width = w;
        self
    }

    /// Lays the field out; `add` draws the control(s).
    pub fn show<R>(self, ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(self.label_width, 26.0), Sense::hover());
            let label =
                if self.required { format!("{} *", self.label) } else { self.label.to_owned() };
            let job =
                truncated(&label, FontId::proportional(13.0), theme::TEXT, self.label_width - 8.0);
            let g = ui.painter().layout_job(job);
            ui.painter().galley(
                pos2(rect.left(), rect.center().y - g.size().y / 2.0),
                g,
                theme::TEXT,
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let r = add(ui);
                if let Some((issues, path)) = self.issues {
                    issue_lines(ui, issues, path);
                }
                if let Some(h) = self.help {
                    hint(ui, h);
                }
                r
            })
            .inner
        })
        .inner
    }
}

/// A single-line text box named `label` for screen readers.
pub fn text_input(
    ui: &mut Ui,
    label: &str,
    text: &mut String,
    hint_text: &str,
    width: f32,
) -> Response {
    widgets::input_style(ui);
    let r = ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint_text)
            .desired_width(width)
            .margin(Margin::symmetric(6, 4)),
    );
    r.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, label));
    r
}

/// A multi-line, read-only monospace block with a Copy button. Returns `true` when copied.
pub fn code_block(ui: &mut Ui, id: &str, text: &str, max_height: f32) -> bool {
    let mut copied = false;
    Frame::new()
        .fill(theme::CANVAS_BG)
        .stroke(Stroke::new(1.0, Color32::from_rgb(52, 54, 60)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt(id).max_height(max_height).show(ui, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(text).monospace().size(12.0).color(theme::TEXT))
                        .selectable(true),
                );
            });
        });
    ui.horizontal(|ui| {
        if ui.button("Copy").on_hover_text("Copy to the clipboard").clicked() {
            ui.ctx().copy_text(text.to_owned());
            copied = true;
        }
    });
    copied
}

/// An on/off switch with a text label to its right; the whole row is clickable.
pub fn switch(ui: &mut Ui, label: &str, on: &mut bool) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = vec2(38.0 + 8.0 + galley.size().x + 4.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let checked = *on;
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, checked, label));
    if ui.is_rect_visible(rect) {
        let track = egui::Rect::from_min_size(rect.min + vec2(0.0, 2.0), vec2(38.0, 18.0));
        let fill = if checked {
            theme::ACCENT
        } else if resp.hovered() {
            Color32::from_rgb(84, 87, 96)
        } else {
            Color32::from_rgb(68, 71, 79)
        };
        ui.painter().rect_filled(track, CornerRadius::same(9), fill);
        let knob_x = if checked { track.right() - 9.0 } else { track.left() + 9.0 };
        ui.painter().circle_filled(pos2(knob_x, track.center().y), 7.0, Color32::WHITE);
        if resp.has_focus() {
            ui.painter().rect_stroke(
                track.expand(2.0),
                CornerRadius::same(11),
                Stroke::new(1.5, Color32::WHITE),
                StrokeKind::Outside,
            );
        }
        ui.painter().galley(
            pos2(track.right() + 8.0, rect.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT,
        );
    }
    resp
}

/// The editor's segmented control laid out on one row; returns the newly chosen value.
pub fn segmented_row<T: PartialEq + Copy>(
    ui: &mut Ui,
    current: T,
    options: &[(T, &str)],
) -> Option<T> {
    ui.horizontal(|ui| widgets::segmented(ui, current, options)).inner
}

/// A rounded, selectable chip (filters, token buttons).
pub fn chip(ui: &mut Ui, text: &str, selected: bool) -> Response {
    chip_padded(ui, text, selected, 18.0)
}

/// A chip with less padding, for dense rows.
pub fn chip_compact(ui: &mut Ui, text: &str, selected: bool) -> Response {
    chip_padded(ui, text, selected, 12.0)
}

fn chip_padded(ui: &mut Ui, text: &str, selected: bool, pad: f32) -> Response {
    let font = FontId::proportional(12.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), theme::TEXT);
    let size = vec2(galley.size().x + pad, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, text));
    if ui.is_rect_visible(rect) {
        let (fill, stroke) = if selected {
            (theme::ACTIVE_BG, Stroke::new(1.0, theme::ACCENT))
        } else if resp.hovered() {
            (theme::HOVER_BG, Stroke::new(1.0, Color32::from_rgb(90, 93, 102)))
        } else {
            (Color32::from_rgb(52, 54, 60), Stroke::new(1.0, Color32::from_rgb(70, 73, 82)))
        };
        ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
        ui.painter().rect_stroke(rect, CornerRadius::same(12), stroke, StrokeKind::Inside);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            font,
            if selected { Color32::WHITE } else { theme::TEXT },
        );
        if resp.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(1.5),
                CornerRadius::same(13),
                Stroke::new(1.5, Color32::WHITE),
                StrokeKind::Outside,
            );
        }
    }
    resp
}

/// A small coloured badge.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let font = FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 12.0, 18.0), Sense::hover());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    ui.painter().rect_filled(rect, CornerRadius::same(9), color.gamma_multiply(0.18));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, color.gamma_multiply(0.55)),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

/// A normal button with a minimum width.
pub fn button(ui: &mut Ui, text: &str) -> Response {
    widgets::input_style(ui);
    ui.add(egui::Button::new(text).min_size(vec2(72.0, 26.0)))
}

/// A button that can be disabled, with the reason as a tooltip when it is.
pub fn button_if(ui: &mut Ui, text: &str, enabled: bool, why_not: &str) -> Response {
    widgets::input_style(ui);
    let r = ui.add_enabled(enabled, egui::Button::new(text).min_size(vec2(72.0, 26.0)));
    if enabled { r } else { r.on_disabled_hover_text(why_not) }
}

/// The accent-coloured primary button.
pub fn primary(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    accent_button_ex(ui, text, None, enabled)
}

/// A red outlined button for destructive actions.
pub fn danger(ui: &mut Ui, text: &str) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, ERROR_TEXT);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 26.0), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    let fill =
        if resp.hovered() { Color32::from_rgb(88, 44, 46) } else { Color32::from_rgb(62, 40, 42) };
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(1.0, ERROR_TEXT.gamma_multiply(0.7)),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        pos2(rect.left() + 11.0, rect.center().y - galley.size().y / 2.0),
        galley,
        ERROR_TEXT,
    );
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(1.5),
            CornerRadius::same(6),
            Stroke::new(1.5, Color32::WHITE),
            StrokeKind::Outside,
        );
    }
    resp
}

/// A compact icon button (24 x 22) for row actions: move up, move down, remove. `tip` is the
/// tooltip and the accessible name.
pub fn mini_icon_button(
    ui: &mut Ui,
    icon: ssx_editor_ui::icons::Icon,
    tip: &str,
    enabled: bool,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(24.0, 22.0),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, tip));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered() && enabled;
        if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(4), theme::HOVER_BG);
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(1.5, Color32::WHITE),
                StrokeKind::Inside,
            );
        }
        let ink = if !enabled {
            DIM_TEXT.gamma_multiply(0.5)
        } else if hovered {
            Color32::WHITE
        } else {
            theme::TEXT
        };
        ssx_editor_ui::icons::paint(
            ui.painter(),
            icon,
            egui::Rect::from_center_size(rect.center(), vec2(15.0, 15.0)),
            ssx_editor_ui::icons::IconColors::with_ink(ink),
        );
    }
    if enabled { resp.on_hover_text(tip) } else { resp }
}

/// A borderless text-like button (links inside a sentence, "Show" toggles).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let r = ui.add(
        egui::Label::new(RichText::new(text).color(ACCENT_TEXT).underline()).sense(Sense::click()),
    );
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Link, true, text));
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A horizontal rule between groups inside a card.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(58, 60, 67));
    ui.add_space(6.0);
}

/// Gives a page the standard scroll area with a centred column of at most
/// [`MAX_CONTENT_WIDTH`].
pub fn page_scroll(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_width();
        let width = avail.min(MAX_CONTENT_WIDTH);
        ui.horizontal_top(|ui| {
            ui.add_space(((avail - width) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(width - 4.0);
                add(ui);
            });
        });
    });
}

/// A page title and blurb.
pub fn page_header(ui: &mut Ui, title: &str, blurb: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).size(20.0).strong().color(Color32::WHITE));
    hint(ui, blurb);
    ui.add_space(12.0);
}

// ---- toasts -------------------------------------------------------------------------------

/// How a toast looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// Something worked.
    Success,
    /// Something is worth knowing.
    Info,
    /// Something failed.
    Error,
}

/// One message.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    /// The text.
    pub text: String,
    /// Look.
    pub kind: ToastKind,
    /// When it disappears (egui time, seconds).
    pub until: f64,
}

/// The queue of toasts.
#[derive(Debug, Default, Clone)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// Adds a message shown for `secs` after `now`.
    pub fn push(&mut self, now: f64, kind: ToastKind, text: impl Into<String>, secs: f64) {
        self.items.push(Toast { text: text.into(), kind, until: now + secs });
        if self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    /// Adds a success message.
    pub fn success(&mut self, now: f64, text: impl Into<String>) {
        self.push(now, ToastKind::Success, text, 3.5);
    }

    /// Adds an informational message.
    pub fn info(&mut self, now: f64, text: impl Into<String>) {
        self.push(now, ToastKind::Info, text, 4.5);
    }

    /// Adds an error message (stays longer).
    pub fn error(&mut self, now: f64, text: impl Into<String>) {
        self.push(now, ToastKind::Error, text, 9.0);
    }

    /// Drops what has expired; returns when the next one expires.
    pub fn expire(&mut self, now: f64) -> Option<f64> {
        self.items.retain(|t| t.until > now);
        self.items.iter().map(|t| t.until - now).min_by(f64::total_cmp)
    }

    /// The messages, oldest first.
    pub fn items(&self) -> &[Toast] {
        &self.items
    }

    /// Removes all.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Draws the toasts at the bottom right and keeps the frame loop alive until they expire.
pub fn show_toasts(ctx: &Context, toasts: &mut Toasts) {
    let now = ctx.input(|i| i.time);
    if let Some(next) = toasts.expire(now) {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(next.clamp(0.05, 1.0)));
    }
    if toasts.items().is_empty() {
        return;
    }
    egui::Area::new(Id::new("ssx-toasts"))
        .order(egui::Order::Foreground)
        .anchor(Align2::RIGHT_BOTTOM, vec2(-16.0, -64.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                for t in toasts.items().iter().rev() {
                    let (accent, text_col) = match t.kind {
                        ToastKind::Success => (OK_TEXT, theme::TEXT),
                        ToastKind::Info => (theme::ACCENT, theme::TEXT),
                        ToastKind::Error => (ERROR_TEXT, theme::TEXT),
                    };
                    Frame::new()
                        .fill(Color32::from_rgb(52, 54, 60))
                        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.8)))
                        .corner_radius(CornerRadius::same(7))
                        .inner_margin(Margin::symmetric(12, 8))
                        .shadow(egui::Shadow {
                            offset: [0, 3],
                            blur: 10,
                            spread: 0,
                            color: Color32::from_black_alpha(100),
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(RichText::new(&t.text).color(text_col));
                        });
                }
            });
        });
}

// ---- dialogs ------------------------------------------------------------------------------

/// The answer of a confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The confirm button.
    Confirm,
    /// The cancel button, Esc, or a click outside.
    Cancel,
}

/// A modal confirmation with a confirm and a cancel button. `Esc` cancels. Returns the answer
/// in the frame it is given.
pub fn confirm(
    ctx: &Context,
    id: &str,
    title: &str,
    body: &str,
    confirm_label: &str,
    destructive: bool,
) -> Option<Answer> {
    let mut answer = None;
    let frame = Frame::popup(&ctx.global_style()).inner_margin(Margin::same(18));
    let m = Modal::new(Id::new(("ssx-confirm", id))).frame(frame).show(ctx, |ui| {
        widgets::input_style(ui);
        ui.set_width(420.0);
        ui.label(RichText::new(title).heading().strong());
        ui.add_space(6.0);
        ui.add(egui::Label::new(body).wrap());
        ui.add_space(12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let ok = if destructive {
                danger(ui, confirm_label)
            } else {
                primary(ui, confirm_label, true)
            };
            if ok.clicked() {
                answer = Some(Answer::Confirm);
            }
            if button(ui, "Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if answer.is_none() && m.should_close() {
        answer = Some(Answer::Cancel);
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_expire_in_order_and_are_capped() {
        let mut t = Toasts::default();
        t.success(0.0, "a");
        t.error(0.0, "b");
        assert_eq!(t.items().len(), 2);
        let next = t.expire(1.0).unwrap();
        assert!((next - 2.5).abs() < 1e-9, "{next}");
        assert_eq!(t.expire(4.0), Some(5.0));
        assert_eq!(t.items().len(), 1, "the success toast is gone, the error stays longer");
        assert_eq!(t.expire(10.0), None);
        assert!(t.items().is_empty());
        for i in 0..10 {
            t.info(0.0, format!("m{i}"));
        }
        assert_eq!(t.items().len(), 4);
        assert_eq!(t.items()[3].text, "m9");
        t.clear();
        assert!(t.items().is_empty());
    }

    #[test]
    fn severity_colours_are_distinct_and_readable() {
        assert_ne!(severity_color(Severity::Error), severity_color(Severity::Warning));
        fn lum(c: Color32) -> f32 {
            let f = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        let contrast = |a: Color32, b: Color32| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        for fg in [ERROR_TEXT, WARN_TEXT, OK_TEXT, theme::TEXT, DIM_TEXT, ACCENT_TEXT] {
            for bg in [CARD_BG, PAGE_BG] {
                assert!(contrast(fg, bg) >= 4.5, "{fg:?} on {bg:?}: {}", contrast(fg, bg));
            }
        }
    }
}
