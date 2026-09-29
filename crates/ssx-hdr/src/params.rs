//! Per-pixel tonemapping maths: [`PixelParams`], [`tonemap_rgb`] and the GPU mirror
//! [`GpuParams`].
//!
//! Everything here is a pure function of `f32` values so a WGSL port can be compared
//! against it directly. All constants that depend on the settings (curve coefficients,
//! the ACES slope-matching solve) are computed once in [`PixelParams::new`] on the CPU and
//! shipped to the shader in [`GpuParams`]; the per-pixel code below only needs `+ - * /`,
//! `min/max`, a `pow` (in the sRGB OETF) and rational functions.
//!
//! # Pipeline
//!
//! For one pixel `c = (r, g, b)` in scRGB (1.0 = 80 nits):
//!
//! 1. **Scale.** `x = c * exposure * 80 / sdr_white_nits`, so SDR white becomes 1.0.
//!    `NaN → 0`, `+Inf → peak`, `-Inf → 0` (applied to the scaled value).
//! 2. **Gamut map** (only if some channel is negative). With Rec.709 luminance
//!    `Y = 0.2126 r + 0.7152 g + 0.0722 b`: if `Y <= 0` the pixel is black; otherwise the
//!    pixel is moved along the straight line towards the achromatic colour `(Y, Y, Y)`
//!    by the *smallest* amount that makes the minimum channel zero,
//!    `x' = Y + t (x - Y)` with `t = Y / (Y - min(x))`. This preserves luminance exactly,
//!    leaves in-gamut colours untouched, is continuous (`t → 1` as `min → 0⁻`) and never
//!    clips a channel independently, so hue does not skew.
//! 3. **Roll-off.** With `m = max(x')` the scalar `f(m)` from the selected operator maps
//!    `m` to `[0, 1]`, and the pixel becomes `x' * f(m) / m`. Scaling all channels by one
//!    factor preserves the channel *ratios* exactly (hue and saturation of the linear-light
//!    colour) and guarantees no channel exceeds 1. `Clip` is the exception: it clamps each
//!    channel independently, by definition.
//! 4. The caller applies the piecewise sRGB OETF and quantises (see [`crate::dither`]).
//!
//! # The roll-off curves
//!
//! Let `k = min(knee, 1)` be the effective knee, `P = peak`, `H = 1 - k` the output
//! headroom above the knee and `W = P - k` the input width of the shoulder. Every roll-off
//! operator satisfies
//!
//! * `f(m) = m` for `m <= k` (bit-exact pass-through: SDR content is never touched),
//! * `f(P) = 1` and `f(m) = 1` for `m >= P`,
//! * `f` continuous and non-decreasing.
//!
//! **Why the knee has to sit below 1 to get a shoulder.** The output range ends at 1.0,
//! which *is* SDR white. With `k = 1` there is no headroom (`H = 0`) and any monotone
//! curve that is the identity up to 1 must be 1 above it. All operators then degenerate to
//! the *hue-preserving clip* `f(m) = min(m, 1)` (channels are scaled by `1 / m`, unlike
//! `Clip` which clamps them), which is what the default settings give: perfect SDR
//! content, colour-correct highlights, no highlight detail. Lowering `knee` (e.g. 0.8)
//! trades the top of the SDR range for real compression of the highlights. `knee > 1` is
//! treated as 1.
//!
//! With `t = (m - k) / W` and `v = (m - k) / H`, for `k < m < P`:
//!
//! * **Reinhard extended** (Reinhard et al. 2002, eq. 4), applied to the excess over the
//!   knee: `f = k + H * g(v)`, `g(v) = v (1 + v / w²) / (1 + v)`, `w = W / H`. `g(0) = 0`,
//!   `g'(0) = 1` (so `f` is C¹ at the knee), `g(w) = 1`, and
//!   `g'(v) = (1 + 2v/w² + v²/w²) / (1 + v)² > 0`.
//! * **BT.2390 EETF** (ITU-R BT.2390-10 §5.4.1): the cubic Hermite spline
//!   `P(T) = (2T³ - 3T² + 1) P0 + (T³ - 2T² + T) (1 - KS) + (-2T³ + 3T²) P1` from the
//!   knee `P0` (slope 1) to the peak `P1` (slope 0), mapped onto `[k, P] → [k, 1]`. In
//!   the normalised `t` domain that is `s(t) = α (t³ - 2t² + t) + 3t² - 2t³` with start
//!   slope `α = W / H` and `f = k + H s(t)`. `s` is monotone iff `α <= 3`; for a wider
//!   shoulder `α` is capped at 3 (the value BT.2390's own `KS = 1.5 maxLum - 0.5` fixes),
//!   which keeps `f` monotone at the price of a slope discontinuity at the knee. The
//!   spline is evaluated on linear light rather than on PQ because the output is sRGB,
//!   not PQ.
//! * **ACES fit** (Narkowicz 2016, `N(z) = z (2.51 z + 0.03) / (z (2.43 z + 0.59) + 0.14)`).
//!   The knee is attached at the point `z0` of the curve's concave shoulder:
//!   `f = k + S (N(z0 + c (m - k)) - N(z0))`, with `S = 1 / (c N'(z0))` (slope 1 at the
//!   knee) and `c` solved on the CPU so that `f(P) = 1`. If no such `c` exists
//!   (`P <= 1`, a convex shoulder is needed) the shoulder falls back to the straight line
//!   `f = k + H t`.
//! * **Clip**: per-channel `min(x, 1)`.
//!
//! # GPU parity
//!
//! A shader must implement [`tonemap_rgb`] using the fields of [`GpuParams`] and the
//! formulas above; results should agree to about `1e-6` in linear light and to ±1 code
//! after quantisation.

