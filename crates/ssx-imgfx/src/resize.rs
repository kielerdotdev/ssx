//! High-quality resampling.
//!
//! Separable two-pass resize with per-output-pixel weight tables (the classic PIL /
//! `ImageMagick` formulation): when shrinking, the filter support is stretched by the scale
//! factor, so Lanczos3 and bilinear both low-pass properly instead of aliasing.
//! Filtering is done on **premultiplied** colour so transparent edges never darken or
//! fringe. Rows of the output are produced in bands: the horizontal pass is only run on
//! the source rows a band needs, which bounds the scratch memory (important for 8K
//! sources) and keeps it in cache.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use ssx_types::Frame;

use crate::{Result, check, solid_frame};

/// Resampling kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeFilter {
    /// Point sampling; keeps hard pixel edges (zooming a screenshot for inspection).
    Nearest,
    /// Triangle filter; fast and smooth, mild blur.
    Bilinear,
    /// Windowed sinc (a = 3); sharpest, tiny ringing. Best for photos and screenshots.
    #[default]
    Lanczos3,
}

impl ResizeFilter {
    fn support(self) -> f64 {
        match self {
            ResizeFilter::Nearest => 0.5,
            ResizeFilter::Bilinear => 1.0,
            ResizeFilter::Lanczos3 => 3.0,
        }
    }

    fn eval(self, x: f64) -> f64 {
        let x = x.abs();
        match self {
            ResizeFilter::Nearest => f64::from(x < 0.5),
            ResizeFilter::Bilinear => (1.0 - x).max(0.0),
            ResizeFilter::Lanczos3 => {
                if x >= 3.0 {
                    0.0
                } else if x < 1e-9 {
                    1.0
                } else {
                    let px = std::f64::consts::PI * x;
                    3.0 * px.sin() * (px / 3.0).sin() / (px * px)
                }
            }
        }
    }
}

struct Weights {
    taps: usize,
    start: Vec<usize>,
    count: Vec<usize>,
    w: Vec<f32>,
}

fn build_weights(src_len: usize, dst_len: usize, filter: ResizeFilter) -> Weights {
    let scale = src_len as f64 / dst_len as f64;
    let fscale = scale.max(1.0);
    let support = filter.support() * fscale;
    let taps = (support * 2.0).ceil() as usize + 2;
    let mut start = Vec::with_capacity(dst_len);
    let mut count = Vec::with_capacity(dst_len);
    let mut w = vec![0f32; dst_len * taps];
    for i in 0..dst_len {
        let center = (i as f64 + 0.5) * scale;
        let lo = ((center - support + 0.5).floor().max(0.0)) as usize;
        let hi = (((center + support + 0.5).floor()) as usize).min(src_len);
        let lo = lo.min(src_len.saturating_sub(1));
        let hi = hi.max(lo + 1).min(src_len);
        let mut sum = 0f64;
        let row = &mut w[i * taps..(i + 1) * taps];
        for (t, x) in (lo..hi).enumerate() {
            let v = filter.eval((x as f64 + 0.5 - center) / fscale);
            row[t] = v as f32;
            sum += v;
        }
        if sum.abs() < 1e-12 {
            // Degenerate (should not happen): fall back to the nearest tap.
            let near = ((center.floor() as usize).clamp(lo, hi - 1)) - lo;
            row[near] = 1.0;
            sum = 1.0;
        }
        for v in &mut row[..hi - lo] {
            *v = (f64::from(*v) / sum) as f32;
        }
        start.push(lo);
        count.push(hi - lo);
    }
    Weights { taps, start, count, w }
}

const BAND: usize = 32;

