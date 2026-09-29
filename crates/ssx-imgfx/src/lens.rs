//! Magnifier lens.
//!
//! For every destination pixel inside the lens the source position is
//! `source_center + (pixel − lens_center) / zoom`, sampled bilinearly (premultiplied,
//! edge-clamped). Circular lenses get an anti-aliased rim. The lens reads from a *separate*
//! source frame — the editor passes a snapshot of what lies below the lens — so the
//! operation is well defined even when the lens overlaps its own source region.

use serde::{Deserialize, Serialize};
use ssx_types::{Frame, Rect};

use crate::{BlendMode, PointF, Result, blend_pixel, check, finite, i32c, region::for_rows_mut};

/// Outline of the lens inside its destination rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LensShape {
    /// Ellipse inscribed in the rectangle (a circle for a square rectangle).
    #[default]
    Ellipse,
    /// The rectangle itself.
    Rect,
}

/// A magnifier description.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Lens {
    /// Where the lens is drawn, in destination-frame pixels (clipped to the frame).
    pub dst: Rect,
    /// Lens outline.
    pub shape: LensShape,
    /// The point of the *source* frame shown at the lens centre (pixel-centre coordinates).
    pub source_center: PointF,
    /// Magnification factor (`> 0`; `2.0` shows half the area at double size).
    pub zoom: f32,
}

/// Bilinear premultiplied sample with clamped coordinates. `(x, y)` are in pixel-centre
/// space (integer = centre of that pixel).
fn sample_clamped(src: &Frame, x: f32, y: f32) -> [f32; 4] {
    let (w, h) = (src.width() as i64, src.height() as i64);
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as i64, y.floor() as i64);
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let mut acc = [0f32; 4];
    for (dy, wy) in [(0i64, 1.0 - fy), (1, fy)] {
        for (dx, wx) in [(0i64, 1.0 - fx), (1, fx)] {
            let wgt = wx * wy;
            if wgt == 0.0 {
                continue;
            }
            let (px, py) = ((x0 + dx).min(w - 1), (y0 + dy).min(h - 1));
            let row = src.row(py as u32);
            let i = px as usize * 4;
            let a = f32::from(row[i + 3]);
            let k = a / 255.0;
            acc[0] += wgt * f32::from(row[i]) * k;
            acc[1] += wgt * f32::from(row[i + 1]) * k;
            acc[2] += wgt * f32::from(row[i + 2]) * k;
            acc[3] += wgt * a;
        }
    }
    acc
}

