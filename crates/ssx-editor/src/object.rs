//! Annotation objects: data, geometry, hit-testing and structural transforms.
//!
//! An [`Object`] is *pure data* (serialisable, cloneable, comparable): a stable id, common
//! flags, a [`Style`] and one [`ObjectKind`]. Everything that needs fonts or pixels
//! (text layout, rasterising) lives in `text` / `render`; everything here is exact
//! geometry, which is why hit-testing, selection handles and dirty rectangles can be unit
//! tested without any rendering.
//!
//! Conventions
//! * Angles are **radians, clockwise on screen** (y grows downwards).
//! * Box-like kinds (`rect` + `rotation`) rotate about their centre.
//! * Point-like kinds (line, arrow, freehand) have no rotation; their points move directly.
//! * Effect kinds (blur, pixelate, magnify, spotlight, grid) never rotate: they operate on
//!   axis-aligned pixel regions.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ssx_types::Frame;

use crate::{
    geom::{Color, PointF, RectF},
    style::Style,
};

/// Stable identity of an object; never reused within a document's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectId(pub u64);

/// Font selection for text objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontSpec {
    /// Family name. Only the bundled `Liberation Sans` is guaranteed; unknown families fall
    /// back to it so documents render identically everywhere.
    pub family: String,
    /// Size in image pixels (em height).
    pub size: f32,
    /// Bold weight.
    pub bold: bool,
    /// Italic style.
    pub italic: bool,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self { family: "Liberation Sans".into(), size: 24.0, bold: false, italic: false }
    }
}

/// Horizontal alignment of text lines inside the text box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    /// Left aligned.
    #[default]
    Left,
    /// Centred.
    Center,
    /// Right aligned.
    Right,
}

/// Outline drawn around glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TextOutline {
    /// Outline colour.
    pub color: Color,
    /// Outline width in image pixels (centred on the glyph edge; the fill is drawn on top).
    pub width: f32,
}

/// Everything about a piece of text except where it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextContent {
    /// The text; lines are separated by `\n`.
    pub text: String,
    /// Font.
    pub font: FontSpec,
    /// Glyph fill colour.
    pub color: Color,
    /// Line alignment.
    pub align: TextAlign,
    /// Line height as a multiple of the font size.
    pub line_spacing: f32,
    /// Optional glyph outline.
    pub outline: Option<TextOutline>,
    /// Optional filled box behind the text.
    pub background: Option<Color>,
    /// Padding between the box edge and the text (image pixels).
    pub padding: f32,
}

impl Default for TextContent {
    fn default() -> Self {
        Self {
            text: String::new(),
            font: FontSpec::default(),
            color: Color::RED,
            align: TextAlign::Left,
            line_spacing: 1.2,
            outline: None,
            background: None,
            padding: 4.0,
        }
    }
}

/// Arrow head shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadStyle {
    /// No head.
    None,
    /// Open "V" head.
    Open,
    /// Filled triangle.
    #[default]
    Filled,
    /// Diamond.
    Diamond,
    /// Round dot.
    Round,
    /// Perpendicular bar (dimension line).
    Bar,
}

/// Arrow head configuration (both ends independently).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArrowHeads {
    /// Head at the start point.
    pub start: HeadStyle,
    /// Head at the end point.
    pub end: HeadStyle,
    /// Head length as a multiple of the stroke width (never below 8 px).
    pub size: f32,
    /// Half-opening angle of triangular heads in degrees.
    pub angle: f32,
}

impl Default for ArrowHeads {
    fn default() -> Self {
        Self { start: HeadStyle::None, end: HeadStyle::Filled, size: 4.0, angle: 25.0 }
    }
}

impl ArrowHeads {
    /// Head length in pixels for a given stroke width.
    pub fn length(&self, stroke_width: f32) -> f32 {
        (self.size * stroke_width).max(8.0)
    }
}

/// Rotatable rectangle shape (rectangle, ellipse and friends).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxShape {
    /// Unrotated rectangle.
    pub rect: RectF,
    /// Rotation about the centre, radians.
    pub rotation: f32,
}

/// Two-point line.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LineShape {
    /// Start.
    pub a: PointF,
    /// End.
    pub b: PointF,
}

/// Arrow between two points.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArrowShape {
    /// Start.
    pub a: PointF,
    /// End.
    pub b: PointF,
    /// Heads.
    pub heads: ArrowHeads,
}

/// Freehand stroke; with `arrow` set it is the "freehand arrow" tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FreehandShape {
    /// Raw sampled points.
    pub points: Vec<PointF>,
    /// Render as a Catmull-Rom spline through the points instead of a polyline.
    pub smooth: bool,
    /// Arrow heads at the ends.
    pub arrow: Option<ArrowHeads>,
}

impl Default for FreehandShape {
    fn default() -> Self {
        Self { points: Vec::new(), smooth: true, arrow: None }
    }
}

/// A text object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextShape {
    /// Box position and (when `auto_width` is off) fixed width. The height tracks the
    /// text and is refreshed by editing operations.
    pub rect: RectF,
    /// Rotation about the box centre, radians.
    pub rotation: f32,
    /// `true`: width follows the text (no wrapping). `false`: `rect.w` is a fixed width and
    /// long lines wrap.
    pub auto_width: bool,
    /// The text itself.
    pub content: TextContent,
}

impl Default for TextShape {
    fn default() -> Self {
        Self {
            rect: RectF::default(),
            rotation: 0.0,
            auto_width: true,
            content: TextContent::default(),
        }
    }
}

/// Speech balloon: rounded rectangle plus a tail whose tip can be dragged anywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalloonShape {
    /// Body rectangle.
    pub rect: RectF,
    /// Tail tip position.
    pub tail: PointF,
    /// Width of the tail where it joins the body.
    pub tail_width: f32,
    /// Text inside the balloon (centred by default).
    pub content: TextContent,
}

impl Default for BalloonShape {
    fn default() -> Self {
        Self {
            rect: RectF::default(),
            tail: PointF::default(),
            tail_width: 24.0,
            content: TextContent {
                align: TextAlign::Center,
                color: Color::BLACK,
                padding: 10.0,
                ..TextContent::default()
            },
        }
    }
}

/// Auto-numbered step marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepShape {
    /// Circle centre.
    pub center: PointF,
    /// Circle diameter.
    pub diameter: f32,
    /// Overrides the automatic number when set.
    pub manual: Option<u32>,
    /// Digit colour.
    pub text_color: Color,
}

impl Default for StepShape {
    fn default() -> Self {
        Self { center: PointF::default(), diameter: 36.0, manual: None, text_color: Color::WHITE }
    }
}

/// Magnifier lens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MagnifyShape {
    /// Where the lens is drawn.
    pub rect: RectF,
    /// Circular (`true`) or rectangular lens.
    pub circular: bool,
    /// Centre of the source region that is magnified.
    pub source: PointF,
    /// Zoom factor.
    pub zoom: f32,
}

impl Default for MagnifyShape {
    fn default() -> Self {
        Self { rect: RectF::default(), circular: true, source: PointF::default(), zoom: 2.0 }
    }
}