use crate::settings::{TonemapOperator, TonemapSettings};

/// Rec.709 / sRGB luminance weights (also the scRGB primaries' Y row).
pub const LUMA_709: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Rec.709 luminance of a linear RGB triple.
pub fn luminance(rgb: [f32; 3]) -> f32 {
    LUMA_709[0] * rgb[0] + LUMA_709[1] * rgb[1] + LUMA_709[2] * rgb[2]
}

/// Which roll-off is in force after resolving degenerate settings.
///
/// The discriminants are the values stored in [`GpuParams::mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ShoulderMode {
    /// Per-channel clamp to `[0, 1]` (the `Clip` operator).
    ChannelClip = 0,
    /// `f(m) = min(m, 1)` applied as a uniform scale (no shoulder headroom or width).
    HueClip = 1,
    /// Extended Reinhard shoulder. `c[0] = w`, `c[1] = 1 / w²`.
    Reinhard = 2,
    /// BT.2390 Hermite shoulder. `c[0] = α` (start slope, capped at 3).
    Hermite = 3,
    /// ACES-fit shoulder. `c = [z0, c, S, N(z0)]`.
    Aces = 4,
    /// Straight line from `(k, k)` to `(P, 1)`.
    Linear = 5,
}

/// Settings resolved into the constants the per-pixel maths needs.
///
/// Build with [`PixelParams::new`]. The fields are public so a shader port can be checked
/// against them, but they are derived data: construct via `new`, do not edit by hand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelParams {
    /// Operator these constants were derived for.
    pub operator: TonemapOperator,
    /// Resolved roll-off.
    pub mode: ShoulderMode,
    /// `exposure * 80 / sdr_white_nits`: multiplies raw scRGB into SDR-relative light.
    pub scale: f32,
    /// Effective knee `min(knee, 1)`.
    pub knee: f32,
    /// Peak (multiples of SDR white).
    pub peak: f32,
    /// Output headroom above the knee, `1 - knee`.
    pub headroom: f32,
    /// Input width of the shoulder, `peak - knee`.
    pub width: f32,
    /// Whether the caller should dither before quantisation.
    pub dither: bool,
    /// Operator specific constants, see [`ShoulderMode`].
    pub c: [f32; 4],
}

