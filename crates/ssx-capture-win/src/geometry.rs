//! DPI, rectangle and cursor maths (pure logic).
//!
//! All rectangles are physical pixels on the virtual desktop; nothing here depends on
//! Windows types so it can be tested anywhere. The backend requires a per-monitor-DPI-aware
//! process (see `ensure_per_monitor_dpi_aware`), otherwise Windows hands out
//! DPI-*virtualised* coordinates and none of this would line up with captured pixels.

use ssx_types::{Point, Rect};

/// Windows' 100 % DPI.
const BASE_DPI: f64 = 96.0;

/// UI scale factor for an effective DPI (`96` -> 1.0, `144` -> 1.5). A zero DPI (API
/// failure) is treated as unscaled rather than dividing to zero.
pub(crate) fn scale_factor_from_dpi(dpi: u32) -> f64 {
    if dpi == 0 { 1.0 } else { f64::from(dpi) / BASE_DPI }
}

/// Builds a [`Rect`] from Win32 `RECT` edges (right/bottom exclusive). Inverted or
/// degenerate rectangles become empty instead of wrapping around.
pub(crate) fn rect_from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    let w = (i64::from(right) - i64::from(left)).clamp(0, i64::from(u32::MAX)) as u32;
    let h = (i64::from(bottom) - i64::from(top)).clamp(0, i64::from(u32::MAX)) as u32;
    Rect::new(left, top, w, h)
}

/// Refresh rate in hertz from a `DISPLAYCONFIG_RATIONAL`. `None` when the denominator or
/// numerator is zero (not reported).
pub(crate) fn refresh_hz(numerator: u32, denominator: u32) -> Option<f32> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    Some(numerator as f32 / denominator as f32)
}

/// The size of the pixels worth copying out of a capture-pool texture.
///
/// A WGC frame carries the *content* size, which can be smaller than the pool texture
/// (the window shrank since the pool was created) or, transiently, larger. Only the
/// overlap is valid. `None` for a non-positive content size.
pub(crate) fn clamp_content_size(content: (i32, i32), texture: (u32, u32)) -> Option<(u32, u32)> {
    let w = u32::try_from(content.0).ok().filter(|w| *w > 0)?;
    let h = u32::try_from(content.1).ok().filter(|h| *h > 0)?;
    let (w, h) = (w.min(texture.0), h.min(texture.1));
    (w > 0 && h > 0).then_some((w, h))
}

/// Picks the monitor a window mostly lives on and the part of the window that lies on it.
///
/// Returns `(monitor index, window ∩ monitor)` for the monitor with the largest overlap
/// (ties go to the earlier monitor). `None` if the window overlaps no monitor.
pub(crate) fn window_crop_target(window: Rect, monitors: &[Rect]) -> Option<(usize, Rect)> {
    let mut best: Option<(usize, Rect)> = None;
    for (i, m) in monitors.iter().enumerate() {
        let Some(overlap) = window.intersect(*m) else { continue };
        if best.is_none_or(|(_, b)| overlap.area() > b.area()) {
            best = Some((i, overlap));
        }
    }
    best
}

