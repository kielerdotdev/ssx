//! The toolbar vocabulary: which tools exist, how they are grouped and what they are called.
//!
//! The engine's [`ssx_editor::Tool`] has one variant per *behaviour*. The toolbar is slightly
//! richer: "text with outline/background" is the text tool started from a different preset,
//! and a few slots (blur/pixelate, highlighter rectangle/pen) hold two engine tools behind one
//! button, like the reference screenshot. [`ToolId`] is that toolbar-level identity; it maps
//! onto the engine tool with [`ToolId::engine`].

use serde::{Deserialize, Serialize};
use ssx_editor::{Tool, object::TextOutline};

use crate::icons::Icon;

/// A selectable toolbar tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolId {
    /// Rectangular crop / region.
    CropRect,
    /// Elliptical crop / region.
    CropEllipse,
    /// Freeform crop / region.
    CropFree,
    /// Select, move, resize, rotate.
    Select,
    /// Rectangle.
    Rectangle,
    /// Ellipse.
    Ellipse,
    /// Freehand pen.
    Freehand,
    /// Line.
    Line,
    /// Arrow.
    Arrow,
    /// Freehand pen with an arrow head.
    FreehandArrow,
    /// Plain text.
    Text,
    /// Text with an outline and a background box.
    TextBoxed,
    /// Speech balloon.
    Balloon,
    /// Auto-numbered step.
    Step,
    /// Magnifier.
    Magnify,
    /// Spotlight.
    Spotlight,
    /// Insert image.
    Image,
    /// Emoji / sticker.
    Sticker,
    /// Cursor stamp.
    Cursor,
    /// Object eraser.
    Eraser,
    /// Blur region.
    Blur,
    /// Pixelate region.
    Pixelate,
    /// Grid / hatch.
    Grid,
    /// Highlighter rectangle.
    Highlight,
    /// Highlighter pen.
    HighlightPen,
    /// Cut-out strip (also reachable from the canvas menu).
    CutOut,
}

impl ToolId {
    /// The engine tool this maps to.
    pub fn engine(self) -> Tool {
        match self {
            ToolId::CropRect => Tool::Crop,
            ToolId::CropEllipse => Tool::CropEllipse,
            ToolId::CropFree => Tool::CropFreeform,
            ToolId::Select => Tool::Select,
            ToolId::Rectangle => Tool::Rectangle,
            ToolId::Ellipse => Tool::Ellipse,
            ToolId::Freehand => Tool::Freehand,
            ToolId::Line => Tool::Line,
            ToolId::Arrow => Tool::Arrow,
            ToolId::FreehandArrow => Tool::FreehandArrow,
            ToolId::Text | ToolId::TextBoxed => Tool::Text,
            ToolId::Balloon => Tool::Balloon,
            ToolId::Step => Tool::Step,
            ToolId::Magnify => Tool::Magnify,
            ToolId::Spotlight => Tool::Spotlight,
            ToolId::Image => Tool::Image,
            ToolId::Sticker => Tool::Sticker,
            ToolId::Cursor => Tool::Cursor,
            ToolId::Eraser => Tool::Eraser,
            ToolId::Blur => Tool::Blur,
            ToolId::Pixelate => Tool::Pixelate,
            ToolId::Grid => Tool::Grid,
            ToolId::Highlight => Tool::Highlight,
            ToolId::HighlightPen => Tool::HighlightPen,
            ToolId::CutOut => Tool::CutOut,
        }
    }

    /// The toolbar tool that best represents an engine tool.
    pub fn from_engine(t: Tool) -> ToolId {
        match t {
            Tool::Crop => ToolId::CropRect,
            Tool::CropEllipse => ToolId::CropEllipse,
            Tool::CropFreeform => ToolId::CropFree,
            Tool::Select => ToolId::Select,
            Tool::Rectangle => ToolId::Rectangle,
            Tool::Ellipse => ToolId::Ellipse,
            Tool::Freehand => ToolId::Freehand,
            Tool::Line => ToolId::Line,
            Tool::Arrow => ToolId::Arrow,
            Tool::FreehandArrow => ToolId::FreehandArrow,
            Tool::Text => ToolId::Text,
            Tool::Balloon => ToolId::Balloon,
            Tool::Step => ToolId::Step,
            Tool::Magnify => ToolId::Magnify,
            Tool::Spotlight => ToolId::Spotlight,
            Tool::Image => ToolId::Image,
            Tool::Sticker => ToolId::Sticker,
            Tool::Cursor => ToolId::Cursor,
            Tool::Eraser => ToolId::Eraser,
            Tool::Blur => ToolId::Blur,
            Tool::Pixelate => ToolId::Pixelate,
            Tool::Grid => ToolId::Grid,
            Tool::Highlight => ToolId::Highlight,
            Tool::HighlightPen => ToolId::HighlightPen,
            Tool::CutOut => ToolId::CutOut,
        }
    }