/// Narkowicz ACES fit.
fn aces_n(z: f64) -> f64 {
    z * (2.51 * z + 0.03) / (z * (2.43 * z + 0.59) + 0.14)
}

/// Derivative of [`aces_n`].
fn aces_dn(z: f64) -> f64 {
    let num = 2.51 * z * z + 0.03 * z;
    let den = 2.43 * z * z + 0.59 * z + 0.14;
    ((5.02 * z + 0.03) * den - num * (4.86 * z + 0.59)) / (den * den)
}

/// Point of the ACES curve at which the shoulder is attached. The curve is concave from
/// here on (checked in the tests), which the slope-matching solve relies on.
const ACES_Z0: f64 = 0.8;

/// Solves for the input scale `c` of the ACES shoulder so that the curve, with unit slope
/// at the knee, reaches 1 exactly at the peak. `None` if no solution exists.
fn solve_aces(width: f64, headroom: f64) -> Option<[f32; 4]> {
    if headroom >= width {
        return None;
    }
    let n0 = aces_n(ACES_Z0);
    let d0 = aces_dn(ACES_Z0);
    // h(c) = mean slope of N over [z0, z0 + c W] divided by N'(z0) scaled by W: strictly
    // decreasing from W (c -> 0) to 0 (c -> inf) because N is concave beyond z0.
    let h = |c: f64| (aces_n(ACES_Z0 + c * width) - n0) / (c * d0);
    let (mut lo, mut hi) = (1e-9f64.ln(), 1e9f64.ln());
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if h(mid.exp()) > headroom {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let c = (0.5 * (lo + hi)).exp();
    let s = 1.0 / (c * d0);
    Some([ACES_Z0 as f32, c as f32, s as f32, n0 as f32])
}

impl PixelParams {
    /// Below this the shoulder has no usable width or headroom in `f32`.
    const EPS: f32 = 1e-6;

    /// Resolves `settings` for a display whose SDR white is `sdr_white_nits`.
    ///
    /// The settings are assumed valid ([`TonemapSettings::validate`]); out-of-range values
    /// do not panic but give unspecified (still finite, in-range) output.
    pub fn new(settings: &TonemapSettings, sdr_white_nits: f32) -> Self {
        let scale = (settings.exposure * 80.0 / sdr_white_nits).min(f32::MAX);
        let knee = settings.knee.min(1.0);
        let peak = settings.peak.max(knee);
        let headroom = 1.0 - knee;
        let width = peak - knee;
        let mut c = [0.0f32; 4];
        let degenerate = headroom <= Self::EPS || width <= Self::EPS;
        let mode = match settings.operator {
            TonemapOperator::Clip => ShoulderMode::ChannelClip,
            _ if degenerate => ShoulderMode::HueClip,
            TonemapOperator::ReinhardExtended => {
                let w = width / headroom;
                c[0] = w;
                c[1] = 1.0 / (w * w);
                ShoulderMode::Reinhard
            }
            TonemapOperator::Bt2390 => {
                c[0] = (width / headroom).min(3.0);
                ShoulderMode::Hermite
            }
            TonemapOperator::AcesFit => match solve_aces(f64::from(width), f64::from(headroom)) {
                Some(k) => {
                    c = k;
                    ShoulderMode::Aces
                }
                None => ShoulderMode::Linear,
            },
        };
        Self {
            operator: settings.operator,
            mode,
            scale,
            knee,
            peak,
            headroom,
            width,
            dither: settings.dither,
            c,
        }
    }

    /// The uniform-buffer mirror of these parameters.
    pub fn to_gpu(&self) -> GpuParams {
        GpuParams::from(self)
    }
}

/// Uniform-buffer layout mirroring [`PixelParams`] for the WGSL port.
///
/// 48 bytes, three `vec4`s, 16-byte aligned. The matching WGSL declaration is
///
/// ```wgsl
/// struct Params {
///     scale: f32, knee: f32, peak: f32, headroom: f32,
///     mode: u32, dither: u32, width: f32, pad: f32,
///     c: vec4<f32>,
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C, align(16))]
pub struct GpuParams {
    /// `exposure * 80 / sdr_white_nits`.
    pub scale: f32,
    /// Effective knee.
    pub knee: f32,
    /// Peak.
    pub peak: f32,
    /// `1 - knee`.
    pub headroom: f32,
    /// [`ShoulderMode`] discriminant.
    pub mode: u32,
    /// `1` to dither, `0` not to.
    pub dither: u32,
    /// `peak - knee`.
    pub width: f32,
    /// Padding to keep the next `vec4` 16-byte aligned; always zero.
    pub pad: f32,
    /// Operator constants, see [`ShoulderMode`].
    pub c: [f32; 4],
}

