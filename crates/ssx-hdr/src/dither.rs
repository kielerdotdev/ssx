//! Deterministic dither and 8-bit quantisation.
//!
//! # Noise
//!
//! Interleaved gradient noise (Jimenez, "Next Generation Post Processing in Call of Duty:
//! Advanced Warfare", SIGGRAPH 2014): `fract(52.9829189 * fract(0.06711056 x + 0.00583715 y))`.
//! It is a pure function of the pixel coordinate (frame-local, so results do not depend
//! on the capture origin or thread scheduling), needs no texture, and is trivial to port
//! to WGSL. **One** noise value is shared by the three channels of a pixel, so the dither
//! is luminance noise and neutral greys stay neutral.
//!
//! # Quantisation and the "representable" dead zone
//!
//! With encoded value `v = 255 * oetf(y)` (in code units) the plain quantiser is
//! `round(v) = floor(v + 0.5)`. Dithering replaces the constant `0.5` by noise
//! `n ∈ [0, 1)`: `floor(v + n)` (equivalent to adding ±0.5 LSB of noise before rounding)
//! which is unbiased, `E[floor(v + n)] = v`.
//!
//! Plain SDR desktop content must stay *byte-identical* to a non-HDR screenshot. In an
//! `Rgba16F` frame such content is only *nearly* on an 8-bit code: half floats carry 11
//! significant bits, giving up to about 0.06 code of error at the bright end. Dithering
//! `j ± 0.06` would flip about 6 % of the pixels of a flat region to `j ± 1`. So a channel
//! whose `v` lies within [`DITHER_DEAD_ZONE`] of an integer is *rounded, never dithered*.
//! The constant is 2.5× the half-float error bound to leave room for a couple of upstream
//! roundings in the compositor.
//!
//! Consequences, by design: solid SDR regions are exact for every noise value; genuine
//! HDR/gradient data within 0.15 code of a level is rounded (worst-case error 0.15 code,
//! a small bias confined to those samples) and the rest is dithered unbiased.

use crate::srgb::srgb_oetf;

/// Distance (in 8-bit code units) from an integer within which a value is rounded rather
/// than dithered. See the module docs.
pub const DITHER_DEAD_ZONE: f32 = 0.15;

/// Fractional part of a non-negative value. Truncating through an integer cast avoids the
/// libm `trunc` call that `f32::fract` needs on baseline x86-64 (no SSE4.1), which
/// dominated the per-pixel cost. Identical to `f32::fract` for `0 <= f < 2^31`.
#[inline]
fn fract_pos(f: f32) -> f32 {
    f - f as i32 as f32
}

/// Interleaved gradient noise in `[0, 1)` for frame-local pixel `(x, y)`.
#[inline]
pub fn dither_noise(x: u32, y: u32) -> f32 {
    let f = fract_pos(0.067_110_56 * x as f32 + 0.005_837_15 * y as f32);
    fract_pos(52.982_918 * f)
}

/// Quantises an encoded value `v` (0..=255 scale, may lie outside) to a byte.
///
/// With `noise = None` this is `round(v)`; with `Some(n)` it is `floor(v + n)` except
/// inside the dead zone, see the module docs. `n` must be in `[0, 1)`. Out-of-range and
/// NaN inputs saturate to `0` / `255` / `0`.
#[inline]
pub fn quantize(v: f32, noise: Option<f32>) -> u8 {
    // Clamping first is equivalent to clamping last (both ends are integers, and the
    // result is 0/255 either way) but keeps everything non-negative, so plain integer
    // casts implement `floor` without libm calls. NaN survives `clamp` and casts to 0.
    let v = v.clamp(0.0, 255.0);
    let r = (v + 0.5) as u32;
    // Both candidates are computed and one is selected: on real HDR data the dead-zone
    // test is essentially random per channel, and a mispredicted branch costs more than
    // the arithmetic. `n = 0.5` makes the dithered candidate equal to plain rounding.
    let d = (v + noise.unwrap_or(0.5)) as u32;
    let out = if (v - r as f32).abs() > DITHER_DEAD_ZONE { d } else { r };
    out.min(255) as u8
}

/// Full per-channel encode: clamps linear light to `[0, 1]`, applies the sRGB OETF and
/// quantises with optional dither noise.
#[inline]
pub fn encode_channel(linear: f32, noise: Option<f32>) -> u8 {
    quantize(255.0 * srgb_oetf(linear.clamp(0.0, 1.0)), noise)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests assert bit-exact pass-through on purpose
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_in_range() {
        for y in 0..64 {
            for x in 0..64 {
                let n = dither_noise(x, y);
                assert!((0.0..1.0).contains(&n), "{n}");
                assert_eq!(n, dither_noise(x, y));
            }
        }
        assert_ne!(dither_noise(0, 0), dither_noise(1, 0));
        assert_eq!(dither_noise(0, 0), 0.0);
    }

    #[test]
    fn noise_is_roughly_uniform() {
        let mut bins = [0u32; 10];
        let mut sum = 0.0f64;
        let n = 256u32;
        for y in 0..n {
            for x in 0..n {
                let v = dither_noise(x, y);
                sum += f64::from(v);
                bins[(v * 10.0) as usize] += 1;
            }
        }
        let count = f64::from(n * n);
        assert!((sum / count - 0.5).abs() < 0.01, "mean {}", sum / count);
        for b in bins {
            let frac = f64::from(b) / count;
            assert!((frac - 0.1).abs() < 0.02, "bin fraction {frac}");
        }
    }

    #[test]
    fn undithered_is_plain_rounding() {
        assert_eq!(quantize(187.49, None), 187);
        assert_eq!(quantize(187.5, None), 188);
        assert_eq!(quantize(-3.0, None), 0);
        assert_eq!(quantize(300.0, None), 255);
        assert_eq!(quantize(f32::NAN, None), 0);
    }

    #[test]
    fn dead_zone_values_ignore_noise() {
        for c in 0..=255u32 {
            for d in [-0.14f32, -0.05, 0.0, 0.05, 0.14] {
                let v = c as f32 + d;
                for i in 0..20 {
                    let n = i as f32 / 20.0;
                    assert_eq!(quantize(v, Some(n)), c as u8, "v={v} n={n}");
                }
            }
        }
    }

    #[test]
    fn dither_is_mean_preserving_outside_dead_zone() {
        for &frac in &[0.2f32, 0.35, 0.5, 0.77, 0.85] {
            let v = 100.0 + frac;
            let mut sum = 0.0f64;
            let n = 128u32;
            for y in 0..n {
                for x in 0..n {
                    sum += f64::from(quantize(v, Some(dither_noise(x, y))));
                }
            }
            let mean = sum / f64::from(n * n);
            assert!((mean - f64::from(v)).abs() < 0.01, "v={v} mean={mean}");
        }
    }

    #[test]
    fn dithered_output_only_uses_the_two_neighbouring_codes() {
        for i in 0..2000 {
            let v = 10.0 + i as f32 * 0.0917;
            for k in 0..10 {
                let q = quantize(v, Some(k as f32 / 10.0));
                assert!(f32::from(q) >= v.floor() && f32::from(q) <= v.floor() + 1.0);
            }
        }
    }

    #[test]
    fn encode_channel_endpoints() {
        assert_eq!(encode_channel(0.0, None), 0);
        assert_eq!(encode_channel(1.0, None), 255);
        assert_eq!(encode_channel(0.5, None), 188);
        assert_eq!(encode_channel(5.0, Some(0.99)), 255);
        assert_eq!(encode_channel(-2.0, Some(0.99)), 0);
    }
}
