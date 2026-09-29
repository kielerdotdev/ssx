//! The [`WindowsCapture`] backend: wires the capturers into a fallback chain and
//! implements [`CaptureBackend`]. Which stage is tried when, and when to stop, is decided in
//! `chain.rs`; this file only assembles the stages.

use std::sync::Arc;

use ssx_capture::{
    Capabilities, CaptureBackend, CaptureError, CaptureOptions, Result as CaptureResult,
};
use ssx_types::{Frame, Monitor, WindowInfo};

use crate::{
    chain::{MonitorCapturer, capture_window_with_fallback, capture_with_chain},
    dda::DdaStage,
    display,
    error::WinError,
    gdi::GdiStage,
    geometry::window_crop_target,
    wgc::{Wgc, WgcStage},
    windows::{self as top_level, WindowTarget},
};

/// The Windows capture backend.
///
/// Monitor capture tries Windows.Graphics.Capture, then DXGI Desktop Duplication, then GDI
/// (see the crate docs); window capture uses WGC and falls back to capturing the monitor
/// the window is on and cropping to the window's visible bounds. That fallback **includes
/// anything overlapping the window** (other windows, the cursor if requested) because it
/// reads the composed desktop.
///
/// HDR monitors produce `Rgba16F` / `ScRgbLinear` frames with `sdr_white_nits` set;
/// tonemapping is the caller's job (see `ssx-hdr`). The process should be
/// per-monitor-DPI-aware; [`WindowsCapture::new`] calls
/// [`ensure_per_monitor_dpi_aware`](crate::ensure_per_monitor_dpi_aware) best-effort.
pub struct WindowsCapture {
    wgc: Arc<Wgc>,
    wgc_supported: bool,
    chain: Vec<Box<dyn MonitorCapturer>>,
}

impl std::fmt::Debug for WindowsCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsCapture")
            .field("wgc_supported", &self.wgc_supported)
            .field("stages", &self.chain.iter().map(|s| s.name()).collect::<Vec<_>>())
            .finish()
    }
}

impl WindowsCapture {
    /// Creates the backend.
    ///
    /// # Errors
    /// [`CaptureError::NoBackend`] if the session has no usable display at all (for
    /// example a service running in session 0).
    pub fn new() -> Result<Self, CaptureError> {
        if let Err(e) = display::ensure_per_monitor_dpi_aware() {
            tracing::warn!(error = %e, "process is not per-monitor DPI aware");
        }
        crate::sys::ensure_com();
        display::enumerate_targets().map_err(|e| CaptureError::NoBackend(e.to_string()))?;

        let wgc = Arc::new(Wgc::new());
        let wgc_supported = Wgc::is_supported();
        let mut chain: Vec<Box<dyn MonitorCapturer>> = Vec::with_capacity(3);
        if wgc_supported {
            chain.push(Box::new(WgcStage(Arc::clone(&wgc))));
        } else {
            tracing::info!(
                "Windows.Graphics.Capture is not supported here; using desktop duplication"
            );
        }
        chain.push(Box::new(DdaStage));
        chain.push(Box::new(GdiStage));
        Ok(Self { wgc, wgc_supported, chain })
    }

    /// Fallback for window capture: capture the monitor with most of the window on it and
    /// crop to the window's bounds.
    fn capture_window_via_monitor(
        &self,
        window: &WindowTarget,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        let targets = display::enumerate_targets()?;
        let rects: Vec<_> = targets.iter().map(|t| t.monitor.rect).collect();
        let (index, crop) = window_crop_target(window.bounds, &rects)
            .ok_or(WinError::InvalidRegion(window.bounds))?;
        let target = targets.get(index).ok_or(WinError::InvalidRegion(window.bounds))?;
        let frame = capture_with_chain(&self.chain, target, opts)?;
        frame.crop_desktop(crop).map_err(WinError::Frame)
    }
}

impl CaptureBackend for WindowsCapture {
    fn name(&self) -> &'static str {
        "windows-wgc"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            enumerate_monitors: true,
            enumerate_windows: true,
            capture_windows: true,
            cursor: true,
            hdr_float: true,
            native_desktop: false,
            needs_user_interaction: false,
        }
    }

    fn monitors(&self) -> CaptureResult<Vec<Monitor>> {
        let targets = display::enumerate_targets()?;
        Ok(targets.into_iter().map(|t| t.monitor).collect())
    }

    fn windows(&self) -> CaptureResult<Vec<WindowInfo>> {
        Ok(top_level::enumerate()?)
    }

    fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> CaptureResult<Frame> {
        let target = display::target_by_id(monitor_id)?;
        Ok(capture_with_chain(&self.chain, &target, *opts)?)
    }

    fn capture_window(&self, window_id: &str, opts: &CaptureOptions) -> CaptureResult<Frame> {
        let opts = *opts;
        let target = top_level::resolve_target(window_id)?;
        let native = || {
            if self.wgc_supported {
                self.wgc.capture_window(&target, opts)
            } else {
                Err(WinError::Unsupported("window capture without Windows.Graphics.Capture"))
            }
        };
        let frame = capture_window_with_fallback(native, || {
            self.capture_window_via_monitor(&target, opts)
        })?;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary_target() -> crate::chain::MonitorTarget {
        display::enumerate_targets()
            .expect("displays")
            .into_iter()
            .find(|t| t.monitor.primary)
            .expect("a primary monitor")
    }

    #[test]
    #[ignore = "needs an interactive Windows desktop"]
    fn stages_are_ordered_wgc_dda_gdi() {
        let backend = WindowsCapture::new().expect("backend");
        let names: Vec<_> = backend.chain.iter().map(|s| s.name()).collect();
        let expected: &[&str] =
            if backend.wgc_supported { &["wgc", "dda", "gdi"] } else { &["dda", "gdi"] };
        assert_eq!(names, expected);
    }

    /// Each stage on its own, so a broken stage is not hidden by the fallback chain.
    #[test]
    #[ignore = "needs an interactive Windows desktop"]
    fn every_stage_captures_the_primary_monitor() {
        let backend = WindowsCapture::new().expect("backend");
        let target = primary_target();
        for stage in &backend.chain {
            let frame = stage
                .capture(&target, CaptureOptions::default())
                .unwrap_or_else(|e| panic!("stage {} failed: {e}", stage.name()));
            println!("{}: {frame:?}", stage.name());
            assert_eq!(frame.size(), target.monitor.rect.size(), "stage {}", stage.name());
            assert_eq!(frame.origin, target.monitor.rect.origin(), "stage {}", stage.name());
            assert!(
                frame.data().iter().any(|b| *b != 0),
                "stage {} returned all zeros",
                stage.name()
            );
        }
    }
}
