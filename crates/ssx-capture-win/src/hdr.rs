//! HDR state interpretation and capture-format selection (pure logic).
//!
//! # What "HDR active" means here
//!
//! `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` reports "advanced color", which covers *both*
//! HDR and Windows' wide-colour-gamut (WCG) mode. Only HDR changes what the compositor's
//! output looks like to a capture API (linear scRGB `R16G16B16A16_FLOAT`, SDR content
//! parked at the "SDR content brightness" level). The bits are:
//!
//! | bit | field                     |
//! |-----|---------------------------|
//! | 0   | `advancedColorSupported`  |
//! | 1   | `advancedColorEnabled`    |
//! | 2   | `wideColorEnforced`       |
//! | 3   | `advancedColorForceDisabled` |
//!
//! We define **HDR active = `advancedColorEnabled && !wideColorEnforced &&
//! !advancedColorForceDisabled`**: advanced colour is on and Windows is not pinning the
//! display to WCG-only. If `DisplayConfig` cannot be queried we fall back to DXGI's output
//! colour space (`DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` means HDR10 signalling).
//!
//! Getting this wrong is cheap to recover from: the capture format is only a *request*.
//! Frames are labelled from the texture format that actually came back
//! ([`CaptureFormat::from_dxgi`]), so a mis-detected display still yields a correctly
//! labelled (if clipped) frame rather than a mislabelled one.

use ssx_types::{ColorSpace, HdrInfo, PixelFormat};

/// Lowest value of Windows' "SDR content brightness" slider, in nits (raw 1000).
pub(crate) const MIN_SDR_WHITE_NITS: f32 = 80.0;
/// Highest value of the slider in current Windows builds (raw 6000).
pub(crate) const MAX_SDR_WHITE_NITS: f32 = 480.0;

/// Converts `DISPLAYCONFIG_SDR_WHITE_LEVEL::SDRWhiteLevel` to nits.
///
/// The raw value is in thousandths of the 80-nit reference white: `1000` = 80 nits,
/// `2500` = 200 nits. `0` (not reported) maps to the reference white, and the result is
/// clamped to the range the Windows slider can produce so a garbage value can never yield
/// an absurd exposure.
pub(crate) fn sdr_white_nits_from_raw(raw: u32) -> f32 {
    if raw == 0 {
        return MIN_SDR_WHITE_NITS;
    }
    let nits = raw as f32 / 1000.0 * HdrInfo::SCRGB_REFERENCE_WHITE_NITS;
    nits.clamp(MIN_SDR_WHITE_NITS, MAX_SDR_WHITE_NITS)
}

/// The decoded `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` bit field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdvancedColor {
    pub(crate) supported: bool,
    pub(crate) enabled: bool,
    pub(crate) wide_color_enforced: bool,
    pub(crate) force_disabled: bool,
}

impl AdvancedColor {
    /// Decodes the `value` bit field of the Win32 struct.
    pub(crate) fn from_bits(bits: u32) -> Self {
        Self {
            supported: bits & 0b0001 != 0,
            enabled: bits & 0b0010 != 0,
            wide_color_enforced: bits & 0b0100 != 0,
            force_disabled: bits & 0b1000 != 0,
        }
    }

    /// See the module docs for the exact semantic.
    pub(crate) fn hdr_active(self) -> bool {
        self.enabled && !self.wide_color_enforced && !self.force_disabled
    }
}

/// What DXGI's `IDXGIOutput6::GetDesc1` says about an output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DxgiOutputHdr {
    /// The output colour space is HDR10 (ST.2084 / BT.2020).
    pub(crate) hdr10: bool,
    /// Panel peak luminance in nits (`0` or negative when unknown).
    pub(crate) max_luminance_nits: f32,
}

