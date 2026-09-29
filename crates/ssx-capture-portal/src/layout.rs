//! Monitor geometry: mapping compositor-reported (logical) monitor rectangles onto the
//! pixels of a whole-desktop screenshot.
//!
//! The portal hands back **one image of the whole desktop** and no monitor list, while
//! the sources we can query without any permission (Mutter's `DisplayConfig`, Wayland
//! `wl_output`/`xdg-output`) report **logical** coordinates (physical pixels divided by
//! the monitor's scale). This module bridges the two:
//!
//! * A monitor's public [`Monitor::rect`](ssx_types::Monitor) is its logical rectangle
//!   multiplied by the largest scale on the desktop. With one scale on every monitor
//!   (the common case) that is exactly the physical-pixel rectangle the `ssx` contract
//!   asks for; with mixed scales it is the pixel space compositors stitch their
//!   whole-desktop screenshots in (lower-DPI monitors are upscaled).
//! * The pixels-per-logical-unit factor of an actual screenshot is measured from the
//!   image itself ([`Layout::image_scale`]) instead of assumed, so fractional scaling and
//!   compositor quirks cannot silently produce a misaligned crop. If the image does not
//!   fit the reported layout, the crop is refused rather than guessed.

use ssx_types::{Monitor, Point, Rect, Size};

/// One monitor as reported by a layout source, in **logical** coordinates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonitorInfo {
    /// Connector name (`DP-1`, `eDP-1`) — the same string `KWin` calls the screen.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Position and size in logical (scaled) coordinates.
    pub logical: Rect,
    /// UI scale factor (1.0, 1.25, 2.0, …).
    pub scale: f64,
    /// Whether the source flags this monitor as the primary one.
    pub primary: bool,
    /// Refresh rate in hertz, when reported.
    pub refresh_hz: Option<f32>,
}

/// A set of monitors making up the virtual desktop.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Layout {
    monitors: Vec<MonitorInfo>,
}

/// Maximum relative disagreement between the horizontal and vertical scale measured from
/// a screenshot before the layout is considered not to match the image.
const SCALE_TOLERANCE: f64 = 0.03;

