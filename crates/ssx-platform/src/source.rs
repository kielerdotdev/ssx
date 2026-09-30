//! [`ScreenSource`]: capture that always yields 8-bit sRGB.

use ssx_capture::{Capabilities, CaptureBackend, CaptureError, CaptureOptions, Result, composite};
use ssx_hdr::TonemapSettings;
use ssx_types::{Frame, Monitor, Rect, WindowInfo};

use crate::detect::{Attempt, BackendKind, detect_backend};

/// Converts a native backend frame to 8-bit sRGB RGBA.
///
/// HDR float frames are tone-mapped with `settings`; 8-bit frames are only channel-swizzled,
/// so an SDR screenshot is never altered.
pub fn to_sdr(frame: Frame, settings: &TonemapSettings) -> Result<Frame> {
    if ssx_hdr::frame_needs_tonemap(&frame) {
        return ssx_hdr::to_sdr8(&frame, settings).map_err(|e| CaptureError::backend("hdr", e));
    }
    Ok(frame.into_rgba8()?)
}

/// A capture backend plus tone-mapping. Every method returns tightly packed `Rgba8`/sRGB
/// frames.
#[derive(Debug)]
pub struct ScreenSource {
    backend: Box<dyn CaptureBackend>,
    kind: Option<BackendKind>,
    attempts: Vec<Attempt>,
    tonemap: TonemapSettings,
}

impl ScreenSource {
    /// Wraps an already-constructed backend (tests, or callers that pick their own).
    pub fn new(backend: Box<dyn CaptureBackend>, tonemap: TonemapSettings) -> Self {
        Self { backend, kind: None, attempts: Vec::new(), tonemap }
    }

    /// Detects the right backend for this session (see [`detect_backend`]).
    pub fn detect(tonemap: TonemapSettings) -> Result<Self> {
        Self::detect_preferring(None, tonemap)
    }

    /// Like [`ScreenSource::detect`] but with an explicit backend choice.
    pub fn detect_preferring(
        preferred: Option<BackendKind>,
        tonemap: TonemapSettings,
    ) -> Result<Self> {
        let d = detect_backend(preferred)?;
        Ok(Self { backend: d.backend, kind: Some(d.kind), attempts: d.attempts, tonemap })
    }

    /// Which backend family is active, when it was chosen by detection.
    pub fn kind(&self) -> Option<BackendKind> {
        self.kind
    }

    /// Every backend detection tried, for `ssx doctor`-style output.
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    pub fn tonemap(&self) -> &TonemapSettings {
        &self.tonemap
    }

    pub fn set_tonemap(&mut self, settings: TonemapSettings) {
        self.tonemap = settings;
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        self.backend.monitors()
    }

