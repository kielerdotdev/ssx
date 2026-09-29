//! Internal error type of the Windows backend.
//!
//! The public [`CaptureError`] is deliberately coarse (its `Backend` variant carries a
//! string). The fallback chain, however, must *decide* based on what went wrong: a
//! minimised window must not silently fall back to "grab the monitor and crop" (that would
//! capture whatever is on top), while a lost GPU device deserves one retry. Those decisions
//! need a typed error, so this crate uses [`WinError`] internally and converts at the trait
//! boundary. It contains no `windows` crate types so the decision logic is unit-testable
//! on any platform.

use std::time::Duration;

use ssx_capture::CaptureError;
use ssx_types::{FrameError, Rect};

/// Name reported in [`CaptureError::Backend`] for errors raised by this crate.
pub(crate) const BACKEND_NAME: &str = "windows-wgc";

/// HRESULT values that the backend reacts to. Kept numerically (rather than importing the
/// `windows` constants) so they can be used, and tested, on every platform; a
/// `cfg(windows)` test asserts they equal the `windows` crate's constants.
pub(crate) mod hresult {
    pub(crate) const E_ACCESSDENIED: i32 = 0x8007_0005_u32.cast_signed();
    pub(crate) const E_INVALIDARG: i32 = 0x8007_0057_u32.cast_signed();
    pub(crate) const DXGI_ERROR_UNSUPPORTED: i32 = 0x887A_0004_u32.cast_signed();
    pub(crate) const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005_u32.cast_signed();
    pub(crate) const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006_u32.cast_signed();
    pub(crate) const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007_u32.cast_signed();
    pub(crate) const DXGI_ERROR_DRIVER_INTERNAL_ERROR: i32 = 0x887A_0020_u32.cast_signed();
    pub(crate) const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: i32 = 0x887A_0022_u32.cast_signed();
    pub(crate) const DXGI_ERROR_MODE_CHANGE_IN_PROGRESS: i32 = 0x887A_0025_u32.cast_signed();
    pub(crate) const DXGI_ERROR_ACCESS_LOST: i32 = 0x887A_0026_u32.cast_signed();
    pub(crate) const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887A_0027_u32.cast_signed();
    pub(crate) const DXGI_ERROR_SESSION_DISCONNECTED: i32 = 0x887A_0028_u32.cast_signed();
    pub(crate) const DXGI_ERROR_ACCESS_DENIED: i32 = 0x887A_002B_u32.cast_signed();

    /// The GPU device must be re-created before retrying.
    pub(crate) fn is_device_lost(code: i32) -> bool {
        matches!(
            code,
            DXGI_ERROR_DEVICE_REMOVED
                | DXGI_ERROR_DEVICE_HUNG
                | DXGI_ERROR_DEVICE_RESET
                | DXGI_ERROR_DRIVER_INTERNAL_ERROR
        )
    }

    /// An actionable hint appended to error messages, or `""`.
    pub(crate) fn hint(code: i32) -> &'static str {
        match code {
            E_ACCESSDENIED | DXGI_ERROR_ACCESS_DENIED => {
                " (access denied: the target may be elevated/protected, or the secure desktop \
                 such as UAC or the lock screen is showing)"
            }
            E_INVALIDARG => {
                " (invalid argument: the window may be protected, not a top-level window, or \
                 already gone)"
            }
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
                " (too many desktop duplications are active, or duplication is unavailable in \
                 this session)"
            }
            DXGI_ERROR_SESSION_DISCONNECTED => " (the remote/console session is disconnected)",
            DXGI_ERROR_MODE_CHANGE_IN_PROGRESS => " (a display mode change is in progress)",
            DXGI_ERROR_UNSUPPORTED => " (not supported by this GPU/driver/Windows version)",
            _ => "",
        }
    }
}