impl MagnifyShape {
    /// The area of the image the lens shows.
    pub fn source_rect(&self) -> RectF {
        let (w, h) = (self.rect.w / self.zoom.max(0.01), self.rect.h / self.zoom.max(0.01));
        RectF::from_center_size(self.source, w, h)
    }
}

/// Spotlight: dims everything outside the shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotlightShape {
    /// Bright area.
    pub rect: RectF,
    /// Ellipse (`true`) or rectangle.
    pub ellipse: bool,
    /// Dim colour (its alpha is the dim strength). The lowest spotlight's value is used.
    pub dim: Color,
    /// Edge feather in pixels (0 = hard).
    pub feather: f32,
}

impl Default for SpotlightShape {
    fn default() -> Self {
        Self {
            rect: RectF::default(),
            ellipse: false,
            dim: Color::rgba(0, 0, 0, 150),
            feather: 0.0,
        }
    }
}

/// A rectangular region effect (blur or pixelate); `amount` is σ or block size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectBox {
    /// Affected region.
    pub rect: RectF,
    /// Blur σ (blur) or block edge (pixelate), image pixels.
    pub amount: f32,
}

/// Highlighter marker: a filled rectangle, or a freehand pen stroke when `points` is used.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightShape {
    /// Rectangle (used when `points` is empty).
    pub rect: RectF,
    /// Freehand pen path (stroke width comes from the style).
    pub points: Vec<PointF>,
}

/// A decoded bitmap embedded in the document, serialised as base64 PNG.
#[derive(Debug, Clone)]
pub struct ImageData(pub Arc<Frame>);

impl PartialEq for ImageData {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl ImageData {
    /// Wraps a frame.
    pub fn new(frame: Frame) -> Self {
        Self(Arc::new(frame))
    }
}

impl Serialize for ImageData {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let png = crate::project::encode_png(&self.0).map_err(serde::ser::Error::custom)?;
        s.serialize_str(&crate::project::to_base64(&png))
    }
}

impl<'de> Deserialize<'de> for ImageData {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = crate::project::from_base64(&s).map_err(serde::de::Error::custom)?;
        let frame = crate::project::decode_png(&bytes).map_err(serde::de::Error::custom)?;
        Ok(ImageData::new(frame))
    }
}

impl Default for ImageData {
    fn default() -> Self {
        Self::new(ssx_imgfx::solid_frame(1, 1, [0; 4]))
    }
}

/// Inserted bitmap.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageShape {
    /// Destination rectangle (the bitmap is scaled to it).
    pub rect: RectF,
    /// Rotation, radians.
    pub rotation: f32,
    /// Pixels.
    pub image: ImageData,
}

/// Built-in vector stickers (deterministic on every OS, no emoji font needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinSticker {
    /// Check mark.
    Check,
    /// Cross.
    Cross,
    /// Five-point star.
    Star,
    /// Heart.
    Heart,
    /// Exclamation mark in a circle.
    Exclamation,
    /// Plus sign.
    Plus,
    /// Minus sign.
    Minus,
    /// Right-pointing arrow.
    ArrowRight,
    /// Lightning bolt.
    Bolt,
}

/// What a sticker draws.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StickerSource {
    /// A built-in vector shape drawn in the style's fill colour.
    Builtin {
        /// Which one.
        which: BuiltinSticker,
    },
    /// A text glyph / emoji shaped with the bundled font (monochrome; glyphs the font lacks
    /// render as the font's "missing glyph"). Colour emoji fonts are a documented follow-up.
    Glyph {
        /// The character(s).
        text: String,
    },
    /// A bitmap stamp.
    Bitmap {
        /// Pixels.
        image: ImageData,
    },
}

/// Emoji / sticker stamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StickerShape {
    /// Where the sticker is drawn.
    pub rect: RectF,
    /// Rotation, radians.
    pub rotation: f32,
    /// Content.
    pub source: StickerSource,
}

impl Default for StickerShape {
    fn default() -> Self {
        Self {
            rect: RectF::default(),
            rotation: 0.0,
            source: StickerSource::Builtin { which: BuiltinSticker::Check },
        }
    }
}

/// Embedded mouse-cursor artwork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorKind {
    /// Standard arrow pointer.
    #[default]
    Arrow,
    /// Text I-beam.
    IBeam,
    /// Crosshair.
    Crosshair,
}

/// Cursor stamp; `pos` is the hot spot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CursorShape {
    /// Hot-spot position.
    pub pos: PointF,
    /// Artwork.
    pub kind: CursorKind,
    /// Size multiplier (1 = about 32 px tall).
    pub scale: f32,
}

impl Default for CursorShape {
    fn default() -> Self {
        Self { pos: PointF::default(), kind: CursorKind::Arrow, scale: 1.0 }
    }
}

impl CursorShape {
    /// Nominal cursor height in pixels at scale 1.
    pub const BASE_SIZE: f32 = 32.0;

    /// The bounding box of the artwork.
    pub fn bounds(&self) -> RectF {
        let s = Self::BASE_SIZE * self.scale;
        match self.kind {
            CursorKind::Arrow => RectF::new(self.pos.x, self.pos.y, s * 0.7, s),
            CursorKind::IBeam => RectF::from_center_size(self.pos, s * 0.5, s),
            CursorKind::Crosshair => RectF::from_center_size(self.pos, s, s),
        }
    }
}

/// Pattern of a grid/hatch fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridPattern {
    /// Horizontal and vertical lines.
    #[default]
    Grid,
    /// Diagonal lines `/`.
    HatchForward,
    /// Diagonal lines `\`.
    HatchBackward,
    /// Both diagonals.
    CrossHatch,
    /// Dots at the grid crossings.
    Dots,
}

/// Grid / hatch fill inside a rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GridShape {
    /// Filled area.
    pub rect: RectF,
    /// Pattern.
    pub pattern: GridPattern,
    /// Distance between lines in image pixels.
    pub spacing: f32,
}

impl Default for GridShape {
    fn default() -> Self {
        Self { rect: RectF::default(), pattern: GridPattern::Grid, spacing: 16.0 }
    }
}

/// The geometry-and-data part of an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObjectKind {
    /// Rectangle (optionally rounded via `style.corner_radius`).
    Rectangle(BoxShape),
    /// Ellipse.
    Ellipse(BoxShape),
    /// Straight line.
    Line(LineShape),
    /// Arrow.
    Arrow(ArrowShape),
    /// Freehand stroke / freehand arrow.
    Freehand(FreehandShape),
    /// Text.
    Text(TextShape),
    /// Speech balloon.
    Balloon(BalloonShape),
    /// Step number.
    Step(StepShape),
    /// Magnifier.
    Magnify(MagnifyShape),
    /// Spotlight.
    Spotlight(SpotlightShape),
    /// Blur region.
    Blur(EffectBox),
    /// Pixelate region.
    Pixelate(EffectBox),
    /// Highlighter.
    Highlight(HighlightShape),
    /// Inserted image.
    Image(ImageShape),
    /// Sticker / emoji.
    Sticker(StickerShape),
    /// Cursor stamp.
    Cursor(CursorShape),
    /// Grid / hatch fill.
    Grid(GridShape),
    /// An object kind written by a newer version of ssx; preserved verbatim so saving the
    /// document again does not destroy it. Never rendered.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// One annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Object {
    /// Stable identity.
    pub id: ObjectId,
    /// Hidden objects are skipped by the renderer and hit-testing.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Locked objects cannot be selected by clicking or moved.
    #[serde(default)]
    pub locked: bool,
    /// Objects sharing a group id are selected and moved together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    /// Visual style.
    #[serde(default)]
    pub style: Style,
    /// Geometry and kind-specific data.
    pub kind: ObjectKind,
}

