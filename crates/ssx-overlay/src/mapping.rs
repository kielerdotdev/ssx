//! Compositor-logical space to virtual-desktop pixel mapping.
//!
//! Wayland speaks *logical* pixels (an output's `xdg_output` rectangle, a surface's
//! configure size, pointer coordinates local to the surface); the workspace speaks
//! *physical desktop* pixels (`ssx_types::Monitor::rect`). The capture crate's coordinate
//! model (`ssx-capture-wayland/docs/wayland-coordinates.md`) defines the bridge: one global
//! scale `S = max(per-output scale)` and `Monitor.rect edge = round(logical edge * S)`.
//!
//! The overlay never needs `S` itself. It needs (a) which `Monitor` a `wl_output` is, and
//! (b) for a pointer position local to that output's surface, the desktop pixel. (b) is a
//! ratio of two sizes we know: `monitor.rect.size / surface logical size`. That holds for
//! fractional scales, mixed scales and the resampled low-DPI monitors of the model alike.

use ssx_types::{Monitor, Point, Rect};

/// An output as the compositor describes it, in logical pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalOutput {
    /// `wl_output.name` / xdg-output name, if advertised.
    pub name: Option<String>,
    /// Logical x.
    pub x: i32,
    /// Logical y.
    pub y: i32,
    /// Logical width.
    pub width: i32,
    /// Logical height.
    pub height: i32,
}

/// Finds which [`Monitor`] each output is. `result[i]` is the index into `monitors` for
/// `outputs[i]`; every monitor is used at most once.
///
/// Names are trusted when unambiguous. Otherwise geometry decides: for each candidate
/// scale `S` implied by an (output, monitor) pair, count how many outputs land on a monitor
/// rectangle within rounding tolerance, and keep the best `S`.
pub fn match_outputs(outputs: &[LogicalOutput], monitors: &[Monitor]) -> Vec<Option<usize>> {
    let mut result: Vec<Option<usize>> = vec![None; outputs.len()];
    let mut taken = vec![false; monitors.len()];

    // 1. Unambiguous names (Monitor.name is the compositor's output name on wlroots).
    for (i, o) in outputs.iter().enumerate() {
        let Some(name) = &o.name else { continue };
        let hits: Vec<usize> = monitors
            .iter()
            .enumerate()
            .filter(|(j, m)| !taken[*j] && (&m.name == name || &m.id == name))
            .map(|(j, _)| j)
            .collect();
        let dup_outputs = outputs.iter().filter(|p| p.name.as_ref() == Some(name)).count();
        if let ([j], 1) = (hits.as_slice(), dup_outputs) {
            result[i] = Some(*j);
            taken[*j] = true;
        }
    }

    // 2. Geometry for the rest.
    let rest: Vec<usize> = (0..outputs.len()).filter(|i| result[*i].is_none()).collect();
    if rest.is_empty() {
        return result;
    }
    let free = |taken: &[bool]| (0..monitors.len()).filter(|j| !taken[*j]).collect::<Vec<_>>();
    let mut best: Option<(usize, f64)> = None;
    for &i in &rest {
        let o = &outputs[i];
        if o.width <= 0 {
            continue;
        }
        for j in free(&taken) {
            let s = f64::from(monitors[j].rect.width) / f64::from(o.width);
            let score = rest
                .iter()
                .filter(|&&k| find_at_scale(&outputs[k], s, monitors, &taken).is_some())
                .count();
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, s));
            }
        }
    }
    if let Some((_, s)) = best {
        for &i in &rest {
            if let Some(j) = find_at_scale(&outputs[i], s, monitors, &taken) {
                result[i] = Some(j);
                taken[j] = true;
            }
        }
    }
    result
}

fn find_at_scale(o: &LogicalOutput, s: f64, monitors: &[Monitor], taken: &[bool]) -> Option<usize> {
    let tol = s.ceil().max(2.0) as i64;
    let want = |v: i32| (f64::from(v) * s).round() as i64;
    monitors.iter().enumerate().find_map(|(j, m)| {
        let r = m.rect;
        let ok = !taken[j]
            && (i64::from(r.x) - want(o.x)).abs() <= tol
            && (i64::from(r.y) - want(o.y)).abs() <= tol
            && (i64::from(r.width) - want(o.width)).abs() <= tol
            && (i64::from(r.height) - want(o.height)).abs() <= tol;
        ok.then_some(j)
    })
}

