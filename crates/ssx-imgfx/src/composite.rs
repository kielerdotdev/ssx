//! Alpha compositing and blend modes.
//!
//! Implements the W3C Compositing and Blending Level 1 "source over" formula with a
//! separable blend function `B(cb, cs)` between the backdrop and source colours, on
//! straight-alpha sRGB values:
//!
//! ```text
//! αo = αs + αb(1 − αs)
//! Co = (αs(1 − αb)·Cs + αs·αb·B(Cb, Cs) + (1 − αs)·αb·Cb) / αo
//! ```
//!
//! [`BlendMode::Multiply`] is what a highlighter marker uses: it darkens like a real marker
//! over text instead of covering it.

use rayon::prelude::*;
use ssx_types::{Frame, Point, Rect};

use crate::{Result, Rgba, check, finite, i32c};

/// Separable blend modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    /// Ordinary alpha-over.
    #[default]
    Normal,
    /// `cb * cs` — darkens; white is neutral. Used for highlighter markers.
    Multiply,
    /// `cb + cs − cb·cs` — lightens; black is neutral.
    Screen,
    /// Multiply on dark backdrops, screen on light ones.
    Overlay,
    /// Per-channel minimum.
    Darken,
    /// Per-channel maximum.
    Lighten,
}

impl BlendMode {
    /// The blend function on 0–1 channel values.
    pub fn apply(self, cb: f32, cs: f32) -> f32 {
        match self {
            BlendMode::Normal => cs,
            BlendMode::Multiply => cb * cs,
            BlendMode::Screen => cb + cs - cb * cs,
            BlendMode::Overlay => {
                if cb <= 0.5 {
                    2.0 * cb * cs
                } else {
                    1.0 - 2.0 * (1.0 - cb) * (1.0 - cs)
                }
            }
            BlendMode::Darken => cb.min(cs),
            BlendMode::Lighten => cb.max(cs),
        }
    }
}

