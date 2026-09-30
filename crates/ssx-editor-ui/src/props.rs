//! The data model behind the properties bar.
//!
//! The bar edits "whatever is selected, or the active tool's remembered style when nothing is".
//! [`Props::current`] reads that into one value (style + kind + which controls apply),
//! widgets emit [`PropEdit`]s, and [`apply`] feeds them back through `EditorSession::set_style`
//! / `set_kind_props` so every edit is undoable and remembered per tool. Keeping this free of
//! egui makes every control's effect unit-testable.

use ssx_editor::{
    Color, EditorSession, Fill, Object, ObjectKind, Tool,
    object::{
        ArrowHeads, BuiltinSticker, CursorKind, GridPattern, HeadStyle, StickerSource, TextAlign,
        TextOutline,
    },
    style::{BlendMode, DashStyle, Shadow, Style},
};

/// A colour input somewhere in the UI; also the target of the eyedropper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorField {
    /// Outline / line colour.
    Stroke,
    /// Interior fill.
    Fill,
    /// Text glyph colour.
    Text,
    /// Text outline colour.
    TextOutline,
    /// Text background box.
    TextBackground,
    /// Drop shadow colour.
    Shadow,
    /// Spotlight dim colour.
    SpotlightDim,
    /// Digit colour of a step marker.
    StepText,
    /// Canvas background.
    Canvas,
    /// Highlighter colour (stroke and fill together).
    Highlight,
    /// A colour parameter of an effect dialog.
    Effect,
}

/// Which controls apply to the current selection/tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names)] // the names are the UI's vocabulary
pub struct Controls {
    /// Stroke colour.
    pub stroke: bool,
    /// Stroke width slider.
    pub stroke_width: bool,
    /// Dash pattern.
    pub dash: bool,
    /// Fill.
    pub fill: bool,
    /// Opacity.
    pub opacity: bool,
    /// Shadow.
    pub shadow: bool,
    /// Corner radius.
    pub corner: bool,
    /// Font, size, alignment, outline, background.
    pub text: bool,
    /// Arrow heads.
    pub arrow: bool,
    /// Step marker options.
    pub step: bool,
    /// Blur radius / pixel size.
    pub amount: bool,
    /// Magnifier options.
    pub magnify: bool,
    /// Spotlight options.
    pub spotlight: bool,
    /// Grid pattern and spacing.
    pub grid: bool,
    /// Cursor stamp options.
    pub cursor: bool,
    /// Balloon tail.
    pub balloon: bool,
    /// Sticker picker.
    pub sticker: bool,
    /// Highlighter colour.
    pub highlight: bool,
    /// Freehand smoothing.
    pub smooth: bool,
}

impl Controls {
    /// The controls that make sense for `kind`.
    pub fn for_kind(kind: &ObjectKind) -> Controls {
        let base = Controls { opacity: true, shadow: true, ..Controls::default() };
        match kind {
            ObjectKind::Rectangle(_) => Controls {
                stroke: true,
                stroke_width: true,
                dash: true,
                fill: true,
                corner: true,
                ..base
            },
            ObjectKind::Ellipse(_) => {
                Controls { stroke: true, stroke_width: true, dash: true, fill: true, ..base }
            }
            ObjectKind::Line(_) => Controls { stroke: true, stroke_width: true, dash: true, ..base },
            ObjectKind::Arrow(_) => {
                Controls { stroke: true, stroke_width: true, dash: true, arrow: true, ..base }
            }
            ObjectKind::Freehand(f) => Controls {
                stroke: true,
                stroke_width: true,
                dash: true,
                arrow: f.arrow.is_some(),
                smooth: true,
                ..base
            },
            ObjectKind::Text(_) => Controls { text: true, ..base },
            ObjectKind::Balloon(_) => Controls {
                text: true,
                stroke: true,
                stroke_width: true,
                fill: true,
                corner: true,
                balloon: true,
                ..base
            },
            ObjectKind::Step(_) => Controls {
                stroke: true,
                stroke_width: true,
                fill: true,
                step: true,
                ..base
            },
            ObjectKind::Magnify(_) => {
                Controls { stroke: true, stroke_width: true, magnify: true, ..base }
            }
            ObjectKind::Spotlight(_) => Controls { spotlight: true, ..Controls::default() },
            ObjectKind::Blur(_) | ObjectKind::Pixelate(_) => {
                Controls { amount: true, ..Controls::default() }
            }
            ObjectKind::Highlight(h) => Controls {
                highlight: true,
                stroke_width: !h.points.is_empty(),
                opacity: true,
                ..Controls::default()
            },
            ObjectKind::Image(_) => Controls { stroke: true, stroke_width: true, ..base },
            ObjectKind::Sticker(_) => Controls { fill: true, sticker: true, ..base },
            ObjectKind::Cursor(_) => {
                Controls { stroke: true, fill: true, cursor: true, ..base }
            }
            ObjectKind::Grid(_) => {
                Controls { stroke: true, stroke_width: true, grid: true, opacity: true, ..Controls::default() }
            }
            ObjectKind::Unknown(_) => Controls::default(),
        }
    }