/// Combines the available sources into the final [`HdrInfo`].
///
/// `DisplayConfig` is authoritative for "active" when it answered; DXGI is used only when it
/// did not. `raw_sdr_white` is only consulted while HDR is active (the slider is
/// meaningless otherwise, and the trait contract says inactive displays report 80 nits).
pub(crate) fn build_hdr_info(
    advanced_color: Option<AdvancedColor>,
    raw_sdr_white: Option<u32>,
    dxgi: Option<DxgiOutputHdr>,
) -> HdrInfo {
    let active = match advanced_color {
        Some(ac) => ac.hdr_active(),
        None => dxgi.is_some_and(|d| d.hdr10),
    };
    let max_luminance_nits =
        dxgi.map(|d| d.max_luminance_nits).filter(|n| n.is_finite() && *n > 0.0);
    let sdr_white_nits = if active {
        raw_sdr_white.map_or(MIN_SDR_WHITE_NITS, sdr_white_nits_from_raw)
    } else {
        HdrInfo::SCRGB_REFERENCE_WHITE_NITS
    };
    HdrInfo { active, sdr_white_nits, max_luminance_nits }
}

/// The pixel layouts this backend reads back from the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureFormat {
    /// `B8G8R8A8_UNORM[_SRGB]`.
    Bgra8,
    /// `R8G8B8A8_UNORM[_SRGB]`.
    Rgba8,
    /// `R16G16B16A16_FLOAT`, linear scRGB.
    Rgba16F,
}

/// Numeric `DXGI_FORMAT` values (kept numeric so the mapping is testable off Windows;
/// a `cfg(windows)` test checks them against the `windows` crate).
pub(crate) mod dxgi_format {
    pub(crate) const R16G16B16A16_FLOAT: i32 = 10;
    pub(crate) const R8G8B8A8_UNORM: i32 = 28;
    pub(crate) const R8G8B8A8_UNORM_SRGB: i32 = 29;
    pub(crate) const B8G8R8A8_UNORM: i32 = 87;
    pub(crate) const B8G8R8A8_UNORM_SRGB: i32 = 91;
}

impl CaptureFormat {
    /// The format to *request* for a monitor: float scRGB when HDR is active, else 8-bit.
    pub(crate) fn for_hdr(hdr: Option<HdrInfo>) -> Self {
        if hdr.is_some_and(|h| h.active) { CaptureFormat::Rgba16F } else { CaptureFormat::Bgra8 }
    }

    /// Maps the format of a texture that actually came back from the GPU.
    pub(crate) fn from_dxgi(format: i32) -> Option<Self> {
        match format {
            dxgi_format::R16G16B16A16_FLOAT => Some(CaptureFormat::Rgba16F),
            dxgi_format::B8G8R8A8_UNORM | dxgi_format::B8G8R8A8_UNORM_SRGB => {
                Some(CaptureFormat::Bgra8)
            }
            dxgi_format::R8G8B8A8_UNORM | dxgi_format::R8G8B8A8_UNORM_SRGB => {
                Some(CaptureFormat::Rgba8)
            }
            _ => None,
        }
    }

    pub(crate) fn pixel_format(self) -> PixelFormat {
        match self {
            CaptureFormat::Bgra8 => PixelFormat::Bgra8,
            CaptureFormat::Rgba8 => PixelFormat::Rgba8,
            CaptureFormat::Rgba16F => PixelFormat::Rgba16F,
        }
    }

    pub(crate) fn color_space(self) -> ColorSpace {
        match self {
            CaptureFormat::Bgra8 | CaptureFormat::Rgba8 => ColorSpace::Srgb,
            CaptureFormat::Rgba16F => ColorSpace::ScRgbLinear,
        }
    }

    pub(crate) fn bytes_per_pixel(self) -> usize {
        self.pixel_format().bytes_per_pixel()
    }

