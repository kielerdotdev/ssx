//! Tools and their remembered ("last used") styles.
//!
//! Each object-creating tool owns a [`Preset`]: a [`Style`] plus a *template*
//! [`ObjectKind`] carrying the kind-specific options (font, arrow heads, blur amount, zoom...).
//! New objects are cloned from the preset; when the user changes the style of a selected
//! object the session writes it back, so the next object drawn with that tool looks the same.
//! [`StyleMemory`] is serialisable so a GUI can persist it between runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    geom::Color,
    object::{
        ArrowHeads, ArrowShape, BalloonShape, BoxShape, BuiltinSticker, CursorShape, EffectBox,
        FreehandShape, GridShape, HeadStyle, HighlightShape, ImageShape, LineShape, MagnifyShape,
        ObjectKind, SpotlightShape, StepShape, StickerShape, StickerSource, TextContent, TextShape,
    },
    style::{BlendMode, Fill, Shadow, Style},
};

/// The editor tools (one per `ShareX` toolbar entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// Select, move, resize, rotate.
    Select,
    /// Rectangle.
    Rectangle,
    /// Ellipse.
    Ellipse,
    /// Straight line.
    Line,
    /// Arrow.
    Arrow,
    /// Freehand pen.
    Freehand,
    /// Freehand pen with arrow heads.
    FreehandArrow,
    /// Text.
    Text,
    /// Speech balloon.
    Balloon,
    /// Auto-numbered step marker.
    Step,
    /// Magnifier lens.
    Magnify,
    /// Spotlight (dims the rest).
    Spotlight,
    /// Blur region.
    Blur,
    /// Pixelate region.
    Pixelate,
    /// Highlighter rectangle.
    Highlight,
    /// Highlighter pen (freehand marker).
    HighlightPen,
    /// Insert the pending image at the clicked position.
    Image,
    /// Sticker / emoji stamp.
    Sticker,
    /// Cursor stamp.
    Cursor,
    /// Grid / hatch fill.
    Grid,
    /// Object eraser.
    Eraser,
    /// Rectangular crop.
    Crop,
    /// Elliptical crop (bakes annotations into the image).
    CropEllipse,
    /// Freeform crop (bakes annotations into the image).
    CropFreeform,
    /// Cut-out: remove a horizontal or vertical strip and join the rest.
    CutOut,
}

impl Tool {
    /// Every tool, in toolbar order.
    pub const ALL: [Tool; 25] = [
        Tool::Select,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Line,
        Tool::Arrow,
        Tool::Freehand,
        Tool::FreehandArrow,
        Tool::Text,
        Tool::Balloon,
        Tool::Step,
        Tool::Magnify,
        Tool::Spotlight,
        Tool::Blur,
        Tool::Pixelate,
        Tool::Highlight,
        Tool::HighlightPen,
        Tool::Image,
        Tool::Sticker,
        Tool::Cursor,
        Tool::Grid,
        Tool::Eraser,
        Tool::Crop,
        Tool::CropEllipse,
        Tool::CropFreeform,
        Tool::CutOut,
    ];

    /// Does this tool create an object?
    pub fn creates_object(self) -> bool {
        self.preset().is_some()
    }

    /// The tool that creates objects like `kind`.
    pub fn for_kind(kind: &ObjectKind) -> Option<Tool> {
        Some(match kind {
            ObjectKind::Rectangle(_) => Tool::Rectangle,
            ObjectKind::Ellipse(_) => Tool::Ellipse,
            ObjectKind::Line(_) => Tool::Line,
            ObjectKind::Arrow(_) => Tool::Arrow,
            ObjectKind::Freehand(f) if f.arrow.is_some() => Tool::FreehandArrow,
            ObjectKind::Freehand(_) => Tool::Freehand,
            ObjectKind::Text(_) => Tool::Text,
            ObjectKind::Balloon(_) => Tool::Balloon,
            ObjectKind::Step(_) => Tool::Step,
            ObjectKind::Magnify(_) => Tool::Magnify,
            ObjectKind::Spotlight(_) => Tool::Spotlight,
            ObjectKind::Blur(_) => Tool::Blur,
            ObjectKind::Pixelate(_) => Tool::Pixelate,
            ObjectKind::Highlight(h) if h.points.is_empty() => Tool::Highlight,
            ObjectKind::Highlight(_) => Tool::HighlightPen,
            ObjectKind::Image(_) => Tool::Image,
            ObjectKind::Sticker(_) => Tool::Sticker,
            ObjectKind::Cursor(_) => Tool::Cursor,
            ObjectKind::Grid(_) => Tool::Grid,
            ObjectKind::Unknown(_) => return None,
        })
    }

