//! Mosaic / pixelate.
//!
//! Blocks are aligned to the *region's* top-left corner (not the image's), so the
//! mosaic looks the same wherever the user drags the box; blocks at the right and bottom
//! edge are simply smaller. Each block becomes the alpha-weighted mean of its pixels,
//! computed with exact integer sums, which makes the operation exactly idempotent.

use rayon::prelude::*;
use ssx_types::{Frame, Rect};

use crate::{
    Result, check,
    region::{clip_region, extract, write_back},
};

/// Pixelates `region` (frame-local; `None` = whole frame) with square blocks of `block`
/// pixels (`0` is treated as `1`, i.e. no change).
pub fn pixelate(frame: &mut Frame, region: Option<Rect>, block: u32) -> Result<()> {
    check(frame)?;
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    // If the caller's region started off-image, keep block alignment relative to *its*
    // origin so the mosaic does not shift when the box is dragged over an edge.
    let (dx, dy) = match region {
        Some(orig) => ((r.x - orig.x).max(0) as usize, (r.y - orig.y).max(0) as usize),
        None => (0, 0),
    };
    let block = block.max(1) as usize;
    if block == 1 {
        return Ok(());
    }
    let (w, h) = (r.width as usize, r.height as usize);
    let mut buf = extract(frame, r);
    // Block boundaries in region-local coordinates, offset by the clipped-away part.
    let first_w = block - dx % block;
    let first_h = block - dy % block;
    let edges = |len: usize, first: usize| -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut s = 0;
        let mut e = first.min(len);
        while s < len {
            v.push((s, e));
            s = e;
            e = (s + block).min(len);
        }
        v
    };
    let xs = edges(w, first_w);
    let ys = edges(h, first_h);
    let rw = w * 4;
    // Parallel over block rows: each band of rows is an independent mutable chunk.
    let mut bands: Vec<&mut [u8]> = Vec::with_capacity(ys.len());
    let mut rest: &mut [u8] = &mut buf;
    for &(s, e) in &ys {
        let (band, tail) = rest.split_at_mut((e - s) * rw);
        bands.push(band);
        rest = tail;
    }
    bands.into_par_iter().zip(ys.par_iter()).for_each(|(band, &(s, e))| {
        let rows = e - s;
        for &(x0, x1) in &xs {
            let (mut sr, mut sg, mut sb, mut sa) = (0u64, 0u64, 0u64, 0u64);
            for y in 0..rows {
                for px in band[y * rw + x0 * 4..y * rw + x1 * 4].chunks_exact(4) {
                    let a = u64::from(px[3]);
                    sr += u64::from(px[0]) * a;
                    sg += u64::from(px[1]) * a;
                    sb += u64::from(px[2]) * a;
                    sa += a;
                }
            }
            let n = (rows * (x1 - x0)) as u64;
            let out = if sa == 0 {
                // Fully transparent block: colour is irrelevant; keep the plain mean so
                // the operation stays idempotent.
                let mut c = [0u64; 3];
                for y in 0..rows {
                    for px in band[y * rw + x0 * 4..y * rw + x1 * 4].chunks_exact(4) {
                        for (ci, cv) in c.iter_mut().enumerate() {
                            *cv += u64::from(px[ci]);
                        }
                    }
                }
                [
                    ((c[0] + n / 2) / n) as u8,
                    ((c[1] + n / 2) / n) as u8,
                    ((c[2] + n / 2) / n) as u8,
                    0,
                ]
            } else {
                [
                    ((sr + sa / 2) / sa) as u8,
                    ((sg + sa / 2) / sa) as u8,
                    ((sb + sa / 2) / sa) as u8,
                    ((sa + n / 2) / n) as u8,
                ]
            };
            for y in 0..rows {
                for px in band[y * rw + x0 * 4..y * rw + x1 * 4].chunks_exact_mut(4) {
                    px.copy_from_slice(&out);
                }
            }
        }
    });
    write_back(frame, r, &buf);
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
    fn exact_block_averages() {
        // 4x2 image, block 2 => two blocks.
        let data = vec![
            0, 0, 0, 255, 100, 100, 100, 255, 10, 20, 30, 255, 30, 20, 10, 255, //
            200, 200, 200, 255, 100, 100, 100, 255, 10, 20, 30, 255, 30, 20, 10, 255,
        ];
        let mut f = Frame::from_rgba8(4, 2, data).unwrap();
        pixelate(&mut f, None, 2).unwrap();
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(px(&f, x, y), [100, 100, 100, 255]);
            }
            for x in 2..4 {
                assert_eq!(px(&f, x, y), [20, 20, 20, 255]);
            }
        }
    }

    #[test]
    fn partial_edge_blocks_and_region_alignment() {
        let base = noise(10, 10, false, 5);
        let mut f = base.clone();
        pixelate(&mut f, Some(Rect::new(3, 3, 5, 5)), 4).unwrap();
        // Blocks are aligned to (3,3): columns 3..7 and 7..8.
        for y in 3..7 {
            assert_eq!(px(&f, 3, y), px(&f, 6, 3));
        }
        assert_eq!(px(&f, 7, 3), px(&f, 7, 6));
        assert_ne!(px(&f, 7, 3), px(&f, 6, 3), "different blocks are (almost surely) different");
        // Outside untouched.
        assert_eq!(px(&f, 2, 3), px(&base, 2, 3));
        assert_eq!(px(&f, 8, 8), px(&base, 8, 8));
    }

    #[test]
    fn idempotent_including_alpha() {
        for alpha in [false, true] {
            let mut f = noise(37, 23, alpha, 9);
            pixelate(&mut f, None, 6).unwrap();
            let once = f.clone();
            pixelate(&mut f, None, 6).unwrap();
            assert_eq!(f, once, "alpha={alpha}");
        }
    }

    #[test]
    fn degenerate_inputs() {
        let mut f = solid_frame(3, 3, [9, 9, 9, 255]);
        pixelate(&mut f, None, 0).unwrap();
        pixelate(&mut f, None, 1000).unwrap();
        assert_eq!(f, solid_frame(3, 3, [9, 9, 9, 255]));
        let mut one = solid_frame(1, 1, [1, 2, 3, 4]);
        pixelate(&mut one, None, 5).unwrap();
        assert_eq!(px(&one, 0, 0), [1, 2, 3, 4]);
        let mut e = solid_frame(0, 0, [0; 4]);
        pixelate(&mut e, None, 5).unwrap();
        pixelate(&mut f, Some(Rect::new(50, 50, 4, 4)), 2).unwrap();
        pixelate(&mut f, Some(Rect::new(-50, -50, 52, 52)), 2).unwrap();
    }

    #[test]
    fn off_image_region_keeps_block_alignment() {
        // Region origin at -1: blocks start at x=-1, so first visible block is 1 px wide
        // (x = 0), then 2..4 etc. for block 3? use block 3: -1..2, 2..5.
        let base = noise(8, 1, false, 2);
        let mut f = base.clone();
        pixelate(&mut f, Some(Rect::new(-1, 0, 9, 1)), 3).unwrap();
        assert_eq!(px(&f, 0, 0), px(&f, 1, 0));
        assert_eq!(px(&f, 2, 0), px(&f, 4, 0));
    }
}