/// Everything that can go wrong inside the Windows backend.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WinError {
    /// The capturer cannot handle this request (e.g. DDA asked to capture a window).
    #[error("{0} is not supported")]
    Unsupported(&'static str),
    #[error("no monitor or window with id {0:?}")]
    NotFound(String),
    #[error("region {0:?} is empty or outside every monitor")]
    InvalidRegion(Rect),
    #[error("the window is minimised; restore it before capturing")]
    Minimized,
    #[error(
        "the window's content is protected (it excludes itself from capture or uses DRM), \
         so Windows will not hand out its pixels"
    )]
    Protected,
    #[error(
        "the capture target went away (window closed or display removed) before a frame arrived"
    )]
    SourceClosed,
    #[error("no frame arrived within {0:?}")]
    Timeout(Duration),
    #[error("the GPU device was lost or reset: {0}")]
    DeviceLost(String),
    /// A failed Windows API call. `message` already includes any actionable hint.
    #[error("{context} failed: {message} (HRESULT {code:#010x})")]
    Api { context: &'static str, code: i32, message: String },
    #[error("{0}")]
    Other(String),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

impl WinError {
    /// Builds an [`WinError::Api`] (or [`WinError::DeviceLost`] when the HRESULT says the
    /// GPU device is gone) from a failed call.
    pub(crate) fn api(context: &'static str, code: i32, message: impl Into<String>) -> Self {
        let message = message.into();
        if hresult::is_device_lost(code) {
            return WinError::DeviceLost(format!("{context}: {message}"));
        }
        let message = format!("{}{}", message.trim_end(), hresult::hint(code));
        WinError::Api { context, code, message }
    }
}

impl From<WinError> for CaptureError {
    fn from(e: WinError) -> Self {
        match e {
            WinError::NotFound(id) => CaptureError::NotFound(id),
            WinError::InvalidRegion(r) => CaptureError::InvalidRegion(r),
            WinError::Frame(f) => CaptureError::Frame(f),
            WinError::Unsupported(what) => CaptureError::unsupported(BACKEND_NAME, what),
            other => CaptureError::backend(BACKEND_NAME, other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_lost_codes_are_recognised() {
        for code in [
            hresult::DXGI_ERROR_DEVICE_REMOVED,
            hresult::DXGI_ERROR_DEVICE_HUNG,
            hresult::DXGI_ERROR_DEVICE_RESET,
            hresult::DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        ] {
            assert!(hresult::is_device_lost(code), "{code:#x}");
            assert!(matches!(WinError::api("x", code, "boom"), WinError::DeviceLost(_)));
        }
        assert!(!hresult::is_device_lost(hresult::DXGI_ERROR_ACCESS_LOST));
        assert!(!hresult::is_device_lost(0));
    }

    #[test]
    fn api_error_carries_hint_and_code() {
        let e =
            WinError::api("CreateForWindow", hresult::E_INVALIDARG, "The parameter is incorrect.");
        assert!(matches!(e, WinError::Api { code, .. } if code == hresult::E_INVALIDARG));
        let text = e.to_string();
        assert!(text.contains("CreateForWindow failed"), "{text}");
        assert!(text.contains("protected"), "{text}");
        assert!(text.contains("0x80070057"), "{text}");
    }

    #[test]
    fn converts_to_the_public_error_variants() {
        assert!(matches!(
            CaptureError::from(WinError::NotFound("a".into())),
            CaptureError::NotFound(id) if id == "a"
        ));
        let r = Rect::new(1, 2, 3, 4);
        assert!(
            matches!(CaptureError::from(WinError::InvalidRegion(r)), CaptureError::InvalidRegion(x) if x == r)
        );
        assert!(matches!(
            CaptureError::from(WinError::Unsupported("windows")),
            CaptureError::Unsupported { .. }
        ));
        let e = CaptureError::from(WinError::Protected);
        assert!(
            matches!(&e, CaptureError::Backend { message, .. } if message.contains("protected"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn hresult_constants_match_the_windows_crate() {
        use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG};
        use windows::Win32::Graphics::Dxgi as d;
        assert_eq!(hresult::E_ACCESSDENIED, E_ACCESSDENIED.0);
        assert_eq!(hresult::E_INVALIDARG, E_INVALIDARG.0);
        assert_eq!(hresult::DXGI_ERROR_UNSUPPORTED, d::DXGI_ERROR_UNSUPPORTED.0);
        assert_eq!(hresult::DXGI_ERROR_DEVICE_REMOVED, d::DXGI_ERROR_DEVICE_REMOVED.0);
        assert_eq!(hresult::DXGI_ERROR_DEVICE_HUNG, d::DXGI_ERROR_DEVICE_HUNG.0);
        assert_eq!(hresult::DXGI_ERROR_DEVICE_RESET, d::DXGI_ERROR_DEVICE_RESET.0);
        assert_eq!(
            hresult::DXGI_ERROR_DRIVER_INTERNAL_ERROR,
            d::DXGI_ERROR_DRIVER_INTERNAL_ERROR.0
        );
        assert_eq!(
            hresult::DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            d::DXGI_ERROR_NOT_CURRENTLY_AVAILABLE.0
        );
        assert_eq!(
            hresult::DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
            d::DXGI_ERROR_MODE_CHANGE_IN_PROGRESS.0
        );
        assert_eq!(hresult::DXGI_ERROR_ACCESS_LOST, d::DXGI_ERROR_ACCESS_LOST.0);
        assert_eq!(hresult::DXGI_ERROR_WAIT_TIMEOUT, d::DXGI_ERROR_WAIT_TIMEOUT.0);
        assert_eq!(hresult::DXGI_ERROR_SESSION_DISCONNECTED, d::DXGI_ERROR_SESSION_DISCONNECTED.0);
        assert_eq!(hresult::DXGI_ERROR_ACCESS_DENIED, d::DXGI_ERROR_ACCESS_DENIED.0);
    }
}