fn round_i32(v: f64) -> i32 {
    v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn round_u32(v: f64) -> u32 {
    v.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

impl Layout {
    /// Builds a layout, dropping monitors with an empty rectangle or a non-finite/zero scale.
    pub(crate) fn new(monitors: Vec<MonitorInfo>) -> Self {
        let monitors = monitors
            .into_iter()
            .filter(|m| !m.logical.is_empty() && m.scale.is_finite() && m.scale > 0.0)
            .collect();
        Self { monitors }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn monitors(&self) -> &[MonitorInfo] {
        &self.monitors
    }

    pub(crate) fn find(&self, id: &str) -> Option<&MonitorInfo> {
        self.monitors.iter().find(|m| m.id == id)
    }

    /// Bounding rectangle of all monitors in logical coordinates.
    pub(crate) fn logical_bounds(&self) -> Option<Rect> {
        Rect::bounding(self.monitors.iter().map(|m| m.logical))
    }

    /// Largest scale factor on the desktop (1.0 for an empty layout).
    pub(crate) fn max_scale(&self) -> f64 {
        self.monitors.iter().map(|m| m.scale).fold(1.0_f64, f64::max)
    }

    /// The public rectangle of `m`: logical rectangle times [`Layout::max_scale`].
    pub(crate) fn public_rect(&self, m: &MonitorInfo) -> Rect {
        let s = self.max_scale();
        Rect::new(
            round_i32(f64::from(m.logical.x) * s),
            round_i32(f64::from(m.logical.y) * s),
            round_u32(f64::from(m.logical.width) * s),
            round_u32(f64::from(m.logical.height) * s),
        )
    }

    /// The monitors as the `ssx` contract's [`Monitor`] type.
    pub(crate) fn to_monitors(&self) -> Vec<Monitor> {
        self.monitors
            .iter()
            .map(|m| Monitor {
                id: m.id.clone(),
                name: m.name.clone(),
                rect: self.public_rect(m),
                scale_factor: m.scale,
                primary: m.primary,
                refresh_hz: m.refresh_hz,
                hdr: None,
            })
            .collect()
    }

    /// Pixels per logical unit of a screenshot of the whole desktop, measured from the
    /// image size. `None` if the horizontal and vertical factors disagree (the image does
    /// not belong to this layout).
    pub(crate) fn image_scale(&self, image: Size) -> Option<f64> {
        let b = self.logical_bounds()?;
        let sx = f64::from(image.width) / f64::from(b.width);
        let sy = f64::from(image.height) / f64::from(b.height);
        let hi = sx.max(sy);
        ((sx - sy).abs() <= SCALE_TOLERANCE * hi && hi > 0.0).then_some(sx)
    }

    /// Where a whole-desktop image at `image_scale` sits on the virtual desktop.
    pub(crate) fn desktop_origin(&self, image_scale: f64) -> Point {
        self.logical_bounds().map_or_else(Point::default, |b| {
            Point::new(
                round_i32(f64::from(b.x) * image_scale),
                round_i32(f64::from(b.y) * image_scale),
            )
        })
    }

    /// The image-local pixel rectangle of monitor `m` inside a whole-desktop image taken
    /// at `image_scale`, clipped to `image`. `None` if nothing of it lies inside.
    pub(crate) fn crop_rect(&self, m: &MonitorInfo, image_scale: f64, image: Size) -> Option<Rect> {
        let b = self.logical_bounds()?;
        let x = round_i32((f64::from(m.logical.x) - f64::from(b.x)) * image_scale);
        let y = round_i32((f64::from(m.logical.y) - f64::from(b.y)) * image_scale);
        let w = round_u32(f64::from(m.logical.width) * image_scale);
        let h = round_u32(f64::from(m.logical.height) * image_scale);
        Rect::new(x, y, w, h).intersect(Rect::new(0, 0, image.width, image.height))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable
mod tests {
    use super::*;

    fn mon(id: &str, x: i32, y: i32, w: u32, h: u32, scale: f64) -> MonitorInfo {
        MonitorInfo {
            id: id.into(),
            name: id.into(),
            logical: Rect::new(x, y, w, h),
            scale,
            primary: false,
            refresh_hz: None,
        }
    }

    #[test]
    fn single_scale_layout_maps_to_physical_pixels() {
        // Left monitor at negative x, both scale 2.
        let l =
            Layout::new(vec![mon("L", -1920, 0, 1920, 1080, 2.0), mon("R", 0, 0, 1280, 720, 2.0)]);
        assert_eq!(l.logical_bounds(), Some(Rect::new(-1920, 0, 3200, 1080)));
        let pubs = l.to_monitors();
        assert_eq!(pubs[0].rect, Rect::new(-3840, 0, 3840, 2160));
        assert_eq!(pubs[1].rect, Rect::new(0, 0, 2560, 1440));
        assert_eq!(pubs[0].scale_factor, 2.0);
        let size = Size::new(6400, 2160);
        assert_eq!(l.image_scale(size), Some(2.0));
        assert_eq!(l.desktop_origin(2.0), Point::new(-3840, 0));
        let r = l.find("R").unwrap();
        assert_eq!(l.crop_rect(r, 2.0, size), Some(Rect::new(3840, 0, 2560, 1440)));
        let left = l.find("L").unwrap();
        assert_eq!(l.crop_rect(left, 2.0, size), Some(Rect::new(0, 0, 3840, 2160)));
    }

    #[test]
    fn fractional_scale_uses_measured_image_scale() {
        let l = Layout::new(vec![mon("A", 0, 0, 1536, 864, 1.25)]);
        assert_eq!(l.to_monitors()[0].rect, Rect::new(0, 0, 1920, 1080));
        let s = l.image_scale(Size::new(1920, 1080)).unwrap();
        assert!((s - 1.25).abs() < 1e-9);
        let m = l.find("A").unwrap();
        assert_eq!(l.crop_rect(m, s, Size::new(1920, 1080)), Some(Rect::new(0, 0, 1920, 1080)));
    }

    #[test]
    fn mismatched_image_is_rejected() {
        let l = Layout::new(vec![mon("A", 0, 0, 1920, 1080, 1.0)]);
        assert_eq!(l.image_scale(Size::new(1920, 540)), None);
        assert_eq!(l.image_scale(Size::new(0, 0)), None);
        assert_eq!(Layout::default().image_scale(Size::new(10, 10)), None);
    }

    #[test]
    fn crop_is_clipped_to_the_image() {
        let l = Layout::new(vec![mon("A", 0, 0, 100, 100, 1.0), mon("B", 100, 0, 100, 100, 1.0)]);
        let b = l.find("B").unwrap();
        // image is a bit smaller than the layout (within tolerance)
        assert_eq!(l.crop_rect(b, 1.0, Size::new(199, 100)), Some(Rect::new(100, 0, 99, 100)));
        assert_eq!(l.crop_rect(b, 1.0, Size::new(50, 100)), None);
    }

    #[test]
    fn invalid_monitors_are_dropped() {
        let l = Layout::new(vec![
            mon("zero", 0, 0, 0, 10, 1.0),
            mon("nan", 0, 0, 10, 10, f64::NAN),
            mon("neg", 0, 0, 10, 10, -1.0),
            mon("ok", 0, 0, 10, 10, 1.0),
        ]);
        assert_eq!(l.monitors().len(), 1);
        assert!(!l.is_empty());
    }

    #[test]
    fn mixed_scales_use_the_largest() {
        let l = Layout::new(vec![
            mon("lo", 0, 0, 1920, 1080, 1.0),
            mon("hi", 1920, 0, 1920, 1080, 2.0),
        ]);
        assert_eq!(l.max_scale(), 2.0);
        let m = l.to_monitors();
        assert_eq!(m[0].rect, Rect::new(0, 0, 3840, 2160));
        assert_eq!(m[1].rect, Rect::new(3840, 0, 3840, 2160));
        assert_eq!(l.image_scale(Size::new(7680, 2160)), Some(2.0));
    }
}