fn default_true() -> bool {
    true
}

/// Axis of a 1-D operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Horizontal axis (x coordinates).
    X,
    /// Vertical axis (y coordinates).
    Y,
}

/// A right-angle rotation or flip of the whole image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orient {
    /// 90° clockwise.
    Rotate90,
    /// 180°.
    Rotate180,
    /// 270° clockwise.
    Rotate270,
    /// Mirror left↔right.
    FlipH,
    /// Mirror top↔bottom.
    FlipV,
}

impl Orient {
    /// Maps a point of a `w`×`h` image to its position in the transformed image.
    pub fn map(self, p: PointF, w: f32, h: f32) -> PointF {
        match self {
            Orient::Rotate90 => PointF::new(h - p.y, p.x),
            Orient::Rotate180 => PointF::new(w - p.x, h - p.y),
            Orient::Rotate270 => PointF::new(p.y, w - p.x),
            Orient::FlipH => PointF::new(w - p.x, p.y),
            Orient::FlipV => PointF::new(p.x, h - p.y),
        }
    }

    /// `true` when width and height swap.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Orient::Rotate90 | Orient::Rotate270)
    }
}

/// Wraps an angle into `(-π, π]`, snapping float noise around zero to exactly `0.0`.
pub fn normalize_angle(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut v = a.rem_euclid(tau);
    if v > std::f32::consts::PI {
        v -= tau;
    }
    if v.abs() < 1e-6 { 0.0 } else { v }
}

/// Distance from `p` to the segment `a`–`b`.
pub fn dist_to_segment(p: PointF, a: PointF, b: PointF) -> f32 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

fn dist_to_polyline(p: PointF, pts: &[PointF]) -> f32 {
    match pts {
        [] => f32::INFINITY,
        [only] => p.distance(*only),
        _ => pts.windows(2).map(|w| dist_to_segment(p, w[0], w[1])).fold(f32::INFINITY, f32::min),
    }
}

/// Is `p` inside the rectangle `r` rotated by `rot` about its centre (inflated by `tol`)?
fn in_rotated_rect(p: PointF, r: RectF, rot: f32, tol: f32) -> bool {
    let local = p.rotate_about(r.center(), -rot);
    r.inflate(tol).contains(local)
}

fn map_axis(v: f32, a: f32, b: f32) -> f32 {
    if v <= a {
        v
    } else if v < b {
        a
    } else {
        v - (b - a)
    }
}

fn cut_point(p: PointF, axis: Axis, a: f32, b: f32) -> PointF {
    match axis {
        Axis::X => PointF::new(map_axis(p.x, a, b), p.y),
        Axis::Y => PointF::new(p.x, map_axis(p.y, a, b)),
    }
}

fn cut_rect(r: RectF, axis: Axis, a: f32, b: f32) -> RectF {
    match axis {
        Axis::X => {
            let (x0, x1) = (map_axis(r.x, a, b), map_axis(r.right(), a, b));
            RectF::new(x0, r.y, (x1 - x0).max(0.0), r.h)
        }
        Axis::Y => {
            let (y0, y1) = (map_axis(r.y, a, b), map_axis(r.bottom(), a, b));
            RectF::new(r.x, y0, r.w, (y1 - y0).max(0.0))
        }
    }
}

