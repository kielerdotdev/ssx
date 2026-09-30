//! [`ScreenCapturer`]: the [`Capturer`] service on top of `ssx-platform`'s [`ScreenSource`].
//!
//! Design notes:
//!
//! * **Lazy backend.** Detecting a capture backend needs a graphical session (and, on
//!   Wayland, a round trip to the compositor). Building the service bundle must not fail
//!   for `ssx upload` in an SSH session, so the [`ScreenSource`] is created on first use and
//!   failures are *not* cached (a tray app can start before the session is fully up).
//! * **Tone mapping per request.** The [`ScreenSource`] owns the tone-map settings; each
//!   request carries its own [`HdrConfig`](ssx_core::settings::HdrConfig), so the settings are
//!   swapped in under the source's lock. Captures are therefore serialised, which is what
//!   the underlying backends want anyway (one screencopy / portal session at a time).
//! * **Interactive region selection is injected.** The overlay lives in its own crate and
//!   implements [`RegionSelector`]. It gets the *already captured* desktop frame, so what the
//!   user sees while dragging is exactly what ends up in the file (a frozen-frame overlay).
//!   Without a selector the `Region` target fails with an explanation and the CLI's
//!   `--rect x,y,w,h` remains the way to capture an exact region.
//! * **Last region** is persisted in the data directory, so it survives restarts and is
//!   shared between the tray app and CLI invocations.
//!
//! The "monitor under the cursor" target ([`CaptureTarget::Monitor`]) has no cursor position
//! to work with (the capture backends do not expose one), so it uses the monitor showing most
//! of the focused window, then the primary monitor.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use ssx_capture::{Capabilities, CaptureError, CaptureOptions};
use ssx_core::{
    settings::{HdrConfig, atomic_write},
    workflow::{CancelToken, CaptureRequest, CaptureTarget, Captured, Capturer, ServiceError},
};
use ssx_hdr::TonemapSettings;
use ssx_platform::{BackendKind, ScreenSource};
use ssx_types::{Frame, Monitor, Rect, WindowInfo};

use crate::hdr::tonemap_settings;

/// Interactive region selection, implemented by the overlay crate.
///
/// Called on the capturing thread; it may block until the user has chosen. Return
/// `Ok(None)` when the user pressed Esc. Rectangles are in **virtual-desktop physical
/// pixels**; they are clipped to the desktop afterwards.
///
/// [`select`](Self::select) is the minimal contract (a rectangle). Selectors that can do more
/// (window and monitor picking, ellipse and freeform masks, starting from the last region)
/// implement [`pick`](Self::pick), which the capturer calls instead.
pub trait RegionSelector: Send + Sync {
    /// Lets the user pick a rectangle on `desktop` (a frozen capture of the whole virtual
    /// desktop, `desktop.origin` is its top-left). `monitors` is empty when the backend
    /// cannot enumerate them.
    fn select(&self, monitors: &[Monitor], desktop: &Frame) -> Result<Option<Rect>, ServiceError>;

    /// The richer form of [`select`](Self::select). The default asks for a plain rectangle.
    fn pick(&self, req: &PickRequest<'_>) -> Result<Option<Picked>, ServiceError> {
        Ok(self.select(req.monitors, req.desktop)?.map(Picked::rect))
    }
}

/// What [`RegionSelector::pick`] is given.
#[derive(Debug, Clone, Copy)]
pub struct PickRequest<'a> {
    /// The frozen desktop.
    pub desktop: &'a Frame,
    /// Monitors (empty when the backend cannot enumerate them).
    pub monitors: &'a [Monitor],
    /// Windows front to back, for hover-snap (empty when the backend cannot enumerate them).
    pub windows: &'a [WindowInfo],
    /// The previously captured region, to start the selection from.
    pub initial: Option<Rect>,
    /// Cancelled when the caller gives up (the tray's Cancel entry): a selector that shows a
    /// window should close it and answer `Ok(None)`.
    pub cancel: &'a CancelToken,
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    /// Bounding rectangle in virtual-desktop pixels.
    pub rect: Rect,
    /// Row-major coverage mask of `rect` (255 inside, 0 outside) for ellipse and freeform
    /// selections. Pixels outside become transparent.
    pub mask: Option<Vec<u8>>,
    /// The window that was clicked, if the choice is a window (its title feeds `%t`).
    pub window: Option<WindowInfo>,
}

