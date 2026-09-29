//! User-facing tonemapping settings and the crate error type.

use serde::{Deserialize, Serialize};
use ssx_types::{ColorSpace, PixelFormat};

/// Errors from HDR → SDR conversion.
#[derive(Debug, thiserror::Error)]
pub enum HdrError {
    /// A [`TonemapSettings`] field is out of range.
    #[error("invalid tonemap setting `{field}`: {reason}")]
    InvalidSetting {
        /// Name of the offending field.
        field: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The frame's `sdr_white_nits` is not a positive finite number.
    #[error("invalid SDR white level {0} nits; expected a positive finite value")]
    InvalidSdrWhite(f32),
    /// The frame's pixel format / colour space combination cannot be converted.
    #[error(
        "cannot convert a {0:?} frame in {1:?} colour space to SDR; \
         expected Rgba16F/ScRgbLinear (HDR) or 8-bit sRGB (passed through)"
    )]
    UnsupportedFrame(PixelFormat, ColorSpace),
    /// Building the output frame failed (indicates an internal size mismatch).
    #[error("output frame construction failed: {0}")]
    Frame(#[from] ssx_types::FrameError),
}

/// Highlight roll-off operator. See the crate docs for the exact math of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TonemapOperator {
    /// Per-channel clamp to SDR white: the "exact SDR look" of a naive conversion. Blows
    /// highlights out to white and shifts the hue of bright saturated colours.
    Clip,
    /// Extended Reinhard with a white point, attached to the knee. The default.
    #[default]
    ReinhardExtended,
    /// ITU-R BT.2390 EETF Hermite spline shoulder.
    Bt2390,
    /// Narkowicz's fit of the ACES filmic curve, used as the shoulder.
    AcesFit,
}

/// Tonemapping parameters for [`to_sdr8`](crate::to_sdr8).
///
/// All brightness values are in **multiples of SDR white** (so `4.0` is 800 nits when the
/// user's SDR white is 200 nits).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TonemapSettings {
    /// Highlight operator.
    pub operator: TonemapOperator,
    /// Brightest content value to preserve. Values above are clipped after roll-off.
    pub peak: f32,
    /// Where roll-off begins. Everything at or below the knee is passed through
    /// untouched; the default `1.0` keeps all SDR-range content byte-exact. See the crate
    /// docs for why a knee below `1.0` is required to get a visible shoulder.
    pub knee: f32,
    /// Apply deterministic dither before 8-bit quantisation (never touches pixels that
    /// are already exactly representable, see the crate docs).
    pub dither: bool,
    /// Linear-light exposure multiplier applied before tonemapping.
    pub exposure: f32,
}

impl Default for TonemapSettings {
    fn default() -> Self {
        Self {
            operator: TonemapOperator::default(),
            peak: 4.0,
            knee: 1.0,
            dither: true,
            exposure: 1.0,
        }
    }
}

impl TonemapSettings {
    /// Checks that every field is finite and in range: `exposure > 0`, `knee > 0` and
    /// `peak >= knee`.
    pub fn validate(&self) -> Result<(), HdrError> {
        let bad = |field, reason| Err(HdrError::InvalidSetting { field, reason });
        if !self.exposure.is_finite() {
            return bad("exposure", "must be finite");
        }
        if self.exposure <= 0.0 {
            return bad("exposure", "must be greater than zero");
        }
        if !self.knee.is_finite() {
            return bad("knee", "must be finite");
        }
        if self.knee <= 0.0 {
            return bad("knee", "must be greater than zero");
        }
        if !self.peak.is_finite() {
            return bad("peak", "must be finite");
        }
        if self.peak < self.knee {
            return bad("peak", "must be at least as large as `knee`");
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests assert bit-exact pass-through on purpose
mod tests {
    use super::*;

    fn field(r: Result<(), HdrError>) -> Option<&'static str> {
        match r {
            Err(HdrError::InvalidSetting { field, .. }) => Some(field),
            _ => None,
        }
    }

    #[test]
    fn default_is_valid() {
        assert!(TonemapSettings::default().validate().is_ok());
        assert_eq!(TonemapSettings::default().operator, TonemapOperator::ReinhardExtended);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let ok = TonemapSettings::default();
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert_eq!(field(TonemapSettings { exposure: v, ..ok }.validate()), Some("exposure"));
            assert_eq!(field(TonemapSettings { knee: v, ..ok }.validate()), Some("knee"));
        }
        for v in [f32::NAN, f32::INFINITY, 0.5] {
            assert_eq!(field(TonemapSettings { peak: v, ..ok }.validate()), Some("peak"));
        }
        // peak == knee is allowed (a pure clip at the knee).
        assert!(TonemapSettings { peak: 1.0, knee: 1.0, ..ok }.validate().is_ok());
        assert!(TonemapSettings { peak: 8.0, knee: 0.5, exposure: 0.25, ..ok }.validate().is_ok());
    }

    #[test]
    fn serde_round_trip_and_names() {
        let s = TonemapSettings {
            operator: TonemapOperator::AcesFit,
            peak: 6.0,
            knee: 0.75,
            dither: false,
            exposure: 1.5,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"aces-fit\""), "{json}");
        assert_eq!(serde_json::from_str::<TonemapSettings>(&json).unwrap(), s);

        for (op, name) in [
            (TonemapOperator::Clip, "clip"),
            (TonemapOperator::ReinhardExtended, "reinhard-extended"),
            (TonemapOperator::Bt2390, "bt2390"),
            (TonemapOperator::AcesFit, "aces-fit"),
        ] {
            let j = serde_json::to_string(&op).unwrap();
            assert_eq!(j, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<TonemapOperator>(&j).unwrap(), op);
        }
    }

    #[test]
    fn partial_json_uses_defaults() {
        let s: TonemapSettings = serde_json::from_str(r#"{"operator":"clip"}"#).unwrap();
        assert_eq!(s.operator, TonemapOperator::Clip);
        assert_eq!(s.peak, TonemapSettings::default().peak);
        assert!(serde_json::from_str::<TonemapOperator>("\"reinhard\"").is_err());
    }
}
