//! The [`Scene`]: a plain-data description of what one frame of the overlay shows.
//!
//! The model produces a `Scene`; the renderer turns it into pixels; [`crate::model::damage`]
//! diffs two scenes into dirty rectangles. Keeping the scene as data (rather than draw calls)
//! is what makes the state machine testable without a window and lets the renderer be
//! verified against golden images.

use ssx_types::{Point, Rect, Size};

use super::geometry::{Handle, clamp_inside, inflate_i64};

/// Width and height of one glyph of the built-in bitmap font.
pub const GLYPH: u32 = 8;

/// Integer magnification of the bitmap font for a UI scale.
pub fn text_scale(ui_scale: f32) -> u32 {
    (ui_scale.round().max(1.0)) as u32
}

/// Size of a text block of `lines` at `ui_scale`, including padding.
pub fn label_size(lines: &[String], ui_scale: f32) -> Size {
    let s = text_scale(ui_scale);
    let chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let pad = 3 * s;
    let line_h = GLYPH * s + 2 * s;
    Size::new(chars * GLYPH * s + 2 * pad, lines.len() as u32 * line_h + 2 * pad - 2 * s)
}

/// Which system cursor shape the backend should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorHint {
    /// Crosshair (default).
    Crosshair,
    /// Four-way move.
    Move,
    /// Resize in the direction of a handle.
    Resize(Handle),
}

/// Shape of the bright cut-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cutout {
    /// Rectangle.
    Rect,
    /// Ellipse inscribed in the selection rectangle.
    Ellipse,
}

/// The dimensions/position label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelScene {
    /// Where the label box is drawn.
    pub rect: Rect,
    /// Text lines.
    pub lines: Vec<String>,
}

/// The magnifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoupeScene {
    /// Whole loupe including the info panel below the pixel grid.
    pub outer: Rect,
    /// The zoomed pixel grid.
    pub grid: Rect,
    /// Desktop pixel shown in the centre cell.
    pub centre: Point,
    /// Cells per side (odd).
    pub cells: u32,
    /// Size of one cell in screen pixels.
    pub cell_px: u32,
}

/// Everything visible in one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// Desktop bounds (the renderer's coordinate space).
    pub bounds: Rect,
    /// Desktop pixels per UI unit at the pointer.
    pub ui_scale: f32,
    /// Cut-out shape for `selection`.
    pub cutout: Cutout,
    /// Committed or in-progress selection rectangle.
    pub selection: Option<Rect>,
    /// Draw resize handles on `selection`.
    pub handles: bool,
    /// Handle currently being dragged.
    pub active_handle: Option<Handle>,
    /// Freeform outline being drawn (also the polygon whose interior is bright).
    pub freeform: Vec<Point>,
    /// Hover highlight (window or monitor).
    pub highlight: Option<Rect>,
    /// Crosshair guides through this pixel.
    pub crosshair: Option<Point>,
    /// Text label.
    pub label: Option<LabelScene>,
    /// Magnifier.
    pub loupe: Option<LoupeScene>,
    /// Cursor shape hint for the backend.
    pub cursor: CursorHint,
}

impl Scene {
    /// An empty scene (dimmed desktop only).
    pub fn empty(bounds: Rect) -> Self {
        Self {
            bounds,
            ui_scale: 1.0,
            cutout: Cutout::Rect,
            selection: None,
            handles: false,
            active_handle: None,
            freeform: Vec::new(),
            highlight: None,
            crosshair: None,
            label: None,
            loupe: None,
            cursor: CursorHint::Crosshair,
        }
    }
}