impl Picked {
    /// A plain rectangle.
    pub fn rect(rect: Rect) -> Self {
        Self { rect, mask: None, window: None }
    }
}

/// The explicit targets the CLI can ask for beyond the workflow engine's [`CaptureTarget`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplicitTarget {
    /// A rectangle of the virtual desktop (remembered as the last region).
    Rect(Rect),
    /// One monitor by [`Monitor::id`].
    Monitor(String),
    /// One window by [`WindowInfo::id`].
    Window(String),
}

/// Persistent storage of the last captured region.
#[derive(Debug, Clone, Default)]
pub struct LastRegionStore {
    path: Option<PathBuf>,
}

/// File name inside the data directory.
const LAST_REGION_FILE: &str = "last_region.json";
/// Regions larger than this on a side are treated as corrupt.
const MAX_SIDE: u32 = 1 << 20;

impl LastRegionStore {
    /// Stores the region as `<data_dir>/last_region.json`.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self { path: Some(data_dir.join(LAST_REGION_FILE)) }
    }

    /// A store that remembers nothing (tests, read-only environments).
    pub fn none() -> Self {
        Self::default()
    }

    /// The remembered region, if there is a sane one.
    pub fn load(&self) -> Option<Rect> {
        let path = self.path.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        let rect: Rect = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring unreadable last-region file: {e}");
                return None;
            }
        };
        (!rect.is_empty() && rect.width <= MAX_SIDE && rect.height <= MAX_SIDE).then_some(rect)
    }

    /// Remembers `rect`. Failures are logged, never fatal: losing the last region must not
    /// fail a capture that has already succeeded.
    pub fn save(&self, rect: Rect) {
        let Some(path) = &self.path else { return };
        let Ok(text) = serde_json::to_string(&rect) else { return };
        if let Err(e) = atomic_write(path, text.as_bytes()) {
            tracing::warn!(path = %path.display(), "could not remember the last region: {e}");
        }
    }
}

/// What detection found, for `ssx doctor`.
#[derive(Debug, Clone)]
pub struct BackendReport {
    /// Backend name, e.g. `x11` or `wayland-wlr-screencopy`.
    pub name: &'static str,
    /// Backend family chosen by detection (`None` for injected sources).
    pub kind: Option<BackendKind>,
    /// Every candidate tried, in order: `Ok(())` for the winner, the reason otherwise.
    pub attempts: Vec<(BackendKind, Result<(), String>)>,
    /// What the backend can do.
    pub capabilities: Capabilities,
}

enum Slot {
    /// Not detected yet; the preferred backend, if forced.
    Pending(Option<BackendKind>),
    Ready(Box<ScreenSource>),
}

/// The [`Capturer`] implementation. See the [module docs](self).
pub struct ScreenCapturer {
    slot: Mutex<Slot>,
    selector: Option<Arc<dyn RegionSelector>>,
    last_region: LastRegionStore,
}

impl std::fmt::Debug for ScreenCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCapturer")
            .field("selector", &self.selector.is_some())
            .field("last_region", &self.last_region)
            .finish_non_exhaustive()
    }
}

/// Maps backend errors onto the service errors the engine understands, adding hints.
pub fn map_capture_error(e: CaptureError) -> ServiceError {
    match e {
        CaptureError::Cancelled => ServiceError::Cancelled,
        CaptureError::Unsupported { backend, what } => {
            ServiceError::Unsupported(format!("{what} on the {backend} capture backend"))
        }
        CaptureError::PermissionDenied(m) => ServiceError::failed(format!(
            "the system refused the screen capture: {m}. Grant screen-capture permission to ssx and try again"
        )),
        CaptureError::NoBackend(m) => ServiceError::failed(format!(
            "no usable capture backend: {m}. Run `ssx doctor` to see why each backend was rejected"
        )),
        CaptureError::NotFound(id) => ServiceError::failed(format!(
            "no monitor or window with id {id:?}; list them with `ssx monitors` and `ssx windows`"
        )),
        CaptureError::InvalidRegion(r) => ServiceError::failed(format!(
            "the region {}x{} at ({}, {}) is empty or outside every monitor",
            r.width, r.height, r.x, r.y
        )),
        CaptureError::Io(e) => ServiceError::Io(e),
        other => ServiceError::failed(other.to_string()),
    }
}

impl ScreenCapturer {
    /// Detects the backend on first use. `preferred` forces one (as `SSX_BACKEND` does).
    pub fn detect(preferred: Option<BackendKind>, last_region: LastRegionStore) -> Self {
        Self { slot: Mutex::new(Slot::Pending(preferred)), selector: None, last_region }
    }

