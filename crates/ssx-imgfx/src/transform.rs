//! Geometric transforms: crop, pad, flip, rotate, auto-crop.
//!
//! Right-angle rotations and flips are exact pixel permutations (so four 90° turns are the
//! identity byte for byte). Arbitrary-angle rotation resamples bilinearly in premultiplied
//! space; pixels that map outside the source are transparent, which anti-aliases the
//! rotated border for free.

use rayon::prelude::*;
use ssx_types::{Frame, Point, Rect};

use crate::i32c;

use crate::{
    BlendMode, Placed, Result, Rgba, blend_pixel, check, finite, resize::tight_copy, solid_frame,
};

/// Crops to `rect` (frame-local). The rectangle is clipped to the frame, so the result may
/// be smaller than requested — or empty (0×0) when there is no overlap.
pub fn crop(frame: &Frame, rect: Rect) -> Result<Frame> {
    check(frame)?;
    let Some(r) = rect.intersect(Rect::new(0, 0, frame.width(), frame.height())) else {
        return Ok(solid_frame(0, 0, [0; 4]));
    };
    let rw = r.width as usize * 4;
    let mut out = vec![0u8; rw * r.height as usize];
    out.par_chunks_mut(rw).enumerate().for_each(|(y, dst)| {
        let src = frame.row(r.y as u32 + y as u32);
        dst.copy_from_slice(&src[r.x as usize * 4..r.x as usize * 4 + rw]);
    });
    Ok(Frame::from_rgba8(r.width, r.height, out).expect("exact-size buffer"))
}

/// Adds `left/top/right/bottom` pixels of `fill` around the image.
pub fn pad(
    frame: &Frame,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    fill: Rgba,
) -> Result<Placed> {
    check(frame)?;
    let w = frame.width() + left + right;
    let h = frame.height() + top + bottom;
    let mut out = solid_frame(w, h, fill);
    let rw = frame.width() as usize * 4;
    if rw > 0 {
        let stride = out.stride();
        out.data_mut()
            .par_chunks_mut(stride)
            .enumerate()
            .skip(top as usize)
            .take(frame.height() as usize)
            .for_each(|(y, row)| {
                let src = frame.row((y - top as usize) as u32);
                row[left as usize * 4..left as usize * 4 + rw].copy_from_slice(src);
            });
    }
    Ok(Placed { frame: out, origin: Point::new(i32c(left), i32c(top)) })
}

/// Mirrors left↔right.
pub fn flip_horizontal(frame: &Frame) -> Result<Frame> {
    check(frame)?;
    let (w, h) = (frame.width(), frame.height());
    let mut out = vec![0u8; w as usize * h as usize * 4];
    if w > 0 {
        out.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, dst)| {
            let src = frame.row(y as u32);
            for (x, d) in dst.chunks_exact_mut(4).enumerate() {
                let sx = w as usize - 1 - x;
                d.copy_from_slice(&src[sx * 4..sx * 4 + 4]);
            }
        });
    }
    Ok(Frame::from_rgba8(w, h, out).expect("exact-size buffer"))
}

/// Mirrors top↔bottom.
pub fn flip_vertical(frame: &Frame) -> Result<Frame> {
    check(frame)?;
    let (w, h) = (frame.width(), frame.height());
    let mut out = vec![0u8; w as usize * h as usize * 4];
    if w > 0 {
        out.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, dst)| {
            dst.copy_from_slice(frame.row(h - 1 - y as u32));
        });
    }
    Ok(Frame::from_rgba8(w, h, out).expect("exact-size buffer"))
}

/// Rotates 90° clockwise.
pub fn rotate_90(frame: &Frame) -> Result<Frame> {
    check(frame)?;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let mut out = vec![0u8; w * h * 4];
    if w > 0 && h > 0 {
        // Output is h wide, w tall: out(x', y') = src(y', h-1-x').
        out.par_chunks_mut(h * 4).enumerate().for_each(|(yo, dst)| {
            for (xo, d) in dst.chunks_exact_mut(4).enumerate() {
                let src = frame.row((h - 1 - xo) as u32);
                d.copy_from_slice(&src[yo * 4..yo * 4 + 4]);
            }
        });
    }
    Ok(Frame::from_rgba8(h as u32, w as u32, out).expect("exact-size buffer"))
}

/// Rotates 180°.
pub fn rotate_180(frame: &Frame) -> Result<Frame> {
    check(frame)?;
    let (w, h) = (frame.width(), frame.height());
    let mut out = vec![0u8; w as usize * h as usize * 4];
    if w > 0 {
        out.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, dst)| {
            let src = frame.row(h - 1 - y as u32);
            for (x, d) in dst.chunks_exact_mut(4).enumerate() {
                let sx = w as usize - 1 - x;
                d.copy_from_slice(&src[sx * 4..sx * 4 + 4]);
            }
        });
    }
    Ok(Frame::from_rgba8(w, h, out).expect("exact-size buffer"))
}

