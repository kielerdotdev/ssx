//! Pure, parallel image effects for ssx.
//!
//! Everything here works on [`ssx_types::Frame`]s that are **8-bit RGBA, sRGB, straight
//! (non-premultiplied) alpha** — the format the rest of ssx passes around. Other layouts
//! are rejected with [`FxError::UnsupportedFormat`]; convert first with
//! [`Frame::into_rgba8`](ssx_types::Frame::into_rgba8).
//!
//! # Design decisions
//!
//! * **Never panic on geometry.** Regions are `Option<Rect>` in *frame-local* pixels; they
//!   are clipped to the image, and an empty intersection is a no-op. Zero-sized and 1-px
//!   images are valid inputs everywhere.
//! * **Premultiplied maths where it matters.** Blur, resize, rotate, shadow and lens
//!   sampling filter in premultiplied space so transparent pixels never bleed their (often
//!   black) colour into neighbours.
//! * **sRGB-space filtering.** Like GDI+/Photoshop defaults (and unlike linear-light
//!   pipelines) filters run on the gamma-encoded values. This keeps results identical to
//!   what users of `ShareX` see and keeps the maths cheap.
//! * **Region ops are self-contained.** A region operation gives exactly the result of
//!   cropping the region, filtering it, and pasting it back (edge pixels clamp to the
//!   *region* border, not to neighbouring content). This is what an "obscure this area"
//!   tool wants and makes the ops trivially testable.
//! * **Parallelism** is rayon over rows; no global state, no unsafe.
//!
//! The size-changing operations return a [`Placed`] so callers (the editor) can shift
//! annotations by [`Placed::origin`].

#![forbid(unsafe_code)]

mod blur;
mod color;
mod composite;
mod edge;
mod effect;
mod lens;
mod pixelate;
mod region;
mod resize;
mod shadow;
mod transform;

use serde::{Deserialize, Serialize};
use ssx_types::{ColorSpace, Frame, PixelFormat, Point};

pub use blur::{BlurMethod, gaussian_blur, gaussian_blur_premultiplied, sharpen, unsharp_mask};
pub use color::{
    brightness, contrast, gamma, grayscale, hue_rotate, invert, saturation, sepia, threshold,
};
pub use composite::{BlendMode, blend_pixel, composite_over, fill_rect};
pub use edge::{EdgeKind, EdgeSides, edge_effect, round_corners};
pub use effect::Effect;
pub use lens::{Lens, LensShape, magnify};
pub use pixelate::pixelate;
pub use region::clip_region;
pub use resize::{ResizeFilter, resize};
pub use shadow::{ShadowParams, add_border, drop_shadow, outline};
pub use transform::{
    auto_crop_bounds, crop, flip_horizontal, flip_vertical, pad, rotate, rotate_90, rotate_180,
    rotate_270,
};

/// A straight-alpha RGBA colour, one byte per channel.
pub type Rgba = [u8; 4];

/// A sub-pixel position. Used by the lens and rotation code; the editor re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PointF {
    /// Horizontal position in pixels.
    pub x: f32,
    /// Vertical position in pixels.
    pub y: f32,
}

impl PointF {
    /// Creates a point.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to `o`.
    pub fn distance(self, o: PointF) -> f32 {
        (self - o).length()
    }

    /// Length of the vector from the origin.
    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }

    /// Linear interpolation towards `o` (`t = 0` is `self`, `t = 1` is `o`).
    pub fn lerp(self, o: PointF, t: f32) -> PointF {
        PointF::new(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }

    /// Dot product.
    pub fn dot(self, o: PointF) -> f32 {
        self.x * o.x + self.y * o.y
    }

    /// Rotates about `centre` by `radians` (clockwise on screen, y down).
    pub fn rotate_about(self, centre: PointF, radians: f32) -> PointF {
        let (s, c) = radians.sin_cos();
        let d = self - centre;
        PointF::new(centre.x + d.x * c - d.y * s, centre.y + d.x * s + d.y * c)
    }

    /// `true` when both coordinates are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl std::ops::Add for PointF {
    type Output = PointF;
    fn add(self, o: PointF) -> PointF {
        PointF::new(self.x + o.x, self.y + o.y)
    }
}