    /// Uses an already constructed source (tests, or callers that pick their own backend).
    pub fn with_source(source: ScreenSource, last_region: LastRegionStore) -> Self {
        Self { slot: Mutex::new(Slot::Ready(Box::new(source))), selector: None, last_region }
    }

    /// Plugs in the interactive region selector.
    #[must_use]
    pub fn with_selector(mut self, selector: Arc<dyn RegionSelector>) -> Self {
        self.selector = Some(selector);
        self
    }

    /// Whether interactive region selection is available.
    pub fn has_selector(&self) -> bool {
        self.selector.is_some()
    }

    /// The last remembered region.
    pub fn last_region(&self) -> Option<Rect> {
        self.last_region.load()
    }

    /// Runs `f` with the (lazily created) source locked.
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut ScreenSource) -> Result<T, CaptureError>,
    ) -> Result<T, ServiceError> {
        let mut guard = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        let source = Self::ready(&mut guard)?;
        f(source).map_err(map_capture_error)
    }

    fn ready<'a>(
        guard: &'a mut MutexGuard<'_, Slot>,
    ) -> Result<&'a mut ScreenSource, ServiceError> {
        if let Slot::Pending(preferred) = &**guard {
            let source = ScreenSource::detect_preferring(*preferred, TonemapSettings::default())
                .map_err(map_capture_error)?;
            **guard = Slot::Ready(Box::new(source));
        }
        match &mut **guard {
            Slot::Ready(s) => Ok(s),
            Slot::Pending(_) => unreachable!("replaced by Ready above"),
        }
    }

    /// Describes the chosen backend and the candidates that were rejected.
    pub fn backend_report(&self) -> Result<BackendReport, ServiceError> {
        self.with(|s| {
            Ok(BackendReport {
                name: s.backend_name(),
                kind: s.kind(),
                attempts: s.attempts().iter().map(|a| (a.kind, a.result.clone())).collect(),
                capabilities: s.capabilities(),
            })
        })
    }

    /// All monitors.
    pub fn monitors(&self) -> Result<Vec<Monitor>, ServiceError> {
        self.with(|s| s.monitors())
    }

    /// All top-level windows.
    pub fn windows(&self) -> Result<Vec<WindowInfo>, ServiceError> {
        self.with(|s| s.windows())
    }

    /// Captures an explicit target (CLI `--rect`, `--id`).
    pub fn capture_explicit(
        &self,
        target: &ExplicitTarget,
        include_cursor: bool,
        hdr: &HdrConfig,
    ) -> Result<Captured, ServiceError> {
        let opts = CaptureOptions { include_cursor };
        let tonemap = tonemap_settings(hdr);
        let captured = self.with(|s| {
            s.set_tonemap(tonemap);
            match target {
                ExplicitTarget::Rect(r) => s.capture_region(*r, &opts).map(Captured::new),
                ExplicitTarget::Monitor(id) => s.capture_monitor(id, &opts).map(Captured::new),
                ExplicitTarget::Window(id) => {
                    let frame = s.capture_window(id, &opts)?;
                    let info = s.windows()?.into_iter().find(|w| &w.id == id);
                    Ok(with_window_info(frame, info.as_ref()))
                }
            }
        })?;
        if let ExplicitTarget::Rect(r) = target {
            self.last_region.save(*r);
        }
        Ok(captured)
    }

    fn capture_interactive(
        &self,
        opts: CaptureOptions,
        tonemap: TonemapSettings,
        cancel: &CancelToken,
    ) -> Result<Captured, ServiceError> {
        let Some(selector) = &self.selector else {
            return Err(ServiceError::NotConfigured(
                "interactive region selection needs the `ssx-overlay` selection overlay, which \
                 was not found (it must sit next to ssx or on PATH, or SSX_OVERLAY must name \
                 it); capture an exact region with `ssx capture region --rect x,y,w,h`, \
                 or repeat the previous one with `ssx capture last-region`"
                    .to_owned(),
            ));
        };
        // Capture first, select second: the overlay shows exactly what will be saved.
        let (desktop, monitors, windows) = self.with(|s| {
            s.set_tonemap(tonemap);
            let desktop = s.capture_desktop(&opts)?;
            // Not every backend can enumerate (the portal cannot): the overlay copes.
            Ok((desktop, s.monitors().unwrap_or_default(), s.windows().unwrap_or_default()))
        })?;
        let req = PickRequest {
            desktop: &desktop,
            monitors: &monitors,
            windows: &windows,
            initial: self.last_region.load(),
            cancel,
        };
        let Some(picked) = selector.pick(&req)? else {
            return Err(ServiceError::Cancelled);
        };
        let clipped = picked
            .rect
            .intersect(desktop.rect())
            .filter(|r| !r.is_empty())
            .ok_or_else(|| map_capture_error(CaptureError::InvalidRegion(picked.rect)))?;
        let mut frame = desktop
            .crop_desktop(clipped)
            .map_err(|e| ServiceError::failed(format!("cannot crop the selection: {e}")))?;
        if let Some(mask) = &picked.mask {
            apply_mask(&mut frame, picked.rect, clipped, mask);
        }
        self.last_region.save(clipped);
        Ok(with_window_info(frame, picked.window.as_ref()))
    }
}