    /// Name used for tooltips, accessibility labels and the cheat sheet.
    pub fn label(self) -> &'static str {
        match self {
            ToolId::CropRect => "Rectangle region",
            ToolId::CropEllipse => "Ellipse region",
            ToolId::CropFree => "Freeform region",
            ToolId::Select => "Select and move",
            ToolId::Rectangle => "Rectangle",
            ToolId::Ellipse => "Ellipse",
            ToolId::Freehand => "Freehand",
            ToolId::Line => "Line",
            ToolId::Arrow => "Arrow",
            ToolId::FreehandArrow => "Freehand arrow",
            ToolId::Text => "Text",
            ToolId::TextBoxed => "Text with outline and background",
            ToolId::Balloon => "Speech balloon",
            ToolId::Step => "Step number",
            ToolId::Magnify => "Magnify",
            ToolId::Spotlight => "Spotlight",
            ToolId::Image => "Image",
            ToolId::Sticker => "Emoji and stickers",
            ToolId::Cursor => "Cursor stamp",
            ToolId::Eraser => "Eraser",
            ToolId::Blur => "Blur",
            ToolId::Pixelate => "Pixelate",
            ToolId::Grid => "Grid",
            ToolId::Highlight => "Highlighter",
            ToolId::HighlightPen => "Highlighter pen",
            ToolId::CutOut => "Cut out",
        }
    }

    /// One-line usage hint for the status bar.
    pub fn hint(self) -> &'static str {
        match self {
            ToolId::CropRect | ToolId::CropEllipse | ToolId::CropFree => {
                "Drag to mark the region, Enter to apply, Esc to cancel"
            }
            ToolId::Select => {
                "Click to select, drag to move, Shift+click for several, Del deletes, double-click edits text"
            }
            ToolId::Rectangle | ToolId::Ellipse => {
                "Drag to draw. Shift = square/circle, Alt = from centre, Ctrl = snap"
            }
            ToolId::Line | ToolId::Arrow => "Drag to draw. Shift = 45 degree steps, Alt = from centre",
            ToolId::Freehand | ToolId::FreehandArrow | ToolId::HighlightPen => "Drag to draw freehand",
            ToolId::Text | ToolId::TextBoxed => {
                "Click and type. Enter commits, Shift+Enter starts a new line, Esc leaves"
            }
            ToolId::Balloon => "Drag to place a balloon, type, then drag the tail handle",
            ToolId::Step => "Click to drop the next step number",
            ToolId::Magnify => "Drag the lens, then move its source handle",
            ToolId::Spotlight => "Drag to light up an area and dim the rest",
            ToolId::Image => "Click to insert the image (use the menu to pick a file or the clipboard)",
            ToolId::Sticker => "Pick a sticker in the properties bar, click to place it",
            ToolId::Cursor => "Click to stamp a mouse cursor",
            ToolId::Eraser => "Drag over annotations to delete them",
            ToolId::Blur | ToolId::Pixelate => "Drag over the area to obscure",
            ToolId::Grid => "Drag to fill an area with a grid or hatch",
            ToolId::Highlight => "Drag to highlight an area",
            ToolId::CutOut => "Drag across the image to cut a strip out and join the rest",
        }
    }

    /// Toolbar icon.
    pub fn icon(self) -> Icon {
        match self {
            ToolId::CropRect => Icon::RegionRect,
            ToolId::CropEllipse => Icon::RegionEllipse,
            ToolId::CropFree => Icon::RegionFree,
            ToolId::Select => Icon::Select,
            ToolId::Rectangle => Icon::Rectangle,
            ToolId::Ellipse => Icon::Ellipse,
            ToolId::Freehand => Icon::Freehand,
            ToolId::Line => Icon::Line,
            ToolId::Arrow => Icon::Arrow,
            ToolId::FreehandArrow => Icon::FreehandArrow,
            ToolId::Text => Icon::Text,
            ToolId::TextBoxed => Icon::TextBoxed,
            ToolId::Balloon => Icon::Balloon,
            ToolId::Step => Icon::Step,
            ToolId::Magnify => Icon::Magnify,
            ToolId::Spotlight => Icon::Spotlight,
            ToolId::Image => Icon::Image,
            ToolId::Sticker => Icon::Sticker,
            ToolId::Cursor => Icon::CursorStamp,
            ToolId::Eraser => Icon::Eraser,
            ToolId::Blur => Icon::Blur,
            ToolId::Pixelate => Icon::Pixelate,
            ToolId::Grid => Icon::Grid,
            ToolId::Highlight | ToolId::HighlightPen => Icon::Highlighter,
            ToolId::CutOut => Icon::CutOut,
        }
    }

    /// Every tool.
    pub const ALL: [ToolId; 26] = [
        ToolId::CropRect,
        ToolId::CropEllipse,
        ToolId::CropFree,
        ToolId::Select,
        ToolId::Rectangle,
        ToolId::Ellipse,
        ToolId::Freehand,
        ToolId::Line,
        ToolId::Arrow,
        ToolId::FreehandArrow,
        ToolId::Text,
        ToolId::TextBoxed,
        ToolId::Balloon,
        ToolId::Step,
        ToolId::Magnify,
        ToolId::Spotlight,
        ToolId::Image,
        ToolId::Sticker,
        ToolId::Cursor,
        ToolId::Eraser,
        ToolId::Blur,
        ToolId::Pixelate,
        ToolId::Grid,
        ToolId::Highlight,
        ToolId::HighlightPen,
        ToolId::CutOut,
    ];
}

