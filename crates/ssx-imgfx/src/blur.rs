//! Gaussian blur, sharpen and unsharp mask.
//!
//! Two implementations of the same operator:
//!
//! * **Exact**: a true separable Gaussian, kernel radius `ceil(3σ)` (99.7 % of the mass),
//!   normalised so a constant image stays constant. Cost is `O(radius)` per pixel.
//! * **Box3**: three successive *extended* box blurs per axis (integer window plus
//!   fractional end taps) whose summed variance equals σ² exactly (the classic
//!   Kutskir / W3C SVG idea, without its integer-width rounding error). `O(1)` per
//!   pixel. The equivalent kernel is a quadratic B-spline: on a step edge it deviates from
//!   the true Gaussian by at most ~1.05 % of full scale (≤ 3 grey levels, all σ), but an
//!   isolated point's peak comes out ≈ 6 % lower in 1-D (≈ 12 % in 2-D). Fine for
//!   obscuring and soft shadows; use [`BlurMethod::Exact`] when the exact profile matters.
//!   See `box_matches_exact_within_documented_bound`.
//!
//! [`BlurMethod::Auto`] uses Exact up to σ = 4 and Box3 above, where the exact kernel
//! becomes the dominating cost of a whole-screenshot blur.
//!
//! All filtering happens in **premultiplied** f32 so translucent pixels do not leak
//! colour, with edge pixels clamped (replicated) at the region border.
//! The pipeline is: horizontal pass, transpose, horizontal pass, transpose back. Turning
//! the vertical pass into a second horizontal one keeps every pass row-contiguous and
//! trivially parallel over rows.

use rayon::prelude::*;
use ssx_types::{Frame, Rect};

use crate::{
    Result, check, finite,
    region::{clip_region, extract, for_rows_mut, write_back},
};

/// Which blur algorithm to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlurMethod {
    /// Exact for small σ, box approximation for large σ.
    #[default]
    Auto,
    /// True separable Gaussian.
    Exact,
    /// Three-pass box approximation.
    Box3,
}

/// Largest σ (in pixels) `Auto` still runs as the exact kernel.
const AUTO_EXACT_MAX_SIGMA: f32 = 4.0;

/// σ is clamped to this: beyond it a blur is a flat average anyway, and unbounded kernels
/// would exhaust memory and time on absurd input.
const MAX_SIGMA: f32 = 1000.0;

/// Blurs `region` (frame-local; `None` = whole frame) with a Gaussian of standard
/// deviation `sigma` pixels. Edge pixels clamp to the region border. `sigma <= 0` is a
/// no-op.
pub fn gaussian_blur(
    frame: &mut Frame,
    region: Option<Rect>,
    sigma: f32,
    method: BlurMethod,
) -> Result<()> {
    check(frame)?;
    let sigma = finite(sigma, "sigma")?.min(MAX_SIGMA);
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    if sigma <= 0.0 {
        return Ok(());
    }
    let mut buf = extract(frame, r);
    blur_u8(&mut buf, r.width as usize, r.height as usize, sigma, method, false);
    write_back(frame, r, &buf);
    Ok(())
}

/// Blurs a tightly packed **premultiplied** RGBA8 buffer in place. This is the entry point
/// for callers that already hold premultiplied pixels (for example the editor's drop
/// shadows on `tiny-skia` pixmaps).
pub fn gaussian_blur_premultiplied(
    data: &mut [u8],
    width: usize,
    height: usize,
    sigma: f32,
    method: BlurMethod,
) {
    if width == 0
        || height == 0
        || data.len() < width * height * 4
        || sigma.is_nan()
        || sigma <= 0.0
    {
        return;
    }
    if !sigma.is_finite() {
        return;
    }
    blur_u8(&mut data[..width * height * 4], width, height, sigma.min(MAX_SIGMA), method, true);
}

fn blur_u8(
    data: &mut [u8],
    w: usize,
    h: usize,
    sigma: f32,
    method: BlurMethod,
    premultiplied: bool,
) {
    let mut a = to_f32(data, premultiplied);
    blur_px(&mut a, w, h, sigma, method);
    from_f32(&a, data, premultiplied);
}