const _: () = assert!(size_of::<GpuParams>() == 48 && align_of::<GpuParams>() == 16);

impl From<&PixelParams> for GpuParams {
    fn from(p: &PixelParams) -> Self {
        Self {
            scale: p.scale,
            knee: p.knee,
            peak: p.peak,
            headroom: p.headroom,
            mode: p.mode as u32,
            dither: u32::from(p.dither),
            width: p.width,
            pad: 0.0,
            c: p.c,
        }
    }
}

/// Scales one raw scRGB channel to SDR-relative light and sanitises non-finite values:
/// `NaN → 0`, `+Inf → peak` (at least SDR white, so it always ends up white), `-Inf → 0`.
#[inline]
pub fn scale_channel(v: f32, p: &PixelParams) -> f32 {
    let x = v * p.scale;
    if x.is_nan() || x == f32::NEG_INFINITY {
        0.0
    } else if x == f32::INFINITY {
        p.peak.max(1.0)
    } else {
        x
    }
}

/// Moves a colour with negative channels onto the sRGB gamut boundary along the line to
/// its own grey, preserving luminance. In-gamut colours are returned unchanged.
#[inline]
pub fn gamut_map(x: [f32; 3]) -> [f32; 3] {
    let min = x[0].min(x[1]).min(x[2]);
    if min >= 0.0 {
        return x;
    }
    let y = luminance(x);
    if y <= 0.0 {
        return [0.0; 3];
    }
    let t = y / (y - min);
    // `.max(0.0)` only absorbs rounding: by construction the minimum lands on 0.
    x.map(|v| (y + t * (v - y)).max(0.0))
}

/// The scalar roll-off `f(m)` for max-channel value `m` (multiples of SDR white, `>= 0`).
///
/// For [`ShoulderMode::ChannelClip`] this is plain `min(m, 1)`; that mode is applied
/// per channel by [`tonemap_rgb`].
#[inline]
pub fn rolloff(m: f32, p: &PixelParams) -> f32 {
    let k = p.knee;
    match p.mode {
        ShoulderMode::ChannelClip | ShoulderMode::HueClip => return m.min(1.0),
        _ if m <= k => return m,
        _ if m >= p.peak => return 1.0,
        _ => {}
    }
    let d = m - k;
    let f = match p.mode {
        ShoulderMode::Reinhard => {
            let v = d / p.headroom;
            k + p.headroom * (v * (1.0 + v * p.c[1]) / (1.0 + v))
        }
        ShoulderMode::Hermite => {
            let t = d / p.width;
            let a = p.c[0];
            // Two algebraically identical forms of the Hermite spline, each accurate
            // where it is used: near t = 0 the direct form, near t = 1 the form in
            // u = 1 - t (which cannot dip below 1 through cancellation).
            let s = if t < 0.5 {
                t * (1.0 - t) * (1.0 - t) * a + t * t * (3.0 - 2.0 * t)
            } else {
                let u = 1.0 - t;
                1.0 - u * u * ((3.0 - a) + (a - 2.0) * u)
            };
            k + p.headroom * s
        }
        ShoulderMode::Aces => {
            let z = p.c[0] + p.c[1] * d;
            let n = z * (2.51 * z + 0.03) / (z * (2.43 * z + 0.59) + 0.14);
            k + p.c[2] * (n - p.c[3])
        }
        ShoulderMode::Linear => k + p.headroom * (d / p.width),
        ShoulderMode::ChannelClip | ShoulderMode::HueClip => m.min(1.0),
    };
    f.clamp(k, 1.0)
}

