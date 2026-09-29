//! The piecewise sRGB transfer functions of IEC 61966-2-1.
//!
//! These are deliberately the *piecewise* curves (linear toe + 2.4 power), not the
//! "gamma 2.2" approximation: the two differ by several 8-bit codes in the shadows, and
//! every image viewer and browser decodes sRGB with the piecewise curve, so a 2.2 encode
//! would visibly lift or crush dark UI colours.

/// Linear-light value below which the OETF is the straight `12.92 * x` segment.
pub const OETF_TOE: f32 = 0.003_130_8;
/// Encoded value below which the EOTF is the straight `x / 12.92` segment.
pub const EOTF_TOE: f32 = 0.040_45;

/// Linear light (1.0 = SDR white) to gamma-encoded sRGB signal.
///
/// Values at or below zero stay on the linear toe (so the function is odd-extended near
/// zero and never returns NaN for finite input); callers that need `[0, 1]` clamp first.
pub fn srgb_oetf(linear: f32) -> f32 {
    if linear <= OETF_TOE { 12.92 * linear } else { 1.055 * linear.powf(1.0 / 2.4) - 0.055 }
}

/// Gamma-encoded sRGB signal to linear light (1.0 = SDR white). Inverse of [`srgb_oetf`].
pub fn srgb_eotf(encoded: f32) -> f32 {
    if encoded <= EOTF_TOE { encoded / 12.92 } else { ((encoded + 0.055) / 1.055).powf(2.4) }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests assert bit-exact pass-through on purpose
mod tests {
    use super::*;

    #[test]
    fn golden_values() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        // 0.5 linear is the canonical 188/255 mid-grey check.
        assert_eq!((srgb_oetf(0.5) * 255.0 + 0.5).floor() as u8, 188);
        assert!((srgb_oetf(0.5) - 0.735_357).abs() < 1e-5);
        // The two ends of the toe.
        assert!((srgb_oetf(0.001) - 0.012_92).abs() < 1e-7);
        assert!((srgb_eotf(0.5) - 0.214_041).abs() < 1e-5);
    }

    #[test]
    fn round_trip_error_is_tiny() {
        let mut worst = 0.0f64;
        for i in 0..=100_000u32 {
            let x = i as f32 / 100_000.0;
            let back = srgb_eotf(srgb_oetf(x));
            worst = worst.max(f64::from((back - x).abs()));
            let e_back = srgb_oetf(srgb_eotf(x));
            worst = worst.max(f64::from((e_back - x).abs()));
        }
        assert!(worst < 1e-6, "worst round-trip error {worst}");
    }

    #[test]
    fn monotonic_and_no_nan() {
        let mut prev = f32::NEG_INFINITY;
        for i in -1000..=20_000i32 {
            let v = srgb_oetf(i as f32 / 10_000.0);
            assert!(v.is_finite());
            assert!(v >= prev, "not monotonic at {i}");
            prev = v;
        }
    }

    #[test]
    fn all_256_codes_round_trip_exactly() {
        for c in 0..=255u32 {
            let e = c as f32 / 255.0;
            let code = (srgb_oetf(srgb_eotf(e)) * 255.0 + 0.5).floor() as u32;
            assert_eq!(code, c);
        }
    }
}
