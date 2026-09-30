//! The properties bar: edits whatever is selected, or the active tool's defaults.

use egui::{Popup, PopupCloseBehavior, RichText, Ui, vec2};
use ssx_editor::{
    Color, Fill, ObjectKind, Tool,
    object::{CursorKind, GridPattern, HeadStyle, StickerSource, TextAlign, TextOutline},
    style::{DashStyle, Shadow},
};

use super::{
    color::color_button,
    theme,
    widgets::{self, caption, icon_button, segmented},
};
use crate::{
    action::Action,
    document::EditorDoc,
    icons::Icon,
    props::{
        BUILTIN_STICKERS, ColorField, Controls, GLYPH_STICKERS, HEAD_STYLES, PropEdit, Props,
        sticker_name,
    },
    state::AppState,
    tools::ToolId,
};

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).strong().color(theme::TEXT));
    widgets::separator(ui);
}

fn dash_name(d: DashStyle) -> &'static str {
    match d {
        DashStyle::Solid => "Solid",
        DashStyle::Dash => "Dashed",
        DashStyle::Dot => "Dotted",
        DashStyle::DashDot => "Dash-dot",
    }
}

fn head_name(h: HeadStyle) -> &'static str {
    HEAD_STYLES.iter().find(|(s, _)| *s == h).map_or("?", |(_, n)| n)
}

/// Draws the bar; edits are queued as [`Action::Prop`].
pub fn show(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    let tool = state.tool;
    let engine_tool = tool.engine();
    let session = &doc.session;
    let props = Props::current(session, engine_tool);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 5.0);
        ui.spacing_mut().slider_width = 72.0;
        widgets::input_style(ui);
        ui.set_min_height(30.0);
        match (&props, tool) {
            (_, ToolId::CropRect | ToolId::CropEllipse | ToolId::CropFree) => crop_bar(ui, state, doc),
            (_, ToolId::CutOut) => {
                heading(ui, "Cut out");
                caption(ui, "Drag across the image: the strip you cover is removed and the two sides are joined.");
            }
            (_, ToolId::Eraser) => {
                heading(ui, "Eraser");
                caption(ui, "Drag over annotations to delete them. Nothing is deleted until you release the mouse.");
            }
            (_, ToolId::Image) => image_bar(ui, state),
            (None, _) => {
                heading(ui, "Select");
                caption(ui, "Click an object to edit its style. Drag to move it, use the handles to resize or rotate.");
                widgets::separator(ui);
                step_start(ui, state, doc);
            }
            (Some(p), _) => {
                let title = if p.from_selection {
                    if p.count > 1 { format!("{} objects", p.count) } else { pretty(p.kind.name()) }
                } else {
                    format!("{} (defaults)", tool.label())
                };
                heading(ui, &title);
                if p.controls.is_empty() {
                    caption(ui, "Nothing to edit for this selection.");
                } else {
                    controls(ui, state, doc, p);
                }
            }
        }
    });
}

fn pretty(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
}

fn crop_bar(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    let name = state.tool.label();
    heading(ui, name);
    if let Some(c) = doc.session.pending_crop() {
        caption(ui, &format!("{} x {} px", c.rect.w.round() as i32, c.rect.h.round() as i32));
        if widgets::accent_button(ui, "Apply crop", Some(Icon::Check)).clicked() {
            state.push(Action::ApplyPendingCrop);
        }
        if ui.button("Cancel").clicked() {
            state.push(Action::CancelPendingCrop);
        }
        if state.tool != ToolId::CropRect {
            caption(ui, "Annotations are flattened into the image for non-rectangular regions.");
        }
    } else {
        caption(ui, "Drag on the image to mark the region to keep, then press Enter.");
    }
}

fn image_bar(ui: &mut Ui, state: &mut AppState) {
    heading(ui, "Image");
    if ui.button("From file...").clicked() {
        state.push(Action::PickImageFile);
    }
    if ui.button("From clipboard").clicked() {
        state.push(Action::PickImageClipboard);
    }
    caption(
        ui,
        if state.has_pending_image {
            "Click on the picture to place the image."
        } else {
            "Choose an image, then click on the picture to place it."
        },
    );
}

fn step_start(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc) {
    caption(ui, "Step numbers start at");
    let mut n = doc.doc().step_start();
    if ui.add(egui::DragValue::new(&mut n).range(0..=999)).changed() {
        state.push(Action::SetStepStart(n));
    }
}