/// Tonemaps one scRGB pixel (1.0 = 80 nits, may be negative or non-finite) to linear
/// SDR-relative light in `[0, 1]`. Apply [`srgb_oetf`](crate::srgb_oetf) afterwards.
pub fn tonemap_rgb(rgb_scrgb: [f32; 3], p: &PixelParams) -> [f32; 3] {
    tonemap_scaled(rgb_scrgb.map(|v| scale_channel(v, p)), p)
}

/// [`tonemap_rgb`] after step 1: input already scaled by [`scale_channel`] (so finite,
/// SDR white = 1.0). Lets callers that cache the scaling (the table-driven frame
/// converter) skip it.
#[allow(clippy::float_cmp)] // `v == m` picks the exact max channel, not a tolerance test
pub fn tonemap_scaled(scaled: [f32; 3], p: &PixelParams) -> [f32; 3] {
    let x = gamut_map(scaled);
    if p.mode == ShoulderMode::ChannelClip {
        return x.map(|v| v.clamp(0.0, 1.0));
    }
    let m = x[0].max(x[1]).max(x[2]);
    if m <= p.knee {
        // Includes m == 0 (black) and the whole SDR range at the default knee.
        return x.map(|v| v.clamp(0.0, 1.0));
    }
    let f = rolloff(m, p);
    let s = f / m;
    // The brightest channel is set to `f` exactly (not `m * (f / m)`) so that rounding
    // cannot make e.g. a grey ramp dip by an ulp around `f == 1`.
    x.map(|v| if v == m { f } else { (v * s).clamp(0.0, 1.0) })
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests assert bit-exact pass-through on purpose
mod tests {
    use super::*;

    fn params(op: TonemapOperator, knee: f32, peak: f32) -> PixelParams {
        PixelParams::new(
            &TonemapSettings { operator: op, knee, peak, dither: false, exposure: 1.0 },
            80.0,
        )
    }

    const ROLLOFF_OPS: [TonemapOperator; 3] =
        [TonemapOperator::ReinhardExtended, TonemapOperator::Bt2390, TonemapOperator::AcesFit];

    #[test]
    fn aces_shoulder_is_concave_from_z0() {
        let h = 1e-4;
        let mut z = ACES_Z0;
        while z < 200.0 {
            let d2 = aces_n(z + h) - 2.0 * aces_n(z) + aces_n(z - h);
            assert!(d2 <= 1e-12, "N not concave at {z}: {d2}");
            z += 0.05;
        }
    }

    #[test]
    fn aces_solve_hits_peak_with_unit_slope() {
        for &(k, pk) in &[(0.75f32, 4.0f32), (0.5, 2.0), (0.9, 10.0), (0.2, 1.5), (0.8, 1.1)] {
            let p = params(TonemapOperator::AcesFit, k, pk);
            assert_eq!(p.mode, ShoulderMode::Aces, "k={k} pk={pk}");
            assert!((rolloff(pk - 1e-4, &p) - 1.0).abs() < 1e-3, "k={k} pk={pk}");
            let e = 1e-4f32;
            let slope = (rolloff(k + 2.0 * e, &p) - rolloff(k + e, &p)) / e;
            assert!((slope - 1.0).abs() < 0.03, "slope {slope} k={k} pk={pk}");
        }
    }

    #[test]
    fn degenerate_settings_become_hue_clip() {
        for op in ROLLOFF_OPS {
            assert_eq!(params(op, 1.0, 4.0).mode, ShoulderMode::HueClip);
            assert_eq!(params(op, 2.0, 4.0).mode, ShoulderMode::HueClip); // knee > 1
            assert_eq!(params(op, 0.5, 0.5).mode, ShoulderMode::HueClip); // no width
        }
        assert_eq!(params(TonemapOperator::Clip, 0.5, 4.0).mode, ShoulderMode::ChannelClip);
    }

    #[test]
    fn rolloff_is_identity_below_knee_and_one_at_peak() {
        for op in ROLLOFF_OPS {
            for &(k, pk) in &[(0.75f32, 4.0f32), (0.5, 1.0), (0.9, 16.0), (0.3, 0.8)] {
                let p = params(op, k, pk);
                for i in 0..=100 {
                    let m = k * i as f32 / 100.0;
                    assert_eq!(rolloff(m, &p), m, "{op:?} k={k} m={m}");
                }
                assert!((rolloff(pk, &p) - 1.0).abs() < 1e-6, "{op:?} k={k} pk={pk}");
                assert_eq!(rolloff(pk * 3.0 + 1.0, &p), 1.0);
                assert_eq!(rolloff(1e30, &p), 1.0);
            }
        }
    }

    #[test]
    fn rolloff_is_monotonic_bounded_and_continuous() {
        for op in ROLLOFF_OPS {
            for &(k, pk) in &[(0.75f32, 4.0f32), (0.5, 1.0), (0.9, 16.0), (0.3, 0.8), (0.6, 1.2)] {
                let p = params(op, k, pk);
                let n = 20_000;
                let hi = pk * 1.2;
                let mut prev = 0.0f32;
                let mut worst_step = 0.0f32;
                for i in 0..=n {
                    let m = hi * i as f32 / n as f32;
                    let f = rolloff(m, &p);
                    assert!((0.0..=1.0).contains(&f), "{op:?} out of range at {m}: {f}");
                    assert!(f >= prev - 1e-7, "{op:?} k={k} pk={pk} decreasing at {m}");
                    worst_step = worst_step.max(f - prev);
                    prev = f;
                }
                // Lipschitz-style continuity: the largest step must be bounded by the
                // steepest slope times the step size (slopes here are at most ~a).
                let a = (pk - k) / (1.0 - k);
                let bound = (hi / n as f32) * (a + 4.0);
                assert!(worst_step <= bound, "{op:?} jump {worst_step} > {bound} (k={k} pk={pk})");
            }
        }
    }

    #[test]
    fn continuous_at_the_knee_exactly() {
        for op in ROLLOFF_OPS {
            let p = params(op, 0.75, 4.0);
            let below = rolloff(0.75, &p);
            let above = rolloff(0.75 + 1e-6, &p);
            assert_eq!(below, 0.75);
            assert!((above - below).abs() < 1e-5, "{op:?}");
        }
    }

    #[test]
    fn gpu_params_layout() {
        let p = params(TonemapOperator::AcesFit, 0.75, 4.0);
        let g = p.to_gpu();
        assert_eq!(size_of::<GpuParams>(), 48);
        assert_eq!(align_of::<GpuParams>(), 16);
        assert_eq!(g.mode, ShoulderMode::Aces as u32);
        assert_eq!(g.c, p.c);
        assert_eq!(g.scale, p.scale);
        assert_eq!(g.pad, 0.0);
        assert_eq!(GpuParams::from(&p), g);
    }

    #[test]
    fn scale_uses_exposure_and_sdr_white() {
        let s = TonemapSettings { exposure: 2.0, ..TonemapSettings::default() };
        let p = PixelParams::new(&s, 200.0);
        assert!((p.scale - 0.8).abs() < 1e-6);
    }
}