    /// `Frame::sdr_white_nits` for a frame of this format: set (as the standards
    /// require) for float frames, absent for 8-bit ones. A float frame from a display that
    /// is not reported as HDR-active is scRGB with SDR white at the 80-nit reference.
    pub(crate) fn frame_sdr_white_nits(self, hdr: Option<HdrInfo>) -> Option<f32> {
        match self {
            CaptureFormat::Rgba16F => Some(match hdr {
                Some(h) if h.active => h.sdr_white_nits,
                _ => HdrInfo::SCRGB_REFERENCE_WHITE_NITS,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_white_conversion_matches_windows_slider() {
        assert!((sdr_white_nits_from_raw(1000) - 80.0).abs() < 1e-4);
        assert!((sdr_white_nits_from_raw(2500) - 200.0).abs() < 1e-4);
        assert!((sdr_white_nits_from_raw(6000) - 480.0).abs() < 1e-4);
    }

    #[test]
    fn sdr_white_clamps_and_handles_unset() {
        assert!((sdr_white_nits_from_raw(0) - 80.0).abs() < 1e-4, "unset -> reference white");
        assert!((sdr_white_nits_from_raw(1) - 80.0).abs() < 1e-4, "below range clamps up");
        assert!((sdr_white_nits_from_raw(999) - 80.0).abs() < 1e-4);
        assert!((sdr_white_nits_from_raw(6001) - 480.0).abs() < 1e-4, "above range clamps down");
        assert!((sdr_white_nits_from_raw(u32::MAX) - 480.0).abs() < 1e-4);
    }

    #[test]
    fn sdr_white_is_monotonic_and_finite() {
        let mut prev = 0.0;
        for raw in (0..=8000).step_by(50) {
            let n = sdr_white_nits_from_raw(raw);
            assert!(n.is_finite() && n >= prev, "raw {raw}");
            prev = n;
        }
    }

    #[test]
    fn advanced_color_bits_truth_table() {
        // (bits, supported, enabled, wide, forced, hdr_active)
        let cases = [
            (0b0000, false, false, false, false, false),
            (0b0001, true, false, false, false, false), // supported but off
            (0b0011, true, true, false, false, true),   // HDR on
            (0b0111, true, true, true, false, false),   // WCG only
            (0b1011, true, true, false, true, false),   // force disabled wins
            (0b0010, false, true, false, false, true),  // enabled bit alone
            (0b1111, true, true, true, true, false),
        ];
        for (bits, s, e, w, f, active) in cases {
            let ac = AdvancedColor::from_bits(bits);
            assert_eq!(
                (ac.supported, ac.enabled, ac.wide_color_enforced, ac.force_disabled),
                (s, e, w, f),
                "bits {bits:#06b}"
            );
            assert_eq!(ac.hdr_active(), active, "bits {bits:#06b}");
        }
    }

    #[test]
    fn advanced_color_ignores_higher_bits() {
        // Newer Windows may define more bits; they must not flip our decision.
        assert!(AdvancedColor::from_bits(0xFFFF_FF03).hdr_active());
    }

    #[test]
    fn hdr_info_from_display_config() {
        let ac = AdvancedColor::from_bits(0b0011);
        let info = build_hdr_info(
            Some(ac),
            Some(2500),
            Some(DxgiOutputHdr { hdr10: true, max_luminance_nits: 1000.0 }),
        );
        assert!(info.active);
        assert!((info.sdr_white_nits - 200.0).abs() < 1e-4);
        assert_eq!(info.max_luminance_nits, Some(1000.0));
    }

    #[test]
    fn inactive_display_reports_reference_white_regardless_of_slider() {
        let ac = AdvancedColor::from_bits(0b0001);
        let info = build_hdr_info(Some(ac), Some(4000), None);
        assert!(!info.active);
        assert!((info.sdr_white_nits - 80.0).abs() < 1e-4);
        assert_eq!(info.max_luminance_nits, None);
    }

    #[test]
    fn display_config_beats_dxgi_when_both_answer() {
        let ac = AdvancedColor::from_bits(0b0001);
        let info = build_hdr_info(
            Some(ac),
            None,
            Some(DxgiOutputHdr { hdr10: true, max_luminance_nits: 600.0 }),
        );
        assert!(!info.active);
        assert_eq!(info.max_luminance_nits, Some(600.0), "luminance still comes from DXGI");
    }

    #[test]
    fn dxgi_is_the_fallback_when_display_config_is_unavailable() {
        let hdr = DxgiOutputHdr { hdr10: true, max_luminance_nits: 0.0 };
        let info = build_hdr_info(None, None, Some(hdr));
        assert!(info.active);
        assert!((info.sdr_white_nits - 80.0).abs() < 1e-4, "no slider value -> reference");
        assert_eq!(info.max_luminance_nits, None, "zero luminance means unknown");
        assert_eq!(build_hdr_info(None, None, None), HdrInfo::SDR);
    }

    #[test]
    fn hdr_active_without_slider_value_falls_back_to_reference_white() {
        let ac = AdvancedColor::from_bits(0b0011);
        assert!((build_hdr_info(Some(ac), None, None).sdr_white_nits - 80.0).abs() < 1e-4);
    }

    #[test]
    fn nan_luminance_is_dropped() {
        let hdr = DxgiOutputHdr { hdr10: false, max_luminance_nits: f32::NAN };
        assert_eq!(build_hdr_info(None, None, Some(hdr)).max_luminance_nits, None);
    }

    #[test]
    fn requested_format_follows_hdr_state() {
        let hdr = HdrInfo { active: true, sdr_white_nits: 200.0, max_luminance_nits: None };
        assert_eq!(CaptureFormat::for_hdr(Some(hdr)), CaptureFormat::Rgba16F);
        assert_eq!(CaptureFormat::for_hdr(Some(HdrInfo::SDR)), CaptureFormat::Bgra8);
        assert_eq!(CaptureFormat::for_hdr(None), CaptureFormat::Bgra8);
    }

    #[test]
    fn returned_texture_formats_are_mapped() {
        assert_eq!(CaptureFormat::from_dxgi(10), Some(CaptureFormat::Rgba16F));
        assert_eq!(CaptureFormat::from_dxgi(87), Some(CaptureFormat::Bgra8));
        assert_eq!(CaptureFormat::from_dxgi(91), Some(CaptureFormat::Bgra8));
        assert_eq!(CaptureFormat::from_dxgi(28), Some(CaptureFormat::Rgba8));
        assert_eq!(CaptureFormat::from_dxgi(29), Some(CaptureFormat::Rgba8));
        assert_eq!(CaptureFormat::from_dxgi(24), None, "R10G10B10A2 is not handled");
        assert_eq!(CaptureFormat::from_dxgi(0), None);
    }

    #[test]
    fn frame_labels_follow_the_format() {
        assert_eq!(CaptureFormat::Rgba16F.pixel_format(), PixelFormat::Rgba16F);
        assert_eq!(CaptureFormat::Rgba16F.color_space(), ColorSpace::ScRgbLinear);
        assert_eq!(CaptureFormat::Rgba16F.bytes_per_pixel(), 8);
        assert_eq!(CaptureFormat::Bgra8.pixel_format(), PixelFormat::Bgra8);
        assert_eq!(CaptureFormat::Bgra8.color_space(), ColorSpace::Srgb);
        assert_eq!(CaptureFormat::Rgba8.pixel_format(), PixelFormat::Rgba8);
        assert_eq!(CaptureFormat::Rgba8.bytes_per_pixel(), 4);
    }

    #[test]
    fn sdr_white_is_set_only_for_float_frames() {
        let hdr = HdrInfo { active: true, sdr_white_nits: 240.0, max_luminance_nits: None };
        assert_eq!(CaptureFormat::Rgba16F.frame_sdr_white_nits(Some(hdr)), Some(240.0));
        assert_eq!(CaptureFormat::Rgba16F.frame_sdr_white_nits(Some(HdrInfo::SDR)), Some(80.0));
        assert_eq!(CaptureFormat::Rgba16F.frame_sdr_white_nits(None), Some(80.0));
        assert_eq!(CaptureFormat::Bgra8.frame_sdr_white_nits(Some(hdr)), None);
    }

    #[cfg(windows)]
    #[test]
    fn dxgi_format_constants_match_the_windows_crate() {
        use windows::Win32::Graphics::Dxgi::Common as c;
        assert_eq!(dxgi_format::R16G16B16A16_FLOAT, c::DXGI_FORMAT_R16G16B16A16_FLOAT.0);
        assert_eq!(dxgi_format::R8G8B8A8_UNORM, c::DXGI_FORMAT_R8G8B8A8_UNORM.0);
        assert_eq!(dxgi_format::R8G8B8A8_UNORM_SRGB, c::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB.0);
        assert_eq!(dxgi_format::B8G8R8A8_UNORM, c::DXGI_FORMAT_B8G8R8A8_UNORM.0);
        assert_eq!(dxgi_format::B8G8R8A8_UNORM_SRGB, c::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.0);
    }
}
