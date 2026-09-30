//! Pure geometry helpers: handles, snapping, rectangle subtraction, shape masks.
//!
//! Everything works on the workspace's integer physical-pixel [`Rect`]/[`Point`]. Edge
//! coordinates are widened to `i64` internally so hostile inputs cannot overflow.

use ssx_types::{Point, Rect};

/// One of the eight resize handles of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handle {
    /// Top-left corner.
    NorthWest,
    /// Top edge.
    North,
    /// Top-right corner.
    NorthEast,
    /// Right edge.
    East,
    /// Bottom-right corner.
    SouthEast,
    /// Bottom edge.
    South,
    /// Bottom-left corner.
    SouthWest,
    /// Left edge.
    West,
}

impl Handle {
    /// Corners first, so they win hit-tests where handles overlap on small selections.
    pub const ALL: [Handle; 8] = [
        Handle::NorthWest,
        Handle::NorthEast,
        Handle::SouthEast,
        Handle::SouthWest,
        Handle::North,
        Handle::East,
        Handle::South,
        Handle::West,
    ];

    /// `true` for the four corner handles.
    pub const fn is_corner(self) -> bool {
        matches!(self, Handle::NorthWest | Handle::NorthEast | Handle::SouthEast | Handle::SouthWest)
    }

    /// Which edges this handle moves: `(left, top, right, bottom)`.
    pub const fn moves(self) -> (bool, bool, bool, bool) {
        match self {
            Handle::NorthWest => (true, true, false, false),
            Handle::North => (false, true, false, false),
            Handle::NorthEast => (false, true, true, false),
            Handle::East => (false, false, true, false),
            Handle::SouthEast => (false, false, true, true),
            Handle::South => (false, false, false, true),
            Handle::SouthWest => (true, false, false, true),
            Handle::West => (true, false, false, false),
        }
    }

    /// Centre of the handle on `rect`'s outline.
    pub fn centre(self, rect: Rect) -> (i64, i64) {
        let l = i64::from(rect.x);
        let t = i64::from(rect.y);
        let r = rect.right();
        let b = rect.bottom();
        let mx = (l + r) / 2;
        let my = (t + b) / 2;
        match self {
            Handle::NorthWest => (l, t),
            Handle::North => (mx, t),
            Handle::NorthEast => (r, t),
            Handle::East => (r, my),
            Handle::SouthEast => (r, b),
            Handle::South => (mx, b),
            Handle::SouthWest => (l, b),
            Handle::West => (l, my),
        }
    }
}

/// Builds a normalised rectangle from possibly unordered edge coordinates.
pub fn rect_from_edges(l: i64, t: i64, r: i64, b: i64) -> Rect {
    let (x0, x1) = (l.min(r), l.max(r));
    let (y0, y1) = (t.min(b), t.max(b));
    Rect {
        x: x0.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: y0.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        width: x1.saturating_sub(x0).clamp(0, i64::from(u32::MAX)) as u32,
        height: y1.saturating_sub(y0).clamp(0, i64::from(u32::MAX)) as u32,
    }
}

/// Which handles exist for `rect`: the edge-midpoint handles are dropped on sides too short
/// to hold them without overlapping their corner neighbours.
pub fn visible_handles(rect: Rect, size: u32) -> Vec<Handle> {
    Handle::ALL
        .into_iter()
        .filter(|h| match h {
            Handle::North | Handle::South => rect.width >= size.saturating_mul(3),
            Handle::East | Handle::West => rect.height >= size.saturating_mul(3),
            _ => true,
        })
        .collect()
}

/// The square drawn for `handle`, `size` pixels wide, centred on the outline.
pub fn handle_rect(rect: Rect, handle: Handle, size: u32) -> Rect {
    let (cx, cy) = handle.centre(rect);
    let half = i64::from(size / 2);
    rect_from_edges(cx - half, cy - half, cx - half + i64::from(size), cy - half + i64::from(size))
}