/// One button of the toolbar. A slot with several `variants` shows the last used one and
/// offers the others from a small popup.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// Variants; the first is the default.
    pub variants: &'static [ToolId],
}

impl Slot {
    /// Does this slot contain `tool`?
    pub fn contains(&self, tool: ToolId) -> bool {
        self.variants.contains(&tool)
    }
}

const fn one(v: &'static [ToolId]) -> Slot {
    Slot { variants: v }
}

/// Toolbar layout: groups of slots, in the order of the reference screenshot.
pub const TOOLBAR: &[&[Slot]] = &[
    &[
        one(&[ToolId::CropRect]),
        one(&[ToolId::CropEllipse]),
        one(&[ToolId::CropFree]),
    ],
    &[one(&[ToolId::Select])],
    &[
        one(&[ToolId::Rectangle]),
        one(&[ToolId::Ellipse]),
        one(&[ToolId::Freehand]),
        one(&[ToolId::Line]),
        one(&[ToolId::Arrow]),
        one(&[ToolId::FreehandArrow]),
    ],
    &[
        one(&[ToolId::Text]),
        one(&[ToolId::TextBoxed]),
        one(&[ToolId::Balloon]),
        one(&[ToolId::Step]),
    ],
    &[one(&[ToolId::Magnify]), one(&[ToolId::Spotlight])],
    &[one(&[ToolId::Image]), one(&[ToolId::Sticker]), one(&[ToolId::Cursor])],
    &[
        one(&[ToolId::Eraser]),
        one(&[ToolId::Blur, ToolId::Pixelate]),
        one(&[ToolId::Grid]),
        one(&[ToolId::Highlight, ToolId::HighlightPen]),
    ],
];

/// The tool preset tweaks applied when [`ToolId::TextBoxed`] is chosen: white glyphs with a black
/// outline on a translucent dark box (readable on any screenshot, like `ShareX`'s text tool
/// with a background).
pub fn boxed_text_outline() -> TextOutline {
    TextOutline { color: ssx_editor::Color::BLACK, width: 2.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_mapping_round_trips() {
        for t in ToolId::ALL {
            let back = ToolId::from_engine(t.engine());
            assert_eq!(back.engine(), t.engine(), "{t:?}");
        }
        assert_eq!(ToolId::from_engine(Tool::Text), ToolId::Text);
    }

    #[test]
    fn every_engine_tool_is_reachable_from_the_toolbar_or_menu() {
        let in_toolbar: Vec<Tool> = TOOLBAR
            .iter()
            .flat_map(|g| g.iter())
            .flat_map(|s| s.variants.iter())
            .map(|t| t.engine())
            .collect();
        for t in Tool::ALL {
            // Cut-out lives in the canvas menu.
            if t != Tool::CutOut {
                assert!(in_toolbar.contains(&t), "{t:?} is missing from the toolbar");
            }
        }
    }

    #[test]
    fn labels_and_hints_are_non_empty_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for t in ToolId::ALL {
            assert!(!t.label().is_empty() && !t.hint().is_empty());
            assert!(seen.insert(t.label()), "duplicate label {}", t.label());
        }
    }

    #[test]
    fn toolbar_has_no_duplicate_buttons() {
        let mut seen = std::collections::HashSet::new();
        for g in TOOLBAR {
            for s in *g {
                for v in s.variants {
                    assert!(seen.insert(*v), "{v:?} appears twice");
                }
            }
        }
    }
}