#[allow(clippy::too_many_lines)] // one flat list of controls reads better than many tiny fns
fn controls(ui: &mut Ui, state: &mut AppState, doc: &EditorDoc, p: &Props) {
    let c: Controls = p.controls;
    let mut edits: Vec<PropEdit> = Vec::new();
    let st = &p.style;

    if c.highlight {
        caption(ui, "Colour");
        if let Some(col) =
            color_button(ui, "Highlighter colour", st.stroke, ColorField::Highlight, state, false)
        {
            edits.push(PropEdit::HighlightColor(col));
        }
    }
    if c.stroke {
        caption(ui, if c.text { "Border" } else { "Colour" });
        if let Some(col) =
            color_button(ui, "Stroke colour", st.stroke, ColorField::Stroke, state, false)
        {
            edits.push(PropEdit::Stroke(col));
        }
    }
    if c.stroke_width {
        let mut w = st.stroke_width;
        if widgets::slider(ui, "Width", &mut w, 0.0..=40.0, " px") {
            edits.push(PropEdit::StrokeWidth(w));
        }
    }
    if c.dash {
        egui::ComboBox::from_id_salt("dash").selected_text(dash_name(st.dash)).width(80.0).show_ui(
            ui,
            |ui| {
                for d in [DashStyle::Solid, DashStyle::Dash, DashStyle::Dot, DashStyle::DashDot] {
                    if ui.selectable_label(st.dash == d, dash_name(d)).clicked() {
                        edits.push(PropEdit::Dash(d));
                    }
                }
            },
        );
    }
    if c.fill {
        caption(ui, "Fill");
        let cur = match st.fill {
            Fill::Solid { color } => color,
            _ => Color::TRANSPARENT,
        };
        if let Some(col) = color_button(ui, "Fill colour", cur, ColorField::Fill, state, true) {
            edits.push(PropEdit::Fill(if col.is_transparent() {
                Fill::None
            } else {
                Fill::solid(col)
            }));
        }
    }
    if c.corner {
        let mut r = st.corner_radius;
        if widgets::slider(ui, "Corners", &mut r, 0.0..=60.0, " px") {
            edits.push(PropEdit::CornerRadius(r));
        }
    }
    if c.smooth
        && let ObjectKind::Freehand(f) = &p.kind
    {
        let mut s = f.smooth;
        if ui.checkbox(&mut s, "Smooth").changed() {
            edits.push(PropEdit::Smooth(s));
        }
    }

    if c.text {
        text_controls(ui, state, p, &mut edits);
    }
    if c.arrow
        && let Some(h) = p.arrow_heads()
    {
        arrow_controls(ui, h, &mut edits);
    }
    if c.step {
        if let ObjectKind::Step(s) = &p.kind {
            let mut d = s.diameter;
            if widgets::slider(ui, "Size", &mut d, 16.0..=120.0, " px") {
                edits.push(PropEdit::StepDiameter(d));
            }
            caption(ui, "Digits");
            if let Some(col) = color_button(
                ui,
                "Step number colour",
                s.text_color,
                ColorField::StepText,
                state,
                false,
            ) {
                edits.push(PropEdit::StepText(col));
            }
        }
        widgets::separator(ui);
        step_start(ui, state, doc);
    }
    if c.amount {
        let (label, range) = match p.kind {
            ObjectKind::Blur(_) => ("Radius", 1.0..=60.0),
            _ => ("Pixel size", 2.0..=60.0),
        };
        if let ObjectKind::Blur(b) | ObjectKind::Pixelate(b) = &p.kind {
            let mut a = b.amount;
            if widgets::slider(ui, label, &mut a, range, " px") {
                edits.push(PropEdit::Amount(a));
            }
        }
    }
    if c.magnify
        && let ObjectKind::Magnify(m) = &p.kind
    {
        let mut z = m.zoom;
        if widgets::slider(ui, "Zoom", &mut z, 1.25..=8.0, "x") {
            edits.push(PropEdit::MagnifyZoom(z));
        }
        if let Some(v) = segmented(ui, m.circular, &[(true, "Round"), (false, "Square")]) {
            edits.push(PropEdit::MagnifyCircular(v));
        }
    }
    if c.spotlight
        && let ObjectKind::Spotlight(s) = &p.kind
    {
        if let Some(v) = segmented(ui, s.ellipse, &[(false, "Rectangle"), (true, "Ellipse")]) {
            edits.push(PropEdit::SpotlightEllipse(v));
        }
        caption(ui, "Dim");
        if let Some(col) =
            color_button(ui, "Dim colour", s.dim, ColorField::SpotlightDim, state, false)
        {
            edits.push(PropEdit::SpotlightDim(col));
        }
        let mut f = s.feather;
        if widgets::slider(ui, "Feather", &mut f, 0.0..=60.0, " px") {
            edits.push(PropEdit::SpotlightFeather(f));
        }
    }
    if c.grid
        && let ObjectKind::Grid(g) = &p.kind
    {
        egui::ComboBox::from_id_salt("grid-pattern")
            .selected_text(grid_name(g.pattern))
            .width(96.0)
            .show_ui(ui, |ui| {
                for pat in [
                    GridPattern::Grid,
                    GridPattern::HatchForward,
                    GridPattern::HatchBackward,
                    GridPattern::CrossHatch,
                    GridPattern::Dots,
                ] {
                    if ui.selectable_label(g.pattern == pat, grid_name(pat)).clicked() {
                        edits.push(PropEdit::GridPattern(pat));
                    }
                }
            });
        let mut s = g.spacing;
        if widgets::slider(ui, "Spacing", &mut s, 4.0..=80.0, " px") {
            edits.push(PropEdit::GridSpacing(s));
        }
    }
    if c.cursor
        && let ObjectKind::Cursor(cur) = &p.kind
    {
        if let Some(k) = segmented(
            ui,
            cur.kind,
            &[
                (CursorKind::Arrow, "Arrow"),
                (CursorKind::IBeam, "I-beam"),
                (CursorKind::Crosshair, "Cross"),
            ],
        ) {
            edits.push(PropEdit::CursorKind(k));
        }
        let mut s = cur.scale;
        if widgets::slider(ui, "Size", &mut s, 0.5..=4.0, "x") {
            edits.push(PropEdit::CursorScale(s));
        }
    }
    if c.balloon
        && let ObjectKind::Balloon(b) = &p.kind
    {
        let mut w = b.tail_width;
        if widgets::slider(ui, "Tail", &mut w, 4.0..=80.0, " px") {
            edits.push(PropEdit::TailWidth(w));
        }
    }
    if c.sticker
        && let ObjectKind::Sticker(s) = &p.kind
    {
        sticker_picker(ui, &s.source, &mut edits);
    }
    if c.opacity {
        let mut o = st.opacity * 100.0;
        if widgets::slider(ui, "Opacity", &mut o, 5.0..=100.0, "%") {
            edits.push(PropEdit::Opacity(o / 100.0));
        }
    }
    if c.shadow {
        shadow_controls(ui, state, st.shadow, &mut edits);
    }
    for e in edits {
        state.push(Action::Prop(e));
    }
}

