//! Backend-agnostic screen-capture API.
//!
//! Each OS/compositor gets a backend crate (`ssx-capture-win`, `ssx-capture-x11`,
//! `ssx-capture-wayland`, …) that implements [`CaptureBackend`]. Backends return frames
//! in their **native format** — for example `Rgba16F` scRGB on an HDR Windows display.
//! Colour conversion (HDR → SDR) and multi-monitor compositing are done by the layer
//! above (`ssx-platform`), so that a mixed HDR + SDR setup is tonemapped per monitor
//! *before* being stitched together.

mod composite;
mod error;

pub use composite::{CompositeError, blit, composite};
pub use error::{CaptureError, Result};

use ssx_types::{Frame, Monitor, Rect, WindowInfo};

/// What a backend can do. Callers use this to pick UI (e.g. hide "window capture" on a
/// compositor that cannot enumerate windows).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// [`CaptureBackend::monitors`] works.
    pub enumerate_monitors: bool,
    /// [`CaptureBackend::windows`] works.
    pub enumerate_windows: bool,
    /// [`CaptureBackend::capture_window`] works.
    pub capture_windows: bool,
    /// `CaptureOptions::include_cursor` is honoured.
    pub cursor: bool,
    /// May return `Rgba16F` scRGB frames for HDR displays.
    pub hdr_float: bool,
    /// One call ([`CaptureBackend::capture_desktop`]) captures the whole virtual desktop
    /// natively, so per-monitor stitching is unnecessary.
    pub native_desktop: bool,
    /// The OS may show a picker / permission prompt for each capture (xdg portals).
    pub needs_user_interaction: bool,
}

/// Per-capture options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    pub include_cursor: bool,
}

/// A screen-capture implementation for one OS API / compositor protocol.
///
/// Implementations must be usable from any thread. All coordinates are physical pixels on
/// the virtual desktop. Methods block until the capture is complete.
pub trait CaptureBackend: Send + Sync + std::fmt::Debug {
    /// Short stable name, e.g. `"windows-wgc"`, `"x11"`, `"wlr-screencopy"`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Lists displays. Backends that cannot enumerate (e.g. the xdg portal) return
    /// [`CaptureError::Unsupported`].
    fn monitors(&self) -> Result<Vec<Monitor>>;

    /// Lists top-level windows (front-to-back order where the platform provides it).
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        Err(CaptureError::unsupported(self.name(), "window enumeration"))
    }

    /// Captures one monitor by [`Monitor::id`]. The returned frame's `origin`,
    /// `scale_factor` and (for HDR) `sdr_white_nits` must be filled in.
    fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> Result<Frame>;

    /// Captures the whole virtual desktop in one go. Only meaningful when
    /// [`Capabilities::native_desktop`] is set; the default reports `Unsupported`.
    fn capture_desktop(&self, _opts: &CaptureOptions) -> Result<Frame> {
        Err(CaptureError::unsupported(self.name(), "native desktop capture"))
    }

    /// Captures a single window by [`WindowInfo::id`].
    fn capture_window(&self, _window_id: &str, _opts: &CaptureOptions) -> Result<Frame> {
        Err(CaptureError::unsupported(self.name(), "window capture"))
    }

    /// Captures a rectangle of the virtual desktop. The default captures every monitor
    /// intersecting the region and stitches them (or uses `capture_desktop`), then crops.
    /// Backends with a cheaper way (e.g. X11 `GetImage` on a sub-rectangle) should override.
    fn capture_region(&self, region: Rect, opts: &CaptureOptions) -> Result<Frame> {
        default_capture_region(self, region, opts)
    }
}

/// Shared implementation of [`CaptureBackend::capture_region`]. Only valid when the
/// backend's frames are already directly composable (same pixel format on every monitor);
/// `ssx-platform` uses its own tonemap-aware path for mixed HDR/SDR setups.
pub fn default_capture_region<B: CaptureBackend + ?Sized>(
    backend: &B,
    region: Rect,
    opts: &CaptureOptions,
) -> Result<Frame> {
    if region.is_empty() {
        return Err(CaptureError::InvalidRegion(region));
    }
    let desktop = if backend.capabilities().native_desktop {
        backend.capture_desktop(opts)?
    } else {
        let mut frames = Vec::new();
        for m in backend.monitors()? {
            if m.rect.intersect(region).is_some() {
                frames.push(backend.capture_monitor(&m.id, opts)?);
            }
        }
        if frames.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        composite(&frames)?
    };
    desktop.crop_desktop(region).map_err(|_| CaptureError::InvalidRegion(region))
}