/// Unsharp mask: `out = src + amount * (src - blur(src, sigma))`, applied only where the
/// difference exceeds `threshold` (0–255). Alpha is preserved.
pub fn unsharp_mask(
    frame: &mut Frame,
    region: Option<Rect>,
    sigma: f32,
    amount: f32,
    threshold: u8,
) -> Result<()> {
    check(frame)?;
    let sigma = finite(sigma, "sigma")?.min(MAX_SIGMA);
    let amount = finite(amount, "amount")?;
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    if sigma <= 0.0 || amount == 0.0 {
        return Ok(());
    }
    let src = extract(frame, r);
    let mut blurred = src.clone();
    blur_u8(&mut blurred, r.width as usize, r.height as usize, sigma, BlurMethod::Auto, false);
    let rw = r.width as usize * 4;
    let t = i32::from(threshold);
    for_rows_mut(frame, r, |y, row| {
        let off = (y - r.y as u32) as usize * rw;
        let s = &src[off..off + rw];
        let b = &blurred[off..off + rw];
        for ((o, sp), bp) in row.chunks_exact_mut(4).zip(s.chunks_exact(4)).zip(b.chunks_exact(4)) {
            for c in 0..3 {
                let d = i32::from(sp[c]) - i32::from(bp[c]);
                o[c] = if d.abs() > t {
                    (f32::from(sp[c]) + amount * d as f32).round().clamp(0.0, 255.0) as u8
                } else {
                    sp[c]
                };
            }
            o[3] = sp[3];
        }
    });
    Ok(())
}

/// Convenience sharpen: an unsharp mask with σ = 1 and the given strength
/// (0 = none, 1 = strong).
pub fn sharpen(frame: &mut Frame, region: Option<Rect>, amount: f32) -> Result<()> {
    unsharp_mask(frame, region, 1.0, amount, 0)
}

// ---------------------------------------------------------------------------------------
// f32 core
// ---------------------------------------------------------------------------------------

/// One premultiplied RGBA pixel in f32. Working on `[f32; 4]` (instead of flat `f32`)
/// lets the compiler keep each pixel in one SIMD register and drops bounds checks.
type Px = [f32; 4];

fn to_f32(data: &[u8], premultiplied: bool) -> Vec<Px> {
    let mut out = vec![[0f32; 4]; data.len() / 4];
    out.par_chunks_mut(1024).zip(data.par_chunks(4096)).for_each(|(o, s)| {
        for (op, sp) in o.iter_mut().zip(s.chunks_exact(4)) {
            if premultiplied {
                *op = [f32::from(sp[0]), f32::from(sp[1]), f32::from(sp[2]), f32::from(sp[3])];
            } else {
                let a = f32::from(sp[3]);
                let k = a / 255.0;
                *op = [f32::from(sp[0]) * k, f32::from(sp[1]) * k, f32::from(sp[2]) * k, a];
            }
        }
    });
    out
}

fn from_f32(src: &[Px], data: &mut [u8], premultiplied: bool) {
    data.par_chunks_mut(4096).zip(src.par_chunks(1024)).for_each(|(o, s)| {
        for (op, sp) in o.chunks_exact_mut(4).zip(s) {
            let a = sp[3].round().clamp(0.0, 255.0);
            if premultiplied {
                op[0] = sp[0].round().clamp(0.0, a) as u8;
                op[1] = sp[1].round().clamp(0.0, a) as u8;
                op[2] = sp[2].round().clamp(0.0, a) as u8;
            } else if a > 0.0 {
                let k = 255.0 / sp[3].max(f32::MIN_POSITIVE);
                op[0] = (sp[0] * k).round().clamp(0.0, 255.0) as u8;
                op[1] = (sp[1] * k).round().clamp(0.0, 255.0) as u8;
                op[2] = (sp[2] * k).round().clamp(0.0, 255.0) as u8;
            } else {
                op[0] = 0;
                op[1] = 0;
                op[2] = 0;
            }
            op[3] = a as u8;
        }
    });
}

/// Blurs premultiplied f32 RGBA (`w*h*4` floats) in place (test entry point).
#[cfg(test)]
pub(crate) fn blur_f32(buf: &mut [f32], w: usize, h: usize, sigma: f32, method: BlurMethod) {
    let (px, _) = buf.as_chunks_mut::<4>();
    blur_px(px, w, h, sigma, method);
}

