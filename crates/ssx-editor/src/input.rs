//! Toolkit-neutral input and output vocabulary of the interactive session.
//!
//! The GUI translates its native events into [`Modifiers`], [`Key`] and pointer calls, and
//! reads back [`SessionEvent`]s, the [`Overlay`] (selection boxes, handles, guides, caret...)
//! and a [`CursorHint`]. Nothing here mentions a windowing toolkit.

use ssx_types::Rect;

use crate::{
    geom::{PointF, RectF},
    object::ObjectId,
    tool::Tool,
};

/// Keyboard modifiers held during an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift: constrain (45° lines, squares/circles, axis-locked moves, uniform resize).
    pub shift: bool,
    /// Ctrl (Cmd on macOS): snap to guides; with a letter key, a shortcut.
    pub ctrl: bool,
    /// Alt: draw / resize from the centre.
    pub alt: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers { shift: false, ctrl: false, alt: false };
    /// Only shift.
    pub const SHIFT: Modifiers = Modifiers { shift: true, ctrl: false, alt: false };
    /// Only ctrl.
    pub const CTRL: Modifiers = Modifiers { shift: false, ctrl: true, alt: false };
    /// Only alt.
    pub const ALT: Modifiers = Modifiers { shift: false, ctrl: false, alt: true };
}

/// Keys the session reacts to. Printable text should be sent with
/// [`crate::EditorSession::text_insert`] (which is IME-friendly); `Char` is for shortcuts
/// combined with ctrl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Enter / Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// A character key (for shortcuts such as ctrl+Z).
    Char(char),
}

/// Mouse cursor the GUI should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorHint {
    /// Normal arrow.
    #[default]
    Default,
    /// Crosshair (drawing tools).
    Crosshair,
    /// Text I-beam.
    Text,
    /// Four-way move.
    Move,
    /// Closed hand while dragging.
    Grabbing,
    /// Resize north-south (↕).
    ResizeNs,
    /// Resize east-west (↔).
    ResizeEw,
    /// Resize along ↖↘.
    ResizeNwse,
    /// Resize along ↗↙.
    ResizeNesw,
    /// Rotate handle.
    Rotate,
    /// Eraser.
    Eraser,
    /// Action not possible here.
    NotAllowed,
}

/// Handle kinds around the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// Top-left corner.
    NorthWest,
    /// Top edge.
    North,
    /// Top-right corner.
    NorthEast,
    /// Right edge.
    East,
    /// Bottom-right corner.
    SouthEast,
    /// Bottom edge.
    South,
    /// Bottom-left corner.
    SouthWest,
    /// Left edge.
    West,
    /// Rotation knob above the top edge.
    Rotate,
    /// Start (0) or end (1) point of a line/arrow.
    Endpoint(u8),
    /// Tip of a speech balloon's tail.
    Tail,
    /// Source point of a magnifier.
    Source,
}

impl HandleKind {
    /// Direction of a resize handle as `(dx, dy)` in {-1, 0, 1}; `None` for other kinds.
    pub fn direction(self) -> Option<(i8, i8)> {
        Some(match self {
            HandleKind::NorthWest => (-1, -1),
            HandleKind::North => (0, -1),
            HandleKind::NorthEast => (1, -1),
            HandleKind::East => (1, 0),
            HandleKind::SouthEast => (1, 1),
            HandleKind::South => (0, 1),
            HandleKind::SouthWest => (-1, 1),
            HandleKind::West => (-1, 0),
            _ => return None,
        })
    }

    pub(crate) const RESIZE: [HandleKind; 8] = [
        HandleKind::NorthWest,
        HandleKind::North,
        HandleKind::NorthEast,
        HandleKind::East,
        HandleKind::SouthEast,
        HandleKind::South,
        HandleKind::SouthWest,
        HandleKind::West,
    ];
}

/// A draggable handle of the current selection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Handle {
    /// What dragging it does.
    pub kind: HandleKind,
    /// Centre position in image space.
    pub pos: PointF,
    /// Cursor to show while hovering it.
    pub cursor: CursorHint,
}

/// A rectangle rotated about its centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrientedRect {
    /// Unrotated rectangle.
    pub rect: RectF,
    /// Rotation in radians about the rectangle's centre.
    pub rotation: f32,
}

/// A snapping guide line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    /// `true` for a vertical line at `x = position`, `false` for horizontal at `y = position`.
    pub vertical: bool,
    /// Coordinate of the line.
    pub position: f32,
    /// Start of the visible extent along the line.
    pub from: f32,
    /// End of the visible extent along the line.
    pub to: f32,
}

/// Text-editing overlay geometry, in the *unrotated* text frame; rotate by `rotation`
/// about `pivot` to get screen positions.
#[derive(Debug, Clone, PartialEq)]
pub struct CaretOverlay {
    /// The caret rectangle (zero width; draw as a line).
    pub caret: RectF,
    /// Selection highlight rectangles.
    pub selection: Vec<RectF>,
    /// Rotation of the text box.
    pub rotation: f32,
    /// Pivot of the rotation (box centre).
    pub pivot: PointF,
    /// IME pre-edit string to draw at the caret (underlined), if any.
    pub preedit: Option<String>,
}

/// Pending crop rectangle.
#[derive(Debug, Clone, PartialEq)]
pub struct CropOverlay {
    /// Bounding rectangle.
    pub rect: RectF,
    /// For freeform crops, the polygon.
    pub polygon: Vec<PointF>,
    /// `true` for an elliptical crop.
    pub ellipse: bool,
}

/// Everything a GUI should draw on top of the rendered document for the current state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Outlines of selected objects.
    pub selection: Vec<OrientedRect>,
    /// Handles of the selection.
    pub handles: Vec<Handle>,
    /// Marquee rectangle while rubber-band selecting.
    pub marquee: Option<RectF>,
    /// Active snapping guides.
    pub guides: Vec<Guide>,
    /// Text caret / selection.
    pub caret: Option<CaretOverlay>,
    /// Pending crop.
    pub crop: Option<CropOverlay>,
    /// Strip that a cut-out drag would remove.
    pub cut_strip: Option<RectF>,
    /// Objects the eraser stroke has marked for deletion (draw them dimmed).
    pub erase_marks: Vec<ObjectId>,
}

/// Notifications from the session. Poll with [`crate::EditorSession::take_events`].
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Pixels changed inside this image-space rectangle (outward rounded, already expanded
    /// through objects that read the backdrop). Convert with
    /// [`crate::Document::image_rect_to_output`] and re-render that viewport.
    Dirty(Rect),
    /// The canvas size or base image changed (crop, rotate, resize...): repaint everything and
    /// re-fit the view.
    CanvasChanged,
    /// The selection changed.
    SelectionChanged,
    /// The active tool changed.
    ToolChanged(Tool),
    /// The cursor hint changed.
    CursorChanged(CursorHint),
    /// Undo/redo availability changed.
    HistoryChanged {
        /// Something can be undone.
        can_undo: bool,
        /// Something can be redone.
        can_redo: bool,
    },
    /// Text editing started (`true`) or ended (`false`); show/hide the IME and caret.
    TextEditing(bool),
    /// Overlay (handles, guides, caret...) changed; redraw it.
    OverlayChanged,
    /// The session wants this text on the system clipboard (text-edit copy/cut).
    SetClipboardText(String),
    /// The session wants the system clipboard text inserted (ctrl+V while editing text);
    /// answer with [`crate::EditorSession::text_insert`].
    PasteTextRequested,
}