/// Rotates 270° clockwise (= 90° counter-clockwise).
pub fn rotate_270(frame: &Frame) -> Result<Frame> {
    check(frame)?;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let mut out = vec![0u8; w * h * 4];
    if w > 0 && h > 0 {
        // Output is h wide, w tall: out(x', y') = src(w-1-y', x').
        out.par_chunks_mut(h * 4).enumerate().for_each(|(yo, dst)| {
            for (xo, d) in dst.chunks_exact_mut(4).enumerate() {
                let src = frame.row(xo as u32);
                let sx = w - 1 - yo;
                d.copy_from_slice(&src[sx * 4..sx * 4 + 4]);
            }
        });
    }
    Ok(Frame::from_rgba8(h as u32, w as u32, out).expect("exact-size buffer"))
}

/// Rotates by `degrees` clockwise about the image centre.
///
/// With `expand` the canvas grows to contain the whole rotated image, otherwise the
/// original size is kept and corners are clipped. Uncovered area is `fill`.
/// Multiples of 90° with `expand` are exact permutations.
pub fn rotate(frame: &Frame, degrees: f32, fill: Rgba, expand: bool) -> Result<Frame> {
    check(frame)?;
    let degrees = finite(degrees, "degrees")?;
    let norm = f64::from(degrees).rem_euclid(360.0);
    if expand {
        for (turn, f) in [
            (90.0, rotate_90 as fn(&Frame) -> Result<Frame>),
            (180.0, rotate_180),
            (270.0, rotate_270),
        ] {
            if (norm - turn).abs() < 1e-9 {
                return f(frame);
            }
        }
        if norm.abs() < 1e-9 || (norm - 360.0).abs() < 1e-9 {
            return Ok(tight_copy(frame));
        }
    }
    let (sw, sh) = (f64::from(frame.width()), f64::from(frame.height()));
    if sw == 0.0 || sh == 0.0 {
        return Ok(solid_frame(0, 0, [0; 4]));
    }
    let (sin, cos) = norm.to_radians().sin_cos();
    let (dw, dh) = if expand {
        // Snap away float noise so 45° of a square does not grow by a stray pixel.
        let ew = (sw * cos.abs() + sh * sin.abs() - 1e-6).ceil().max(1.0);
        let eh = (sw * sin.abs() + sh * cos.abs() - 1e-6).ceil().max(1.0);
        (ew as usize, eh as usize)
    } else {
        (sw as usize, sh as usize)
    };
    let (cxs, cys) = (sw / 2.0, sh / 2.0);
    let (cxd, cyd) = (dw as f64 / 2.0, dh as f64 / 2.0);
    let mut out = vec![0u8; dw * dh * 4];
    out.par_chunks_mut(dw * 4).enumerate().for_each(|(y, row)| {
        for (x, o) in row.chunks_exact_mut(4).enumerate() {
            let dx = x as f64 + 0.5 - cxd;
            let dy = y as f64 + 0.5 - cyd;
            let sx = cxs + dx * cos + dy * sin - 0.5;
            let sy = cys - dx * sin + dy * cos - 0.5;
            let p = sample_premul(frame, sx, sy);
            let a = p[3];
            let px: Rgba = if a > 0.0 {
                let k = 255.0 / a;
                [
                    (p[0] * k).round().clamp(0.0, 255.0) as u8,
                    (p[1] * k).round().clamp(0.0, 255.0) as u8,
                    (p[2] * k).round().clamp(0.0, 255.0) as u8,
                    a.round().clamp(0.0, 255.0) as u8,
                ]
            } else {
                [0; 4]
            };
            let px = if fill[3] > 0 { blend_pixel(fill, px, 1.0, BlendMode::Normal) } else { px };
            o.copy_from_slice(&px);
        }
    });
    Ok(Frame::from_rgba8(dw as u32, dh as u32, out).expect("exact-size buffer"))
}

