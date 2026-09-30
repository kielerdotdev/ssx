//! Floating-point geometry and colour used by the document model.
//!
//! The model lives in **image space** (`f32`, origin = top-left of the base image, y down).
//! `f32` is deliberate: JSON round-trips `f32` exactly (shortest representation), which the
//! byte-for-byte undo guarantees rely on, and it is what the rasteriser consumes anyway.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ssx_types::Rect;

pub use ssx_imgfx::PointF;

/// An axis-aligned rectangle (`x`, `y` = top-left; `w`, `h` ≥ 0 after [`RectF::normalized`]).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RectF {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl RectF {
    /// Creates a rectangle.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The normalised rectangle spanning two arbitrary corners (drag in any direction).
    pub fn from_points(a: PointF, b: PointF) -> Self {
        Self { x: a.x.min(b.x), y: a.y.min(b.y), w: (a.x - b.x).abs(), h: (a.y - b.y).abs() }
    }

    /// A rectangle centred on `c`.
    pub fn from_center_size(c: PointF, w: f32, h: f32) -> Self {
        Self { x: c.x - w / 2.0, y: c.y - h / 2.0, w, h }
    }

    /// Right edge.
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// Bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Centre point.
    pub fn center(&self) -> PointF {
        PointF::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Top-left corner.
    pub fn origin(&self) -> PointF {
        PointF::new(self.x, self.y)
    }

    /// Flips negative width/height into positive ones, keeping the same area.
    pub fn normalized(&self) -> Self {
        let (x, w) = if self.w < 0.0 { (self.x + self.w, -self.w) } else { (self.x, self.w) };
        let (y, h) = if self.h < 0.0 { (self.y + self.h, -self.h) } else { (self.y, self.h) };
        Self { x, y, w, h }
    }

    /// `true` when either dimension is (nearly) zero.
    pub fn is_empty(&self) -> bool {
        self.w <= f32::EPSILON || self.h <= f32::EPSILON
    }

    /// `true` when `p` lies inside (edges inclusive).
    pub fn contains(&self, p: PointF) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    /// `true` when the two rectangles overlap (touching edges count).
    pub fn intersects(&self, o: &RectF) -> bool {
        self.x <= o.right() && o.x <= self.right() && self.y <= o.bottom() && o.y <= self.bottom()
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, o: &RectF) -> RectF {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        RectF::new(x0, y0, self.right().max(o.right()) - x0, self.bottom().max(o.bottom()) - y0)
    }

    /// Grows by `d` on every side (shrinks for negative `d`, never below zero size).
    pub fn inflate(&self, d: f32) -> RectF {
        let w = (self.w + 2.0 * d).max(0.0);
        let h = (self.h + 2.0 * d).max(0.0);
        RectF::new(self.x + (self.w - w) / 2.0, self.y + (self.h - h) / 2.0, w, h)
    }

    /// Moves the rectangle.
    pub fn translate(&self, dx: f32, dy: f32) -> RectF {
        RectF::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// The four corners clockwise from the top-left.
    pub fn corners(&self) -> [PointF; 4] {
        [
            PointF::new(self.x, self.y),
            PointF::new(self.right(), self.y),
            PointF::new(self.right(), self.bottom()),
            PointF::new(self.x, self.bottom()),
        ]
    }

    /// Bounding box of the rectangle rotated by `radians` about its centre.
    pub fn rotated_bounds(&self, radians: f32) -> RectF {
        if radians == 0.0 {
            return *self;
        }
        let c = self.center();
        let pts = self.corners().map(|p| p.rotate_about(c, radians));
        Self::bounding(&pts).unwrap_or(*self)
    }

    /// Bounding box of points; `None` for an empty slice.
    pub fn bounding(pts: &[PointF]) -> Option<RectF> {
        let first = pts.first()?;
        let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        Some(RectF::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// The smallest integer rectangle containing this one (outward rounding).
    pub fn to_outer_rect(&self) -> Rect {
        let n = self.normalized();
        let x0 = n.x.floor().clamp(-1e9, 1e9);
        let y0 = n.y.floor().clamp(-1e9, 1e9);
        let x1 = n.right().ceil().clamp(-1e9, 1e9);
        let y1 = n.bottom().ceil().clamp(-1e9, 1e9);
        Rect::new(x0 as i32, y0 as i32, (x1 - x0).max(0.0) as u32, (y1 - y0).max(0.0) as u32)
    }

    /// `true` when all four numbers are finite.
    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.w.is_finite() && self.h.is_finite()
    }

    /// Scales positions and size about `anchor`.
    pub fn scale_about(&self, anchor: PointF, sx: f32, sy: f32) -> RectF {
        RectF::new(
            anchor.x + (self.x - anchor.x) * sx,
            anchor.y + (self.y - anchor.y) * sy,
            self.w * sx,
            self.h * sy,
        )
        .normalized()
    }
}

impl From<Rect> for RectF {
    fn from(r: Rect) -> Self {
        RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32)
    }
}

/// An RGBA colour (straight alpha, sRGB), serialised as `#rrggbbaa`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha (255 = opaque).
    pub a: u8,
}