/// The handle under `p`, allowing `slop` extra pixels around each handle square.
pub fn handle_at(rect: Rect, p: Point, size: u32, slop: u32) -> Option<Handle> {
    visible_handles(rect, size).into_iter().find(|h| {
        let hr = handle_rect(rect, *h, size);
        inflate_i64(hr, i64::from(slop)).contains(p)
    })
}

/// `Rect::inflate` with an `i64` amount (never overflows; deflating past zero size gives an
/// empty rectangle rather than a flipped one).
pub fn inflate_i64(r: Rect, by: i64) -> Rect {
    if by < 0 && (i64::from(r.width) + 2 * by <= 0 || i64::from(r.height) + 2 * by <= 0) {
        return Rect { width: 0, height: 0, ..r };
    }
    rect_from_edges(i64::from(r.x) - by, i64::from(r.y) - by, r.right() + by, r.bottom() + by)
}

/// `a` minus `b`: up to four disjoint rectangles covering the part of `a` outside `b`.
pub fn subtract(a: Rect, b: Rect) -> Vec<Rect> {
    if a.is_empty() {
        return Vec::new();
    }
    let Some(i) = a.intersect(b) else { return vec![a] };
    let (al, at, ar, ab) = (i64::from(a.x), i64::from(a.y), a.right(), a.bottom());
    let (il, it, ir, ib) = (i64::from(i.x), i64::from(i.y), i.right(), i.bottom());
    let mut out = Vec::with_capacity(4);
    let mut push = |l, t, r, bt| {
        let rc = rect_from_edges(l, t, r, bt);
        if !rc.is_empty() {
            out.push(rc);
        }
    };
    push(al, at, ar, it); // top band, full width
    push(al, ib, ar, ab); // bottom band, full width
    push(al, it, il, ib); // left of the hole
    push(ir, it, ar, ib); // right of the hole
    out
}

/// The frame around `r`: everything within `outer` pixels of it except the part `inner`
/// pixels inside it.
pub fn ring(r: Rect, inner: u32, outer: u32) -> Vec<Rect> {
    let big = inflate_i64(r, i64::from(outer));
    let hole = inflate_i64(r, -i64::from(inner));
    subtract(big, hole)
}

/// Translates `r` so it lies inside `bounds` (shrinking it if it is larger).
pub fn clamp_inside(r: Rect, bounds: Rect) -> Rect {
    let w = i64::from(r.width.min(bounds.width));
    let h = i64::from(r.height.min(bounds.height));
    let x = i64::from(r.x).clamp(i64::from(bounds.x), bounds.right() - w);
    let y = i64::from(r.y).clamp(i64::from(bounds.y), bounds.bottom() - h);
    rect_from_edges(x, y, x + w, y + h)
}

/// The candidate closest to `v` within `threshold`, if any.
pub fn snap_value(v: i64, candidates: &[i64], threshold: i64) -> Option<i64> {
    candidates
        .iter()
        .copied()
        .filter(|c| (c - v).abs() <= threshold)
        .min_by_key(|c| ((c - v).abs(), *c))
}

/// The pixels of row `y` inside the ellipse inscribed in `rect`, as a half-open x range.
///
/// Pixel-centre sampling. This is the single definition of the ellipse mask: the renderer
/// paints exactly these pixels and [`crate::Selection::contains`] agrees by construction, so
/// what the user sees bright is what gets captured.
pub fn ellipse_span(rect: Rect, y: i64) -> Option<(i64, i64)> {
    if rect.is_empty() {
        return None;
    }
    let rx = f64::from(rect.width) / 2.0;
    let ry = f64::from(rect.height) / 2.0;
    let cx = f64::from(rect.x) + rx;
    let cy = f64::from(rect.y) + ry;
    let dy = ((y as f64) + 0.5 - cy) / ry;
    let t = 1.0 - dy * dy;
    if t < 0.0 {
        return None;
    }
    let hw = rx * t.sqrt();
    // x is inside iff |x + 0.5 - cx| <= hw.
    let x0 = (cx - hw - 0.5).ceil() as i64;
    let x1 = (cx + hw - 0.5).floor() as i64 + 1;
    (x1 > x0).then_some((x0, x1))
}