/// Maps a pointer position local to an output's surface (logical, possibly fractional) to a
/// desktop pixel. `surface` is the surface's logical size as configured by the compositor.
///
/// Positions outside the surface are **not** clamped: during a button press Wayland keeps
/// delivering motion to the surface that received the press, with coordinates that run past
/// its edges, and that is exactly how a drag continues onto the neighbouring monitor. The
/// selection model clamps to the whole desktop.
pub fn surface_to_desktop(monitor: Rect, surface: (u32, u32), local: (f64, f64)) -> Point {
    const LIMIT: f64 = 1.0e7;
    let (sw, sh) = (f64::from(surface.0.max(1)), f64::from(surface.1.max(1)));
    let px = (local.0 * f64::from(monitor.width) / sw).floor().clamp(-LIMIT, LIMIT);
    let py = (local.1 * f64::from(monitor.height) / sh).floor().clamp(-LIMIT, LIMIT);
    Point::new(monitor.x.saturating_add(px as i32), monitor.y.saturating_add(py as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, y: i32, w: u32, h: u32) -> Monitor {
        Monitor {
            id: format!("id-{name}"),
            name: name.into(),
            rect: Rect::new(x, y, w, h),
            scale_factor: 1.0,
            primary: false,
            refresh_hz: None,
            hdr: None,
        }
    }

    fn out(name: Option<&str>, x: i32, y: i32, w: i32, h: i32) -> LogicalOutput {
        LogicalOutput { name: name.map(Into::into), x, y, width: w, height: h }
    }

    #[test]
    fn doc_example_mixed_scale_matches_by_geometry_even_without_names() {
        // 800x600 @1x at (0,0) + 640x480 @2x (logical 320x240) at (800,0) -> S = 2.
        let monitors = [mon("A", 0, 0, 1600, 1200), mon("B", 1600, 0, 640, 480)];
        let outputs = [out(None, 800, 0, 320, 240), out(None, 0, 0, 800, 600)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![Some(1), Some(0)]);
    }

    #[test]
    fn doc_example_fractional_scale() {
        let monitors = [mon("A", 0, 0, 960, 720)];
        assert_eq!(match_outputs(&[out(None, 0, 0, 640, 480)], &monitors), vec![Some(0)]);
    }

    #[test]
    fn doc_example_two_4k_at_2x() {
        let monitors = [mon("L", 0, 0, 3840, 2160), mon("R", 3840, 0, 3840, 2160)];
        let outputs = [out(None, 1920, 0, 1920, 1080), out(None, 0, 0, 1920, 1080)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![Some(1), Some(0)]);
    }

    #[test]
    fn rounding_of_fractional_layouts_is_tolerated() {
        // 2560 px at 1.5x = 1707 logical px; second output at logical x=1707.
        let monitors = [mon("A", 0, 0, 2561, 1440), mon("B", 2561, 0, 1920, 1080)];
        let outputs = [out(None, 0, 0, 1707, 960), out(None, 1707, 0, 1280, 720)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![Some(0), Some(1)]);
    }

    #[test]
    fn names_win_and_negative_origins_work() {
        let monitors = [mon("HDMI-A-1", -1920, 0, 1920, 1080), mon("DP-1", 0, 0, 2560, 1440)];
        let outputs = [out(Some("DP-1"), 0, 0, 2560, 1440), out(Some("HDMI-A-1"), -1920, 0, 1920, 1080)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![Some(1), Some(0)]);
        // Same layout, no names, negative logical origin.
        let outputs = [out(None, 0, 0, 2560, 1440), out(None, -1920, 0, 1920, 1080)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![Some(1), Some(0)]);
    }

    #[test]
    fn mirrored_outputs_each_get_their_own_monitor() {
        let monitors = [mon("A", 0, 0, 1920, 1080), mon("B", 0, 0, 1920, 1080)];
        let outputs = [out(None, 0, 0, 1920, 1080), out(None, 0, 0, 1920, 1080)];
        let m = match_outputs(&outputs, &monitors);
        assert!(m[0].is_some() && m[1].is_some() && m[0] != m[1], "{m:?}");
    }

    #[test]
    fn unknown_outputs_stay_unmatched() {
        let monitors = [mon("A", 0, 0, 1920, 1080)];
        let outputs = [out(None, 5000, 5000, 100, 100)];
        assert_eq!(match_outputs(&outputs, &monitors), vec![None]);
        assert!(match_outputs(&[], &monitors).is_empty());
        assert_eq!(match_outputs(&[out(None, 0, 0, 0, 0)], &monitors), vec![None]);
    }

    #[test]
    fn pointer_mapping_uses_the_rect_to_surface_ratio() {
        // Low-DPI monitor resampled 2x: logical 800x600 surface, desktop rect 1600x1200 at 0.
        let r = Rect::new(0, 0, 1600, 1200);
        assert_eq!(surface_to_desktop(r, (800, 600), (0.0, 0.0)), Point::new(0, 0));
        assert_eq!(surface_to_desktop(r, (800, 600), (10.5, 20.25)), Point::new(21, 40));
        assert_eq!(surface_to_desktop(r, (800, 600), (799.9, 599.9)), Point::new(1599, 1199));
        // Second monitor at an offset; 1:1 (highest scale).
        let r2 = Rect::new(1600, 0, 640, 480);
        assert_eq!(surface_to_desktop(r2, (320, 240), (100.0, 100.0)), Point::new(1800, 200));
        // Out-of-range positions (an implicit grab dragging past the edge) continue linearly,
        // so a drag can leave this monitor for the next one.
        assert_eq!(surface_to_desktop(r2, (320, 240), (-5.0, 250.0)), Point::new(1590, 500));
        assert_eq!(surface_to_desktop(r2, (320, 240), (330.0, 0.0)), Point::new(2260, 0));
        assert_eq!(surface_to_desktop(r2, (320, 240), (f64::MAX, f64::NAN)).y, 0);
        // Negative origin.
        let r3 = Rect::new(-1920, 0, 1920, 1080);
        assert_eq!(surface_to_desktop(r3, (1920, 1080), (5.0, 5.0)), Point::new(-1915, 5));
        // Degenerate surface size does not divide by zero.
        assert_eq!(surface_to_desktop(r3, (0, 0), (1.0, 1.0)), Point::new(-1919, 1));
    }
}