/// Makes every pixel of `frame` (the crop `clipped` of a selection whose bounding box is
/// `full`) transparent where `mask` (row-major over `full`) is zero. A mask of the wrong
/// size is ignored: a selection must not fail because of a bad mask.
fn apply_mask(frame: &mut Frame, full: Rect, clipped: Rect, mask: &[u8]) {
    let (fw, fh) = (full.width as usize, full.height as usize);
    let bpp = match frame.format() {
        ssx_types::PixelFormat::Rgba8 | ssx_types::PixelFormat::Bgra8 => 4,
        ssx_types::PixelFormat::Rgba16F => return,
    };
    if mask.len() != fw * fh || frame.size() != clipped.size() {
        tracing::warn!("ignoring a selection mask that does not match the selection");
        return;
    }
    let dx = usize::try_from(i64::from(clipped.x) - i64::from(full.x)).unwrap_or(0);
    let dy = usize::try_from(i64::from(clipped.y) - i64::from(full.y)).unwrap_or(0);
    for y in 0..clipped.height as usize {
        let row = frame.row_mut(y as u32);
        for x in 0..clipped.width as usize {
            let covered = mask.get((dy + y) * fw + dx + x).is_some_and(|m| *m != 0);
            if !covered && let Some(px) = row.get_mut(x * bpp..(x + 1) * bpp) {
                px.fill(0);
            }
        }
    }
}

fn with_window_info(frame: Frame, info: Option<&WindowInfo>) -> Captured {
    Captured {
        frame,
        window_title: info.map(|w| w.title.clone()).filter(|t| !t.is_empty()),
        process_name: info.and_then(|w| w.app_name.clone()).filter(|t| !t.is_empty()),
    }
}

/// The monitor showing most of the focused window; else the primary; else the first.
fn pick_monitor<'a>(monitors: &'a [Monitor], focused: Option<&WindowInfo>) -> Option<&'a Monitor> {
    let by_window = focused.and_then(|w| {
        monitors
            .iter()
            .filter_map(|m| m.rect.intersect(w.rect).map(|i| (i.area(), m)))
            .max_by_key(|(area, _)| *area)
            .map(|(_, m)| m)
    });
    by_window.or_else(|| monitors.iter().find(|m| m.primary)).or_else(|| monitors.first())
}

impl Capturer for ScreenCapturer {
    fn capture(
        &self,
        req: &CaptureRequest,
        cancel: &CancelToken,
    ) -> Result<Captured, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let opts = CaptureOptions { include_cursor: req.include_cursor };
        let tonemap = tonemap_settings(&req.hdr);
        let captured = match req.target {
            CaptureTarget::Region => return self.capture_interactive(opts, tonemap, cancel),
            CaptureTarget::Fullscreen => self.with(|s| {
                s.set_tonemap(tonemap);
                s.capture_desktop(&opts).map(Captured::new)
            })?,
            CaptureTarget::Monitor => self.with(|s| {
                s.set_tonemap(tonemap);
                let monitors = s.monitors()?;
                let focused = s.active_window().ok().flatten();
                let m = pick_monitor(&monitors, focused.as_ref()).ok_or_else(|| {
                    CaptureError::NoBackend("the backend reported no monitors".to_owned())
                })?;
                s.capture_monitor(&m.id, &opts).map(Captured::new)
            })?,
            CaptureTarget::Window => self.with(|s| {
                s.set_tonemap(tonemap);
                let (frame, info) = s.capture_active_window(&opts)?;
                Ok(with_window_info(frame, Some(&info)))
            })?,
            CaptureTarget::LastRegion => {
                let rect = self.last_region.load().ok_or_else(|| {
                    ServiceError::NotConfigured(
                        "no region has been captured yet; take one with `ssx capture region` first"
                            .to_owned(),
                    )
                })?;
                self.with(|s| {
                    s.set_tonemap(tonemap);
                    s.capture_region(rect, &opts).map(Captured::new)
                })?
            }
        };
        Ok(captured)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use ssx_capture::{CaptureBackend, Result as CaptureResult};
    use ssx_types::{ColorSpace, PixelFormat, Point, Size};