/// Pixel-centre test against the ellipse inscribed in `rect`.
pub fn in_ellipse(rect: Rect, p: Point) -> bool {
    ellipse_span(rect, i64::from(p.y)).is_some_and(|(a, b)| (a..b).contains(&i64::from(p.x)))
}

/// The pixels of row `y` inside the closed polygon (even-odd rule, pixel-centre sampling),
/// as sorted disjoint half-open x ranges.
pub fn polygon_spans(pts: &[Point], y: i64) -> Vec<(i64, i64)> {
    if pts.len() < 3 {
        return Vec::new();
    }
    let yc = y as f64 + 0.5;
    let mut xs: Vec<f64> = Vec::new();
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = (f64::from(pts[i].x), f64::from(pts[i].y));
        let (xj, yj) = (f64::from(pts[j].x), f64::from(pts[j].y));
        if (yi > yc) != (yj > yc) {
            xs.push((xj - xi) * (yc - yi) / (yj - yi) + xi);
        }
        j = i;
    }
    xs.sort_by(f64::total_cmp);
    // x is inside iff an odd number of crossings lie strictly right of its centre.
    xs.chunks_exact(2)
        .filter_map(|c| {
            let a = (c[0] - 0.5).ceil() as i64;
            let b = (c[1] - 0.5).ceil() as i64;
            (b > a).then_some((a, b))
        })
        .collect()
}

/// Even-odd pixel-centre test against a closed polygon.
pub fn in_polygon(pts: &[Point], p: Point) -> bool {
    polygon_spans(pts, i64::from(p.y)).iter().any(|&(a, b)| (a..b).contains(&i64::from(p.x)))
}

/// Bounding box of points; `None` when empty.
pub fn bounding_points(pts: &[Point]) -> Option<Rect> {
    let first = pts.first()?;
    let (mut l, mut t, mut r, mut b) = (first.x, first.y, first.x, first.y);
    for p in pts {
        l = l.min(p.x);
        t = t.min(p.y);
        r = r.max(p.x);
        b = b.max(p.y);
    }
    Some(rect_from_edges(i64::from(l), i64::from(t), i64::from(r), i64::from(b)))
}

/// Twice the polygon's signed area (shoelace), used to reject degenerate freeform shapes.
pub fn polygon_area2(pts: &[Point]) -> i64 {
    let mut s = 0i64;
    for (i, a) in pts.iter().enumerate() {
        let b = pts[(i + 1) % pts.len()];
        s += i64::from(a.x) * i64::from(b.y) - i64::from(b.x) * i64::from(a.y);
    }
    s.abs()
}