/// Bilinear sample of the pixel grid at (`x`, `y`) where integer coordinates are pixel
/// *centres*; taps outside the image are transparent. Returns premultiplied f32 RGBA.
fn sample_premul(frame: &Frame, x: f64, y: f64) -> [f32; 4] {
    let x0 = x.floor();
    let y0 = y.floor();
    let fx = (x - x0) as f32;
    let fy = (y - y0) as f32;
    let (xi, yi) = (x0 as i64, y0 as i64);
    let mut acc = [0f32; 4];
    for (dy, wy) in [(0i64, 1.0 - fy), (1, fy)] {
        for (dx, wx) in [(0i64, 1.0 - fx), (1, fx)] {
            let (px, py) = (xi + dx, yi + dy);
            if px < 0 || py < 0 || px >= i64::from(frame.width()) || py >= i64::from(frame.height())
            {
                continue;
            }
            let row = frame.row(py as u32);
            let i = px as usize * 4;
            let a = f32::from(row[i + 3]);
            let k = a / 255.0;
            let w = wx * wy;
            acc[0] += w * f32::from(row[i]) * k;
            acc[1] += w * f32::from(row[i + 1]) * k;
            acc[2] += w * f32::from(row[i + 2]) * k;
            acc[3] += w * a;
        }
    }
    acc
}