/// Places the dimensions label for `subject`: above its top-left corner if there is room,
/// otherwise inside it; kept within `area`. `None` if the label is larger than `area`
/// (the renderer never draws outside the rectangles the scene declares).
pub fn place_label(
    subject: Rect,
    lines: Vec<String>,
    ui_scale: f32,
    area: Rect,
) -> Option<LabelScene> {
    let size = label_size(&lines, ui_scale);
    if size.width > area.width || size.height > area.height {
        return None;
    }
    let gap = 4 * text_scale(ui_scale);
    let above_y = i64::from(subject.y) - i64::from(size.height) - i64::from(gap);
    let y =
        if above_y >= i64::from(area.y) { above_y } else { i64::from(subject.y) + i64::from(gap) };
    let raw = super::geometry::rect_from_edges(
        i64::from(subject.x),
        y,
        i64::from(subject.x) + i64::from(size.width),
        y + i64::from(size.height),
    );
    Some(LabelScene { rect: clamp_inside(raw, area), lines })
}

/// Lays out the loupe for a pointer at `cursor` within `area` (the monitor under it).
/// The grid shrinks to fit small areas; `None` if not even one cell plus the info panel fits.
pub fn layout_loupe(cursor: Point, zoom: u32, ui_scale: f32, area: Rect) -> Option<LoupeScene> {
    let s = text_scale(ui_scale);
    let zoom = zoom.clamp(2, 32);
    let cell_px = zoom * s;
    let info_h = label_size(&vec![String::new(); 3], ui_scale).height;
    let max_side = area.width.min(area.height.saturating_sub(info_h));
    let mut cells = (128 / zoom).clamp(3, 65);
    if cells.is_multiple_of(2) {
        cells -= 1;
    }
    while cells > 1 && cells * cell_px > max_side {
        cells -= 2;
    }
    let side = cells * cell_px;
    if side > max_side || side < 92 * s {
        return None;
    }
    let (w, h) = (side, side + info_h);
    let off = i64::from(20 * s);
    let (cx, cy) = (i64::from(cursor.x), i64::from(cursor.y));
    let mut x = cx + off;
    let mut y = cy + off;
    if x + i64::from(w) > area.right() {
        x = cx - off - i64::from(w);
    }
    if y + i64::from(h) > area.bottom() {
        y = cy - off - i64::from(h);
    }
    let outer = clamp_inside(
        super::geometry::rect_from_edges(x, y, x + i64::from(w), y + i64::from(h)),
        area,
    );
    debug_assert_eq!((outer.width, outer.height), (w, h));
    let grid = Rect::new(outer.x, outer.y, side, side);
    Some(LoupeScene { outer, grid, centre: cursor, cells, cell_px })
}

/// Text lines of the loupe info panel for a pixel value at `p`.
pub fn loupe_info(p: Point, rgb: Option<[u8; 3]>) -> Vec<String> {
    match rgb {
        Some([r, g, b]) => {
            vec![
                format!("#{r:02X}{g:02X}{b:02X}"),
                format!("{r},{g},{b}"),
                format!("{},{}", p.x, p.y),
            ]
        }
        None => vec!["-".into(), "-".into(), format!("{},{}", p.x, p.y)],
    }
}

/// Area around the crosshair lines that a change of pointer position dirties.
pub fn crosshair_strips(p: Point, ui_scale: f32, bounds: Rect) -> Vec<Rect> {
    let t = i64::from(text_scale(ui_scale)) + 1;
    let (x, y) = (i64::from(p.x), i64::from(p.y));
    let h = super::geometry::rect_from_edges(i64::from(bounds.x), y - t, bounds.right(), y + t + 1);
    let v =
        super::geometry::rect_from_edges(x - t, i64::from(bounds.y), x + t + 1, bounds.bottom());
    vec![h, v]
}

/// Bounding rectangle of everything that decorates `selection` (border and handles).
pub fn decor_bounds(selection: Rect, ui_scale: f32) -> Rect {
    inflate_i64(selection, i64::from(decor_pad(ui_scale)))
}

/// How far decorations reach outside the selection outline.
pub fn decor_pad(ui_scale: f32) -> u32 {
    let s = text_scale(ui_scale);
    handle_size(ui_scale) / 2 + 2 * s + 1
}

/// Edge length of a resize handle.
pub fn handle_size(ui_scale: f32) -> u32 {
    8 * text_scale(ui_scale)
}