impl ObjectKind {
    /// Short machine-friendly name (also the serde tag).
    pub fn name(&self) -> &'static str {
        match self {
            ObjectKind::Rectangle(_) => "rectangle",
            ObjectKind::Ellipse(_) => "ellipse",
            ObjectKind::Line(_) => "line",
            ObjectKind::Arrow(_) => "arrow",
            ObjectKind::Freehand(f) if f.arrow.is_some() => "freehand_arrow",
            ObjectKind::Freehand(_) => "freehand",
            ObjectKind::Text(_) => "text",
            ObjectKind::Balloon(_) => "balloon",
            ObjectKind::Step(_) => "step",
            ObjectKind::Magnify(_) => "magnify",
            ObjectKind::Spotlight(_) => "spotlight",
            ObjectKind::Blur(_) => "blur",
            ObjectKind::Pixelate(_) => "pixelate",
            ObjectKind::Highlight(_) => "highlight",
            ObjectKind::Image(_) => "image",
            ObjectKind::Sticker(_) => "sticker",
            ObjectKind::Cursor(_) => "cursor",
            ObjectKind::Grid(_) => "grid",
            ObjectKind::Unknown(_) => "unknown",
        }
    }

    /// The rotatable box, if this kind is box-like: `(rect, rotation)`.
    pub fn as_box(&self) -> Option<(RectF, f32)> {
        match self {
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => Some((b.rect, b.rotation)),
            ObjectKind::Text(t) => Some((t.rect, t.rotation)),
            ObjectKind::Image(i) => Some((i.rect, i.rotation)),
            ObjectKind::Sticker(s) => Some((s.rect, s.rotation)),
            ObjectKind::Balloon(b) => Some((b.rect, 0.0)),
            ObjectKind::Magnify(m) => Some((m.rect, 0.0)),
            ObjectKind::Spotlight(s) => Some((s.rect, 0.0)),
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => Some((e.rect, 0.0)),
            ObjectKind::Grid(g) => Some((g.rect, 0.0)),
            ObjectKind::Highlight(h) if h.points.is_empty() => Some((h.rect, 0.0)),
            _ => None,
        }
    }

    /// Mutable access to the rectangle of box-like kinds.
    pub fn rect_mut(&mut self) -> Option<&mut RectF> {
        match self {
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => Some(&mut b.rect),
            ObjectKind::Text(t) => Some(&mut t.rect),
            ObjectKind::Image(i) => Some(&mut i.rect),
            ObjectKind::Sticker(s) => Some(&mut s.rect),
            ObjectKind::Balloon(b) => Some(&mut b.rect),
            ObjectKind::Magnify(m) => Some(&mut m.rect),
            ObjectKind::Spotlight(s) => Some(&mut s.rect),
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => Some(&mut e.rect),
            ObjectKind::Grid(g) => Some(&mut g.rect),
            ObjectKind::Highlight(h) if h.points.is_empty() => Some(&mut h.rect),
            _ => None,
        }
    }

    /// Can the user rotate this kind?
    pub fn supports_rotation(&self) -> bool {
        matches!(
            self,
            ObjectKind::Rectangle(_)
                | ObjectKind::Ellipse(_)
                | ObjectKind::Text(_)
                | ObjectKind::Image(_)
                | ObjectKind::Sticker(_)
        )
    }

    /// Current rotation in radians (0 for kinds that cannot rotate).
    pub fn rotation(&self) -> f32 {
        match self {
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => b.rotation,
            ObjectKind::Text(t) => t.rotation,
            ObjectKind::Image(i) => i.rotation,
            ObjectKind::Sticker(s) => s.rotation,
            _ => 0.0,
        }
    }

    /// Sets the rotation; ignored for kinds that cannot rotate.
    pub fn set_rotation(&mut self, radians: f32) {
        match self {
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => b.rotation = radians,
            ObjectKind::Text(t) => t.rotation = radians,
            ObjectKind::Image(i) => i.rotation = radians,
            ObjectKind::Sticker(s) => s.rotation = radians,
            _ => {}
        }
    }

    /// `true` for kinds that read pixels below them (blur, pixelate, magnify, spotlight).
    pub fn reads_backdrop(&self) -> bool {
        matches!(
            self,
            ObjectKind::Blur(_)
                | ObjectKind::Pixelate(_)
                | ObjectKind::Magnify(_)
                | ObjectKind::Spotlight(_)
        )
    }

    /// The text content of text-bearing kinds.
    pub fn text_content(&self) -> Option<&TextContent> {
        match self {
            ObjectKind::Text(t) => Some(&t.content),
            ObjectKind::Balloon(b) => Some(&b.content),
            _ => None,
        }
    }

    /// Mutable text content of text-bearing kinds.
    pub fn text_content_mut(&mut self) -> Option<&mut TextContent> {
        match self {
            ObjectKind::Text(t) => Some(&mut t.content),
            ObjectKind::Balloon(b) => Some(&mut b.content),
            _ => None,
        }
    }

    /// A copy with zeroed geometry but the same properties (font, heads, zoom...). Used
    /// as the per-tool preset that new objects are created from.
    pub fn template(&self) -> ObjectKind {
        let mut k = self.clone();
        match &mut k {
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => *b = BoxShape::default(),
            ObjectKind::Line(l) => *l = LineShape::default(),
            ObjectKind::Arrow(a) => {
                a.a = PointF::default();
                a.b = PointF::default();
            }
            ObjectKind::Freehand(f) => f.points.clear(),
            ObjectKind::Text(t) => {
                t.rect = RectF::default();
                t.rotation = 0.0;
                t.content.text.clear();
            }
            ObjectKind::Balloon(b) => {
                b.rect = RectF::default();
                b.tail = PointF::default();
                b.content.text.clear();
            }
            ObjectKind::Step(s) => {
                s.center = PointF::default();
                s.manual = None;
            }
            ObjectKind::Magnify(m) => {
                m.rect = RectF::default();
                m.source = PointF::default();
            }
            ObjectKind::Spotlight(s) => s.rect = RectF::default(),
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => e.rect = RectF::default(),
            ObjectKind::Highlight(h) => {
                h.rect = RectF::default();
                // A non-empty point list is what marks the pen variant; keep one dummy point.
                h.points = if h.points.is_empty() { Vec::new() } else { vec![PointF::default()] };
            }
            ObjectKind::Image(i) => {
                i.rect = RectF::default();
                i.rotation = 0.0;
            }
            ObjectKind::Sticker(s) => {
                s.rect = RectF::default();
                s.rotation = 0.0;
            }
            ObjectKind::Cursor(c) => c.pos = PointF::default(),
            ObjectKind::Grid(g) => g.rect = RectF::default(),
            ObjectKind::Unknown(_) => {}
        }
        k
    }
}

impl Object {
    /// Creates a visible, unlocked, ungrouped object.
    pub fn new(id: ObjectId, style: Style, kind: ObjectKind) -> Self {
        Self { id, visible: true, locked: false, group: None, style, kind }
    }

    /// Axis-aligned bounds of the geometry (rotation applied, stroke not included).
    pub fn bounds(&self) -> RectF {
        let pts_bounds = |pts: &[PointF]| RectF::bounding(pts).unwrap_or_default();
        match &self.kind {
            ObjectKind::Line(l) => pts_bounds(&[l.a, l.b]),
            ObjectKind::Arrow(a) => pts_bounds(&[a.a, a.b]),
            ObjectKind::Freehand(f) => pts_bounds(&f.points),
            ObjectKind::Highlight(h) if !h.points.is_empty() => pts_bounds(&h.points),
            ObjectKind::Step(s) => RectF::from_center_size(s.center, s.diameter, s.diameter),
            ObjectKind::Cursor(c) => c.bounds(),
            ObjectKind::Balloon(b) => {
                let t = RectF::new(b.tail.x, b.tail.y, 0.0, 0.0);
                b.rect.union(&t)
            }
            ObjectKind::Unknown(_) => RectF::default(),
            other => match other.as_box() {
                Some((r, rot)) => r.rotated_bounds(rot),
                None => RectF::default(),
            },
        }
    }

    /// Conservative bounds of everything the object may paint: stroke, arrow heads, shadow,
    /// blur halo and anti-aliasing fringe. Used for dirty rectangles.
    pub fn render_bounds(&self) -> RectF {
        let mut r = self.bounds();
        let sw = if self.style.has_stroke() { self.style.stroke_width } else { 0.0 };
        let mut grow = sw / 2.0 + 2.0;
        match &self.kind {
            ObjectKind::Arrow(a) => {
                grow += a.heads.length(self.style.stroke_width);
            }
            ObjectKind::Freehand(f) => {
                if let Some(h) = &f.arrow {
                    grow += h.length(self.style.stroke_width);
                }
            }
            ObjectKind::Highlight(h) if !h.points.is_empty() => {
                grow = self.style.stroke_width.max(1.0) / 2.0 + 2.0;
            }
            ObjectKind::Text(t) => {
                if let Some(o) = &t.content.outline {
                    grow += o.width;
                }
                grow += 2.0;
            }
            ObjectKind::Balloon(b) => {
                if let Some(o) = &b.content.outline {
                    grow += o.width;
                }
            }
            ObjectKind::Magnify(_) => grow += 2.0,
            ObjectKind::Spotlight(_) => return RectF::new(-1e6, -1e6, 2e6, 2e6),
            // Region effects paint only inside their own rectangle.
            ObjectKind::Blur(_) | ObjectKind::Pixelate(_) => return self.bounds().inflate(1.0),
            _ => {}
        }
        r = r.inflate(grow);
        if let Some(s) = &self.style.shadow {
            let m = s.blur * 3.0 + 2.0;
            // The blur spreads around both the object and its offset copy; the layer the
            // renderer allocates must hold both.
            let sh = r.inflate(m).union(&r.translate(s.dx, s.dy).inflate(m));
            r = r.union(&sh);
        }
        r
    }