/// What one row pass does: read a row, write a row of the same width.
enum RowOp {
    Conv(Vec<f32>),
    Box3 { r: usize, alpha: f32 },
}

#[derive(Default)]
struct Scratch {
    ext: Vec<Px>,
    a: Vec<Px>,
    b: Vec<Px>,
}

fn blur_px(buf: &mut [Px], w: usize, h: usize, sigma: f32, method: BlurMethod) {
    if w == 0 || h == 0 || buf.len() < w * h {
        return;
    }
    let exact = match method {
        BlurMethod::Exact => true,
        BlurMethod::Box3 => false,
        BlurMethod::Auto => sigma <= AUTO_EXACT_MAX_SIGMA,
    };
    let op = if exact {
        RowOp::Conv(gaussian_kernel(sigma))
    } else {
        let (r, alpha) = box_params(sigma);
        RowOp::Box3 { r, alpha }
    };
    let mut other = vec![[0f32; 4]; w * h];
    // Horizontal.
    pass_rows(buf, &mut other, w, &op);
    // Transpose -> (h wide, w tall); horizontal again == vertical on the original.
    transpose(&other, buf, w, h);
    pass_rows(buf, &mut other, h, &op);
    transpose(&other, buf, h, w);
}

fn pass_rows(src: &[Px], dst: &mut [Px], w: usize, op: &RowOp) {
    dst.par_chunks_mut(w).zip(src.par_chunks(w)).for_each_init(
        Scratch::default,
        |sc, (out, row)| {
            match op {
                RowOp::Conv(kernel) => {
                    extend_row(row, &mut sc.ext, kernel.len() / 2);
                    conv_row(&sc.ext, out, kernel);
                }
                RowOp::Box3 { r, alpha } => {
                    // Three passes entirely inside cache-resident row scratch buffers.
                    sc.a.clear();
                    sc.a.extend_from_slice(row);
                    sc.b.clear();
                    sc.b.resize(row.len(), [0.0; 4]);
                    for _ in 0..3 {
                        extend_row(&sc.a, &mut sc.ext, r + 1);
                        box_row(&sc.ext, &mut sc.b, *r, *alpha);
                        std::mem::swap(&mut sc.a, &mut sc.b);
                    }
                    out.copy_from_slice(&sc.a);
                }
            }
        },
    );
}

/// Normalised Gaussian kernel with radius `ceil(3σ)` (at least 1).
pub(crate) fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let radius = ((sigma * 3.0).ceil() as usize).max(1);
    let two_s2 = 2.0 * f64::from(sigma) * f64::from(sigma);
    let mut k: Vec<f64> = (0..=2 * radius)
        .map(|i| {
            let d = i as f64 - radius as f64;
            (-(d * d) / two_s2).exp()
        })
        .collect();
    let sum: f64 = k.iter().sum();
    for v in &mut k {
        *v /= sum;
    }
    k.into_iter().map(|v| v as f32).collect()
}

/// Parameters `(r, alpha)` of the *extended box* kernel used for all three passes: taps
/// `-r..=r` have weight 1 and taps `±(r+1)` have weight `alpha` (`0..=1`), normalised.
///
/// Three identical passes must have a combined variance of `sigma²`, so each pass has
/// variance `sigma²/3`. The kernel variance is
/// `(2·Σ_{i≤r} i² + 2α(r+1)²) / (2r+1+2α)`, which we solve for `alpha` exactly. (Plain
/// integer-width boxes, as in the classic algorithm, can only hit the target variance
/// to within ~8 %.)
pub(crate) fn box_params(sigma: f32) -> (usize, f32) {
    let v = f64::from(sigma) * f64::from(sigma) / 3.0;
    // Largest r whose plain-box variance r(r+1)/3 does not exceed v.
    let mut r = 0usize;
    while ((r + 1) * (r + 2)) as f64 / 3.0 <= v {
        r += 1;
    }
    let s_r: f64 = (1..=r).map(|i| (i * i) as f64).sum();
    let rr = (r + 1) as f64;
    let alpha = (2.0 * s_r - v * (2 * r + 1) as f64) / (2.0 * (v - rr * rr));
    (r, alpha.clamp(0.0, 1.0) as f32)
}