impl std::ops::Sub for PointF {
    type Output = PointF;
    fn sub(self, o: PointF) -> PointF {
        PointF::new(self.x - o.x, self.y - o.y)
    }
}

impl std::ops::Mul<f32> for PointF {
    type Output = PointF;
    fn mul(self, k: f32) -> PointF {
        PointF::new(self.x * k, self.y * k)
    }
}

impl std::ops::Neg for PointF {
    type Output = PointF;
    fn neg(self) -> PointF {
        PointF::new(-self.x, -self.y)
    }
}

/// Errors from effect operations. Geometry problems are never errors (they clip).
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FxError {
    /// The frame is not 8-bit sRGB RGBA.
    #[error(
        "image effects need an 8-bit sRGB RGBA frame, got {0:?}/{1:?}; convert it with `Frame::into_rgba8` (tonemap HDR first)"
    )]
    UnsupportedFormat(PixelFormat, ColorSpace),
    /// A numeric parameter was NaN or infinite.
    #[error("parameter `{0}` must be a finite number")]
    NotFinite(&'static str),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, FxError>;

/// The result of a size-changing effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    /// The new image.
    pub frame: Frame,
    /// Where the *input's* top-left pixel now lives inside [`Placed::frame`] (may be
    /// negative when content was cropped away on the left/top).
    pub origin: Point,
}

pub(crate) fn check(frame: &Frame) -> Result<()> {
    if frame.format() == PixelFormat::Rgba8 && frame.color_space() == ColorSpace::Srgb {
        Ok(())
    } else {
        Err(FxError::UnsupportedFormat(frame.format(), frame.color_space()))
    }
}

/// Saturating conversion to `i32` (image dimensions never approach the limit; this just
/// keeps the casts lint-clean without silent wrap-around).
pub(crate) fn i32c<T: TryInto<i32>>(v: T) -> i32 {
    v.try_into().unwrap_or(i32::MAX)
}

pub(crate) fn finite(v: f32, name: &'static str) -> Result<f32> {
    if v.is_finite() { Ok(v) } else { Err(FxError::NotFinite(name)) }
}

/// Creates a tightly packed transparent RGBA frame (`w`×`h`), filled with `fill`.
pub fn solid_frame(width: u32, height: u32, fill: Rgba) -> Frame {
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..(width as usize * height as usize) {
        data.extend_from_slice(&fill);
    }
    Frame::from_rgba8(width, height, data).expect("exact-size buffer by construction")
}

/// Reads pixel `(x, y)` (frame-local); out-of-range reads return transparent black.
pub fn get_pixel(frame: &Frame, x: i32, y: i32) -> Rgba {
    if x < 0 || y < 0 || x >= i32c(frame.width()) || y >= i32c(frame.height()) {
        return [0; 4];
    }
    let row = frame.row(y as u32);
    let i = x as usize * 4;
    [row[i], row[i + 1], row[i + 2], row[i + 3]]
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// Deterministic pseudo-random RGBA image (opaque unless `alpha` is set).
    pub fn noise(w: u32, h: u32, alpha: bool, seed: u32) -> Frame {
        let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(12345);
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 8) as u8
        };
        let mut data = Vec::new();
        for _ in 0..(w * h) {
            let (r, g, b) = (next(), next(), next());
            let a = if alpha { next() } else { 255 };
            data.extend_from_slice(&[r, g, b, a]);
        }
        Frame::from_rgba8(w, h, data).unwrap()
    }

    pub fn px(f: &Frame, x: u32, y: u32) -> Rgba {
        get_pixel(f, i32c(x), i32c(y))
    }

    pub fn psnr(a: &Frame, b: &Frame) -> f64 {
        assert_eq!(a.size(), b.size());
        let mut se = 0f64;
        let mut n = 0f64;
        for y in 0..a.height() {
            for (p, q) in a.row(y).iter().zip(b.row(y)) {
                let d = f64::from(*p) - f64::from(*q);
                se += d * d;
                n += 1.0;
            }
        }
        if se == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / (se / n)).log10() }
    }
}