    /// Repairs non-finite or out-of-range numbers (from a buggy caller or hostile input) so an
    /// object can never poison serialisation or rendering. Returns `false` when the geometry
    /// itself is non-finite and cannot be repaired (the caller should reject the change).
    pub fn sanitize(&mut self) -> bool {
        let d = Style::default();
        let fix = |v: &mut f32, default: f32| {
            if !v.is_finite() {
                *v = default;
            }
        };
        let st = &mut self.style;
        fix(&mut st.stroke_width, d.stroke_width);
        st.stroke_width = st.stroke_width.clamp(0.0, 1000.0);
        fix(&mut st.opacity, 1.0);
        st.opacity = st.opacity.clamp(0.0, 1.0);
        fix(&mut st.corner_radius, 0.0);
        st.corner_radius = st.corner_radius.clamp(0.0, 10_000.0);
        if st.shadow.is_some_and(|s| !(s.dx.is_finite() && s.dy.is_finite() && s.blur.is_finite()))
        {
            st.shadow = None;
        }
        if let Some(s) = &mut st.shadow {
            s.blur = s.blur.clamp(0.0, 200.0);
        }
        let text = |c: &mut TextContent| {
            fix(&mut c.font.size, 24.0);
            c.font.size = c.font.size.clamp(1.0, 4096.0);
            fix(&mut c.line_spacing, 1.2);
            c.line_spacing = c.line_spacing.clamp(0.5, 5.0);
            fix(&mut c.padding, 4.0);
            c.padding = c.padding.clamp(0.0, 1000.0);
            if let Some(o) = &mut c.outline {
                fix(&mut o.width, 0.0);
                o.width = o.width.clamp(0.0, 200.0);
            }
        };
        match &mut self.kind {
            ObjectKind::Text(t) => {
                fix(&mut t.rotation, 0.0);
                text(&mut t.content);
            }
            ObjectKind::Balloon(b) => {
                fix(&mut b.tail_width, 24.0);
                text(&mut b.content);
            }
            ObjectKind::Step(s) => {
                fix(&mut s.diameter, 36.0);
                s.diameter = s.diameter.clamp(1.0, 10_000.0);
            }
            ObjectKind::Magnify(m) => {
                fix(&mut m.zoom, 2.0);
                m.zoom = m.zoom.clamp(0.05, 100.0);
            }
            ObjectKind::Spotlight(s) => {
                fix(&mut s.feather, 0.0);
                s.feather = s.feather.clamp(0.0, 500.0);
            }
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => {
                fix(&mut e.amount, 10.0);
                e.amount = e.amount.clamp(0.0, 500.0);
            }
            ObjectKind::Grid(g) => {
                fix(&mut g.spacing, 16.0);
                g.spacing = g.spacing.clamp(2.0, 10_000.0);
            }
            ObjectKind::Cursor(c) => {
                fix(&mut c.scale, 1.0);
                c.scale = c.scale.clamp(0.05, 50.0);
            }
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => fix(&mut b.rotation, 0.0),
            ObjectKind::Image(i) => fix(&mut i.rotation, 0.0),
            ObjectKind::Sticker(s) => fix(&mut s.rotation, 0.0),
            _ => {}
        }
        let b = self.bounds();
        b.is_finite() && b.x.abs() < 1.0e9 && b.y.abs() < 1.0e9 && b.w < 1.0e9 && b.h < 1.0e9
    }

    /// Moves the object.
    pub fn translate(&mut self, dx: f32, dy: f32) {
        let d = PointF::new(dx, dy);
        let mv = |p: &mut PointF| *p = *p + d;
        match &mut self.kind {
            ObjectKind::Line(l) => {
                mv(&mut l.a);
                mv(&mut l.b);
            }
            ObjectKind::Arrow(a) => {
                mv(&mut a.a);
                mv(&mut a.b);
            }
            ObjectKind::Freehand(f) => f.points.iter_mut().for_each(mv),
            ObjectKind::Highlight(h) => {
                h.rect = h.rect.translate(dx, dy);
                h.points.iter_mut().for_each(mv);
            }
            ObjectKind::Step(s) => mv(&mut s.center),
            ObjectKind::Cursor(c) => mv(&mut c.pos),
            ObjectKind::Balloon(b) => {
                b.rect = b.rect.translate(dx, dy);
                mv(&mut b.tail);
            }
            ObjectKind::Magnify(m) => {
                m.rect = m.rect.translate(dx, dy);
                // The source point is deliberately *not* moved: dragging a lens shows a
                // different lens position over the same source, like ShareX. Callers that
                // want the source to follow use `translate_with_source`.
            }
            ObjectKind::Unknown(_) => {}
            other => {
                if let Some(r) = other.rect_mut() {
                    *r = r.translate(dx, dy);
                }
            }
        }
    }

    /// Like [`Object::translate`] but a magnifier's source moves along with the lens.
    pub fn translate_with_source(&mut self, dx: f32, dy: f32) {
        self.translate(dx, dy);
        if let ObjectKind::Magnify(m) = &mut self.kind {
            m.source = m.source + PointF::new(dx, dy);
        }
    }

    /// Scales the geometry about `anchor` (stroke widths and fonts are untouched; see
    /// [`Object::scale_style`]).
    pub fn scale_about(&mut self, anchor: PointF, sx: f32, sy: f32) {
        let sc = |p: &mut PointF| {
            *p = PointF::new(anchor.x + (p.x - anchor.x) * sx, anchor.y + (p.y - anchor.y) * sy);
        };
        match &mut self.kind {
            ObjectKind::Line(l) => {
                sc(&mut l.a);
                sc(&mut l.b);
            }
            ObjectKind::Arrow(a) => {
                sc(&mut a.a);
                sc(&mut a.b);
            }
            ObjectKind::Freehand(f) => f.points.iter_mut().for_each(sc),
            ObjectKind::Highlight(h) => {
                h.rect = h.rect.scale_about(anchor, sx, sy);
                h.points.iter_mut().for_each(sc);
            }
            ObjectKind::Step(s) => {
                sc(&mut s.center);
                s.diameter *= ((sx.abs() * sy.abs()).sqrt()).max(1e-3);
            }
            ObjectKind::Cursor(c) => {
                sc(&mut c.pos);
                c.scale *= ((sx.abs() * sy.abs()).sqrt()).max(1e-3);
            }
            ObjectKind::Balloon(b) => {
                b.rect = b.rect.scale_about(anchor, sx, sy);
                sc(&mut b.tail);
            }
            ObjectKind::Magnify(m) => {
                m.rect = m.rect.scale_about(anchor, sx, sy);
                sc(&mut m.source);
            }
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => {
                e.rect = e.rect.scale_about(anchor, sx, sy);
            }
            ObjectKind::Unknown(_) => {}
            other => {
                if let Some(r) = other.rect_mut() {
                    *r = r.scale_about(anchor, sx, sy);
                }
            }
        }
    }