/// Where to draw a cursor image inside a frame whose top-left is `frame_origin` on the
/// desktop, given the cursor's desktop position and the image's hotspot.
pub(crate) fn cursor_draw_position(cursor: Point, hotspot: Point, frame_origin: Point) -> Point {
    Point::new(
        cursor.x.saturating_sub(hotspot.x).saturating_sub(frame_origin.x),
        cursor.y.saturating_sub(hotspot.y).saturating_sub(frame_origin.y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_to_scale() {
        assert!((scale_factor_from_dpi(96) - 1.0).abs() < 1e-12);
        assert!((scale_factor_from_dpi(120) - 1.25).abs() < 1e-12);
        assert!((scale_factor_from_dpi(144) - 1.5).abs() < 1e-12);
        assert!((scale_factor_from_dpi(192) - 2.0).abs() < 1e-12);
        assert!((scale_factor_from_dpi(0) - 1.0).abs() < 1e-12, "failure -> unscaled");
    }

    #[test]
    fn rect_edges_including_negative_origin() {
        assert_eq!(rect_from_edges(-1920, 0, 0, 1080), Rect::new(-1920, 0, 1920, 1080));
        assert_eq!(rect_from_edges(0, 0, 2560, 1440), Rect::new(0, 0, 2560, 1440));
    }

    #[test]
    fn inverted_and_extreme_rects_do_not_wrap() {
        assert_eq!(rect_from_edges(10, 10, 5, 5).size(), ssx_types::Size::new(0, 0));
        let r = rect_from_edges(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert_eq!(r.width, u32::MAX);
        assert_eq!(r.height, u32::MAX);
    }

    #[test]
    fn refresh_rate_from_rational() {
        assert_eq!(refresh_hz(60, 1), Some(60.0));
        let hz = refresh_hz(143_856, 1000).unwrap();
        assert!((hz - 143.856).abs() < 1e-3);
        assert_eq!(refresh_hz(0, 0), None);
        assert_eq!(refresh_hz(60, 0), None);
        assert_eq!(refresh_hz(0, 1), None);
    }

    #[test]
    fn content_size_is_clamped_to_the_texture() {
        assert_eq!(clamp_content_size((800, 600), (1920, 1080)), Some((800, 600)));
        assert_eq!(clamp_content_size((3000, 600), (1920, 1080)), Some((1920, 600)));
        assert_eq!(clamp_content_size((800, 2000), (1920, 1080)), Some((800, 1080)));
    }

    #[test]
    fn content_size_rejects_non_positive_and_empty_textures() {
        assert_eq!(clamp_content_size((0, 600), (1920, 1080)), None);
        assert_eq!(clamp_content_size((800, -1), (1920, 1080)), None);
        assert_eq!(clamp_content_size((-5, -5), (1920, 1080)), None);
        assert_eq!(clamp_content_size((10, 10), (0, 1080)), None);
    }

    #[test]
    fn window_picks_monitor_with_largest_overlap() {
        let left = Rect::new(-1920, 0, 1920, 1080);
        let right = Rect::new(0, 0, 2560, 1440);
        // 100 px on the left monitor, 400 px on the right one.
        let w = Rect::new(-100, 100, 500, 300);
        assert_eq!(window_crop_target(w, &[left, right]), Some((1, Rect::new(0, 100, 400, 300))));
        // Entirely on the left monitor.
        let w = Rect::new(-500, 10, 200, 200);
        assert_eq!(window_crop_target(w, &[left, right]), Some((0, w)));
    }

    #[test]
    fn window_overlap_tie_prefers_first_and_none_when_offscreen() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(100, 0, 100, 100);
        let w = Rect::new(50, 0, 100, 100);
        assert_eq!(window_crop_target(w, &[a, b]).map(|t| t.0), Some(0));
        assert_eq!(window_crop_target(Rect::new(500, 500, 10, 10), &[a, b]), None);
        assert_eq!(window_crop_target(w, &[]), None);
        assert_eq!(window_crop_target(Rect::new(0, 0, 0, 0), &[a]), None, "empty window");
    }

    #[test]
    fn cursor_position_accounts_for_hotspot_and_origin() {
        let p = cursor_draw_position(Point::new(110, 220), Point::new(4, 6), Point::new(100, 200));
        assert_eq!(p, Point::new(6, 14));
        // Frame on a monitor with a negative origin.
        let p = cursor_draw_position(Point::new(-1900, 50), Point::new(0, 0), Point::new(-1920, 0));
        assert_eq!(p, Point::new(20, 50));
        // Cursor left of the frame yields a negative (clipped-by-GDI) position, no overflow.
        let p = cursor_draw_position(Point::new(i32::MIN, 0), Point::new(10, 0), Point::default());
        assert_eq!(p.x, i32::MIN);
    }
}
