//! Conversion from the settings' [`HdrConfig`] to `ssx-hdr`'s [`TonemapSettings`].
//!
//! The two crates deliberately do not depend on each other (`ssx-core` must stay free of the
//! pixel code), so their vocabularies were written independently and differ in two places:
//!
//! | field | `ssx-core` `HdrConfig` | `ssx-hdr` `TonemapSettings` | conversion |
//! |---|---|---|---|
//! | `exposure` | **EV stops**, `-10..=10`, `0` = unchanged | **linear multiplier**, `> 0`, `1` = unchanged | `2^EV` |
//! | `knee` | fraction of SDR white, `0..=1` | multiples of SDR white, must be `> 0` (values above 1 are clamped by the tonemapper) | identity, but `0` is raised to [`MIN_KNEE`] |
//! | `peak` | multiple of SDR white, `1..=100` | multiples of SDR white, `>= knee` | identity |
//! | `operator`, `dither` | same set | same set | 1:1 |
//!
//! Getting exposure wrong is silent (the picture is merely too dark or too bright), which is
//! why the conversion has its own tests, including a pixel-level one.

use ssx_core::settings::{HdrConfig, TonemapOperator as CoreOperator};
use ssx_hdr::{TonemapOperator, TonemapSettings};

/// Smallest knee handed to the tone mapper. Core accepts `0` ("start rolling off at black"),
/// which `ssx-hdr` rejects; a hundredth of SDR white is indistinguishable in practice.
pub const MIN_KNEE: f32 = 0.01;

/// Converts settings to the tone mapper's configuration. Never fails: values are clamped
/// into what `ssx-hdr` accepts, so a hand-edited config cannot make captures fail (the
/// settings validator reports out-of-range values separately).
pub fn tonemap_settings(cfg: &HdrConfig) -> TonemapSettings {
    let knee = if cfg.knee.is_finite() { cfg.knee.clamp(MIN_KNEE, 1.0) } else { 1.0 };
    let peak = if cfg.peak.is_finite() { cfg.peak.max(knee) } else { 4.0_f32.max(knee) };
    // 2^EV. Clamped to the documented +-10 stops so garbage cannot overflow to inf/0.
    let ev = if cfg.exposure.is_finite() { cfg.exposure.clamp(-10.0, 10.0) } else { 0.0 };
    TonemapSettings {
        operator: match cfg.operator {
            CoreOperator::Clip => TonemapOperator::Clip,
            CoreOperator::ReinhardExtended => TonemapOperator::ReinhardExtended,
            CoreOperator::Bt2390 => TonemapOperator::Bt2390,
            CoreOperator::AcesFit => TonemapOperator::AcesFit,
        },
        peak,
        knee,
        dither: cfg.dither,
        exposure: ev.exp2(),
    }
}

#[cfg(test)]
mod tests {
    use half::f16;
    use ssx_hdr::{srgb_eotf, to_sdr8};
    use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

    use super::*;