    /// Scales stroke widths, shadow, corner radius, fonts and effect amounts by `k`
    /// (used when the whole image is resized).
    pub fn scale_style(&mut self, k: f32) {
        self.style.stroke_width *= k;
        self.style.corner_radius *= k;
        if let Some(s) = &mut self.style.shadow {
            s.dx *= k;
            s.dy *= k;
            s.blur *= k;
        }
        let scale_text = |c: &mut TextContent| {
            c.font.size *= k;
            c.padding *= k;
            if let Some(o) = &mut c.outline {
                o.width *= k;
            }
        };
        match &mut self.kind {
            ObjectKind::Text(t) => scale_text(&mut t.content),
            ObjectKind::Balloon(b) => {
                scale_text(&mut b.content);
                b.tail_width *= k;
            }
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => e.amount *= k,
            ObjectKind::Grid(g) => g.spacing *= k,
            ObjectKind::Spotlight(s) => s.feather *= k,
            _ => {}
        }
    }

    /// Applies a whole-image rotation/flip of a `w`×`h` image to this object.
    pub fn orient(&mut self, o: Orient, w: f32, h: f32) {
        let map = |p: &mut PointF| *p = o.map(*p, w, h);
        let rot_delta = match o {
            Orient::Rotate90 => std::f32::consts::FRAC_PI_2,
            Orient::Rotate180 => std::f32::consts::PI,
            Orient::Rotate270 => -std::f32::consts::FRAC_PI_2,
            Orient::FlipH | Orient::FlipV => 0.0,
        };
        let mirror = matches!(o, Orient::FlipH | Orient::FlipV);
        // Box-like kinds map their centre. Rotatable kinds keep their side lengths and
        // adjust the rotation angle (so text and bitmaps turn with the image); axis-aligned
        // effect kinds swap width and height on quarter turns.
        let remap_box = |r: &mut RectF, rot: Option<&mut f32>| {
            let c = o.map(r.center(), w, h);
            if let Some(rot) = rot {
                *r = RectF::from_center_size(c, r.w, r.h);
                *rot = normalize_angle(if mirror { -*rot } else { *rot + rot_delta });
            } else {
                let (bw, bh) = if o.swaps_axes() { (r.h, r.w) } else { (r.w, r.h) };
                *r = RectF::from_center_size(c, bw, bh);
            }
        };
        match &mut self.kind {
            ObjectKind::Line(l) => {
                map(&mut l.a);
                map(&mut l.b);
            }
            ObjectKind::Arrow(a) => {
                map(&mut a.a);
                map(&mut a.b);
            }
            ObjectKind::Freehand(f) => f.points.iter_mut().for_each(map),
            ObjectKind::Highlight(hl) => {
                if hl.points.is_empty() {
                    remap_box(&mut hl.rect, None);
                } else {
                    hl.points.iter_mut().for_each(map);
                }
            }
            ObjectKind::Step(s) => map(&mut s.center),
            ObjectKind::Cursor(c) => map(&mut c.pos),
            ObjectKind::Balloon(b) => {
                remap_box(&mut b.rect, None);
                map(&mut b.tail);
            }
            ObjectKind::Magnify(m) => {
                remap_box(&mut m.rect, None);
                map(&mut m.source);
            }
            ObjectKind::Spotlight(s) => {
                remap_box(&mut s.rect, None);
            }
            ObjectKind::Blur(e) | ObjectKind::Pixelate(e) => {
                remap_box(&mut e.rect, None);
            }
            ObjectKind::Grid(g) => {
                remap_box(&mut g.rect, None);
            }
            ObjectKind::Rectangle(b) | ObjectKind::Ellipse(b) => {
                remap_box(&mut b.rect, Some(&mut b.rotation));
            }
            ObjectKind::Text(t) => remap_box(&mut t.rect, Some(&mut t.rotation)),
            ObjectKind::Image(i) => remap_box(&mut i.rect, Some(&mut i.rotation)),
            ObjectKind::Sticker(s) => remap_box(&mut s.rect, Some(&mut s.rotation)),
            ObjectKind::Unknown(_) => {}
        }
    }

    /// Removes the band `a..b` along `axis` from the geometry and joins the two sides.
    pub fn cut(&mut self, axis: Axis, a: f32, b: f32) {
        let cp = |p: &mut PointF| *p = cut_point(*p, axis, a, b);
        match &mut self.kind {
            ObjectKind::Line(l) => {
                cp(&mut l.a);
                cp(&mut l.b);
            }
            ObjectKind::Arrow(ar) => {
                cp(&mut ar.a);
                cp(&mut ar.b);
            }
            ObjectKind::Freehand(f) => f.points.iter_mut().for_each(cp),
            ObjectKind::Highlight(h) => {
                h.rect = cut_rect(h.rect, axis, a, b);
                h.points.iter_mut().for_each(cp);
            }
            ObjectKind::Step(s) => cp(&mut s.center),
            ObjectKind::Cursor(c) => cp(&mut c.pos),
            ObjectKind::Balloon(bl) => {
                bl.rect = cut_rect(bl.rect, axis, a, b);
                cp(&mut bl.tail);
            }
            ObjectKind::Magnify(m) => {
                m.rect = cut_rect(m.rect, axis, a, b);
                cp(&mut m.source);
            }
            ObjectKind::Unknown(_) => {}
            other => {
                let rot = other.rotation();
                if let Some(r) = other.rect_mut() {
                    if rot == 0.0 {
                        *r = cut_rect(*r, axis, a, b);
                    } else {
                        let c = cut_point(r.center(), axis, a, b);
                        *r = RectF::from_center_size(c, r.w, r.h);
                    }
                }
            }
        }
    }

    /// Is `p` inside the object's (rotated) box, regardless of fill? Used for placing a text
    /// caret by clicking inside an edited text box.
    pub fn hit_test_box(&self, p: PointF, tol: f32) -> bool {
        match self.kind.as_box() {
            Some((r, rot)) => in_rotated_rect(p, r.normalized(), rot, tol),
            None => false,
        }
    }