    /// Controls shared by two sets (for a multi-selection of different kinds).
    pub fn intersect(self, o: Controls) -> Controls {
        Controls {
            stroke: self.stroke && o.stroke,
            stroke_width: self.stroke_width && o.stroke_width,
            dash: self.dash && o.dash,
            fill: self.fill && o.fill,
            opacity: self.opacity && o.opacity,
            shadow: self.shadow && o.shadow,
            corner: self.corner && o.corner,
            text: self.text && o.text,
            arrow: self.arrow && o.arrow,
            step: self.step && o.step,
            amount: self.amount && o.amount,
            magnify: self.magnify && o.magnify,
            spotlight: self.spotlight && o.spotlight,
            grid: self.grid && o.grid,
            cursor: self.cursor && o.cursor,
            balloon: self.balloon && o.balloon,
            sticker: self.sticker && o.sticker,
            highlight: self.highlight && o.highlight,
            smooth: self.smooth && o.smooth,
        }
    }

    /// `true` when nothing is editable.
    pub fn is_empty(&self) -> bool {
        *self == Controls::default()
    }
}

/// The current values the properties bar shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Props {
    /// Common style.
    pub style: Style,
    /// Kind-specific data of the first selected object (or the tool's template).
    pub kind: ObjectKind,
    /// What can be edited.
    pub controls: Controls,
    /// `true` when editing selected objects, `false` when editing the tool's defaults.
    pub from_selection: bool,
    /// How many objects are selected.
    pub count: usize,
}

impl Props {
    /// Reads the properties to show for `tool` given the session's selection.
    pub fn current(session: &EditorSession, tool: Tool) -> Option<Props> {
        let doc = session.document();
        let objs: Vec<&Object> =
            session.selection().iter().filter_map(|id| doc.object(*id)).collect();
        if let Some(first) = objs.first() {
            let controls = objs
                .iter()
                .map(|o| Controls::for_kind(&o.kind))
                .reduce(Controls::intersect)
                .unwrap_or_default();
            return Some(Props {
                style: first.style.clone(),
                kind: first.kind.clone(),
                controls,
                from_selection: true,
                count: objs.len(),
            });
        }
        let preset = session.styles().get(tool)?;
        Some(Props {
            controls: Controls::for_kind(&preset.kind),
            style: preset.style,
            kind: preset.kind,
            from_selection: false,
            count: 0,
        })
    }

    /// The text content of the current kind, if it has any.
    pub fn text(&self) -> Option<&ssx_editor::object::TextContent> {
        self.kind.text_content()
    }

    /// The current arrow heads, if the kind has them.
    pub fn arrow_heads(&self) -> Option<ArrowHeads> {
        match &self.kind {
            ObjectKind::Arrow(a) => Some(a.heads),
            ObjectKind::Freehand(f) => f.arrow,
            _ => None,
        }
    }
}

