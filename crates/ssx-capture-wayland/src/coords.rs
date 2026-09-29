//! The coordinate model. See `docs/wayland-coordinates.md` for the full rationale.
//!
//! Wayland lays outputs out in *logical* pixels, but frames are captured in *physical*
//! pixels. `ssx` promises that `Monitor.rect` is the rectangle its captured frame covers
//! on the virtual desktop, so both must be reconciled by one rule:
//!
//! > **virtual desktop = logical layout × S**, where `S` is the largest effective scale of
//! > any monitor.
//!
//! The highest-DPI monitor is therefore pixel-exact (`rect.size == native pixels`), and in
//! a uniform-scale layout *every* monitor is. Lower-scale monitors cover more desktop
//! pixels than they have native pixels and are resampled up (`resample.rs`).
//!
//! This module is pure maths so it can be unit-tested without a compositor.

use ssx_types::{Rect, Size};

use crate::transform::Transform;

/// One output as described by the compositor, before any desktop mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputGeom {
    /// Stable-for-the-session id (the connector name where known).
    pub id: String,
    /// Layout position and size in logical pixels.
    pub logical: Rect,
    /// Current mode in scan-out orientation (what the framebuffer holds).
    pub mode: Size,
    pub transform: Transform,
    /// Integer `wl_output.scale`.
    pub int_scale: i32,
}

impl OutputGeom {
    /// The pixel size of the output as the user sees it (transform applied).
    pub fn displayed(&self) -> Size {
        let (w, h) = self.transform.upright_size(self.mode.width, self.mode.height);
        Size::new(w, h)
    }
}

/// Effective (possibly fractional) scale: physical pixels per logical pixel.
///
/// Compositors round the logical size of fractionally scaled outputs (2560 px at 1.5x is
/// 1706.67 → 1707 logical), so the raw ratio is 1.4997. If a multiple of 1/120 (the unit
/// of `wp-fractional-scale`) reproduces the logical size exactly it is used instead, so
/// callers see 1.5.
pub fn effective_scale(displayed: Size, logical: Size) -> f64 {
    if logical.is_empty() || displayed.is_empty() {
        return 1.0;
    }
    let raw = f64::from(displayed.width) / f64::from(logical.width);
    let snapped = (raw * 120.0).round() / 120.0;
    let consistent = snapped > 0.0
        && (f64::from(displayed.width) / snapped).round() as u32 == logical.width
        && (f64::from(displayed.height) / snapped).round() as u32 == logical.height;
    if consistent { snapped } else { raw }
}

/// An output placed on the virtual desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorGeom {
    pub id: String,
    /// Rectangle on the virtual desktop, in desktop pixels.
    pub rect: Rect,
    /// This monitor's own effective scale.
    pub scale: f64,
    /// Native pixel size of the displayed image (transform applied).
    pub native: Size,
    pub transform: Transform,
    pub int_scale: i32,
    pub logical: Rect,
}

impl MonitorGeom {
    /// `true` when the captured frame is used as-is (no resampling).
    pub fn is_pixel_exact(&self) -> bool {
        self.native == self.rect.size()
    }
}

/// How to fetch part of a monitor straight from the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectRegion {
    /// Region in output-local **logical** pixels for the protocol request.
    pub logical: Rect,
    /// Where the wanted pixels sit inside the frame the compositor returns.
    pub crop: Rect,
}

/// One monitor's contribution to a region capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Index into [`Layout::monitors`].
    pub monitor: usize,
    /// The overlap in desktop pixels.
    pub overlap: Rect,
    /// `Some` when a protocol-level region capture is exact for this part.
    pub direct: Option<DirectRegion>,
}

/// The whole virtual desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// `S`: desktop pixels per logical pixel.
    pub desktop_scale: f64,
    pub monitors: Vec<MonitorGeom>,
}