/// Composites one straight-alpha `src` pixel (scaled by `opacity`, 0–1) over `dst`.
pub fn blend_pixel(dst: Rgba, src: Rgba, opacity: f32, mode: BlendMode) -> Rgba {
    let a_s = f32::from(src[3]) / 255.0 * opacity.clamp(0.0, 1.0);
    if a_s <= 0.0 {
        return dst;
    }
    let a_b = f32::from(dst[3]) / 255.0;
    if mode == BlendMode::Normal && dst[3] == 255 && (a_s - 1.0).abs() < f32::EPSILON {
        return [src[0], src[1], src[2], 255];
    }
    let a_o = a_s + a_b * (1.0 - a_s);
    let mut out = [0u8; 4];
    for c in 0..3 {
        let cb = f32::from(dst[c]) / 255.0;
        let cs = f32::from(src[c]) / 255.0;
        let b = mode.apply(cb, cs);
        let co = (a_s * (1.0 - a_b) * cs + a_s * a_b * b + (1.0 - a_s) * a_b * cb) / a_o;
        out[c] = (co * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    out[3] = (a_o * 255.0).round().clamp(0.0, 255.0) as u8;
    out
}

/// Composites `src` onto `dst` with its top-left corner at `at` (dst-local pixels),
/// clipping to `dst`. `opacity` multiplies the source alpha.
pub fn composite_over(
    dst: &mut Frame,
    src: &Frame,
    at: Point,
    opacity: f32,
    mode: BlendMode,
) -> Result<()> {
    check(dst)?;
    check(src)?;
    let opacity = finite(opacity, "opacity")?;
    let src_rect = Rect::new(at.x, at.y, src.width(), src.height());
    let Some(r) = src_rect.intersect(Rect::new(0, 0, dst.width(), dst.height())) else {
        return Ok(());
    };
    let stride = dst.stride();
    let rw = r.width as usize * 4;
    dst.data_mut()
        .par_chunks_mut(stride)
        .enumerate()
        .skip(r.y as usize)
        .take(r.height as usize)
        .for_each(|(y, row)| {
            let sy = u32::try_from(i64::from(i32c(y)) - i64::from(at.y)).unwrap_or(0);
            let sx = (i64::from(r.x) - i64::from(at.x)) as usize * 4;
            let srow = &src.row(sy)[sx..sx + rw];
            for (d, s) in row[r.x as usize * 4..r.x as usize * 4 + rw]
                .chunks_exact_mut(4)
                .zip(srow.chunks_exact(4))
            {
                let o =
                    blend_pixel([d[0], d[1], d[2], d[3]], [s[0], s[1], s[2], s[3]], opacity, mode);
                d.copy_from_slice(&o);
            }
        });
    Ok(())
}

/// Fills `rect` (clipped) with `colour` using `mode`.
pub fn fill_rect(frame: &mut Frame, rect: Rect, colour: Rgba, mode: BlendMode) -> Result<()> {
    check(frame)?;
    let Some(r) = rect.intersect(Rect::new(0, 0, frame.width(), frame.height())) else {
        return Ok(());
    };
    crate::region::for_rows_mut(frame, r, |_, row| {
        for d in row.chunks_exact_mut(4) {
            let o = blend_pixel([d[0], d[1], d[2], d[3]], colour, 1.0, mode);
            d.copy_from_slice(&o);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        solid_frame,
        testutil::{noise, px},
    };

    #[test]
    fn normal_over_opaque() {
        assert_eq!(
            blend_pixel([255, 255, 255, 255], [255, 0, 0, 128], 1.0, BlendMode::Normal),
            [255, 127, 127, 255]
        );
        assert_eq!(
            blend_pixel([9, 9, 9, 255], [1, 2, 3, 255], 1.0, BlendMode::Normal),
            [1, 2, 3, 255]
        );
        assert_eq!(
            blend_pixel([9, 9, 9, 255], [1, 2, 3, 0], 1.0, BlendMode::Normal),
            [9, 9, 9, 255]
        );
        assert_eq!(
            blend_pixel([9, 9, 9, 255], [1, 2, 3, 255], 0.0, BlendMode::Normal),
            [9, 9, 9, 255]
        );
    }

    #[test]
    fn over_transparent_dst_takes_source() {
        assert_eq!(
            blend_pixel([0, 0, 0, 0], [10, 20, 30, 100], 1.0, BlendMode::Normal),
            [10, 20, 30, 100]
        );
        assert_eq!(
            blend_pixel([0, 0, 0, 0], [10, 20, 30, 100], 1.0, BlendMode::Multiply),
            [10, 20, 30, 100]
        );
    }

    #[test]
    fn multiply_and_screen_known_values() {
        // Yellow marker over white text background stays yellow; over black text stays black.
        let marker = [255, 255, 0, 255];
        assert_eq!(
            blend_pixel([255, 255, 255, 255], marker, 1.0, BlendMode::Multiply),
            [255, 255, 0, 255]
        );
        assert_eq!(blend_pixel([0, 0, 0, 255], marker, 1.0, BlendMode::Multiply), [0, 0, 0, 255]);
        assert_eq!(
            blend_pixel([128, 128, 128, 255], [128, 128, 128, 255], 1.0, BlendMode::Multiply),
            [64, 64, 64, 255]
        );
        assert_eq!(
            blend_pixel([128, 128, 128, 255], [128, 128, 128, 255], 1.0, BlendMode::Screen),
            [192, 192, 192, 255]
        );
        assert_eq!(
            blend_pixel([50, 60, 70, 255], [0, 0, 0, 255], 1.0, BlendMode::Screen),
            [50, 60, 70, 255]
        );
        assert_eq!(
            blend_pixel([50, 200, 70, 255], [100, 100, 100, 255], 1.0, BlendMode::Darken),
            [50, 100, 70, 255]
        );
        assert_eq!(
            blend_pixel([50, 200, 70, 255], [100, 100, 100, 255], 1.0, BlendMode::Lighten),
            [100, 200, 100, 255]
        );
    }

    #[test]
    fn multiply_is_commutative_on_opaque() {
        let a = noise(8, 8, false, 1);
        let b = noise(8, 8, false, 2);
        let mut x = a.clone();
        composite_over(&mut x, &b, Point::new(0, 0), 1.0, BlendMode::Multiply).unwrap();
        let mut y = b.clone();
        composite_over(&mut y, &a, Point::new(0, 0), 1.0, BlendMode::Multiply).unwrap();
        assert_eq!(x, y);
    }

    #[test]
    fn composite_clips_all_sides() {
        let src = solid_frame(4, 4, [255, 0, 0, 255]);
        for at in [
            Point::new(-2, -2),
            Point::new(3, 3),
            Point::new(-10, 0),
            Point::new(0, 100),
            Point::new(i32::MAX, i32::MIN),
        ] {
            let mut d = solid_frame(5, 5, [0, 0, 255, 255]);
            composite_over(&mut d, &src, at, 1.0, BlendMode::Normal).unwrap();
        }
        let mut d = solid_frame(5, 5, [0, 0, 255, 255]);
        composite_over(&mut d, &src, Point::new(-2, -2), 1.0, BlendMode::Normal).unwrap();
        assert_eq!(px(&d, 1, 1), [255, 0, 0, 255]);
        assert_eq!(px(&d, 2, 2), [0, 0, 255, 255]);
        let mut e = solid_frame(0, 0, [0; 4]);
        composite_over(&mut e, &src, Point::new(0, 0), 1.0, BlendMode::Normal).unwrap();
        let mut d2 = solid_frame(3, 3, [1, 1, 1, 255]);
        composite_over(
            &mut d2,
            &solid_frame(0, 3, [0; 4]),
            Point::new(0, 0),
            1.0,
            BlendMode::Normal,
        )
        .unwrap();
    }

    #[test]
    fn opacity_scales_alpha() {
        let mut d = solid_frame(1, 1, [0, 0, 0, 255]);
        composite_over(
            &mut d,
            &solid_frame(1, 1, [255, 255, 255, 255]),
            Point::new(0, 0),
            0.5,
            BlendMode::Normal,
        )
        .unwrap();
        assert_eq!(px(&d, 0, 0), [128, 128, 128, 255]);
    }

    #[test]
    fn fill_rect_partial() {
        let mut d = solid_frame(4, 4, [0, 0, 0, 255]);
        fill_rect(&mut d, Rect::new(-1, 2, 3, 10), [9, 8, 7, 255], BlendMode::Normal).unwrap();
        assert_eq!(px(&d, 1, 3), [9, 8, 7, 255]);
        assert_eq!(px(&d, 2, 3), [0, 0, 0, 255]);
        assert_eq!(px(&d, 0, 1), [0, 0, 0, 255]);
    }
}