/// Resizes `frame` to `new_width`×`new_height`. A zero target yields an empty frame; an
/// empty source yields a transparent frame of the target size.
pub fn resize(
    frame: &Frame,
    new_width: u32,
    new_height: u32,
    filter: ResizeFilter,
) -> Result<Frame> {
    check(frame)?;
    if new_width == 0 || new_height == 0 {
        return Ok(solid_frame(new_width, new_height, [0; 4]));
    }
    if frame.width() == 0 || frame.height() == 0 {
        return Ok(solid_frame(new_width, new_height, [0; 4]));
    }
    if frame.width() == new_width && frame.height() == new_height {
        return Ok(tight_copy(frame));
    }
    let (sw, sh) = (frame.width() as usize, frame.height() as usize);
    let (dw, dh) = (new_width as usize, new_height as usize);
    let mut out = vec![0u8; dw * dh * 4];

    if filter == ResizeFilter::Nearest {
        let xs: Vec<usize> = (0..dw)
            .map(|i| (((i as f64 + 0.5) * sw as f64 / dw as f64) as usize).min(sw - 1))
            .collect();
        out.par_chunks_mut(dw * 4).enumerate().for_each(|(y, orow)| {
            let sy = (((y as f64 + 0.5) * sh as f64 / dh as f64) as usize).min(sh - 1);
            let srow = frame.row(sy as u32);
            for (o, &sx) in orow.chunks_exact_mut(4).zip(&xs) {
                o.copy_from_slice(&srow[sx * 4..sx * 4 + 4]);
            }
        });
        return Ok(Frame::from_rgba8(new_width, new_height, out).expect("exact-size buffer"));
    }

    let wx = build_weights(sw, dw, filter);
    let wy = build_weights(sh, dh, filter);
    out.par_chunks_mut(dw * 4 * BAND).enumerate().for_each(|(bi, band)| {
        let j0 = bi * BAND;
        let rows = band.len() / (dw * 4);
        let s_lo = (j0..j0 + rows).map(|j| wy.start[j]).min().unwrap_or(0);
        let s_hi = (j0..j0 + rows).map(|j| wy.start[j] + wy.count[j]).max().unwrap_or(0);
        // Horizontal pass on the needed source rows.
        let mut inter = vec![0f32; (s_hi - s_lo) * dw * 4];
        let mut pre = vec![0f32; sw * 4];
        for (ri, irow) in inter.chunks_exact_mut(dw * 4).enumerate() {
            let src = frame.row((s_lo + ri) as u32);
            for (p, s) in pre.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                let k = f32::from(s[3]) / 255.0;
                p[0] = f32::from(s[0]) * k;
                p[1] = f32::from(s[1]) * k;
                p[2] = f32::from(s[2]) * k;
                p[3] = f32::from(s[3]);
            }
            for (x, o) in irow.chunks_exact_mut(4).enumerate() {
                let ws = &wx.w[x * wx.taps..x * wx.taps + wx.count[x]];
                let px = &pre[wx.start[x] * 4..(wx.start[x] + wx.count[x]) * 4];
                let mut acc = [0f32; 4];
                for (wv, p) in ws.iter().zip(px.chunks_exact(4)) {
                    acc[0] += wv * p[0];
                    acc[1] += wv * p[1];
                    acc[2] += wv * p[2];
                    acc[3] += wv * p[3];
                }
                o.copy_from_slice(&acc);
            }
        }
        // Vertical pass.
        let mut acc = vec![0f32; dw * 4];
        for (r, orow) in band.chunks_exact_mut(dw * 4).enumerate() {
            let j = j0 + r;
            acc.fill(0.0);
            let ws = &wy.w[j * wy.taps..j * wy.taps + wy.count[j]];
            for (t, &wv) in ws.iter().enumerate() {
                let ir = wy.start[j] + t - s_lo;
                let irow = &inter[ir * dw * 4..(ir + 1) * dw * 4];
                for (a, v) in acc.iter_mut().zip(irow) {
                    *a += wv * v;
                }
            }
            for (o, a) in orow.chunks_exact_mut(4).zip(acc.chunks_exact(4)) {
                let alpha = a[3].round().clamp(0.0, 255.0);
                if alpha > 0.0 {
                    let k = 255.0 / a[3].max(f32::MIN_POSITIVE);
                    o[0] = (a[0] * k).round().clamp(0.0, 255.0) as u8;
                    o[1] = (a[1] * k).round().clamp(0.0, 255.0) as u8;
                    o[2] = (a[2] * k).round().clamp(0.0, 255.0) as u8;
                    o[3] = alpha as u8;
                }
            }
        }
    });
    Ok(Frame::from_rgba8(new_width, new_height, out).expect("exact-size buffer"))
}