    use super::*;

    /// A fake backend: two side-by-side monitors filled with distinct per-pixel patterns.
    #[derive(Debug)]
    struct Fake {
        monitors: Vec<Monitor>,
        windows: Vec<WindowInfo>,
        hdr: bool,
    }

    /// Pixel colour for desktop coordinate `(x, y)`, unique enough to notice any shift.
    fn pixel(x: i32, y: i32) -> [u8; 3] {
        [(x & 0xff) as u8, (y & 0xff) as u8, (((x >> 8) & 0xf) | (((y >> 8) & 0xf) << 4)) as u8]
    }

    fn monitor(id: &str, x: i32, w: u32, h: u32, primary: bool) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            rect: Rect::new(x, 0, w, h),
            scale_factor: 1.0,
            primary,
            refresh_hz: None,
            hdr: None,
        }
    }

    fn window(id: &str, rect: Rect, focused: bool) -> WindowInfo {
        WindowInfo {
            id: id.into(),
            title: format!("title of {id}"),
            app_name: Some(format!("app-{id}")),
            rect,
            minimized: false,
            focused,
        }
    }

    impl Fake {
        fn standard() -> Self {
            Self {
                monitors: vec![monitor("L", 0, 40, 30, true), monitor("R", 40, 20, 30, false)],
                windows: vec![
                    window("bg", Rect::new(0, 0, 40, 30), false),
                    window("fg", Rect::new(45, 5, 10, 10), true),
                ],
                hdr: false,
            }
        }

        fn frame_for(&self, rect: Rect) -> Frame {
            let (w, h) = (rect.width, rect.height);
            let mut f = if self.hdr {
                // Mid-grey at 200-nit SDR white as linear scRGB half floats.
                let v = half::f16::from_f32(0.2158 * 2.5).to_le_bytes();
                let one = half::f16::from_f32(1.0).to_le_bytes();
                let px = [v, v, v, one].concat();
                let data = px.iter().copied().cycle().take((w * h * 8) as usize).collect();
                let mut f = Frame::from_raw(
                    Size::new(w, h),
                    (w * 8) as usize,
                    PixelFormat::Rgba16F,
                    ColorSpace::ScRgbLinear,
                    data,
                )
                .unwrap();
                f.sdr_white_nits = Some(200.0);
                f
            } else {
                let mut data = Vec::new();
                for y in 0..i32::try_from(h).unwrap() {
                    for x in 0..i32::try_from(w).unwrap() {
                        let [r, g, b] = pixel(rect.x + x, rect.y + y);
                        data.extend_from_slice(&[b, g, r, 255]); // BGRA like real APIs
                    }
                }
                Frame::from_raw(
                    Size::new(w, h),
                    (w * 4) as usize,
                    PixelFormat::Bgra8,
                    ColorSpace::Srgb,
                    data,
                )
                .unwrap()
            };
            f.origin = Point::new(rect.x, rect.y);
            f
        }
    }

    impl CaptureBackend for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                enumerate_monitors: true,
                enumerate_windows: true,
                capture_windows: true,
                ..Capabilities::default()
            }
        }
        fn monitors(&self) -> CaptureResult<Vec<Monitor>> {
            Ok(self.monitors.clone())
        }
        fn windows(&self) -> CaptureResult<Vec<WindowInfo>> {
            Ok(self.windows.clone())
        }
        fn capture_monitor(&self, id: &str, _: &CaptureOptions) -> CaptureResult<Frame> {
            let m = self
                .monitors
                .iter()
                .find(|m| m.id == id)
                .ok_or_else(|| CaptureError::NotFound(id.into()))?;
            Ok(self.frame_for(m.rect))
        }
        fn capture_window(&self, id: &str, _: &CaptureOptions) -> CaptureResult<Frame> {
            let w = self
                .windows
                .iter()
                .find(|w| w.id == id)
                .ok_or_else(|| CaptureError::NotFound(id.into()))?;
            Ok(self.frame_for(w.rect))
        }
    }

    fn capturer(fake: Fake, store: LastRegionStore) -> ScreenCapturer {
        ScreenCapturer::with_source(
            ScreenSource::new(Box::new(fake), TonemapSettings::default()),
            store,
        )
    }

    fn request(target: CaptureTarget) -> CaptureRequest {
        CaptureRequest { target, include_cursor: false, hdr: HdrConfig::default() }
    }

    fn assert_matches_pattern(f: &Frame, origin: (i32, i32)) {
        assert_eq!(f.format(), PixelFormat::Rgba8, "always 8-bit sRGB RGBA");
        for y in 0..f.height() {
            for x in 0..f.width() {
                let p = &f.row(y)[x as usize * 4..x as usize * 4 + 4];
                let [r, g, b] = pixel(
                    origin.0 + i32::try_from(x).unwrap(),
                    origin.1 + i32::try_from(y).unwrap(),
                );
                assert_eq!(p, [r, g, b, 255], "at ({x},{y})");
            }
        }
    }

    #[test]
    fn fullscreen_stitches_all_monitors_pixel_exactly() {
        let c = capturer(Fake::standard(), LastRegionStore::none());
        let got = c.capture(&request(CaptureTarget::Fullscreen), &CancelToken::new()).unwrap();
        assert_eq!((got.frame.width(), got.frame.height()), (60, 30));
        assert_matches_pattern(&got.frame, (0, 0));
        assert!(got.window_title.is_none() && got.process_name.is_none());
    }

    #[test]
    fn window_capture_reports_title_and_process() {
        let c = capturer(Fake::standard(), LastRegionStore::none());
        let got = c.capture(&request(CaptureTarget::Window), &CancelToken::new()).unwrap();
        assert_eq!(got.window_title.as_deref(), Some("title of fg"));
        assert_eq!(got.process_name.as_deref(), Some("app-fg"));
        assert_matches_pattern(&got.frame, (45, 5));
    }

    #[test]
    fn monitor_target_follows_the_focused_window_then_the_primary() {
        let c = capturer(Fake::standard(), LastRegionStore::none());
        let got = c.capture(&request(CaptureTarget::Monitor), &CancelToken::new()).unwrap();
        assert_eq!(got.frame.width(), 20, "focused window is on monitor R");
        assert_matches_pattern(&got.frame, (40, 0));

        let mut fake = Fake::standard();
        fake.windows.clear();
        let c = capturer(fake, LastRegionStore::none());
        let got = c.capture(&request(CaptureTarget::Monitor), &CancelToken::new()).unwrap();
        assert_eq!(got.frame.width(), 40, "no focused window: primary monitor");
    }

    #[test]
    fn explicit_targets_work_and_rect_is_remembered_for_last_region() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(Fake::standard(), LastRegionStore::in_dir(dir.path()));
        assert_eq!(c.last_region(), None);
        let e = c.capture(&request(CaptureTarget::LastRegion), &CancelToken::new()).unwrap_err();
        assert!(matches!(e, ServiceError::NotConfigured(m) if m.contains("no region")));

        let rect = Rect::new(35, 3, 12, 9); // spans both monitors
        let got =
            c.capture_explicit(&ExplicitTarget::Rect(rect), false, &HdrConfig::default()).unwrap();
        assert_matches_pattern(&got.frame, (35, 3));
        assert_eq!(c.last_region(), Some(rect));

        // A brand-new capturer sees the persisted region.
        let c2 = capturer(Fake::standard(), LastRegionStore::in_dir(dir.path()));
        let again = c2.capture(&request(CaptureTarget::LastRegion), &CancelToken::new()).unwrap();
        assert_matches_pattern(&again.frame, (35, 3));

        let win = c
            .capture_explicit(&ExplicitTarget::Window("fg".into()), false, &HdrConfig::default())
            .unwrap();
        assert_eq!(win.window_title.as_deref(), Some("title of fg"));
        let mon = c
            .capture_explicit(&ExplicitTarget::Monitor("R".into()), false, &HdrConfig::default())
            .unwrap();
        assert_eq!(mon.frame.width(), 20);
        let err = c
            .capture_explicit(&ExplicitTarget::Monitor("nope".into()), false, &HdrConfig::default())
            .unwrap_err();
        assert!(err.to_string().contains("ssx monitors"), "{err}");
    }

    #[test]
    fn corrupt_last_region_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = LastRegionStore::in_dir(dir.path());
        std::fs::write(dir.path().join(LAST_REGION_FILE), "not json").unwrap();
        assert_eq!(store.load(), None);
        std::fs::write(dir.path().join(LAST_REGION_FILE), r#"{"x":0,"y":0,"width":0,"height":5}"#)
            .unwrap();
        assert_eq!(store.load(), None, "empty rectangles are rejected");
        std::fs::write(
            dir.path().join(LAST_REGION_FILE),
            r#"{"x":0,"y":0,"width":4000000000,"height":5}"#,
        )
        .unwrap();
        assert_eq!(store.load(), None, "absurd sizes are rejected");
    }

    struct FixedSelector(Option<Rect>, AtomicBool);
    impl RegionSelector for FixedSelector {
        fn select(
            &self,
            monitors: &[Monitor],
            desktop: &Frame,
        ) -> Result<Option<Rect>, ServiceError> {
            assert_eq!(monitors.len(), 2);
            assert_eq!(desktop.rect(), Rect::new(0, 0, 60, 30), "gets the whole frozen desktop");
            self.1.store(true, Ordering::SeqCst);
            Ok(self.0)
        }
    }

    #[test]
    fn interactive_region_without_a_selector_explains_what_is_needed() {
        let c = capturer(Fake::standard(), LastRegionStore::none());
        let e = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("overlay") && msg.contains("--rect"), "{msg}");
    }

    #[test]
    fn interactive_region_crops_the_frozen_desktop_and_remembers_it() {
        let dir = tempfile::tempdir().unwrap();
        let sel = Arc::new(FixedSelector(Some(Rect::new(30, 10, 20, 10)), AtomicBool::new(false)));
        let c = capturer(Fake::standard(), LastRegionStore::in_dir(dir.path()))
            .with_selector(sel.clone());
        let got = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap();
        assert!(sel.1.load(Ordering::SeqCst));
        assert_matches_pattern(&got.frame, (30, 10));
        assert_eq!(c.last_region(), Some(Rect::new(30, 10, 20, 10)));
    }

    #[test]
    fn selection_is_clipped_and_esc_is_cancellation() {
        let sel =
            Arc::new(FixedSelector(Some(Rect::new(50, 20, 100, 100)), AtomicBool::new(false)));
        let c = capturer(Fake::standard(), LastRegionStore::none()).with_selector(sel);
        let got = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap();
        assert_eq!((got.frame.width(), got.frame.height()), (10, 10));
        assert_matches_pattern(&got.frame, (50, 20));

        let sel = Arc::new(FixedSelector(None, AtomicBool::new(false)));
        let c = capturer(Fake::standard(), LastRegionStore::none()).with_selector(sel);
        let e = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap_err();
        assert!(e.is_cancelled());

        let sel = Arc::new(FixedSelector(Some(Rect::new(500, 500, 5, 5)), AtomicBool::new(false)));
        let c = capturer(Fake::standard(), LastRegionStore::none()).with_selector(sel);
        let e = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap_err();
        assert!(e.to_string().contains("outside every monitor"), "{e}");
    }

    /// A selector that answers `pick` with a canned [`Picked`] and records the request.
    struct PickSelector(Option<Picked>, Mutex<Option<(usize, usize, Option<Rect>)>>);
    impl RegionSelector for PickSelector {
        fn select(&self, _: &[Monitor], _: &Frame) -> Result<Option<Rect>, ServiceError> {
            panic!("pick must be preferred over select");
        }
        fn pick(&self, req: &PickRequest<'_>) -> Result<Option<Picked>, ServiceError> {
            *self.1.lock().unwrap() = Some((req.monitors.len(), req.windows.len(), req.initial));
            Ok(self.0.clone())
        }
    }

    #[test]
    fn pick_gets_windows_and_the_last_region_and_masks_become_transparent() {
        let dir = tempfile::tempdir().unwrap();
        let store = LastRegionStore::in_dir(dir.path());
        store.save(Rect::new(1, 2, 3, 4));
        // A 6x4 selection with only its left half covered.
        let mut mask = vec![0u8; 24];
        for row in 0..4 {
            mask[row * 6..row * 6 + 3].fill(255);
        }
        let win = window("w", Rect::new(30, 10, 6, 4), true);
        let sel = Arc::new(PickSelector(
            Some(Picked { rect: Rect::new(30, 10, 6, 4), mask: Some(mask), window: Some(win) }),
            Mutex::new(None),
        ));
        let c = capturer(Fake::standard(), store).with_selector(sel.clone());
        let got = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap();
        let (monitors, windows, initial) = sel.1.lock().unwrap().unwrap();
        assert_eq!(monitors, 2);
        assert!(windows > 0, "the selector gets the windows for hover-snap");
        assert_eq!(initial, Some(Rect::new(1, 2, 3, 4)), "and the previous region");
        assert_eq!(got.window_title.as_deref(), Some("title of w"));
        let f = &got.frame;
        assert_eq!((f.width(), f.height()), (6, 4));
        for y in 0..4u32 {
            let row = f.row(y);
            assert!(row[..12].chunks(4).all(|p| p[3] == 255), "covered pixels stay opaque");
            assert!(row[12..24].chunks(4).all(|p| p == [0, 0, 0, 0]), "the rest is transparent");
        }
        assert_eq!(c.last_region(), Some(Rect::new(30, 10, 6, 4)));
    }

    #[test]
    fn a_mask_is_cropped_with_a_clipped_selection_and_a_bad_mask_is_ignored() {
        // Selection sticks out of the 60x30 desktop on the right: the mask must follow.
        let mut mask = vec![255u8; 10 * 2];
        mask[9] = 0; // (9, 0): outside the desktop anyway
        mask[10] = 0; // (0, 1): inside, must become transparent
        let sel = Arc::new(PickSelector(
            Some(Picked { rect: Rect::new(55, 10, 10, 2), mask: Some(mask), window: None }),
            Mutex::new(None),
        ));
        let c = capturer(Fake::standard(), LastRegionStore::none()).with_selector(sel);
        let got = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap();
        assert_eq!((got.frame.width(), got.frame.height()), (5, 2));
        assert_eq!(&got.frame.row(1)[..4], &[0, 0, 0, 0]);
        assert_eq!(got.frame.row(0)[3], 255);

        let sel = Arc::new(PickSelector(
            Some(Picked { rect: Rect::new(5, 5, 4, 4), mask: Some(vec![0; 3]), window: None }),
            Mutex::new(None),
        ));
        let c = capturer(Fake::standard(), LastRegionStore::none()).with_selector(sel);
        let got = c.capture(&request(CaptureTarget::Region), &CancelToken::new()).unwrap();
        assert_matches_pattern(&got.frame, (5, 5));
    }

    #[test]
    fn a_cancelled_token_never_touches_the_backend() {
        let c = ScreenCapturer::detect(None, LastRegionStore::none());
        let cancel = CancelToken::new();
        cancel.cancel();
        let e = c.capture(&request(CaptureTarget::Fullscreen), &cancel).unwrap_err();
        assert!(e.is_cancelled());
    }

    #[test]
    fn hdr_frames_are_tonemapped_with_the_requests_own_settings() {
        let fake = Fake { hdr: true, ..Fake::standard() };
        let c = capturer(fake, LastRegionStore::none());
        let grey = |exposure_ev: f32| {
            let req = CaptureRequest {
                target: CaptureTarget::Monitor,
                include_cursor: false,
                hdr: HdrConfig { exposure: exposure_ev, dither: false, ..HdrConfig::default() },
            };
            let got = c.capture(&req, &CancelToken::new()).unwrap();
            assert!(got.frame.is_sdr8(), "the engine requires 8-bit sRGB");
            i32::from(got.frame.data()[0])
        };
        let base = grey(0.0);
        assert!((base - 128).abs() <= 1, "SDR-range content is preserved: {base}");
        // Per-request settings are applied every time (no stale tone map).
        assert!(grey(-1.0) < base - 20);
        assert!(grey(1.0) > base + 20);
        assert_eq!(grey(0.0), base);
    }

    #[test]
    fn pick_monitor_prefers_overlap() {
        let ms = vec![monitor("a", 0, 10, 10, true), monitor("b", 10, 10, 10, false)];
        let w = window("w", Rect::new(8, 0, 10, 5), true); // 2 px on a, 8 px on b
        assert_eq!(pick_monitor(&ms, Some(&w)).unwrap().id, "b");
        assert_eq!(pick_monitor(&ms, None).unwrap().id, "a");
        assert!(pick_monitor(&[], None).is_none());
    }
}
