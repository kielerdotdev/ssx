//! Descriptions of displays and windows as reported by a capture backend.

use serde::{Deserialize, Serialize};

use crate::geometry::Rect;

/// High-dynamic-range state of a display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HdrInfo {
    /// `true` if the OS is currently compositing this display in an HDR mode
    /// ("Advanced Color" on Windows).
    pub active: bool,
    /// Luminance, in nits, that SDR white is mapped to while HDR is active. On Windows
    /// this is the "SDR content brightness" slider (80 nits at 0%, ~200 typical).
    pub sdr_white_nits: f32,
    /// Peak luminance the panel reports, if known.
    pub max_luminance_nits: Option<f32>,
}

impl HdrInfo {
    /// The reference SDR white of scRGB: 80 nits.
    pub const SCRGB_REFERENCE_WHITE_NITS: f32 = 80.0;

    /// A display in plain SDR mode.
    pub const SDR: HdrInfo = HdrInfo {
        active: false,
        sdr_white_nits: Self::SCRGB_REFERENCE_WHITE_NITS,
        max_luminance_nits: None,
    };

    /// scRGB value that SDR white sits at (e.g. 2.5 for 200 nits).
    pub fn sdr_white_scrgb(self) -> f32 {
        self.sdr_white_nits / Self::SCRGB_REFERENCE_WHITE_NITS
    }
}

/// A physical display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    /// Stable-for-the-session identifier, opaque to callers (backend-specific).
    pub id: String,
    /// Human-readable name ("DELL U2723QE", "eDP-1", …).
    pub name: String,
    /// Bounds on the virtual desktop in **physical pixels**.
    pub rect: Rect,
    /// UI scale factor (1.0, 1.25, 2.0, …).
    pub scale_factor: f64,
    pub primary: bool,
    /// Refresh rate in hertz, if known.
    pub refresh_hz: Option<f32>,
    /// `Some` when the backend can report HDR state (even if inactive).
    pub hdr: Option<HdrInfo>,
}

/// A top-level window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Opaque backend-specific identifier (HWND, X11 window id, toplevel handle, …).
    pub id: String,
    pub title: String,
    /// Executable / app name if known.
    pub app_name: Option<String>,
    /// Bounds on the virtual desktop in physical pixels.
    pub rect: Rect,
    pub minimized: bool,
    pub focused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_white_scrgb_matches_windows_convention() {
        let hdr = HdrInfo { active: true, sdr_white_nits: 200.0, max_luminance_nits: Some(1000.0) };
        assert!((hdr.sdr_white_scrgb() - 2.5).abs() < 1e-6);
        assert!((HdrInfo::SDR.sdr_white_scrgb() - 1.0).abs() < 1e-6);
    }
}