/// Reduces a set of dirty rectangles to at most `max` non-empty rectangles inside `bounds`.
///
/// Overlapping or touching rectangles are merged; if that is not enough, the pair whose
/// union wastes the least area is merged until the limit is met. The result covers every
/// input pixel (soundness matters more than tightness).
pub fn coalesce(rects: impl IntoIterator<Item = Rect>, bounds: Rect, max: usize) -> Vec<Rect> {
    let mut v: Vec<Rect> =
        rects.into_iter().filter_map(|r| r.intersect(bounds)).filter(|r| !r.is_empty()).collect();
    // Merge pairs whose union wastes (almost) nothing, e.g. adjacent strips. Crossing strips
    // (crosshair lines) must not merge: their union is the whole screen.
    let cheap = |a: Rect, b: Rect| {
        let sum = a.area() + b.area();
        a.union(b).area() <= sum + sum / 8
    };
    loop {
        let mut merged = false;
        'outer: for i in 0..v.len() {
            for j in (i + 1)..v.len() {
                if cheap(v[i], v[j]) {
                    v[i] = v[i].union(v[j]);
                    v.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            break;
        }
    }
    let max = max.max(1);
    while v.len() > max {
        let mut best = (u64::MAX, 0, 1);
        for i in 0..v.len() {
            for j in (i + 1)..v.len() {
                let u = v[i].union(v[j]);
                let waste = u.area().saturating_sub(v[i].area()).saturating_sub(v[j].area());
                if waste < best.0 {
                    best = (waste, i, j);
                }
            }
        }
        let (_, i, j) = best;
        v[i] = v[i].union(v[j]);
        v.swap_remove(j);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_rect() -> impl Strategy<Value = Rect> {
        (-50i32..50, -50i32..50, 0u32..60, 0u32..60).prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
    }

    fn pixels(rs: &[Rect]) -> std::collections::BTreeSet<(i32, i32)> {
        let mut s = std::collections::BTreeSet::new();
        for r in rs {
            for y in r.y..r.bottom() as i32 {
                for x in r.x..r.right() as i32 {
                    s.insert((x, y));
                }
            }
        }
        s
    }

    proptest! {
        #[test]
        fn subtract_is_exact_and_disjoint(a in arb_rect(), b in arb_rect()) {
            let parts = subtract(a, b);
            let total: u64 = parts.iter().map(|r| r.area()).sum();
            let got = pixels(&parts);
            prop_assert_eq!(got.len() as u64, total, "parts overlap");
            let want: std::collections::BTreeSet<_> =
                pixels(&[a]).difference(&pixels(&[b])).copied().collect();
            prop_assert_eq!(got, want);
        }

        #[test]
        fn coalesce_covers_inputs(rs in proptest::collection::vec(arb_rect(), 0..12), max in 1usize..5) {
            let bounds = Rect::new(-30, -30, 70, 70);
            let out = coalesce(rs.clone(), bounds, max);
            prop_assert!(out.len() <= max);
            let covered = pixels(&out);
            let clipped: Vec<Rect> = rs.iter().filter_map(|r| r.intersect(bounds)).collect();
            for px in pixels(&clipped) {
                prop_assert!(covered.contains(&px), "pixel {:?} lost", px);
            }
            for r in &out {
                prop_assert!(bounds.intersect(*r) == Some(*r));
            }
        }

        #[test]
        fn clamp_inside_stays_inside(r in arb_rect(), bx in -20i32..20, by in -20i32..20, bw in 1u32..80, bh in 1u32..80) {
            let bounds = Rect::new(bx, by, bw, bh);
            let c = clamp_inside(r, bounds);
            prop_assert_eq!(c.intersect(bounds).map_or(0, Rect::area), c.area());
            prop_assert_eq!(c.width, r.width.min(bw));
            prop_assert_eq!(c.height, r.height.min(bh));
        }

        #[test]
        fn ring_plus_hole_covers_outer(r in arb_rect(), inner in 0u32..6, outer in 0u32..6) {
            let parts = ring(r, inner, outer);
            let covered = pixels(&parts);
            // Every pixel of the outer box that is not deep inside must be in the ring.
            let big = inflate_i64(r, i64::from(outer));
            let hole = inflate_i64(r, -i64::from(inner));
            for y in big.y..big.bottom() as i32 {
                for x in big.x..big.right() as i32 {
                    let in_hole = hole.contains(Point::new(x, y));
                    prop_assert_eq!(covered.contains(&(x, y)), !in_hole);
                }
            }
        }
    }

    #[test]
    fn handles_hide_mid_handles_on_small_rects() {
        let small = Rect::new(0, 0, 10, 100);
        let v = visible_handles(small, 8);
        assert!(!v.contains(&Handle::North) && !v.contains(&Handle::South));
        assert!(v.contains(&Handle::East) && v.contains(&Handle::West));
        assert_eq!(visible_handles(Rect::new(0, 0, 100, 100), 8).len(), 8);
        assert_eq!(visible_handles(Rect::new(0, 0, 1, 1), 8).len(), 4);
    }

    #[test]
    fn corner_wins_over_edge_when_overlapping() {
        let r = Rect::new(100, 100, 20, 20);
        // 8px handles on a 20px rect: only the corners exist.
        assert_eq!(handle_at(r, Point::new(100, 100), 8, 0), Some(Handle::NorthWest));
        assert_eq!(handle_at(r, Point::new(110, 100), 8, 0), None, "mid handles are hidden");
        let big = Rect::new(100, 100, 200, 200);
        assert_eq!(handle_at(big, Point::new(200, 100), 8, 0), Some(Handle::North));
        assert_eq!(handle_at(big, Point::new(203, 103), 8, 0), Some(Handle::North));
        assert_eq!(handle_at(big, Point::new(210, 100), 8, 0), None);
        assert_eq!(handle_at(big, Point::new(210, 100), 8, 8), Some(Handle::North));
    }

    #[test]
    fn snap_prefers_nearest_then_lowest() {
        assert_eq!(snap_value(10, &[0, 12, 8], 5), Some(8));
        assert_eq!(snap_value(10, &[100], 5), None);
        assert_eq!(snap_value(10, &[8, 12], 5), Some(8));
    }

    #[test]
    fn ellipse_and_polygon_masks() {
        let r = Rect::new(0, 0, 10, 10);
        assert!(in_ellipse(r, Point::new(5, 5)));
        assert!(!in_ellipse(r, Point::new(0, 0)));
        assert!(!in_ellipse(r, Point::new(9, 0)));
        let tri = [Point::new(0, 0), Point::new(10, 0), Point::new(0, 10)];
        assert!(in_polygon(&tri, Point::new(1, 1)));
        assert!(!in_polygon(&tri, Point::new(8, 8)));
        assert!(!in_polygon(&tri[..2], Point::new(1, 1)));
        assert_eq!(polygon_area2(&tri), 100);
        assert_eq!(bounding_points(&tri), Some(Rect::new(0, 0, 10, 10)));
        assert_eq!(bounding_points(&[]), None);
    }

    #[test]
    fn spans_agree_with_float_predicates() {
        // Reference: the textbook float predicates.
        let rect = Rect::new(-7, 3, 41, 23);
        let rx = f64::from(rect.width) / 2.0;
        let ry = f64::from(rect.height) / 2.0;
        for y in -5..40 {
            for x in -20..50 {
                let dx = (f64::from(x) + 0.5 - (f64::from(rect.x) + rx)) / rx;
                let dy = (f64::from(y) + 0.5 - (f64::from(rect.y) + ry)) / ry;
                let want = dx * dx + dy * dy <= 1.0;
                assert_eq!(in_ellipse(rect, Point::new(x, y)), want, "ellipse {x},{y}");
            }
        }
        let poly = [Point::new(0, 0), Point::new(30, 5), Point::new(10, 20), Point::new(25, 30), Point::new(-5, 12)];
        for y in -3..35 {
            for x in -10..40 {
                let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                let mut inside = false;
                let mut j = poly.len() - 1;
                for i in 0..poly.len() {
                    let (xi, yi) = (f64::from(poly[i].x), f64::from(poly[i].y));
                    let (xj, yj) = (f64::from(poly[j].x), f64::from(poly[j].y));
                    if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                        inside = !inside;
                    }
                    j = i;
                }
                assert_eq!(in_polygon(&poly, Point::new(x, y)), inside, "poly {x},{y}");
            }
        }
    }

    #[test]
    fn rect_from_edges_survives_extremes() {
        let r = rect_from_edges(i64::MIN, i64::MIN, i64::MAX, i64::MAX);
        assert_eq!(r.x, i32::MIN);
        assert_eq!(r.width, u32::MAX);
    }
}