    /// ShareX-like factory defaults for the tool, `None` for non-creating tools.
    pub fn preset(self) -> Option<Preset> {
        let red = Style::default();
        let none = |style: Style, kind: ObjectKind| Some(Preset { style, kind });
        match self {
            Tool::Rectangle => {
                none(Style { stroke_width: 3.0, ..red }, ObjectKind::Rectangle(BoxShape::default()))
            }
            Tool::Ellipse => {
                none(Style { stroke_width: 3.0, ..red }, ObjectKind::Ellipse(BoxShape::default()))
            }
            Tool::Line => none(red, ObjectKind::Line(LineShape::default())),
            Tool::Arrow => none(red, ObjectKind::Arrow(ArrowShape::default())),
            Tool::Freehand => none(red, ObjectKind::Freehand(FreehandShape::default())),
            Tool::FreehandArrow => none(
                red,
                ObjectKind::Freehand(FreehandShape {
                    arrow: Some(ArrowHeads { end: HeadStyle::Filled, ..ArrowHeads::default() }),
                    ..FreehandShape::default()
                }),
            ),
            Tool::Text => {
                none(Style { stroke_width: 0.0, ..red }, ObjectKind::Text(TextShape::default()))
            }
            Tool::Balloon => none(
                Style {
                    stroke: Color::BLACK,
                    stroke_width: 2.0,
                    fill: Fill::solid(Color::rgb(255, 255, 204)),
                    corner_radius: 12.0,
                    ..red
                },
                ObjectKind::Balloon(BalloonShape::default()),
            ),
            Tool::Step => none(
                Style {
                    stroke: Color::WHITE,
                    stroke_width: 2.0,
                    fill: Fill::solid(Color::RED),
                    ..red
                },
                ObjectKind::Step(StepShape::default()),
            ),
            Tool::Magnify => none(
                Style {
                    stroke: Color::WHITE,
                    stroke_width: 3.0,
                    shadow: Some(Shadow::default()),
                    ..red
                },
                ObjectKind::Magnify(MagnifyShape::default()),
            ),
            Tool::Spotlight => none(red, ObjectKind::Spotlight(SpotlightShape::default())),
            Tool::Blur => {
                none(red, ObjectKind::Blur(EffectBox { amount: 10.0, ..EffectBox::default() }))
            }
            Tool::Pixelate => {
                none(red, ObjectKind::Pixelate(EffectBox { amount: 10.0, ..EffectBox::default() }))
            }
            Tool::Highlight => none(
                Style {
                    stroke: Color::YELLOW,
                    stroke_width: 0.0,
                    fill: Fill::solid(Color::YELLOW),
                    blend: BlendMode::Multiply,
                    ..red
                },
                ObjectKind::Highlight(HighlightShape::default()),
            ),
            Tool::HighlightPen => none(
                Style {
                    stroke: Color::YELLOW,
                    stroke_width: 22.0,
                    blend: BlendMode::Multiply,
                    ..red
                },
                ObjectKind::Highlight(HighlightShape {
                    points: vec![crate::geom::PointF::default()],
                    ..HighlightShape::default()
                }),
            ),
            Tool::Image => none(red, ObjectKind::Image(ImageShape::default())),
            Tool::Sticker => none(
                Style { fill: Fill::solid(Color::rgb(0, 170, 60)), stroke_width: 0.0, ..red },
                ObjectKind::Sticker(StickerShape {
                    source: StickerSource::Builtin { which: BuiltinSticker::Check },
                    ..StickerShape::default()
                }),
            ),
            Tool::Cursor => none(
                Style {
                    stroke: Color::BLACK,
                    stroke_width: 1.5,
                    fill: Fill::solid(Color::WHITE),
                    ..red
                },
                ObjectKind::Cursor(CursorShape::default()),
            ),
            Tool::Grid => none(
                Style { stroke: Color::rgba(0, 0, 0, 180), stroke_width: 1.0, ..red },
                ObjectKind::Grid(GridShape::default()),
            ),
            _ => None,
        }
    }
}