    /// Does a click at `p` (image space) hit this object? `tol` is extra slack in image
    /// pixels (a GUI passes a few screen pixels divided by the zoom).
    pub fn hit_test(&self, p: PointF, tol: f32) -> bool {
        if !self.visible {
            return false;
        }
        let sw = if self.style.has_stroke() { self.style.stroke_width } else { 0.0 };
        match &self.kind {
            ObjectKind::Rectangle(b) => {
                let local = p.rotate_about(b.rect.center(), -b.rotation);
                let r = b.rect.normalized();
                let outer = r.inflate(sw / 2.0 + tol);
                if !outer.contains(local) {
                    return false;
                }
                if !self.style.fill.is_none() {
                    return true;
                }
                let inner = r.inflate(-(sw / 2.0 + tol));
                inner.w <= 0.0 || inner.h <= 0.0 || !inner.contains(local)
            }
            ObjectKind::Ellipse(b) => {
                let r = b.rect.normalized();
                let local = p.rotate_about(r.center(), -b.rotation);
                let (rx, ry) = ((r.w / 2.0).max(0.5), (r.h / 2.0).max(0.5));
                let (dx, dy) = ((local.x - r.center().x) / rx, (local.y - r.center().y) / ry);
                let n = dx.hypot(dy);
                let slack = sw / 2.0 + tol;
                let outer = 1.0 + slack / rx.min(ry);
                if n > outer {
                    return false;
                }
                if !self.style.fill.is_none() {
                    return true;
                }
                n >= 1.0 - slack / rx.min(ry)
            }
            ObjectKind::Line(l) => dist_to_segment(p, l.a, l.b) <= sw / 2.0 + tol + 1.0,
            ObjectKind::Arrow(a) => {
                let head = a.heads.length(self.style.stroke_width);
                dist_to_segment(p, a.a, a.b) <= sw / 2.0 + tol + 1.0
                    || (a.heads.end != HeadStyle::None && p.distance(a.b) <= head * 0.5 + tol)
                    || (a.heads.start != HeadStyle::None && p.distance(a.a) <= head * 0.5 + tol)
            }
            ObjectKind::Freehand(f) => dist_to_polyline(p, &f.points) <= sw / 2.0 + tol + 1.0,
            ObjectKind::Highlight(h) => {
                if h.points.is_empty() {
                    h.rect.normalized().inflate(tol).contains(p)
                } else {
                    dist_to_polyline(p, &h.points) <= self.style.stroke_width / 2.0 + tol
                }
            }
            ObjectKind::Step(s) => p.distance(s.center) <= s.diameter / 2.0 + tol,
            ObjectKind::Cursor(c) => c.bounds().inflate(tol).contains(p),
            ObjectKind::Balloon(b) => {
                b.rect.normalized().inflate(tol).contains(p)
                    || point_in_triangle(p, balloon_tail_triangle(b), tol)
            }
            ObjectKind::Magnify(m) => {
                let r = m.rect.normalized();
                if m.circular {
                    let c = r.center();
                    let (rx, ry) = ((r.w / 2.0).max(0.5) + tol, (r.h / 2.0).max(0.5) + tol);
                    ((p.x - c.x) / rx).hypot((p.y - c.y) / ry) <= 1.0
                } else {
                    r.inflate(tol).contains(p)
                }
            }
            ObjectKind::Spotlight(s) => {
                let r = s.rect.normalized();
                if s.ellipse {
                    let c = r.center();
                    let (rx, ry) = ((r.w / 2.0).max(0.5) + tol, (r.h / 2.0).max(0.5) + tol);
                    ((p.x - c.x) / rx).hypot((p.y - c.y) / ry) <= 1.0
                } else {
                    r.inflate(tol).contains(p)
                }
            }
            ObjectKind::Unknown(_) => false,
            other => match other.as_box() {
                Some((r, rot)) => in_rotated_rect(p, r.normalized(), rot, tol),
                None => false,
            },
        }
    }
}

/// Which side of the balloon body the tail attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailSide {
    /// Top edge.
    Top,
    /// Right edge.
    Right,
    /// Bottom edge.
    Bottom,
    /// Left edge.
    Left,
}

/// Geometry of a balloon tail: the two base points on the body edge (ordered by increasing
/// x for top/bottom, increasing y for left/right) and the tip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BalloonTail {
    /// Attachment side.
    pub side: TailSide,
    /// First base point (smaller coordinate along the edge).
    pub a: PointF,
    /// Second base point.
    pub b: PointF,
    /// Tail tip.
    pub tip: PointF,
}

/// Computes the tail of a balloon, or `None` when the tip is inside the body (no tail).
pub fn balloon_tail(b: &BalloonShape) -> Option<BalloonTail> {
    let r = b.rect.normalized();
    if r.is_empty() || r.contains(b.tail) {
        return None;
    }
    let c = r.center();
    let d = b.tail - c;
    let half = (b.tail_width / 2.0).min(r.w / 2.0).min(r.h / 2.0).max(1.0);
    // Pick the side by comparing the tip direction to the rectangle's aspect ratio.
    let horizontal = d.x.abs() * r.h > d.y.abs() * r.w;
    Some(if horizontal {
        let (x, side) =
            if d.x > 0.0 { (r.right(), TailSide::Right) } else { (r.x, TailSide::Left) };
        let cy = b.tail.y.clamp(r.y + half, (r.bottom() - half).max(r.y + half));
        BalloonTail {
            side,
            a: PointF::new(x, cy - half),
            b: PointF::new(x, cy + half),
            tip: b.tail,
        }
    } else {
        let (y, side) =
            if d.y > 0.0 { (r.bottom(), TailSide::Bottom) } else { (r.y, TailSide::Top) };
        let cx = b.tail.x.clamp(r.x + half, (r.right() - half).max(r.x + half));
        BalloonTail {
            side,
            a: PointF::new(cx - half, y),
            b: PointF::new(cx + half, y),
            tip: b.tail,
        }
    })
}

/// The triangle joining a balloon body to its tail tip (degenerate when there is no tail).
pub fn balloon_tail_triangle(b: &BalloonShape) -> [PointF; 3] {
    match balloon_tail(b) {
        Some(t) => [t.a, t.b, t.tip],
        None => [b.tail; 3],
    }
}