fn grid_name(p: GridPattern) -> &'static str {
    match p {
        GridPattern::Grid => "Grid",
        GridPattern::HatchForward => "Hatch /",
        GridPattern::HatchBackward => "Hatch \\",
        GridPattern::CrossHatch => "Cross-hatch",
        GridPattern::Dots => "Dots",
    }
}

fn shadow_controls(
    ui: &mut Ui,
    state: &mut AppState,
    sh: Option<Shadow>,
    edits: &mut Vec<PropEdit>,
) {
    let mut on = sh.is_some();
    if ui.checkbox(&mut on, "Shadow").changed() {
        edits.push(PropEdit::Shadow(on.then(Shadow::default)));
    }
    if let Some(mut s) = sh {
        let mut changed = false;
        if let Some(c) =
            color_button(ui, "Shadow colour", s.color, ColorField::Shadow, state, false)
        {
            s.color = c;
            changed = true;
        }
        caption(ui, "X");
        changed |= ui.add(egui::DragValue::new(&mut s.dx).range(-40.0..=40.0).speed(0.3)).changed();
        caption(ui, "Y");
        changed |= ui.add(egui::DragValue::new(&mut s.dy).range(-40.0..=40.0).speed(0.3)).changed();
        caption(ui, "Blur");
        changed |= ui.add(egui::DragValue::new(&mut s.blur).range(0.0..=40.0).speed(0.2)).changed();
        if changed {
            edits.push(PropEdit::Shadow(Some(s)));
        }
    }
}

fn arrow_controls(ui: &mut Ui, h: ssx_editor::object::ArrowHeads, edits: &mut Vec<PropEdit>) {
    for (label, is_start) in [("Start", true), ("End", false)] {
        caption(ui, label);
        let cur = if is_start { h.start } else { h.end };
        egui::ComboBox::from_id_salt(("head", is_start))
            .selected_text(head_name(cur))
            .width(70.0)
            .show_ui(ui, |ui| {
                for (s, n) in HEAD_STYLES {
                    if ui.selectable_label(cur == s, n).clicked() {
                        let mut nh = h;
                        if is_start {
                            nh.start = s;
                        } else {
                            nh.end = s;
                        }
                        edits.push(PropEdit::Arrow(nh));
                    }
                }
            });
    }
    let mut size = h.size;
    if widgets::slider(ui, "Head", &mut size, 2.0..=12.0, "x") {
        edits.push(PropEdit::Arrow(ssx_editor::object::ArrowHeads { size, ..h }));
    }
}