/// One change made in the properties bar.
#[derive(Debug, Clone, PartialEq)]
pub enum PropEdit {
    /// Stroke colour.
    Stroke(Color),
    /// Stroke width.
    StrokeWidth(f32),
    /// Dash pattern.
    Dash(DashStyle),
    /// Fill (none, solid or gradient).
    Fill(Fill),
    /// Opacity 0-1.
    Opacity(f32),
    /// Shadow on/off with parameters.
    Shadow(Option<Shadow>),
    /// Corner radius.
    CornerRadius(f32),
    /// Blend mode.
    Blend(BlendMode),
    /// Font family.
    FontFamily(String),
    /// Font size.
    FontSize(f32),
    /// Bold.
    Bold(bool),
    /// Italic.
    Italic(bool),
    /// Text colour.
    TextColor(Color),
    /// Text alignment.
    Align(TextAlign),
    /// Text outline.
    TextOutline(Option<TextOutline>),
    /// Text background box.
    TextBackground(Option<Color>),
    /// Text padding.
    Padding(f32),
    /// Arrow heads.
    Arrow(ArrowHeads),
    /// Step diameter.
    StepDiameter(f32),
    /// Step digit colour.
    StepText(Color),
    /// Blur radius / pixel block size.
    Amount(f32),
    /// Magnifier zoom.
    MagnifyZoom(f32),
    /// Magnifier shape.
    MagnifyCircular(bool),
    /// Spotlight shape.
    SpotlightEllipse(bool),
    /// Spotlight dim colour.
    SpotlightDim(Color),
    /// Spotlight edge feather.
    SpotlightFeather(f32),
    /// Grid pattern.
    GridPattern(GridPattern),
    /// Grid spacing.
    GridSpacing(f32),
    /// Cursor artwork.
    CursorKind(CursorKind),
    /// Cursor size.
    CursorScale(f32),
    /// Balloon tail width.
    TailWidth(f32),
    /// Sticker artwork.
    Sticker(StickerSource),
    /// Highlighter colour (stroke and fill together).
    HighlightColor(Color),
    /// Freehand smoothing.
    Smooth(bool),
}

impl PropEdit {
    fn touches_style(&self) -> bool {
        matches!(
            self,
            PropEdit::Stroke(_)
                | PropEdit::StrokeWidth(_)
                | PropEdit::Dash(_)
                | PropEdit::Fill(_)
                | PropEdit::Opacity(_)
                | PropEdit::Shadow(_)
                | PropEdit::CornerRadius(_)
                | PropEdit::Blend(_)
                | PropEdit::HighlightColor(_)
        )
    }

    /// Applies the style part of the edit.
    pub fn apply_style(&self, s: &mut Style) {
        match self {
            PropEdit::Stroke(c) => s.stroke = *c,
            PropEdit::StrokeWidth(w) => s.stroke_width = *w,
            PropEdit::Dash(d) => s.dash = *d,
            PropEdit::Fill(f) => s.fill = *f,
            PropEdit::Opacity(o) => s.opacity = *o,
            PropEdit::Shadow(sh) => s.shadow = *sh,
            PropEdit::CornerRadius(r) => s.corner_radius = *r,
            PropEdit::Blend(b) => s.blend = *b,
            PropEdit::HighlightColor(c) => {
                s.stroke = *c;
                if s.solid_fill().is_some() {
                    s.fill = Fill::solid(*c);
                }
            }
            _ => {}
        }
    }