/// Finds the bounding box of content that differs from the border colour (taken from the
/// top-left pixel) by more than `tolerance` in any channel. `None` if the image is empty
/// or uniformly the background colour.
pub fn auto_crop_bounds(frame: &Frame, tolerance: u8) -> Result<Option<Rect>> {
    check(frame)?;
    if frame.width() == 0 || frame.height() == 0 {
        return Ok(None);
    }
    let bg = {
        let r = frame.row(0);
        [r[0], r[1], r[2], r[3]]
    };
    let tol = i32::from(tolerance);
    let differs = |p: &[u8]| (0..4).any(|c| (i32::from(p[c]) - i32::from(bg[c])).abs() > tol);
    let spans: Vec<Option<(usize, usize)>> = (0..frame.height())
        .into_par_iter()
        .map(|y| {
            let row = frame.row(y);
            let first = row.chunks_exact(4).position(differs)?;
            let last = row.chunks_exact(4).rposition(differs)?;
            Some((first, last))
        })
        .collect();
    let mut top = None;
    let mut bottom = 0;
    let (mut left, mut right) = (usize::MAX, 0usize);
    for (y, s) in spans.iter().enumerate() {
        if let Some((a, b)) = s {
            top.get_or_insert(y);
            bottom = y;
            left = left.min(*a);
            right = right.max(*b);
        }
    }
    Ok(top.map(|t| {
        Rect::new(i32c(left), i32c(t), (right - left + 1) as u32, (bottom - t + 1) as u32)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{noise, px};

    #[test]
    fn four_quarter_turns_are_identity() {
        for (w, h) in [(7, 3), (1, 1), (1, 9), (8, 8), (2, 5)] {
            let f = noise(w, h, true, w + h);
            let mut g = f.clone();
            for _ in 0..4 {
                g = rotate_90(&g).unwrap();
            }
            assert_eq!(g, f, "{w}x{h}");
            let mut g = f.clone();
            for _ in 0..4 {
                g = rotate_270(&g).unwrap();
            }
            assert_eq!(g, f);
            assert_eq!(rotate_180(&rotate_180(&f).unwrap()).unwrap(), f);
            assert_eq!(rotate_90(&rotate_270(&f).unwrap()).unwrap(), f);
            assert_eq!(rotate_90(&rotate_90(&f).unwrap()).unwrap(), rotate_180(&f).unwrap());
        }
    }

    #[test]
    fn rotate_90_exact_pixels() {
        // 3x2:  a b c / d e f  ->  rotated CW: d a / e b / f c
        let f = Frame::from_rgba8(
            3,
            2,
            [
                [1u8, 0, 0, 255],
                [2, 0, 0, 255],
                [3, 0, 0, 255],
                [4, 0, 0, 255],
                [5, 0, 0, 255],
                [6, 0, 0, 255],
            ]
            .concat(),
        )
        .unwrap();
        let r = rotate_90(&f).unwrap();
        assert_eq!((r.width(), r.height()), (2, 3));
        let vals: Vec<u8> = (0..3)
            .flat_map(|y| (0..2).map(move |x| (x, y)))
            .map(|(x, y)| px(&r, x, y)[0])
            .collect();
        assert_eq!(vals, vec![4, 1, 5, 2, 6, 3]);
        let l = rotate_270(&f).unwrap();
        let vals: Vec<u8> = (0..3)
            .flat_map(|y| (0..2).map(move |x| (x, y)))
            .map(|(x, y)| px(&l, x, y)[0])
            .collect();
        assert_eq!(vals, vec![3, 6, 2, 5, 1, 4]);
    }

    #[test]
    fn flips_are_involutions() {
        let f = noise(6, 5, true, 2);
        assert_eq!(flip_horizontal(&flip_horizontal(&f).unwrap()).unwrap(), f);
        assert_eq!(flip_vertical(&flip_vertical(&f).unwrap()).unwrap(), f);
        assert_eq!(flip_horizontal(&flip_vertical(&f).unwrap()).unwrap(), rotate_180(&f).unwrap());
        assert_eq!(px(&flip_horizontal(&f).unwrap(), 0, 0), px(&f, 5, 0));
    }

    #[test]
    fn crop_clips() {
        let f = noise(10, 10, false, 1);
        let c = crop(&f, Rect::new(-5, 2, 8, 3)).unwrap();
        assert_eq!((c.width(), c.height()), (3, 3));
        assert_eq!(px(&c, 0, 0), px(&f, 0, 2));
        let none = crop(&f, Rect::new(20, 20, 3, 3)).unwrap();
        assert_eq!((none.width(), none.height()), (0, 0));
        let z = crop(&solid_frame(0, 0, [0; 4]), Rect::new(0, 0, 4, 4)).unwrap();
        assert_eq!(z.width(), 0);
        let one = crop(&noise(1, 1, false, 1), Rect::new(0, 0, 1, 1)).unwrap();
        assert_eq!(one.width(), 1);
    }

    #[test]
    fn pad_places_content() {
        let f = noise(3, 2, false, 1);
        let p = pad(&f, 2, 1, 0, 4, [1, 2, 3, 4]).unwrap();
        assert_eq!((p.frame.width(), p.frame.height()), (5, 7));
        assert_eq!(p.origin, Point::new(2, 1));
        assert_eq!(px(&p.frame, 0, 0), [1, 2, 3, 4]);
        assert_eq!(px(&p.frame, 2, 1), px(&f, 0, 0));
        assert_eq!(px(&p.frame, 4, 2), px(&f, 2, 1));
        assert_eq!(px(&p.frame, 4, 6), [1, 2, 3, 4]);
        let e = pad(&solid_frame(0, 0, [0; 4]), 2, 2, 2, 2, [9; 4]).unwrap();
        assert_eq!(e.frame, solid_frame(4, 4, [9; 4]));
    }

    #[test]
    fn arbitrary_rotation_identity_and_size() {
        let f = noise(9, 6, false, 5);
        let same = rotate(&f, 0.0, [0; 4], false).unwrap();
        // Sampling exactly at pixel centres reproduces the image.
        assert_eq!(same, f);
        assert_eq!(rotate(&f, 90.0, [0; 4], true).unwrap(), rotate_90(&f).unwrap());
        assert_eq!(rotate(&f, -90.0, [0; 4], true).unwrap(), rotate_270(&f).unwrap());
        let r45 = rotate(&solid_frame(10, 10, [255, 0, 0, 255]), 45.0, [0; 4], true).unwrap();
        assert!(r45.width() >= 14 && r45.width() <= 15, "{}", r45.width());
        // Corner is uncovered (transparent), centre is red.
        assert_eq!(px(&r45, 0, 0)[3], 0);
        assert_eq!(px(&r45, r45.width() / 2, r45.height() / 2), [255, 0, 0, 255]);
        // Fill shows up in the uncovered corners.
        let filled =
            rotate(&solid_frame(10, 10, [255, 0, 0, 255]), 45.0, [0, 0, 255, 255], true).unwrap();
        assert_eq!(px(&filled, 0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn arbitrary_rotation_degenerate() {
        assert_eq!(rotate(&solid_frame(0, 4, [0; 4]), 33.0, [0; 4], true).unwrap().width(), 0);
        let one = noise(1, 1, true, 1);
        let r = rotate(&one, 30.0, [0; 4], true).unwrap();
        assert!(r.width() >= 1);
        assert!(rotate(&one, f32::NAN, [0; 4], true).is_err());
        let _ = rotate(&one, 1e9, [0; 4], false).unwrap();
    }

    #[test]
    fn auto_crop_finds_content() {
        let mut f = solid_frame(20, 15, [255, 255, 255, 255]);
        for y in 4..9usize {
            for x in 6..13usize {
                f.data_mut()[(y * 20 + x) * 4..(y * 20 + x) * 4 + 4]
                    .copy_from_slice(&[10, 10, 10, 255]);
            }
        }
        assert_eq!(auto_crop_bounds(&f, 0).unwrap(), Some(Rect::new(6, 4, 7, 5)));
        assert_eq!(auto_crop_bounds(&solid_frame(5, 5, [1, 2, 3, 4]), 0).unwrap(), None);
        assert_eq!(auto_crop_bounds(&solid_frame(0, 0, [0; 4]), 0).unwrap(), None);
        // Tolerance ignores slight noise.
        let mut g = solid_frame(6, 6, [100, 100, 100, 255]);
        g.data_mut()[(2 * 6 + 2) * 4] = 103;
        assert_eq!(auto_crop_bounds(&g, 5).unwrap(), None);
        assert!(auto_crop_bounds(&g, 2).unwrap().is_some());
    }
}