fn round_i32(v: f64) -> i32 {
    v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

impl Layout {
    /// Builds the layout. `force_scale` overrides `S` (useful to pin a 1:1 desktop, e.g.
    /// `Some(1.0)` gives the raw logical layout).
    pub fn build(outputs: &[OutputGeom], force_scale: Option<f64>) -> Layout {
        let usable: Vec<&OutputGeom> =
            outputs.iter().filter(|o| !o.logical.is_empty() && !o.mode.is_empty()).collect();
        let scales: Vec<f64> =
            usable.iter().map(|o| effective_scale(o.displayed(), o.logical.size())).collect();
        let max = scales.iter().copied().fold(0.0f64, f64::max);
        let s = force_scale.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(if max > 0.0 {
            max
        } else {
            1.0
        });
        let monitors = usable
            .iter()
            .zip(&scales)
            .map(|(o, &scale)| {
                let native = o.displayed();
                let l = o.logical;
                let x0 = round_i32(f64::from(l.x) * s);
                let y0 = round_i32(f64::from(l.y) * s);
                let (w, h) = if (scale - s).abs() < 1e-9 {
                    // Highest-DPI monitor: exactly its native pixels.
                    (native.width, native.height)
                } else {
                    let x1 = round_i32(l.right() as f64 * s);
                    let y1 = round_i32(l.bottom() as f64 * s);
                    ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32)
                };
                MonitorGeom {
                    id: o.id.clone(),
                    rect: Rect::new(x0, y0, w, h),
                    scale,
                    native,
                    transform: o.transform,
                    int_scale: o.int_scale,
                    logical: l,
                }
            })
            .collect();
        Layout { desktop_scale: s, monitors }
    }

    /// Bounding rectangle of all monitors.
    pub fn bounds(&self) -> Option<Rect> {
        Rect::bounding(self.monitors.iter().map(|m| m.rect))
    }

    /// Index of the "primary" monitor: the one at the layout origin, else the first.
    pub fn primary(&self) -> Option<usize> {
        self.monitors
            .iter()
            .position(|m| m.logical.x == 0 && m.logical.y == 0)
            .or(if self.monitors.is_empty() { None } else { Some(0) })
    }

    /// Converts a rectangle in the compositor's logical space (as reported by sway or
    /// Hyprland IPC) to desktop pixels. Edges are rounded, not the size, so adjacent
    /// rectangles stay adjacent.
    pub fn logical_to_desktop(&self, r: Rect) -> Rect {
        let s = self.desktop_scale;
        let x0 = round_i32(f64::from(r.x) * s);
        let y0 = round_i32(f64::from(r.y) * s);
        let x1 = round_i32(r.right() as f64 * s);
        let y1 = round_i32(r.bottom() as f64 * s);
        Rect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
    }

    /// Splits a desktop-pixel region into per-monitor parts. Regions (or parts of them)
    /// outside every monitor simply yield no part.
    pub fn plan(&self, region: Rect) -> Vec<Part> {
        let mut parts = Vec::new();
        for (i, m) in self.monitors.iter().enumerate() {
            let Some(overlap) = m.rect.intersect(region) else { continue };
            let direct = Self::direct_region(m, overlap);
            parts.push(Part { monitor: i, overlap, direct });
        }
        parts
    }

    /// A protocol-level region is exact only for pixel-exact, untransformed monitors at an
    /// integer scale: the protocols take *logical* regions, so a fractional scale would
    /// force rounding of the request and a sub-pixel guess about where the compositor put
    /// the buffer edge. Everything else falls back to "capture whole output, then crop".
    fn direct_region(m: &MonitorGeom, overlap: Rect) -> Option<DirectRegion> {
        let s = m.scale.round();
        if !m.is_pixel_exact()
            || m.transform != Transform::Normal
            || s < 1.0
            || (m.scale - s).abs() > 1e-9
        {
            return None;
        }
        let s = s as i64;
        let local = overlap.translate(-m.rect.x, -m.rect.y);
        let (x0, y0) = (i64::from(local.x), i64::from(local.y));
        let (x1, y1) = (local.right(), local.bottom());
        let lx0 = x0.div_euclid(s);
        let ly0 = y0.div_euclid(s);
        let lx1 = (x1 + s - 1).div_euclid(s);
        let ly1 = (y1 + s - 1).div_euclid(s);
        let logical = Rect::new(lx0 as i32, ly0 as i32, (lx1 - lx0) as u32, (ly1 - ly0) as u32);
        let crop =
            Rect::new((x0 - lx0 * s) as i32, (y0 - ly0 * s) as i32, local.width, local.height);
        Some(DirectRegion { logical, crop })
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // scales are snapped or copied, never accumulated: exact by design
mod tests {
    use super::*;

    fn out(
        id: &str,
        (lx, ly, lw, lh): (i32, i32, u32, u32),
        (mw, mh): (u32, u32),
        scale: i32,
    ) -> OutputGeom {
        OutputGeom {
            id: id.into(),
            logical: Rect::new(lx, ly, lw, lh),
            mode: Size::new(mw, mh),
            transform: Transform::Normal,
            int_scale: scale,
        }
    }

    #[test]
    fn effective_scale_snaps_fractional_scales() {
        assert_eq!(effective_scale(Size::new(1920, 1080), Size::new(1920, 1080)), 1.0);
        assert_eq!(effective_scale(Size::new(3840, 2160), Size::new(1920, 1080)), 2.0);
        // 2560x1440 at 1.5x: compositor reports 1707x960.
        assert_eq!(effective_scale(Size::new(2560, 1440), Size::new(1707, 960)), 1.5);
        // 1.25x on 1920x1080 -> 1536x864 exactly.
        assert_eq!(effective_scale(Size::new(1920, 1080), Size::new(1536, 864)), 1.25);
        // Garbage in, sane out.
        assert_eq!(effective_scale(Size::new(0, 0), Size::new(0, 0)), 1.0);
    }

    #[test]
    fn uniform_layout_is_physical_pixels() {
        // Two 2x monitors side by side: logical 1920x1080 each, native 3840x2160.
        let l = Layout::build(
            &[
                out("A", (0, 0, 1920, 1080), (3840, 2160), 2),
                out("B", (1920, 0, 1920, 1080), (3840, 2160), 2),
            ],
            None,
        );
        assert_eq!(l.desktop_scale, 2.0);
        assert_eq!(l.monitors[0].rect, Rect::new(0, 0, 3840, 2160));
        assert_eq!(l.monitors[1].rect, Rect::new(3840, 0, 3840, 2160));
        assert!(l.monitors.iter().all(MonitorGeom::is_pixel_exact));
        assert_eq!(l.bounds(), Some(Rect::new(0, 0, 7680, 2160)));
    }

    #[test]
    fn mixed_scale_layout_uses_max_scale_and_marks_lower_monitor_for_resampling() {
        // 1x 1920x1080 at left, 2x (native 2560x1440, logical 1280x720) at right.
        let l = Layout::build(
            &[
                out("lo", (0, 0, 1920, 1080), (1920, 1080), 1),
                out("hi", (1920, 0, 1280, 720), (2560, 1440), 2),
            ],
            None,
        );
        assert_eq!(l.desktop_scale, 2.0);
        assert_eq!(l.monitors[0].rect, Rect::new(0, 0, 3840, 2160));
        assert!(!l.monitors[0].is_pixel_exact());
        assert_eq!(l.monitors[0].native, Size::new(1920, 1080));
        assert_eq!(l.monitors[1].rect, Rect::new(3840, 0, 2560, 1440));
        assert!(l.monitors[1].is_pixel_exact());
        // Monitors stay adjacent: no gap or overlap introduced by rounding.
        assert_eq!(l.monitors[0].rect.right(), i64::from(l.monitors[1].rect.x));
    }

    #[test]
    fn fractional_scale_and_negative_origin() {
        let l = Layout::build(
            &[
                out("L", (-1707, 0, 1707, 960), (2560, 1440), 2),
                out("R", (0, 0, 1707, 960), (2560, 1440), 2),
            ],
            None,
        );
        assert_eq!(l.desktop_scale, 1.5);
        assert_eq!(l.monitors[0].rect, Rect::new(-2561, 0, 2560, 1440));
        assert_eq!(l.monitors[1].rect, Rect::new(0, 0, 2560, 1440));
        // Half-pixel rounding may leave a 1px seam but never an overlap.
        assert!(l.monitors[0].rect.right() <= i64::from(l.monitors[1].rect.x));
    }

    #[test]
    fn rotated_output_uses_displayed_size() {
        let mut o = out("V", (0, 0, 1080, 1920), (1920, 1080), 1);
        o.transform = Transform::Rot90;
        assert_eq!(o.displayed(), Size::new(1080, 1920));
        let l = Layout::build(&[o], None);
        assert_eq!(l.monitors[0].rect, Rect::new(0, 0, 1080, 1920));
        assert_eq!(l.monitors[0].native, Size::new(1080, 1920));
        assert!(l.monitors[0].is_pixel_exact());
    }

    #[test]
    fn force_scale_overrides_and_bad_values_are_ignored() {
        let outs = [out("A", (10, 20, 100, 50), (200, 100), 2)];
        let forced = Layout::build(&outs, Some(1.0));
        assert_eq!(forced.desktop_scale, 1.0);
        assert_eq!(forced.monitors[0].rect, Rect::new(10, 20, 100, 50));
        assert!(!forced.monitors[0].is_pixel_exact(), "native is 200x100 -> downsampled");
        assert_eq!(Layout::build(&outs, Some(f64::NAN)).desktop_scale, 2.0);
        assert_eq!(Layout::build(&outs, Some(-3.0)).desktop_scale, 2.0);
    }

    #[test]
    fn degenerate_outputs_are_dropped() {
        let l = Layout::build(&[out("off", (0, 0, 0, 0), (0, 0), 1)], None);
        assert!(l.monitors.is_empty());
        assert_eq!(l.bounds(), None);
        assert_eq!(l.primary(), None);
    }

    #[test]
    fn logical_rects_map_by_edges() {
        let l = Layout::build(&[out("A", (0, 0, 100, 100), (150, 150), 2)], None);
        assert_eq!(l.desktop_scale, 1.5);
        // 1,1 .. 4,4 logical -> 1.5,1.5 .. 6,6 -> edges round to 2 .. 6
        assert_eq!(l.logical_to_desktop(Rect::new(1, 1, 3, 3)), Rect::new(2, 2, 4, 4));
    }

    #[test]
    fn plan_uses_direct_region_only_when_exact() {
        // 2x output at logical (0,0): desktop px = 2 * logical.
        let l = Layout::build(&[out("A", (0, 0, 320, 240), (640, 480), 2)], None);
        // Odd, unaligned desktop region: 5,7 .. 12,10 -> logical 2,3 .. 6,5.
        let parts = l.plan(Rect::new(5, 7, 7, 3));
        assert_eq!(parts.len(), 1);
        let d = parts[0].direct.expect("integer scale is exact");
        assert_eq!(d.logical, Rect::new(2, 3, 4, 2));
        // The compositor returns 8x4 px starting at desktop px (4,6): crop (1,1,7,3).
        assert_eq!(d.crop, Rect::new(1, 1, 7, 3));
        assert!(d.crop.right() <= i64::from(d.logical.width) * 2);
        assert!(d.crop.bottom() <= i64::from(d.logical.height) * 2);
    }

    #[test]
    fn plan_falls_back_for_fractional_rotated_and_resampled() {
        let mut rot = out("R", (0, 0, 1080, 1920), (1920, 1080), 1);
        rot.transform = Transform::Rot90;
        let frac = out("F", (2000, 0, 1707, 960), (2560, 1440), 2);
        let lo = out("lo", (5000, 0, 800, 600), (800, 600), 1);
        let l = Layout::build(&[rot, frac, lo], None);
        for m in &l.monitors {
            let parts = l.plan(m.rect);
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].direct, None, "monitor {}", m.id);
        }
    }

    #[test]
    fn plan_spanning_two_monitors_and_gaps() {
        let l = Layout::build(
            &[
                out("A", (0, 0, 100, 100), (100, 100), 1),
                out("B", (150, 0, 100, 100), (100, 100), 1),
            ],
            None,
        );
        let parts = l.plan(Rect::new(90, 10, 100, 10));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].overlap, Rect::new(90, 10, 10, 10));
        assert_eq!(parts[1].overlap, Rect::new(150, 10, 40, 10));
        assert!(l.plan(Rect::new(100, 0, 50, 50)).is_empty(), "the gap is nobody's");
        assert!(l.plan(Rect::new(-50, -50, 10, 10)).is_empty());
    }

    #[test]
    fn primary_prefers_origin_monitor() {
        let l = Layout::build(
            &[
                out("A", (-100, 0, 100, 100), (100, 100), 1),
                out("B", (0, 0, 100, 100), (100, 100), 1),
            ],
            None,
        );
        assert_eq!(l.primary(), Some(1));
    }
}