    pub fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.backend.windows()
    }

    /// The focused, non-minimised window, if the backend can enumerate windows.
    pub fn active_window(&self) -> Result<Option<WindowInfo>> {
        Ok(self.backend.windows()?.into_iter().find(|w| w.focused && !w.minimized))
    }

    /// The whole virtual desktop.
    pub fn capture_desktop(&self, opts: &CaptureOptions) -> Result<Frame> {
        if self.backend.capabilities().native_desktop {
            return to_sdr(self.backend.capture_desktop(opts)?, &self.tonemap);
        }
        let monitors = self.backend.monitors()?;
        if monitors.is_empty() {
            return Err(CaptureError::NoBackend("the backend reported no monitors".into()));
        }
        Ok(composite(&self.capture_each(&monitors, *opts)?)?)
    }

    /// One monitor by [`Monitor::id`].
    pub fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        to_sdr(self.backend.capture_monitor(monitor_id, opts)?, &self.tonemap)
    }

    /// A rectangle of the virtual desktop, clipped to the monitors it overlaps.
    pub fn capture_region(&self, region: Rect, opts: &CaptureOptions) -> Result<Frame> {
        if region.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        let caps = self.backend.capabilities();
        if caps.native_desktop || !caps.hdr_float {
            // The backend's own region capture is safe: its frames are already 8-bit (or it
            // captures the desktop in one go), so there is no HDR/SDR mixing to worry about.
            return to_sdr(self.backend.capture_region(region, opts)?, &self.tonemap);
        }
        // HDR-capable per-monitor backend: tone-map each monitor first, then stitch.
        let hit: Vec<Monitor> = self
            .backend
            .monitors()?
            .into_iter()
            .filter(|m| m.rect.intersect(region).is_some())
            .collect();
        if hit.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        let desktop = composite(&self.capture_each(&hit, *opts)?)?;
        let clipped =
            region.intersect(desktop.rect()).ok_or(CaptureError::InvalidRegion(region))?;
        desktop.crop_desktop(clipped).map_err(|_| CaptureError::InvalidRegion(region))
    }

    /// One window by [`WindowInfo::id`].
    pub fn capture_window(&self, window_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        to_sdr(self.backend.capture_window(window_id, opts)?, &self.tonemap)
    }

    /// The active window together with its description (for `%t` / `%pn` in file names).
    pub fn capture_active_window(&self, opts: &CaptureOptions) -> Result<(Frame, WindowInfo)> {
        let info = self
            .active_window()?
            .ok_or_else(|| CaptureError::NotFound("no focused window".into()))?;
        let frame = self.capture_window(&info.id, opts)?;
        Ok((frame, info))
    }

    fn capture_each(&self, monitors: &[Monitor], opts: CaptureOptions) -> Result<Vec<Frame>> {
        monitors.iter().map(|m| self.capture_monitor(&m.id, &opts)).collect()
    }
}

#[cfg(test)]
mod tests {
    use half::f16;
    use ssx_hdr::srgb_eotf;
    use ssx_types::{ColorSpace, HdrInfo, PixelFormat, Point, Size};

    use super::*;

    /// A test double serving canned frames.
    #[derive(Debug)]
    struct Fake {
        monitors: Vec<(Monitor, Frame)>,
        caps: Capabilities,
        windows: Vec<WindowInfo>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(monitors: Vec<(Monitor, Frame)>, caps: Capabilities) -> Self {
            Self { monitors, caps, windows: vec![], calls: std::sync::Mutex::default() }
        }
    }