/// A tightly packed copy (drops stride padding).
pub(crate) fn tight_copy(frame: &Frame) -> Frame {
    let mut data = Vec::with_capacity(frame.width() as usize * frame.height() as usize * 4);
    for y in 0..frame.height() {
        data.extend_from_slice(frame.row(y));
    }
    Frame::from_rgba8(frame.width(), frame.height(), data).expect("exact-size buffer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{noise, psnr, px};

    fn gradient(w: u32, h: u32) -> Frame {
        let mut d = Vec::new();
        for y in 0..h {
            for x in 0..w {
                d.extend_from_slice(&[
                    (x * 255 / w.max(1)) as u8,
                    (y * 255 / h.max(1)) as u8,
                    ((x + y) * 255 / (w + h).max(1)) as u8,
                    255,
                ]);
            }
        }
        Frame::from_rgba8(w, h, d).unwrap()
    }

    const ALL: [ResizeFilter; 3] =
        [ResizeFilter::Nearest, ResizeFilter::Bilinear, ResizeFilter::Lanczos3];

    #[test]
    fn identity_is_exact() {
        let f = noise(17, 9, true, 3);
        for filt in ALL {
            assert_eq!(resize(&f, 17, 9, filt).unwrap(), f);
        }
    }

    #[test]
    fn constant_stays_constant() {
        for filt in ALL {
            for (w, h) in [(3, 3), (40, 25), (1, 1), (7, 100)] {
                let f = solid_frame(11, 13, [12, 200, 99, 180]);
                let r = resize(&f, w, h, filt).unwrap();
                assert_eq!(r, solid_frame(w, h, [12, 200, 99, 180]), "{filt:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn nearest_exact_2x_up_and_down() {
        let f = Frame::from_rgba8(2, 1, vec![1, 1, 1, 255, 9, 9, 9, 255]).unwrap();
        let up = resize(&f, 4, 2, ResizeFilter::Nearest).unwrap();
        assert_eq!(px(&up, 0, 0), [1, 1, 1, 255]);
        assert_eq!(px(&up, 1, 1), [1, 1, 1, 255]);
        assert_eq!(px(&up, 2, 0), [9, 9, 9, 255]);
        assert_eq!(px(&up, 3, 1), [9, 9, 9, 255]);
        let down = resize(&up, 2, 1, ResizeFilter::Nearest).unwrap();
        assert_eq!(down, f);
    }

    #[test]
    fn bilinear_2x_down_is_box_average() {
        let f = Frame::from_rgba8(
            2,
            2,
            vec![0, 0, 0, 255, 100, 100, 100, 255, 200, 200, 200, 255, 100, 100, 100, 255],
        )
        .unwrap();
        let r = resize(&f, 1, 1, ResizeFilter::Bilinear).unwrap();
        assert_eq!(px(&r, 0, 0), [100, 100, 100, 255]);
    }

    #[test]
    fn round_trip_psnr_bounds() {
        let g = gradient(96, 64);
        for (filt, min_db) in [(ResizeFilter::Bilinear, 36.0), (ResizeFilter::Lanczos3, 40.0)] {
            let down = resize(&g, 48, 32, filt).unwrap();
            let up = resize(&down, 96, 64, filt).unwrap();
            let db = psnr(&g, &up);
            assert!(db > min_db, "{filt:?} down/up PSNR {db}");
            let up2 = resize(&g, 192, 128, filt).unwrap();
            let back = resize(&up2, 96, 64, filt).unwrap();
            let db = psnr(&g, &back);
            assert!(db > 45.0, "{filt:?} up/down PSNR {db}");
        }
    }

    #[test]
    fn alpha_is_premultiplied_correct() {
        // Opaque red left, transparent black right; shrink 2:1 horizontally.
        let f = Frame::from_rgba8(2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]).unwrap();
        let r = resize(&f, 1, 1, ResizeFilter::Bilinear).unwrap();
        let p = px(&r, 0, 0);
        assert_eq!(p, [255, 0, 0, 128], "colour must stay pure red, alpha halves");
    }

    #[test]
    fn extreme_shapes_do_not_panic() {
        let f = noise(5, 3, true, 1);
        for filt in ALL {
            let _ = resize(&f, 1, 1, filt).unwrap();
            let _ = resize(&f, 1000, 1, filt).unwrap();
            let _ = resize(&f, 1, 1000, filt).unwrap();
            assert_eq!(resize(&f, 0, 5, filt).unwrap().width(), 0);
            assert_eq!(resize(&f, 5, 0, filt).unwrap().height(), 0);
            let one = noise(1, 1, true, 1);
            assert_eq!(resize(&one, 9, 9, filt).unwrap().width(), 9);
            let empty = solid_frame(0, 0, [0; 4]);
            assert_eq!(resize(&empty, 4, 4, filt).unwrap().size().width, 4);
        }
    }

    #[test]
    fn lanczos_is_sharper_than_bilinear_on_edge() {
        let mut d = Vec::new();
        for _y in 0..8 {
            for x in 0..32 {
                let v = if x < 16 { 0 } else { 255 };
                d.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let f = Frame::from_rgba8(32, 8, d).unwrap();
        let l = resize(&f, 16, 4, ResizeFilter::Lanczos3).unwrap();
        let b = resize(&f, 16, 4, ResizeFilter::Bilinear).unwrap();
        // Pixel just before the edge: lanczos may under-shoot, bilinear stays >= 0;
        // pixel 7 (source 14-15) is fully dark for both.
        assert_eq!(px(&b, 6, 1)[0], 0);
        assert!(px(&l, 8, 1)[0] >= px(&b, 8, 1)[0]);
    }
}