/// Draws the magnified `src` content into `dst` through `lens`. The border/frame around
/// the lens is the caller's business.
pub fn magnify(src: &Frame, dst: &mut Frame, lens: &Lens) -> Result<()> {
    check(src)?;
    check(dst)?;
    let zoom = finite(lens.zoom, "zoom")?.max(0.01);
    let sx = finite(lens.source_center.x, "source_center.x")?;
    let sy = finite(lens.source_center.y, "source_center.y")?;
    if src.width() == 0 || src.height() == 0 || lens.dst.is_empty() {
        return Ok(());
    }
    let Some(r) = lens.dst.intersect(Rect::new(0, 0, dst.width(), dst.height())) else {
        return Ok(());
    };
    let cx = lens.dst.x as f32 + lens.dst.width as f32 / 2.0;
    let cy = lens.dst.y as f32 + lens.dst.height as f32 / 2.0;
    let rx = lens.dst.width as f32 / 2.0;
    let ry = lens.dst.height as f32 / 2.0;
    let rmin = rx.min(ry);
    for_rows_mut(dst, r, |y, row| {
        let py = y as f32 + 0.5 - cy;
        for (i, d) in row.chunks_exact_mut(4).enumerate() {
            let px = (i64::from(r.x) + i64::from(i32c(i))) as f32 + 0.5 - cx;
            let coverage = match lens.shape {
                LensShape::Rect => 1.0,
                LensShape::Ellipse => {
                    let e = ((px / rx).powi(2) + (py / ry).powi(2)).sqrt();
                    ((1.0 - e) * rmin + 0.5).clamp(0.0, 1.0)
                }
            };
            if coverage <= 0.0 {
                continue;
            }
            // Source pixel-centre convention: subtract 0.5 after mapping continuous coords.
            let s = sample_clamped(src, sx + px / zoom - 0.5, sy + py / zoom - 0.5);
            let a = s[3];
            let colour = if a > 0.0 {
                let k = 255.0 / a;
                [
                    (s[0] * k).round().clamp(0.0, 255.0) as u8,
                    (s[1] * k).round().clamp(0.0, 255.0) as u8,
                    (s[2] * k).round().clamp(0.0, 255.0) as u8,
                    a.round().clamp(0.0, 255.0) as u8,
                ]
            } else {
                [0; 4]
            };
            // A lens *replaces* what is below it (it is a window onto the source), so blend
            // over a fully transparent backdrop weighted by coverage only at the rim.
            let out = if coverage >= 1.0 {
                colour
            } else {
                blend_pixel([d[0], d[1], d[2], d[3]], colour, coverage, BlendMode::Normal)
            };
            d.copy_from_slice(&out);
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
    fn zoom_one_at_same_position_is_identity_in_rect() {
        let src = noise(20, 20, false, 3);
        let mut dst = solid_frame(20, 20, [0, 0, 0, 255]);
        let lens = Lens {
            dst: Rect::new(4, 4, 10, 10),
            shape: LensShape::Rect,
            source_center: PointF::new(9.0, 9.0),
            zoom: 1.0,
        };
        magnify(&src, &mut dst, &lens).unwrap();
        for y in 4..14 {
            for x in 4..14 {
                assert_eq!(px(&dst, x, y), px(&src, x, y), "({x},{y})");
            }
        }
        assert_eq!(px(&dst, 3, 3), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 14, 14), [0, 0, 0, 255]);
    }

    #[test]
    fn zoom_two_replicates_and_interpolates() {
        // Checker of 2x2 src with big zoom: centre shows the boundary blend.
        let src = Frame::from_rgba8(2, 1, vec![0, 0, 0, 255, 200, 200, 200, 255]).unwrap();
        let mut dst = solid_frame(8, 8, [9, 9, 9, 255]);
        let lens = Lens {
            dst: Rect::new(0, 0, 8, 8),
            shape: LensShape::Rect,
            source_center: PointF::new(1.0, 0.5),
            zoom: 4.0,
        };
        magnify(&src, &mut dst, &lens).unwrap();
        let left = px(&dst, 0, 4)[0];
        let right = px(&dst, 7, 4)[0];
        assert!(left < 60 && right > 140, "{left} {right}");
        assert!(px(&dst, 3, 4)[0] <= px(&dst, 4, 4)[0]);
    }

    #[test]
    fn circle_masks_corners_and_clips() {
        let src = solid_frame(30, 30, [255, 0, 0, 255]);
        let mut dst = solid_frame(30, 30, [0, 0, 255, 255]);
        let lens = Lens {
            dst: Rect::new(-5, -5, 20, 20),
            shape: LensShape::Ellipse,
            source_center: PointF::new(15.0, 15.0),
            zoom: 2.0,
        };
        magnify(&src, &mut dst, &lens).unwrap();
        assert_eq!(px(&dst, 5, 5), [255, 0, 0, 255], "centre of lens (5,5) shows source");
        assert_eq!(px(&dst, 0, 0), [255, 0, 0, 255], "(0,0) is inside the circle");
        assert_eq!(px(&dst, 14, 14), [0, 0, 255, 255], "(14,14) is outside the circle");
        assert_eq!(px(&dst, 20, 20), [0, 0, 255, 255]);
    }

    #[test]
    fn degenerate_inputs() {
        let src = noise(4, 4, true, 1);
        let mut dst = noise(4, 4, true, 2);
        let orig = dst.clone();
        for lens in [
            Lens {
                dst: Rect::new(0, 0, 0, 5),
                shape: LensShape::Rect,
                source_center: PointF::new(1.0, 1.0),
                zoom: 2.0,
            },
            Lens {
                dst: Rect::new(100, 100, 5, 5),
                shape: LensShape::Ellipse,
                source_center: PointF::new(1.0, 1.0),
                zoom: 2.0,
            },
        ] {
            magnify(&src, &mut dst, &lens).unwrap();
        }
        assert_eq!(dst, orig);
        let lens = Lens {
            dst: Rect::new(0, 0, 4, 4),
            shape: LensShape::Rect,
            source_center: PointF::new(-100.0, 1e6),
            zoom: 0.0,
        };
        magnify(&src, &mut dst, &lens).unwrap();
        let empty = solid_frame(0, 0, [0; 4]);
        magnify(&empty, &mut dst, &lens).unwrap();
        let bad = Lens { zoom: f32::NAN, ..lens };
        assert!(magnify(&src, &mut dst, &bad).is_err());
    }
}