    #[test]
    fn exposure_is_converted_from_stops_to_a_multiplier() {
        let at = |ev: f32| {
            tonemap_settings(&HdrConfig { exposure: ev, ..HdrConfig::default() }).exposure
        };
        assert!((at(0.0) - 1.0).abs() < 1e-6, "0 EV is no change");
        assert!((at(1.0) - 2.0).abs() < 1e-6);
        assert!((at(-1.0) - 0.5).abs() < 1e-6);
        assert!((at(3.0) - 8.0).abs() < 1e-5);
        assert!((at(-10.0) - 1.0 / 1024.0).abs() < 1e-9);
        // Beyond the documented range is clamped rather than exploding.
        assert!((at(50.0) - 1024.0).abs() < 1e-3);
        assert!((at(f32::NAN) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn defaults_match_the_tone_mappers_own_defaults() {
        // The faithful preset must be exactly what ssx-hdr calls its default, otherwise the
        // "SDR content is byte-identical" guarantee would depend on which crate built it.
        assert_eq!(tonemap_settings(&HdrConfig::default()), TonemapSettings::default());
    }

    #[test]
    fn knee_peak_operator_and_dither_pass_through() {
        let s = tonemap_settings(&HdrConfig {
            operator: CoreOperator::Bt2390,
            peak: 8.0,
            knee: 0.8,
            dither: false,
            exposure: 0.0,
        });
        assert_eq!(s.operator, TonemapOperator::Bt2390);
        assert!((s.peak - 8.0).abs() < 1e-6 && (s.knee - 0.8).abs() < 1e-6);
        assert!(!s.dither);
        let p = tonemap_settings(&HdrConfig::preserve_highlights());
        assert!((p.knee - 0.8).abs() < 1e-6);
        for (core, hdr) in [
            (CoreOperator::Clip, TonemapOperator::Clip),
            (CoreOperator::ReinhardExtended, TonemapOperator::ReinhardExtended),
            (CoreOperator::AcesFit, TonemapOperator::AcesFit),
        ] {
            let s = tonemap_settings(&HdrConfig { operator: core, ..HdrConfig::default() });
            assert_eq!(s.operator, hdr);
        }
    }

    #[test]
    fn out_of_range_values_are_clamped_into_what_the_tone_mapper_accepts() {
        for cfg in [
            HdrConfig { knee: 0.0, ..HdrConfig::default() },
            HdrConfig { knee: -3.0, ..HdrConfig::default() },
            HdrConfig { knee: 7.0, ..HdrConfig::default() },
            HdrConfig { knee: f32::NAN, peak: f32::INFINITY, ..HdrConfig::default() },
            HdrConfig { peak: 0.1, knee: 1.0, ..HdrConfig::default() },
            HdrConfig { exposure: f32::INFINITY, ..HdrConfig::default() },
        ] {
            let s = tonemap_settings(&cfg);
            assert!(s.validate().is_ok(), "{cfg:?} -> {s:?}");
        }
        assert!(
            (tonemap_settings(&HdrConfig { knee: 0.0, ..HdrConfig::default() }).knee - MIN_KNEE)
                .abs()
                < 1e-9
        );
    }

    /// A 1x1 scRGB frame whose pixel is the linear-light encoding of `rgb` at SDR white
    /// `nits`.
    fn hdr_pixel(rgb: [u8; 3], nits: f32) -> Frame {
        let scale = nits / 80.0;
        let lin = |c: u8| f16::from_f32(srgb_eotf(f32::from(c) / 255.0) * scale);
        let bytes: Vec<u8> = [lin(rgb[0]), lin(rgb[1]), lin(rgb[2]), f16::from_f32(1.0)]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut f = Frame::from_raw(
            Size::new(1, 1),
            8,
            PixelFormat::Rgba16F,
            ColorSpace::ScRgbLinear,
            bytes,
        )
        .unwrap();
        f.sdr_white_nits = Some(nits);
        f
    }

    #[test]
    fn one_stop_of_exposure_halves_or_doubles_linear_light() {
        // Mid grey sRGB 128 -> linear ~0.2158. -1 EV -> ~0.1079 -> sRGB ~ 92. +1 EV -> 0.4316
        // -> sRGB ~ 174. With the *wrong* conversion (EV used as a multiplier) -1 would
        // black out the image and +1 would leave it unchanged.
        let frame = hdr_pixel([128, 128, 128], 200.0);
        let convert = |ev: f32| {
            let s = tonemap_settings(&HdrConfig {
                exposure: ev,
                dither: false,
                ..HdrConfig::default()
            });
            to_sdr8(&frame, &s).unwrap().data()[0]
        };
        assert_eq!(convert(0.0), 128, "0 EV leaves SDR content byte-identical");
        let dark = i32::from(convert(-1.0));
        let bright = i32::from(convert(1.0));
        assert!((dark - 92).abs() <= 1, "-1 EV gave {dark}");
        assert!((bright - 174).abs() <= 2, "+1 EV gave {bright}");
    }
}