    /// Applies the kind-specific part of the edit.
    pub fn apply_kind(&self, k: &mut ObjectKind) {
        if let Some(t) = k.text_content_mut() {
            match self {
                PropEdit::FontFamily(f) => t.font.family.clone_from(f),
                PropEdit::FontSize(v) => t.font.size = *v,
                PropEdit::Bold(b) => t.font.bold = *b,
                PropEdit::Italic(i) => t.font.italic = *i,
                PropEdit::TextColor(c) => t.color = *c,
                PropEdit::Align(a) => t.align = *a,
                PropEdit::TextOutline(o) => t.outline = *o,
                PropEdit::TextBackground(b) => t.background = *b,
                PropEdit::Padding(p) => t.padding = *p,
                _ => {}
            }
        }
        match (self, k) {
            (PropEdit::Arrow(h), ObjectKind::Arrow(a)) => a.heads = *h,
            (PropEdit::Arrow(h), ObjectKind::Freehand(f)) if f.arrow.is_some() => {
                f.arrow = Some(*h);
            }
            (PropEdit::Smooth(s), ObjectKind::Freehand(f)) => f.smooth = *s,
            (PropEdit::StepDiameter(d), ObjectKind::Step(s)) => s.diameter = *d,
            (PropEdit::StepText(c), ObjectKind::Step(s)) => s.text_color = *c,
            (PropEdit::Amount(v), ObjectKind::Blur(b) | ObjectKind::Pixelate(b)) => b.amount = *v,
            (PropEdit::MagnifyZoom(z), ObjectKind::Magnify(m)) => m.zoom = *z,
            (PropEdit::MagnifyCircular(c), ObjectKind::Magnify(m)) => m.circular = *c,
            (PropEdit::SpotlightEllipse(e), ObjectKind::Spotlight(s)) => s.ellipse = *e,
            (PropEdit::SpotlightDim(c), ObjectKind::Spotlight(s)) => s.dim = *c,
            (PropEdit::SpotlightFeather(f), ObjectKind::Spotlight(s)) => s.feather = *f,
            (PropEdit::GridPattern(p), ObjectKind::Grid(g)) => g.pattern = *p,
            (PropEdit::GridSpacing(v), ObjectKind::Grid(g)) => g.spacing = *v,
            (PropEdit::CursorKind(c), ObjectKind::Cursor(cur)) => cur.kind = *c,
            (PropEdit::CursorScale(s), ObjectKind::Cursor(cur)) => cur.scale = *s,
            (PropEdit::TailWidth(w), ObjectKind::Balloon(b)) => b.tail_width = *w,
            (PropEdit::Sticker(src), ObjectKind::Sticker(s)) => s.source = src.clone(),
            _ => {}
        }
    }
}

/// Applies `edit` to the selection (or the tool's remembered style when nothing is selected).
pub fn apply(session: &mut EditorSession, edit: &PropEdit) {
    if edit.touches_style() {
        let e = edit.clone();
        session.set_style(move |s| e.apply_style(s));
    }
    let e = edit.clone();
    session.set_kind_props(move |k| e.apply_kind(k));
}

/// The built-in stickers in picker order.
pub const BUILTIN_STICKERS: [BuiltinSticker; 9] = [
    BuiltinSticker::Check,
    BuiltinSticker::Cross,
    BuiltinSticker::Star,
    BuiltinSticker::Heart,
    BuiltinSticker::Exclamation,
    BuiltinSticker::Plus,
    BuiltinSticker::Minus,
    BuiltinSticker::ArrowRight,
    BuiltinSticker::Bolt,
];

/// Symbols offered as glyph stickers; all are in the WGL4 set the bundled Liberation Sans
/// covers, so they render identically everywhere (colour emoji fonts are not bundled).
pub const GLYPH_STICKERS: [&str; 24] = [
    "☺", "☻", "♥", "♦", "♣", "♠", "★", "☼", "♪", "♫", "←", "↑", "→", "↓", "↔", "↕", "●", "○", "■",
    "□", "▲", "▼", "©", "®",
];

/// Human name of a built-in sticker.
pub fn sticker_name(s: BuiltinSticker) -> &'static str {
    match s {
        BuiltinSticker::Check => "Check",
        BuiltinSticker::Cross => "Cross",
        BuiltinSticker::Star => "Star",
        BuiltinSticker::Heart => "Heart",
        BuiltinSticker::Exclamation => "Exclamation",
        BuiltinSticker::Plus => "Plus",
        BuiltinSticker::Minus => "Minus",
        BuiltinSticker::ArrowRight => "Arrow",
        BuiltinSticker::Bolt => "Bolt",
    }
}

/// Head styles in picker order.
pub const HEAD_STYLES: [(HeadStyle, &str); 6] = [
    (HeadStyle::None, "None"),
    (HeadStyle::Open, "Open"),
    (HeadStyle::Filled, "Filled"),
    (HeadStyle::Diamond, "Diamond"),
    (HeadStyle::Round, "Round"),
    (HeadStyle::Bar, "Bar"),
];