fn text_controls(ui: &mut Ui, state: &mut AppState, p: &Props, edits: &mut Vec<PropEdit>) {
    let Some(t) = p.text() else { return };
    caption(ui, "Font");
    egui::ComboBox::from_id_salt("font-family").selected_text(&t.font.family).width(120.0).show_ui(
        ui,
        |ui| {
            for fam in ["Liberation Sans"] {
                if ui.selectable_label(t.font.family == fam, fam).clicked() {
                    edits.push(PropEdit::FontFamily(fam.into()));
                }
            }
            ui.label(
                RichText::new("Only the bundled font renders identically on every system.")
                    .small()
                    .weak(),
            );
        },
    );
    let mut size = t.font.size;
    if ui.add(egui::DragValue::new(&mut size).range(6.0..=300.0).speed(0.5).suffix(" px")).changed()
    {
        edits.push(PropEdit::FontSize(size));
    }
    let mut bold = t.font.bold;
    if ui.toggle_value(&mut bold, RichText::new("B").strong()).on_hover_text("Bold").changed() {
        edits.push(PropEdit::Bold(bold));
    }
    let mut italic = t.font.italic;
    if ui.toggle_value(&mut italic, RichText::new("I").italics()).on_hover_text("Italic").changed()
    {
        edits.push(PropEdit::Italic(italic));
    }
    for (a, icon, name) in [
        (TextAlign::Left, Icon::AlignLeft, "Align left"),
        (TextAlign::Center, Icon::AlignCenter, "Align centre"),
        (TextAlign::Right, Icon::AlignRight, "Align right"),
    ] {
        if icon_button(ui, icon, name, None, t.align == a, true).clicked() {
            edits.push(PropEdit::Align(a));
        }
    }
    caption(ui, "Colour");
    if let Some(c) = color_button(ui, "Text colour", t.color, ColorField::Text, state, false) {
        edits.push(PropEdit::TextColor(c));
    }
    // Outline.
    let mut has_outline = t.outline.is_some();
    if ui.checkbox(&mut has_outline, "Outline").changed() {
        edits.push(PropEdit::TextOutline(
            has_outline.then_some(TextOutline { color: Color::BLACK, width: 2.0 }),
        ));
    }
    if let Some(o) = t.outline {
        if let Some(c) =
            color_button(ui, "Outline colour", o.color, ColorField::TextOutline, state, false)
        {
            edits.push(PropEdit::TextOutline(Some(TextOutline { color: c, ..o })));
        }
        let mut w = o.width;
        if ui.add(egui::DragValue::new(&mut w).range(0.5..=20.0).speed(0.1).suffix(" px")).changed()
        {
            edits.push(PropEdit::TextOutline(Some(TextOutline { width: w, ..o })));
        }
    }
    // Background.
    let mut has_bg = t.background.is_some();
    if ui.checkbox(&mut has_bg, "Background").changed() {
        edits.push(PropEdit::TextBackground(has_bg.then(|| Color::rgba(20, 20, 20, 200))));
    }
    if let Some(bg) = t.background {
        if let Some(c) =
            color_button(ui, "Background colour", bg, ColorField::TextBackground, state, false)
        {
            edits.push(PropEdit::TextBackground(Some(c)));
        }
        let mut pad = t.padding;
        caption(ui, "Padding");
        if ui.add(egui::DragValue::new(&mut pad).range(0.0..=60.0).speed(0.3)).changed() {
            edits.push(PropEdit::Padding(pad));
        }
    }
}

fn sticker_picker(ui: &mut Ui, current: &StickerSource, edits: &mut Vec<PropEdit>) {
    let label = match current {
        StickerSource::Builtin { which } => sticker_name(*which).to_owned(),
        StickerSource::Glyph { text } => text.clone(),
        StickerSource::Bitmap { .. } => "Bitmap".to_owned(),
    };
    let b = ui.button(format!("Sticker: {label}"));
    Popup::from_toggle_button_response(&b).close_behavior(PopupCloseBehavior::CloseOnClick).show(
        |ui| {
            ui.set_min_width(240.0);
            widgets::caption(ui, "Shapes");
            ui.horizontal_wrapped(|ui| {
                for s in BUILTIN_STICKERS {
                    if ui.button(sticker_name(s)).clicked() {
                        edits.push(PropEdit::Sticker(StickerSource::Builtin { which: s }));
                    }
                }
            });
            widgets::caption(ui, "Symbols");
            ui.horizontal_wrapped(|ui| {
                for g in GLYPH_STICKERS {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(g).size(17.0))
                                .min_size(vec2(28.0, 28.0)),
                        )
                        .clicked()
                    {
                        edits.push(PropEdit::Sticker(StickerSource::Glyph { text: g.to_owned() }));
                    }
                }
            });
        },
    );
    let _ = Tool::Sticker;
}