/// Copies `row` into `ext` with `r` replicated pixels on each side.
fn extend_row(row: &[Px], ext: &mut Vec<Px>, r: usize) {
    ext.clear();
    let (Some(&first), Some(&last)) = (row.first(), row.last()) else { return };
    ext.resize(r, first);
    ext.extend_from_slice(row);
    ext.resize(r + row.len() + r, last);
}

/// `out[i] = Σ_k kernel[k] * ext[i + k]`, exploiting the kernel's symmetry.
fn conv_row(ext: &[Px], out: &mut [Px], kernel: &[f32]) {
    let n = out.len();
    let taps = kernel.len();
    let r = taps / 2;
    let centre = &ext[r..r + n];
    let wc = kernel[r];
    for (o, c) in out.iter_mut().zip(centre) {
        *o = [wc * c[0], wc * c[1], wc * c[2], wc * c[3]];
    }
    for (k, &wk) in kernel[..r].iter().enumerate() {
        let lo = &ext[k..k + n];
        let hi = &ext[taps - 1 - k..taps - 1 - k + n];
        for ((o, a), b) in out.iter_mut().zip(lo).zip(hi) {
            for c in 0..4 {
                o[c] += wk * (a[c] + b[c]);
            }
        }
    }
}

/// One extended-box pass: `ext` is padded by `r + 1` pixels on both sides.
fn box_row(ext: &[Px], out: &mut [Px], r: usize, alpha: f32) {
    if r == 0 && alpha == 0.0 {
        let n = out.len();
        out.copy_from_slice(&ext[1..=n]);
        return;
    }
    let norm = 1.0 / ((2 * r + 1) as f32 + 2.0 * alpha);
    let mut sum = [0f32; 4];
    // Core window for output 0 is ext[1..=2r+1]; fractional taps ext[x] and ext[x+2r+2].
    for p in &ext[1..=2 * r + 1] {
        for c in 0..4 {
            sum[c] += p[c];
        }
    }
    for (x, o) in out.iter_mut().enumerate() {
        let (lo, hi) = (&ext[x], &ext[x + 2 * r + 2]);
        for c in 0..4 {
            o[c] = (sum[c] + alpha * (lo[c] + hi[c])) * norm;
        }
        let drop = &ext[x + 1];
        for c in 0..4 {
            sum[c] += hi[c] - drop[c];
        }
    }
}