/// Applies the tweaks that make the text tool the "text with outline and background" variant:
/// only ever called with nothing selected, so it edits the tool's remembered style.
pub fn make_text_boxed(session: &mut EditorSession) {
    debug_assert!(session.tool() == Tool::Text);
    let outline = crate::tools::boxed_text_outline();
    session.set_kind_props(move |k| {
        if let Some(t) = k.text_content_mut() {
            t.color = Color::WHITE;
            t.outline = Some(outline);
            t.background = Some(Color::rgba(20, 20, 20, 200));
            t.padding = 6.0;
        }
    });
}

#[cfg(test)]
mod tests {
    use ssx_editor::{Modifiers, PointF, object::Axis};
    use ssx_imgfx::solid_frame;

    use super::*;

    fn session() -> EditorSession {
        EditorSession::from_frame(solid_frame(300, 200, [230, 230, 230, 255])).unwrap()
    }

    fn draw(s: &mut EditorSession, tool: Tool, a: (f32, f32), b: (f32, f32)) {
        s.set_tool(tool);
        s.pointer_down(PointF::new(a.0, a.1), Modifiers::NONE, None);
        s.pointer_move(PointF::new(b.0, b.1), Modifiers::NONE, None);
        s.pointer_up(PointF::new(b.0, b.1), Modifiers::NONE);
    }

    #[test]
    fn selected_object_wins_over_tool_defaults() {
        let mut s = session();
        draw(&mut s, Tool::Rectangle, (10.0, 10.0), (100.0, 80.0));
        let p = Props::current(&s, Tool::Rectangle).unwrap();
        assert!(p.from_selection && p.count == 1);
        assert!(p.controls.fill && p.controls.corner && !p.controls.text);
        s.clear_selection();
        let p = Props::current(&s, Tool::Ellipse).unwrap();
        assert!(!p.from_selection);
        assert!(p.controls.fill && !p.controls.corner, "ellipses have no corner radius");
        assert!(Props::current(&s, Tool::Select).is_none(), "select has no defaults to edit");
    }

    #[test]
    fn edits_reach_the_selected_object_and_are_undoable() {
        let mut s = session();
        draw(&mut s, Tool::Rectangle, (10.0, 10.0), (100.0, 80.0));
        apply(&mut s, &PropEdit::Stroke(Color::rgb(0, 0, 255)));
        apply(&mut s, &PropEdit::StrokeWidth(9.0));
        apply(&mut s, &PropEdit::Fill(Fill::solid(Color::rgb(1, 2, 3))));
        apply(&mut s, &PropEdit::CornerRadius(12.0));
        apply(&mut s, &PropEdit::Shadow(Some(Shadow::default())));
        let o = s.document().objects()[0].clone();
        assert_eq!(o.style.stroke, Color::rgb(0, 0, 255));
        assert_eq!(o.style.stroke_width, 9.0);
        assert_eq!(o.style.solid_fill(), Some(Color::rgb(1, 2, 3)));
        assert_eq!(o.style.corner_radius, 12.0);
        assert!(o.style.shadow.is_some());
        // The five style edits merged into one undo step; the creation is another.
        s.undo();
        assert_ne!(s.document().objects()[0].style.stroke_width, 9.0);
        assert!(s.document().objects()[0].style.shadow.is_none());
        s.undo();
        assert!(s.document().objects().is_empty());
        assert!(!s.can_undo());
    }

    #[test]
    fn tool_defaults_are_remembered_for_the_next_object() {
        let mut s = session();
        s.set_tool(Tool::Arrow);
        apply(&mut s, &PropEdit::Stroke(Color::rgb(0, 200, 0)));
        apply(&mut s, &PropEdit::Arrow(ArrowHeads { start: HeadStyle::Round, end: HeadStyle::Bar, ..ArrowHeads::default() }));
        draw(&mut s, Tool::Arrow, (10.0, 10.0), (200.0, 120.0));
        let o = &s.document().objects()[0];
        assert_eq!(o.style.stroke, Color::rgb(0, 200, 0));
        let ObjectKind::Arrow(a) = &o.kind else { panic!("not an arrow") };
        assert_eq!((a.heads.start, a.heads.end), (HeadStyle::Round, HeadStyle::Bar));
    }