/// Style plus kind-specific options a tool creates objects with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    /// Common style.
    pub style: Style,
    /// Template kind (zeroed geometry, real options). See [`ObjectKind::template`].
    pub kind: ObjectKind,
}

/// Per-tool "last used style" memory.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StyleMemory {
    presets: BTreeMap<Tool, Preset>,
}

impl StyleMemory {
    /// Empty memory: every tool falls back to its factory preset.
    pub fn new() -> Self {
        Self::default()
    }

    /// The preset new objects of `tool` are created from.
    pub fn get(&self, tool: Tool) -> Option<Preset> {
        self.presets.get(&tool).cloned().or_else(|| tool.preset())
    }

    /// Remembers `preset` as the last used style of `tool` (ignored for non-creating tools).
    pub fn remember(&mut self, tool: Tool, preset: Preset) {
        if tool.creates_object() {
            self.presets.insert(tool, preset);
        }
    }

    /// Remembers the style and options of an existing object for its tool.
    pub fn remember_object(&mut self, o: &crate::object::Object) {
        if let Some(tool) = Tool::for_kind(&o.kind) {
            self.remember(tool, Preset { style: o.style.clone(), kind: o.kind.template() });
        }
    }

    /// Forgets the remembered style of one tool.
    pub fn reset(&mut self, tool: Tool) {
        self.presets.remove(&tool);
    }

    /// Forgets everything.
    pub fn reset_all(&mut self) {
        self.presets.clear();
    }
}

/// Convenience: default text content used when creating a text object.
pub fn default_text() -> TextContent {
    TextContent::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_creating_tool_has_a_preset_that_maps_back() {
        for t in Tool::ALL {
            match t.preset() {
                Some(p) => {
                    assert_eq!(Tool::for_kind(&p.kind), Some(t), "{t:?}");
                    assert_eq!(p.kind.template(), p.kind.template().template());
                }
                None => assert!(!t.creates_object()),
            }
        }
        for t in [
            Tool::Select,
            Tool::Eraser,
            Tool::Crop,
            Tool::CropEllipse,
            Tool::CropFreeform,
            Tool::CutOut,
        ] {
            assert!(!t.creates_object(), "{t:?}");
        }
    }

    #[test]
    fn memory_overrides_and_resets() {
        let mut m = StyleMemory::new();
        let mut p = Tool::Rectangle.preset().unwrap();
        assert_eq!(m.get(Tool::Rectangle), Some(p.clone()));
        p.style.stroke = Color::rgb(1, 2, 3);
        m.remember(Tool::Rectangle, p.clone());
        assert_eq!(m.get(Tool::Rectangle).unwrap().style.stroke, Color::rgb(1, 2, 3));
        assert_eq!(m.get(Tool::Ellipse), Tool::Ellipse.preset(), "other tools unaffected");
        m.remember(Tool::Select, p);
        assert_eq!(m.get(Tool::Select), None);
        m.reset(Tool::Rectangle);
        assert_eq!(m.get(Tool::Rectangle), Tool::Rectangle.preset());
    }

    #[test]
    fn memory_serialises() {
        let mut m = StyleMemory::new();
        let mut p = Tool::Arrow.preset().unwrap();
        p.style.stroke_width = 9.0;
        m.remember(Tool::Arrow, p);
        let json = serde_json::to_string(&m).unwrap();
        let back: StyleMemory = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn highlight_variants_are_distinguished() {
        assert_eq!(Tool::for_kind(&Tool::Highlight.preset().unwrap().kind), Some(Tool::Highlight));
        assert_eq!(
            Tool::for_kind(&Tool::HighlightPen.preset().unwrap().kind),
            Some(Tool::HighlightPen)
        );
        assert_eq!(
            Tool::for_kind(&Tool::FreehandArrow.preset().unwrap().kind),
            Some(Tool::FreehandArrow)
        );
    }
}