    impl CaptureBackend for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn monitors(&self) -> Result<Vec<Monitor>> {
            Ok(self.monitors.iter().map(|(m, _)| m.clone()).collect())
        }
        fn windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(self.windows.clone())
        }
        fn capture_monitor(&self, id: &str, _: &CaptureOptions) -> Result<Frame> {
            self.calls.lock().unwrap().push(format!("monitor:{id}"));
            self.monitors
                .iter()
                .find(|(m, _)| m.id == id)
                .map(|(_, f)| f.clone())
                .ok_or_else(|| CaptureError::NotFound(id.into()))
        }
        fn capture_window(&self, id: &str, _: &CaptureOptions) -> Result<Frame> {
            self.calls.lock().unwrap().push(format!("window:{id}"));
            Ok(self.monitors[0].1.clone())
        }
    }

    fn monitor(id: &str, x: i32, w: u32, h: u32, hdr: Option<HdrInfo>) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            rect: Rect::new(x, 0, w, h),
            scale_factor: 1.0,
            primary: x == 0,
            refresh_hz: None,
            hdr,
        }
    }

    fn sdr_frame(x: i32, w: u32, h: u32, rgb: [u8; 3]) -> Frame {
        let px = [rgb[2], rgb[1], rgb[0], 255]; // Bgra8, as most OS APIs deliver
        let data = px.iter().copied().cycle().take((w * h * 4) as usize).collect();
        let mut f = Frame::from_raw(
            Size::new(w, h),
            (w * 4) as usize,
            PixelFormat::Bgra8,
            ColorSpace::Srgb,
            data,
        )
        .unwrap();
        f.origin = Point::new(x, 0);
        f
    }

    /// An scRGB frame whose pixels are the linear-light encoding of `rgb` at `sdr_white`.
    fn hdr_frame(x: i32, w: u32, h: u32, rgb: [u8; 3], sdr_white_nits: f32) -> Frame {
        let scale = sdr_white_nits / 80.0;
        let lin = |c: u8| f16::from_f32(srgb_eotf(f32::from(c) / 255.0) * scale);
        let px: Vec<u8> = [lin(rgb[0]), lin(rgb[1]), lin(rgb[2]), f16::from_f32(1.0)]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let data = px.iter().copied().cycle().take((w * h * 8) as usize).collect();
        let mut f = Frame::from_raw(
            Size::new(w, h),
            (w * 8) as usize,
            PixelFormat::Rgba16F,
            ColorSpace::ScRgbLinear,
            data,
        )
        .unwrap();
        f.origin = Point::new(x, 0);
        f.sdr_white_nits = Some(sdr_white_nits);
        f
    }

    const HDR_ON: HdrInfo =
        HdrInfo { active: true, sdr_white_nits: 200.0, max_luminance_nits: None };

    fn per_monitor_caps() -> Capabilities {
        Capabilities { enumerate_monitors: true, hdr_float: true, ..Capabilities::default() }
    }

    #[test]
    fn sdr_frames_are_only_swizzled_never_altered() {
        let f = to_sdr(sdr_frame(0, 2, 1, [10, 20, 30]), &TonemapSettings::default()).unwrap();
        assert_eq!(f.format(), PixelFormat::Rgba8);
        assert_eq!(&f.data()[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn hdr_ui_content_is_byte_identical_to_an_sdr_screenshot() {
        // The key product promise: normal desktop colours survive the HDR path exactly, at any
        // "SDR content brightness" setting.
        for nits in [80.0, 200.0, 480.0] {
            for rgb in [[255, 255, 255], [0, 0, 0], [37, 99, 235], [128, 128, 128], [1, 2, 3]] {
                let f = to_sdr(hdr_frame(0, 4, 2, rgb, nits), &TonemapSettings::default()).unwrap();
                assert_eq!(
                    &f.data()[..4],
                    &[rgb[0], rgb[1], rgb[2], 255],
                    "{rgb:?} at {nits} nits"
                );
            }
        }
    }

    #[test]
    fn mixed_hdr_and_sdr_monitors_are_tonemapped_before_stitching() {
        let backend = Fake::new(
            vec![
                (monitor("L", 0, 4, 2, Some(HDR_ON)), hdr_frame(0, 4, 2, [200, 100, 50], 200.0)),
                (monitor("R", 4, 3, 2, Some(HdrInfo::SDR)), sdr_frame(4, 3, 2, [9, 8, 7])),
            ],
            per_monitor_caps(),
        );
        let src = ScreenSource::new(Box::new(backend), TonemapSettings::default());
        let d = src.capture_desktop(&CaptureOptions::default()).unwrap();
        assert_eq!((d.width(), d.height(), d.format()), (7, 2, PixelFormat::Rgba8));
        assert_eq!(&d.row(0)[..4], &[200, 100, 50, 255], "HDR monitor decoded to its SDR look");
        assert_eq!(&d.row(1)[16..20], &[9, 8, 7, 255], "SDR monitor passed through");
    }

    #[test]
    fn region_across_monitors_only_captures_what_it_overlaps_and_clips() {
        let backend = Fake::new(
            vec![
                (monitor("L", 0, 4, 2, Some(HDR_ON)), hdr_frame(0, 4, 2, [200, 100, 50], 200.0)),
                (monitor("R", 4, 4, 2, None), sdr_frame(4, 4, 2, [9, 8, 7])),
                (monitor("FAR", 100, 4, 2, None), sdr_frame(100, 4, 2, [1, 1, 1])),
            ],
            per_monitor_caps(),
        );
        let src = ScreenSource::new(Box::new(backend), TonemapSettings::default());
        let f = src.capture_region(Rect::new(2, 0, 4, 2), &CaptureOptions::default()).unwrap();
        assert_eq!((f.width(), f.height()), (4, 2));
        assert_eq!(f.origin, Point::new(2, 0));
        assert_eq!(&f.row(0)[..4], &[200, 100, 50, 255]);
        assert_eq!(&f.row(0)[8..12], &[9, 8, 7, 255]);
        // Sticks out past the desktop edge: clipped, not an error.
        let f = src.capture_region(Rect::new(6, 0, 50, 100), &CaptureOptions::default());
        assert_eq!(f.unwrap().size(), Size::new(2, 2));
        assert!(matches!(
            src.capture_region(Rect::new(50, 0, 4, 4), &CaptureOptions::default()),
            Err(CaptureError::InvalidRegion(_))
        ));
        assert!(matches!(
            src.capture_region(Rect::new(0, 0, 0, 4), &CaptureOptions::default()),
            Err(CaptureError::InvalidRegion(_))
        ));
    }

    /// Lets a test keep a handle on the fake after boxing it into a `ScreenSource`.
    #[derive(Debug)]
    struct Shared(std::sync::Arc<Fake>);

    impl CaptureBackend for Shared {
        fn name(&self) -> &'static str {
            self.0.name()
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
        fn monitors(&self) -> Result<Vec<Monitor>> {
            self.0.monitors()
        }
        fn capture_monitor(&self, id: &str, o: &CaptureOptions) -> Result<Frame> {
            self.0.capture_monitor(id, o)
        }
    }

    #[test]
    fn region_capture_skips_monitors_it_does_not_overlap() {
        let fake = std::sync::Arc::new(Fake::new(
            vec![
                (monitor("L", 0, 4, 2, None), sdr_frame(0, 4, 2, [1, 2, 3])),
                (monitor("R", 4, 4, 2, None), sdr_frame(4, 4, 2, [9, 8, 7])),
            ],
            per_monitor_caps(),
        ));
        let src = ScreenSource::new(Box::new(Shared(fake.clone())), TonemapSettings::default());
        src.capture_region(Rect::new(0, 0, 2, 2), &CaptureOptions::default()).unwrap();
        assert_eq!(*fake.calls.lock().unwrap(), ["monitor:L"], "R must not be captured at all");
    }

    #[test]
    fn active_window_picks_the_focused_visible_one() {
        let mk = |id: &str, focused, minimized| WindowInfo {
            id: id.into(),
            title: id.into(),
            app_name: None,
            rect: Rect::new(0, 0, 4, 2),
            minimized,
            focused,
        };
        let mut backend = Fake::new(
            vec![(monitor("L", 0, 4, 2, None), sdr_frame(0, 4, 2, [1, 2, 3]))],
            per_monitor_caps(),
        );
        backend.windows = vec![mk("a", false, false), mk("b", true, true), mk("c", true, false)];
        let src = ScreenSource::new(Box::new(backend), TonemapSettings::default());
        assert_eq!(src.active_window().unwrap().unwrap().id, "c");
        let (frame, info) = src.capture_active_window(&CaptureOptions::default()).unwrap();
        assert_eq!(info.id, "c");
        assert_eq!(frame.format(), PixelFormat::Rgba8);
    }

    #[test]
    fn no_focused_window_is_not_found() {
        let backend = Fake::new(
            vec![(monitor("L", 0, 4, 2, None), sdr_frame(0, 4, 2, [1, 2, 3]))],
            per_monitor_caps(),
        );
        let src = ScreenSource::new(Box::new(backend), TonemapSettings::default());
        assert!(matches!(
            src.capture_active_window(&CaptureOptions::default()),
            Err(CaptureError::NotFound(_))
        ));
    }

    #[test]
    fn unknown_monitor_propagates_not_found() {
        let backend = Fake::new(vec![], per_monitor_caps());
        let src = ScreenSource::new(Box::new(backend), TonemapSettings::default());
        assert!(matches!(
            src.capture_monitor("nope", &CaptureOptions::default()),
            Err(CaptureError::NotFound(_))
        ));
        assert!(src.capture_desktop(&CaptureOptions::default()).is_err(), "no monitors at all");
    }
}