    #[test]
    fn kind_specific_edits() {
        let mut s = session();
        draw(&mut s, Tool::Blur, (20.0, 20.0), (120.0, 90.0));
        apply(&mut s, &PropEdit::Amount(33.0));
        let ObjectKind::Blur(b) = &s.document().objects()[0].kind else { panic!() };
        assert_eq!(b.amount, 33.0);

        draw(&mut s, Tool::Step, (150.0, 100.0), (150.0, 100.0));
        let step_idx = s.document().objects().len() - 1;
        apply(&mut s, &PropEdit::StepDiameter(60.0));
        let ObjectKind::Step(st) = &s.document().objects()[step_idx].kind else { panic!() };
        assert_eq!(st.diameter, 60.0);

        // An edit that does not apply to the kind is a no-op.
        let before = s.document().objects()[step_idx].clone();
        apply(&mut s, &PropEdit::MagnifyZoom(9.0));
        assert_eq!(s.document().objects()[step_idx], before);
    }

    #[test]
    fn text_edits_change_font_and_colours() {
        let mut s = session();
        s.set_tool(Tool::Text);
        apply(&mut s, &PropEdit::FontSize(40.0));
        apply(&mut s, &PropEdit::Bold(true));
        apply(&mut s, &PropEdit::TextColor(Color::rgb(9, 9, 9)));
        let p = Props::current(&s, Tool::Text).unwrap();
        let t = p.text().unwrap();
        assert_eq!(t.font.size, 40.0);
        assert!(t.font.bold);
        assert_eq!(t.color, Color::rgb(9, 9, 9));
        assert!(p.controls.text && !p.controls.stroke);
    }

    #[test]
    fn boxed_text_preset_adds_outline_and_background() {
        let mut s = session();
        s.set_tool(Tool::Text);
        make_text_boxed(&mut s);
        let p = Props::current(&s, Tool::Text).unwrap();
        let t = p.text().unwrap();
        assert!(t.outline.is_some() && t.background.is_some());
        assert_eq!(t.color, Color::WHITE);
    }

    #[test]
    fn highlight_colour_sets_stroke_and_fill() {
        let mut s = session();
        s.set_tool(Tool::Highlight);
        apply(&mut s, &PropEdit::HighlightColor(Color::rgb(0, 255, 0)));
        let p = Props::current(&s, Tool::Highlight).unwrap();
        assert_eq!(p.style.stroke, Color::rgb(0, 255, 0));
        assert_eq!(p.style.solid_fill(), Some(Color::rgb(0, 255, 0)));
        assert!(p.controls.highlight && !p.controls.stroke_width, "rect highlighter has no width");
        s.set_tool(Tool::HighlightPen);
        let p = Props::current(&s, Tool::HighlightPen).unwrap();
        assert!(p.controls.stroke_width, "the pen has a width");
    }

    #[test]
    fn multi_selection_shows_only_common_controls() {
        let mut s = session();
        draw(&mut s, Tool::Rectangle, (10.0, 10.0), (60.0, 60.0));
        draw(&mut s, Tool::Blur, (100.0, 10.0), (160.0, 60.0));
        s.select_all();
        let p = Props::current(&s, Tool::Select).unwrap();
        assert_eq!(p.count, 2);
        assert!(!p.controls.amount && !p.controls.fill, "{:?}", p.controls);
    }

    #[test]
    fn every_kind_has_some_control_except_unknown() {
        for t in Tool::ALL {
            if let Some(preset) = t.preset() {
                assert!(!Controls::for_kind(&preset.kind).is_empty(), "{t:?}");
            }
        }
        assert!(Controls::for_kind(&ObjectKind::Unknown(serde_json::json!({}))).is_empty());
    }

    #[test]
    fn stickers_and_glyphs_are_available() {
        assert_eq!(BUILTIN_STICKERS.len(), 9);
        for b in BUILTIN_STICKERS {
            assert!(!sticker_name(b).is_empty());
        }
        let _ = Axis::X;
        assert!(GLYPH_STICKERS.iter().all(|g| g.chars().count() == 1));
    }
}