/// Transposes a `w`×`h` pixel image into `dst` (`h`×`w`).
fn transpose(src: &[Px], dst: &mut [Px], w: usize, h: usize) {
    const BLK: usize = 8;
    dst.par_chunks_mut(h * BLK).enumerate().for_each(|(bi, block)| {
        let x0 = bi * BLK;
        let cols = block.len() / h;
        for y in 0..h {
            for xx in 0..cols {
                block[xx * h + y] = src[y * w + x0 + xx];
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        solid_frame,
        testutil::{noise, px},
    };

    #[test]
    fn constant_image_stays_constant() {
        for method in [BlurMethod::Exact, BlurMethod::Box3, BlurMethod::Auto] {
            for sigma in [0.5, 1.0, 3.7, 12.0] {
                let mut f = solid_frame(23, 17, [200, 30, 99, 255]);
                gaussian_blur(&mut f, None, sigma, method).unwrap();
                assert_eq!(f, solid_frame(23, 17, [200, 30, 99, 255]), "{method:?} σ={sigma}");
            }
        }
    }

    #[test]
    fn constant_translucent_image_stays_constant() {
        let mut f = solid_frame(19, 11, [200, 30, 99, 100]);
        gaussian_blur(&mut f, None, 5.0, BlurMethod::Exact).unwrap();
        assert_eq!(f, solid_frame(19, 11, [200, 30, 99, 100]));
    }

    #[test]
    fn exact_pixels_small_kernel() {
        // 1-D impulse of 255 in the middle of a black 5x1 image, sigma 1.
        let mut data = vec![0u8; 5 * 4];
        for px in data.chunks_exact_mut(4) {
            px[3] = 255;
        }
        data[2 * 4] = 255;
        let mut f = Frame::from_rgba8(5, 1, data).unwrap();
        gaussian_blur(&mut f, None, 1.0, BlurMethod::Exact).unwrap();
        // Kernel radius 3, weights exp(-d²/2)/Σ: centre 0.3989.. at infinite support;
        // the finite 7-tap normalisation lifts it slightly. Edge clamping replicates the
        // black border so the image stays symmetric.
        let vals: Vec<u8> = (0..5).map(|x| px(&f, x, 0)[0]).collect();
        assert_eq!(vals[0], vals[4]);
        assert_eq!(vals[1], vals[3]);
        assert!(vals[2] > vals[1] && vals[1] > vals[0]);
        assert_eq!(vals, vec![14, 62, 102, 62, 14]);
    }

    #[test]
    fn one_pixel_and_empty_inputs() {
        let mut f = solid_frame(1, 1, [1, 2, 3, 4]);
        gaussian_blur(&mut f, None, 9.0, BlurMethod::Auto).unwrap();
        assert_eq!(px(&f, 0, 0), [1, 2, 3, 4]);
        let mut e = solid_frame(0, 5, [0; 4]);
        gaussian_blur(&mut e, None, 3.0, BlurMethod::Auto).unwrap();
        let mut e = solid_frame(5, 0, [0; 4]);
        gaussian_blur(&mut e, Some(Rect::new(0, 0, 5, 5)), 3.0, BlurMethod::Auto).unwrap();
    }

    #[test]
    fn region_is_clipped_and_outside_untouched() {
        let base = noise(16, 16, false, 7);
        let mut f = base.clone();
        gaussian_blur(&mut f, Some(Rect::new(-4, 4, 10, 6)), 2.0, BlurMethod::Exact).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let inside = x < 6 && (4..10).contains(&y);
                if !inside {
                    assert_eq!(px(&f, x, y), px(&base, x, y), "({x},{y})");
                }
            }
        }
        assert_ne!(px(&f, 2, 6), px(&base, 2, 6));
        // Entirely outside: no-op, no panic.
        let mut g = base.clone();
        gaussian_blur(&mut g, Some(Rect::new(100, 100, 5, 5)), 2.0, BlurMethod::Exact).unwrap();
        assert_eq!(g, base);
    }

    #[test]
    fn region_blur_equals_crop_blur_paste() {
        let base = noise(20, 14, true, 3);
        let r = Rect::new(3, 2, 11, 9);
        let mut whole = base.clone();
        gaussian_blur(&mut whole, Some(r), 1.5, BlurMethod::Exact).unwrap();
        let mut piece = base.crop(r).unwrap();
        gaussian_blur(&mut piece, None, 1.5, BlurMethod::Exact).unwrap();
        for y in 0..r.height {
            for x in 0..r.width {
                assert_eq!(px(&whole, x + 3, y + 2), px(&piece, x, y));
            }
        }
    }

    #[test]
    fn energy_is_preserved_on_interior_impulse() {
        // Checked on the f32 core: 8-bit output rounding would lose the sub-half-level tails.
        for method in [BlurMethod::Exact, BlurMethod::Box3] {
            let mut buf = vec![0f32; 41 * 41 * 4];
            buf[(20 * 41 + 20) * 4] = 255.0;
            blur_f32(&mut buf, 41, 41, 3.0, method);
            let sum: f32 = buf.chunks_exact(4).map(|p| p[0]).sum();
            assert!((sum - 255.0).abs() < 0.5, "{method:?} sum {sum}");
        }
    }

    #[test]
    fn box_matches_exact_within_documented_bound() {
        // Step edge on the f32 core (so 8-bit rounding does not enter the picture): the
        // three-box kernel is a quadratic B-spline, whose CDF is within ~1.05 % of the
        // Gaussian CDF for every σ.
        for sigma in [3.0f32, 5.0, 8.0, 12.0, 25.0] {
            let (w, h) = ((sigma * 16.0) as usize, 3usize);
            let mut base = vec![0f32; w * h * 4];
            for px in base.chunks_exact_mut(4).enumerate().filter(|(i, _)| i % w >= w / 2) {
                px.1.copy_from_slice(&[255.0; 4]);
            }
            let mut e = base.clone();
            let mut b = base;
            blur_f32(&mut e, w, h, sigma, BlurMethod::Exact);
            blur_f32(&mut b, w, h, sigma, BlurMethod::Box3);
            let worst = e.iter().zip(&b).map(|(p, q)| (p - q).abs()).fold(0f32, f32::max);
            assert!(worst <= 0.0125 * 255.0, "σ={sigma}: step-edge deviation {worst} levels");
        }
    }

    #[test]
    fn box_impulse_peak_is_about_six_percent_low() {
        let sigma = 8.0f32;
        let n = 129usize;
        let c = n / 2;
        let mut base = vec![0f32; n * n * 4];
        base[(c * n + c) * 4] = 10_000.0;
        let mut e = base.clone();
        let mut b = base;
        blur_f32(&mut e, n, n, sigma, BlurMethod::Exact);
        blur_f32(&mut b, n, n, sigma, BlurMethod::Box3);
        let ratio = b[(c * n + c) * 4] / e[(c * n + c) * 4];
        assert!((0.85..0.92).contains(&ratio), "2-D peak ratio {ratio}");
    }

    #[test]
    fn box_params_have_exact_variance() {
        for sigma in [0.7f32, 2.0, 4.0, 9.0, 30.0, 150.0] {
            let (r, alpha) = box_params(sigma);
            let s_r: f64 = (1..=r).map(|i| (i * i) as f64).sum();
            let a = f64::from(alpha);
            let var1 =
                (2.0 * s_r + 2.0 * a * ((r + 1) * (r + 1)) as f64) / ((2 * r + 1) as f64 + 2.0 * a);
            let want = f64::from(sigma) * f64::from(sigma);
            assert!((3.0 * var1 - want).abs() / want < 1e-4, "σ={sigma}: {} vs {want}", 3.0 * var1);
        }
    }

    #[test]
    fn transparent_pixels_do_not_bleed_colour() {
        // Left half opaque red, right half fully transparent *black*.
        let mut f = solid_frame(10, 3, [0, 0, 0, 0]);
        for y in 0..3u32 {
            for x in 0..5u32 {
                let i = (y as usize * 10 + x as usize) * 4;
                f.data_mut()[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
            }
        }
        gaussian_blur(&mut f, None, 1.5, BlurMethod::Exact).unwrap();
        // Every partially transparent pixel must still be pure red.
        for x in 0..10 {
            let p = px(&f, x, 1);
            if p[3] > 0 {
                assert!(p[0] >= 254 && p[1] == 0 && p[2] == 0, "x={x} {p:?}");
            }
        }
    }

    #[test]
    fn rejects_non_finite_and_wrong_format() {
        let mut f = solid_frame(2, 2, [0; 4]);
        assert!(gaussian_blur(&mut f, None, f32::NAN, BlurMethod::Auto).is_err());
        let bgra = Frame::new(
            ssx_types::Size::new(2, 2),
            ssx_types::PixelFormat::Bgra8,
            ssx_types::ColorSpace::Srgb,
        );
        let mut bgra = bgra;
        assert!(matches!(
            gaussian_blur(&mut bgra, None, 1.0, BlurMethod::Auto),
            Err(crate::FxError::UnsupportedFormat(..))
        ));
    }

    #[test]
    fn unsharp_increases_edge_contrast_and_keeps_flat_areas() {
        let mut f = solid_frame(12, 4, [100, 100, 100, 255]);
        for y in 0..4usize {
            for x in 6..12usize {
                f.data_mut()[(y * 12 + x) * 4..(y * 12 + x) * 4 + 3].copy_from_slice(&[150; 3]);
            }
        }
        sharpen(&mut f, None, 1.0).unwrap();
        assert!(px(&f, 5, 1)[0] < 100, "dark side of the edge gets darker");
        assert!(px(&f, 6, 1)[0] > 150, "bright side gets brighter");
        assert_eq!(px(&f, 0, 1), [100, 100, 100, 255], "flat area untouched");
        assert_eq!(px(&f, 11, 1), [150, 150, 150, 255]);
    }

    #[test]
    fn premultiplied_entry_point_blurs() {
        let mut d = vec![0u8; 9 * 9 * 4];
        let i = (4 * 9 + 4) * 4;
        d[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        gaussian_blur_premultiplied(&mut d, 9, 9, 1.0, BlurMethod::Exact);
        assert!(d[i] < 255 && d[i] > 0);
        assert!(d[i + 4] > 0);
    }
}