fn point_in_triangle(p: PointF, t: [PointF; 3], tol: f32) -> bool {
    let sign =
        |a: PointF, b: PointF, c: PointF| (a.x - c.x) * (b.y - c.y) - (b.x - c.x) * (a.y - c.y);
    let (d1, d2, d3) = (sign(p, t[0], t[1]), sign(p, t[1], t[2]), sign(p, t[2], t[0]));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    if !(neg && pos) {
        return true;
    }
    tol > 0.0
        && (dist_to_segment(p, t[0], t[1]) <= tol
            || dist_to_segment(p, t[1], t[2]) <= tol
            || dist_to_segment(p, t[2], t[0]) <= tol)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact geometry values are what these tests assert
mod tests {
    use super::*;

    fn rect_obj(x: f32, y: f32, w: f32, h: f32) -> Object {
        Object::new(
            ObjectId(1),
            Style::default(),
            ObjectKind::Rectangle(BoxShape { rect: RectF::new(x, y, w, h), rotation: 0.0 }),
        )
    }

    #[test]
    fn unfilled_rectangle_hits_border_not_interior() {
        let o = rect_obj(10.0, 10.0, 100.0, 60.0);
        assert!(o.hit_test(PointF::new(10.0, 40.0), 0.0), "left edge");
        assert!(o.hit_test(PointF::new(111.0, 40.0), 0.0), "just outside the stroke");
        assert!(!o.hit_test(PointF::new(60.0, 40.0), 0.0), "interior of unfilled rect");
        assert!(o.hit_test(PointF::new(60.0, 40.0), 60.0), "big tolerance reaches the border");
        assert!(!o.hit_test(PointF::new(200.0, 40.0), 3.0));
    }

    #[test]
    fn filled_rectangle_hits_interior() {
        let mut o = rect_obj(10.0, 10.0, 100.0, 60.0);
        o.style.fill = crate::style::Fill::solid(Color::WHITE);
        assert!(o.hit_test(PointF::new(60.0, 40.0), 0.0));
        assert!(!o.hit_test(PointF::new(60.0, 100.0), 0.0));
    }

    #[test]
    fn rotated_rectangle_hit() {
        let mut o = rect_obj(0.0, 0.0, 100.0, 10.0);
        o.style.fill = crate::style::Fill::solid(Color::WHITE);
        o.kind.set_rotation(std::f32::consts::FRAC_PI_2);
        // Rotated 90° about (50, 5): now a vertical bar x in [45,55], y in [-45,55].
        assert!(o.hit_test(PointF::new(50.0, -30.0), 0.0));
        assert!(!o.hit_test(PointF::new(90.0, 5.0), 0.0));
    }

    #[test]
    fn line_and_arrow_hit() {
        let mut o = Object::new(
            ObjectId(2),
            Style { stroke_width: 4.0, ..Style::default() },
            ObjectKind::Line(LineShape { a: PointF::new(0.0, 0.0), b: PointF::new(100.0, 0.0) }),
        );
        assert!(o.hit_test(PointF::new(50.0, 2.5), 0.0));
        assert!(!o.hit_test(PointF::new(50.0, 10.0), 2.0));
        assert!(o.hit_test(PointF::new(50.0, 10.0), 8.0));
        o.kind = ObjectKind::Arrow(ArrowShape {
            a: PointF::new(0.0, 0.0),
            b: PointF::new(100.0, 0.0),
            heads: ArrowHeads::default(),
        });
        assert!(o.hit_test(PointF::new(100.0, 6.0), 0.0), "arrow head widens the target");
    }

    #[test]
    fn ellipse_hit_outline_and_fill() {
        let mut o = Object::new(
            ObjectId(3),
            Style { stroke_width: 2.0, ..Style::default() },
            ObjectKind::Ellipse(BoxShape {
                rect: RectF::new(0.0, 0.0, 100.0, 50.0),
                rotation: 0.0,
            }),
        );
        assert!(o.hit_test(PointF::new(0.0, 25.0), 0.0));
        assert!(o.hit_test(PointF::new(50.0, 0.0), 0.0));
        assert!(!o.hit_test(PointF::new(50.0, 25.0), 0.0));
        assert!(!o.hit_test(PointF::new(2.0, 2.0), 0.0), "bounding-box corner is outside");
        o.style.fill = crate::style::Fill::solid(Color::WHITE);
        assert!(o.hit_test(PointF::new(50.0, 25.0), 0.0));
    }

    #[test]
    fn hidden_objects_never_hit() {
        let mut o = rect_obj(0.0, 0.0, 10.0, 10.0);
        o.visible = false;
        assert!(!o.hit_test(PointF::new(0.0, 5.0), 5.0));
    }

    #[test]
    fn orient_maps_points_and_boxes() {
        let mut o = rect_obj(10.0, 20.0, 30.0, 40.0);
        // 100x50 image, rotate 90° CW.
        o.orient(Orient::Rotate90, 100.0, 50.0);
        let (r, rot) = o.kind.as_box().unwrap();
        assert!((rot - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        // Rotatable boxes keep their sides; centre (25,40) -> (50-40, 25) = (10, 25).
        assert_eq!((r.w, r.h), (30.0, 40.0));
        assert_eq!(r.center(), PointF::new(10.0, 25.0));
        // Four quarter turns bring a line back exactly.
        let mut l = Object::new(
            ObjectId(4),
            Style::default(),
            ObjectKind::Line(LineShape { a: PointF::new(3.0, 4.0), b: PointF::new(90.0, 40.0) }),
        );
        let orig = l.clone();
        let (mut w, mut h) = (100.0, 50.0);
        for _ in 0..4 {
            l.orient(Orient::Rotate90, w, h);
            std::mem::swap(&mut w, &mut h);
        }
        assert_eq!(l, orig);
        l.orient(Orient::FlipH, 100.0, 50.0);
        l.orient(Orient::FlipH, 100.0, 50.0);
        assert_eq!(l, orig);
    }

    #[test]
    fn cut_joins_sides() {
        let mut o = Object::new(
            ObjectId(5),
            Style::default(),
            ObjectKind::Line(LineShape { a: PointF::new(10.0, 0.0), b: PointF::new(100.0, 0.0) }),
        );
        o.cut(Axis::X, 40.0, 60.0);
        match &o.kind {
            ObjectKind::Line(l) => {
                assert_eq!(l.a.x, 10.0);
                assert_eq!(l.b.x, 80.0);
            }
            _ => unreachable!(),
        }
        let mut r = rect_obj(30.0, 0.0, 50.0, 10.0);
        r.cut(Axis::X, 40.0, 60.0);
        let (rr, _) = r.kind.as_box().unwrap();
        assert_eq!((rr.x, rr.w), (30.0, 30.0));
        // Entirely inside the removed band -> zero width.
        let mut inside = rect_obj(45.0, 0.0, 10.0, 10.0);
        inside.cut(Axis::X, 40.0, 60.0);
        assert_eq!(inside.kind.as_box().unwrap().0.w, 0.0);
    }

    #[test]
    fn scale_about_and_style() {
        let mut o = rect_obj(10.0, 10.0, 20.0, 20.0);
        o.scale_about(PointF::new(0.0, 0.0), 2.0, 3.0);
        assert_eq!(o.kind.as_box().unwrap().0, RectF::new(20.0, 30.0, 40.0, 60.0));
        o.scale_style(2.0);
        assert_eq!(o.style.stroke_width, 8.0);
    }

    #[test]
    fn unknown_kind_survives_round_trip() {
        let json = r##"{"id":7,"kind":{"type":"hologram","depth":3,"tint":"#fff"}}"##;
        let o: Object = serde_json::from_str(json).unwrap();
        assert!(matches!(o.kind, ObjectKind::Unknown(_)));
        let back = serde_json::to_value(&o).unwrap();
        assert_eq!(back["kind"]["type"], "hologram");
        assert_eq!(back["kind"]["depth"], 3);
    }

    #[test]
    fn balloon_tail_is_hit() {
        let b = BalloonShape {
            rect: RectF::new(0.0, 0.0, 100.0, 50.0),
            tail: PointF::new(50.0, 90.0),
            ..BalloonShape::default()
        };
        let o = Object::new(ObjectId(8), Style::default(), ObjectKind::Balloon(b));
        assert!(o.hit_test(PointF::new(50.0, 70.0), 0.0));
        assert!(!o.hit_test(PointF::new(90.0, 70.0), 0.0));
    }

    #[test]
    fn render_bounds_cover_stroke_and_shadow() {
        let mut o = rect_obj(10.0, 10.0, 20.0, 20.0);
        o.style.stroke_width = 10.0;
        let base = o.render_bounds();
        assert!(base.x <= 10.0 - 5.0 && base.right() >= 30.0 + 5.0);
        o.style.shadow =
            Some(crate::style::Shadow { dx: 10.0, dy: 10.0, blur: 4.0, ..Default::default() });
        let with_shadow = o.render_bounds();
        assert!(with_shadow.right() >= base.right() + 10.0);
    }
}