impl Color {
    /// Fully transparent.
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    /// Opaque black.
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    /// Opaque red (`ShareX`'s default shape colour).
    pub const RED: Color = Color::rgb(255, 0, 0);
    /// Opaque yellow (highlighter).
    pub const YELLOW: Color = Color::rgb(255, 255, 0);

    /// Builds a colour from all four channels.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Builds an opaque colour.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Same colour with a different alpha.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// As `[r, g, b, a]`.
    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// `true` when fully transparent.
    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// `#rrggbbaa`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
    }

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (leading `#` optional).
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if !s.is_ascii() {
            return None;
        }
        let p = |i: usize, n: usize| u8::from_str_radix(&s[i..i + n], 16).ok();
        match s.len() {
            3 => {
                let d = |i: usize| p(i, 1).map(|v| v * 17);
                Some(Color::rgb(d(0)?, d(1)?, d(2)?))
            }
            6 => Some(Color::rgb(p(0, 2)?, p(2, 2)?, p(4, 2)?)),
            8 => Some(Color::rgba(p(0, 2)?, p(2, 2)?, p(4, 2)?, p(6, 2)?)),
            _ => None,
        }
    }

    /// Converts to `tiny-skia`'s float colour.
    pub fn to_skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }

    /// Scales alpha by `opacity` (0–1).
    pub fn with_opacity(self, opacity: f32) -> Self {
        Self { a: (f32::from(self.a) * opacity.clamp(0.0, 1.0)).round() as u8, ..self }
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::from_hex(&s).ok_or_else(|| serde::de::Error::custom(format!("bad colour {s:?}")))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact geometry values are what these tests assert
mod tests {
    use super::*;

    #[test]
    fn colour_hex_round_trip() {
        let c = Color::rgba(1, 2, 3, 4);
        assert_eq!(Color::from_hex(&c.to_hex()), Some(c));
        assert_eq!(Color::from_hex("#f80"), Some(Color::rgb(255, 136, 0)));
        assert_eq!(Color::from_hex("00ff00"), Some(Color::rgb(0, 255, 0)));
        assert_eq!(Color::from_hex("#12"), None);
        assert_eq!(Color::from_hex("#gg0000"), None);
        assert_eq!(Color::from_hex("#é12345"), None);
        assert_eq!(serde_json::to_string(&Color::RED).unwrap(), "\"#ff0000ff\"");
    }

    #[test]
    fn rect_basics() {
        let r = RectF::from_points(PointF::new(10.0, 20.0), PointF::new(2.0, 5.0));
        assert_eq!(r, RectF::new(2.0, 5.0, 8.0, 15.0));
        assert_eq!(RectF::new(5.0, 5.0, -3.0, -2.0).normalized(), RectF::new(2.0, 3.0, 3.0, 2.0));
        assert!(r.contains(PointF::new(2.0, 5.0)));
        assert!(!r.contains(PointF::new(1.9, 5.0)));
        assert_eq!(r.union(&RectF::new(0.0, 0.0, 1.0, 1.0)), RectF::new(0.0, 0.0, 10.0, 20.0));
        assert_eq!(RectF::new(0.2, 0.2, 1.0, 1.0).to_outer_rect(), Rect::new(0, 0, 2, 2));
        assert_eq!(RectF::new(0.0, 0.0, 4.0, 2.0).inflate(-5.0).w, 0.0);
    }

    #[test]
    fn rotated_bounds_of_square() {
        let r = RectF::new(0.0, 0.0, 10.0, 10.0);
        let b = r.rotated_bounds(std::f32::consts::FRAC_PI_4);
        assert!((b.w - 14.142_136).abs() < 1e-3);
        assert!((b.center().x - 5.0).abs() < 1e-4);
    }
}
