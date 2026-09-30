//! Vector geometry builders: rounded rectangles, arrow heads, smoothed freehand paths,
//! balloons, built-in stickers, cursor artwork and grid/hatch line sets.
//!
//! Everything returns `tiny_skia::Path` in *document space* (or a unit box for stickers);
//! the renderer applies the view transform. No pixels are touched here, which keeps the
//! functions unit-testable through path bounds and point counts.

use tiny_skia::{FillRule, Path, PathBuilder, Rect as SkRect};

use crate::{
    geom::{PointF, RectF},
    object::{
        BalloonShape, BuiltinSticker, CursorKind, GridPattern, HeadStyle, TailSide, balloon_tail,
    },
};

/// `RectF` → skia rect (`None` for non-finite or negative sizes).
pub fn sk_rect(r: RectF) -> Option<SkRect> {
    let n = r.normalized();
    SkRect::from_xywh(n.x, n.y, n.w.max(0.0), n.h.max(0.0))
}

/// Circular-arc control-point factor for a quarter circle drawn with one cubic.
const KAPPA: f32 = 0.552_284_8;

/// Rounded rectangle (`radius` clamped to half the shorter side). A zero radius gives a
/// plain rectangle.
pub fn rounded_rect(r: RectF, radius: f32) -> Option<Path> {
    let r = r.normalized();
    if !r.is_finite() {
        return None;
    }
    let rr = radius.max(0.0).min(r.w / 2.0).min(r.h / 2.0);
    let mut pb = PathBuilder::new();
    outline_with_tail(&mut pb, r, rr, None);
    pb.finish()
}

/// Ellipse inscribed in `r`.
pub fn ellipse(r: RectF) -> Option<Path> {
    PathBuilder::from_oval(sk_rect(r)?)
}

/// Emits a clockwise rounded-rectangle outline, optionally splicing a tail triangle into one
/// edge (`(side, base_a, base_b, tip)`).
fn outline_with_tail(
    pb: &mut PathBuilder,
    r: RectF,
    rr: f32,
    tail: Option<(TailSide, PointF, PointF, PointF)>,
) {
    let k = rr * KAPPA;
    let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
    let line = |pb: &mut PathBuilder, p: PointF| pb.line_to(p.x, p.y);
    pb.move_to(x0 + rr, y0);
    // Top edge, left -> right.
    if let Some((TailSide::Top, a, b, t)) = tail {
        line(pb, a);
        line(pb, t);
        line(pb, b);
    }
    pb.line_to(x1 - rr, y0);
    pb.cubic_to(x1 - rr + k, y0, x1, y0 + rr - k, x1, y0 + rr);
    // Right edge, top -> bottom.
    if let Some((TailSide::Right, a, b, t)) = tail {
        line(pb, a);
        line(pb, t);
        line(pb, b);
    }
    pb.line_to(x1, y1 - rr);
    pb.cubic_to(x1, y1 - rr + k, x1 - rr + k, y1, x1 - rr, y1);
    // Bottom edge, right -> left (so `b`, the larger x, comes first).
    if let Some((TailSide::Bottom, a, b, t)) = tail {
        line(pb, b);
        line(pb, t);
        line(pb, a);
    }
    pb.line_to(x0 + rr, y1);
    pb.cubic_to(x0 + rr - k, y1, x0, y1 - rr + k, x0, y1 - rr);
    // Left edge, bottom -> top.
    if let Some((TailSide::Left, a, b, t)) = tail {
        line(pb, b);
        line(pb, t);
        line(pb, a);
    }
    pb.line_to(x0, y0 + rr);
    pb.cubic_to(x0, y0 + rr - k, x0 + rr - k, y0, x0 + rr, y0);
    pb.close();
}

/// One closed outline for a speech balloon: rounded body with the tail spliced in, so a
/// stroke shows no seam between body and tail.
pub fn balloon_outline(b: &BalloonShape, radius: f32) -> Option<Path> {
    let r = b.rect.normalized();
    if !r.is_finite() || !b.tail.is_finite() {
        return None;
    }
    let tail = balloon_tail(b);
    // Keep the tail base on the straight part of the edge when the corners allow it.
    let rr = radius.max(0.0).min(r.w / 2.0).min(r.h / 2.0);
    let mut pb = PathBuilder::new();
    outline_with_tail(&mut pb, r, rr, tail.map(|t| (t.side, t.a, t.b, t.tip)));
    pb.finish()
}

/// Geometry of one arrow head.
#[derive(Debug, Clone)]
pub struct HeadGeom {
    /// Region to fill with the stroke colour (filled/diamond/round heads).
    pub fill: Option<Path>,
    /// Lines to stroke (open/bar heads).
    pub stroke: Option<Path>,
    /// Where the shaft should end so it does not poke through the head.
    pub shaft_end: PointF,
}

/// Builds the head at `tip` for a shaft arriving from direction `from` (a point on the
/// shaft), with the given length and half-angle in degrees.
pub fn head_geometry(
    style: HeadStyle,
    tip: PointF,
    from: PointF,
    length: f32,
    angle_deg: f32,
) -> HeadGeom {
    let dir = tip - from;
    let len = dir.length();
    let none = HeadGeom { fill: None, stroke: None, shaft_end: tip };
    if len < 1e-4 || style == HeadStyle::None {
        return none;
    }
    let d = dir * (1.0 / len); // unit vector pointing at the tip
    let n = PointF::new(-d.y, d.x);
    let half = length * angle_deg.clamp(5.0, 80.0).to_radians().tan();
    let back = tip - d * length;
    let (w1, w2) = (back + n * half, back - n * half);
    let poly = |pts: &[PointF]| {
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].x, pts[0].y);
        for p in &pts[1..] {
            pb.line_to(p.x, p.y);
        }
        pb.close();
        pb.finish()
    };
    match style {
        HeadStyle::None => none,
        HeadStyle::Filled => HeadGeom {
            fill: poly(&[tip, w1, w2]),
            stroke: None,
            shaft_end: tip - d * (length * 0.9),
        },
        HeadStyle::Open => {
            let mut pb = PathBuilder::new();
            pb.move_to(w1.x, w1.y);
            pb.line_to(tip.x, tip.y);
            pb.line_to(w2.x, w2.y);
            HeadGeom { fill: None, stroke: pb.finish(), shaft_end: tip }
        }
        HeadStyle::Diamond => {
            let mid = tip - d * (length / 2.0);
            let hw = half.min(length * 0.6) * 0.75;
            HeadGeom {
                fill: poly(&[tip, mid + n * hw, back, mid - n * hw]),
                stroke: None,
                shaft_end: mid,
            }
        }
        HeadStyle::Round => {
            let r = length * 0.4;
            let c = tip - d * r;
            HeadGeom { fill: PathBuilder::from_circle(c.x, c.y, r), stroke: None, shaft_end: c }
        }
        HeadStyle::Bar => {
            let h = length * 0.45;
            let mut pb = PathBuilder::new();
            let (a, b) = (tip + n * h, tip - n * h);
            pb.move_to(a.x, a.y);
            pb.line_to(b.x, b.y);
            HeadGeom { fill: None, stroke: pb.finish(), shaft_end: tip }
        }
    }
}

/// A reference point on a polyline at least `min_dist` away from its end (or start), for
/// deriving an arrow head direction that ignores tiny jitter at the tip.
pub fn direction_ref(points: &[PointF], at_end: bool, min_dist: f32) -> Option<PointF> {
    let tip = if at_end { *points.last()? } else { *points.first()? };
    let mut iter: Box<dyn Iterator<Item = &PointF>> =
        if at_end { Box::new(points.iter().rev()) } else { Box::new(points.iter()) };
    let mut best = None;
    for p in iter.by_ref() {
        best = Some(*p);
        if p.distance(tip) >= min_dist {
            break;
        }
    }
    best.filter(|p| p.distance(tip) > 1e-3)
}

/// Polyline (`smooth = false`) or Catmull-Rom spline through `points` as cubic Béziers.
/// Fewer than two distinct points yield `None` (the renderer draws a dot instead).
pub fn freehand_path(points: &[PointF], smooth: bool) -> Option<Path> {
    let mut pts: Vec<PointF> = Vec::with_capacity(points.len());
    for p in points {
        if p.is_finite() && pts.last().is_none_or(|l| l.distance(*p) > 1e-4) {
            pts.push(*p);
        }
    }
    if pts.len() < 2 {
        return None;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].x, pts[0].y);
    if !smooth || pts.len() == 2 {
        for p in &pts[1..] {
            pb.line_to(p.x, p.y);
        }
    } else {
        let n = pts.len();
        for i in 0..n - 1 {
            let p0 = pts[i.saturating_sub(1)];
            let p1 = pts[i];
            let p2 = pts[i + 1];
            let p3 = pts[(i + 2).min(n - 1)];
            // Uniform Catmull-Rom converted to a cubic Bézier (tension 0.5).
            let c1 = p1 + (p2 - p0) * (1.0 / 6.0);
            let c2 = p2 - (p3 - p1) * (1.0 / 6.0);
            pb.cubic_to(c1.x, c1.y, c2.x, c2.y, p2.x, p2.y);
        }
    }
    pb.finish()
}

/// Drops points closer than `min_dist` to the previous kept point (the last point is always
/// kept), so long drags stay light without visibly changing the stroke.
pub fn thin_points(points: &[PointF], min_dist: f32) -> Vec<PointF> {
    let mut out: Vec<PointF> = Vec::new();
    for (i, p) in points.iter().enumerate() {
        let last = i + 1 == points.len();
        if out.last().is_none_or(|l| l.distance(*p) >= min_dist) || (last && out.last() != Some(p))
        {
            out.push(*p);
        }
    }
    out
}

/// Polygon helper.
fn polygon(pts: &[(f32, f32)]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    let (x, y) = *pts.first()?;
    pb.move_to(x, y);
    for &(x, y) in &pts[1..] {
        pb.line_to(x, y);
    }
    pb.close();
    pb.finish()
}

/// A built-in sticker in the unit square (y down) with the fill rule it needs.
pub fn builtin_sticker(which: BuiltinSticker) -> Option<(Path, FillRule)> {
    let winding = |p: Option<Path>| p.map(|p| (p, FillRule::Winding));
    match which {
        BuiltinSticker::Check => winding(polygon(&[
            (0.10, 0.55),
            (0.22, 0.43),
            (0.40, 0.62),
            (0.78, 0.18),
            (0.90, 0.30),
            (0.40, 0.86),
        ])),
        BuiltinSticker::Plus => winding(polygon(&PLUS)),
        BuiltinSticker::Cross => {
            let p = polygon(&PLUS)?;
            let t = tiny_skia::Transform::from_rotate_at(45.0, 0.5, 0.5);
            winding(p.transform(t))
        }
        BuiltinSticker::Minus => {
            winding(polygon(&[(0.05, 0.4), (0.95, 0.4), (0.95, 0.6), (0.05, 0.6)]))
        }
        BuiltinSticker::ArrowRight => winding(polygon(&[
            (0.02, 0.35),
            (0.55, 0.35),
            (0.55, 0.08),
            (0.98, 0.5),
            (0.55, 0.92),
            (0.55, 0.65),
            (0.02, 0.65),
        ])),
        BuiltinSticker::Bolt => winding(polygon(&[
            (0.62, 0.0),
            (0.18, 0.56),
            (0.44, 0.56),
            (0.34, 1.0),
            (0.82, 0.38),
            (0.56, 0.38),
            (0.74, 0.0),
        ])),
        BuiltinSticker::Star => {
            let mut pts = Vec::with_capacity(10);
            for i in 0..10 {
                let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                let r = if i % 2 == 0 { 0.5 } else { 0.21 };
                pts.push((0.5 + r * a.cos(), 0.53 + r * a.sin()));
            }
            winding(polygon(&pts))
        }
        BuiltinSticker::Heart => {
            let mut pb = PathBuilder::new();
            pb.move_to(0.5, 0.92);
            pb.cubic_to(0.08, 0.62, 0.02, 0.30, 0.22, 0.13);
            pb.cubic_to(0.38, 0.01, 0.50, 0.16, 0.5, 0.26);
            pb.cubic_to(0.50, 0.16, 0.62, 0.01, 0.78, 0.13);
            pb.cubic_to(0.98, 0.30, 0.92, 0.62, 0.5, 0.92);
            pb.close();
            winding(pb.finish())
        }
        BuiltinSticker::Exclamation => {
            let mut pb = PathBuilder::new();
            pb.push_circle(0.5, 0.5, 0.5);
            // Even-odd holes: the bar and the dot.
            pb.push_rect(SkRect::from_xywh(0.45, 0.2, 0.10, 0.42)?);
            pb.push_circle(0.5, 0.75, 0.06);
            pb.finish().map(|p| (p, FillRule::EvenOdd))
        }
    }
}

const PLUS: [(f32, f32); 12] = [
    (0.4, 0.05),
    (0.6, 0.05),
    (0.6, 0.4),
    (0.95, 0.4),
    (0.95, 0.6),
    (0.6, 0.6),
    (0.6, 0.95),
    (0.4, 0.95),
    (0.4, 0.6),
    (0.05, 0.6),
    (0.05, 0.4),
    (0.4, 0.4),
];

/// Cursor artwork positioned with its hot spot at `pos` and scaled to `size` pixels tall.
/// Returns `(fill_path, stroke_path)`: the arrow is a filled polygon with an outline, the
/// I-beam and crosshair are strokes only.
pub fn cursor_paths(kind: CursorKind, pos: PointF, size: f32) -> (Option<Path>, Option<Path>) {
    match kind {
        CursorKind::Arrow => {
            let pts = [
                (0.0, 0.0),
                (0.0, 0.83),
                (0.19, 0.65),
                (0.33, 1.0),
                (0.45, 0.95),
                (0.31, 0.61),
                (0.56, 0.61),
            ];
            let scaled: Vec<(f32, f32)> =
                pts.iter().map(|&(x, y)| (pos.x + x * size, pos.y + y * size)).collect();
            let p = polygon(&scaled);
            (p.clone(), p)
        }
        CursorKind::IBeam => {
            let (h, s) = (size * 0.5, size * 0.14);
            let mut pb = PathBuilder::new();
            pb.move_to(pos.x, pos.y - h);
            pb.line_to(pos.x, pos.y + h);
            pb.move_to(pos.x - s, pos.y - h);
            pb.line_to(pos.x + s, pos.y - h);
            pb.move_to(pos.x - s, pos.y + h);
            pb.line_to(pos.x + s, pos.y + h);
            (None, pb.finish())
        }
        CursorKind::Crosshair => {
            let (h, g) = (size * 0.5, size * 0.09);
            let mut pb = PathBuilder::new();
            pb.move_to(pos.x - h, pos.y);
            pb.line_to(pos.x - g, pos.y);
            pb.move_to(pos.x + g, pos.y);
            pb.line_to(pos.x + h, pos.y);
            pb.move_to(pos.x, pos.y - h);
            pb.line_to(pos.x, pos.y - g);
            pb.move_to(pos.x, pos.y + g);
            pb.line_to(pos.x, pos.y + h);
            (None, pb.finish())
        }
    }
}

/// Upper bound on generated grid segments (protects against `spacing = 0.01`).
const MAX_SEGMENTS: usize = 20_000;

/// Line segments of a grid/hatch pattern clipped to `r`, anchored at the rectangle's
/// top-left so the pattern moves with the object. `Dots` returns nothing here (see
/// [`grid_dots`]).
pub fn grid_segments(r: RectF, pattern: GridPattern, spacing: f32) -> Vec<(PointF, PointF)> {
    let r = r.normalized();
    let sp = spacing.max(2.0);
    let mut out = Vec::new();
    if r.is_empty() || !r.is_finite() {
        return out;
    }
    let vertical_and_horizontal = matches!(pattern, GridPattern::Grid);
    if vertical_and_horizontal {
        let mut x = r.x;
        while x <= r.right() && out.len() < MAX_SEGMENTS {
            out.push((PointF::new(x, r.y), PointF::new(x, r.bottom())));
            x += sp;
        }
        let mut y = r.y;
        while y <= r.bottom() && out.len() < MAX_SEGMENTS {
            out.push((PointF::new(r.x, y), PointF::new(r.right(), y)));
            y += sp;
        }
    }
    if matches!(pattern, GridPattern::HatchForward | GridPattern::CrossHatch) {
        // Lines x + y = c.
        let mut c = r.x + r.y;
        while c <= r.right() + r.bottom() && out.len() < MAX_SEGMENTS {
            let xa = r.x.max(c - r.bottom());
            let xb = r.right().min(c - r.y);
            if xa < xb {
                out.push((PointF::new(xa, c - xa), PointF::new(xb, c - xb)));
            }
            c += sp;
        }
    }
    if matches!(pattern, GridPattern::HatchBackward | GridPattern::CrossHatch) {
        // Lines x - y = c.
        let mut c = r.x - r.bottom();
        while c <= r.right() - r.y && out.len() < MAX_SEGMENTS {
            let xa = r.x.max(c + r.y);
            let xb = r.right().min(c + r.bottom());
            if xa < xb {
                out.push((PointF::new(xa, xa - c), PointF::new(xb, xb - c)));
            }
            c += sp;
        }
    }
    out
}

/// Dot centres of the `Dots` pattern.
pub fn grid_dots(r: RectF, spacing: f32) -> Vec<PointF> {
    let r = r.normalized();
    let sp = spacing.max(2.0);
    let mut out = Vec::new();
    if r.is_empty() || !r.is_finite() {
        return out;
    }
    let mut y = r.y + sp / 2.0;
    while y < r.bottom() && out.len() < MAX_SEGMENTS {
        let mut x = r.x + sp / 2.0;
        while x < r.right() && out.len() < MAX_SEGMENTS {
            out.push(PointF::new(x, y));
            x += sp;
        }
        y += sp;
    }
    out
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact geometry values are what these tests assert
mod tests {
    use super::*;

    fn b(p: &Path) -> (f32, f32, f32, f32) {
        let r = p.bounds();
        (r.left(), r.top(), r.right(), r.bottom())
    }

    #[test]
    fn rounded_rect_bounds_and_degenerate() {
        let p = rounded_rect(RectF::new(10.0, 20.0, 100.0, 50.0), 8.0).unwrap();
        assert_eq!(b(&p), (10.0, 20.0, 110.0, 70.0));
        // Radius clamps; negative sizes normalise; NaN is refused.
        let q = rounded_rect(RectF::new(0.0, 0.0, 10.0, 4.0), 100.0).unwrap();
        assert_eq!(b(&q), (0.0, 0.0, 10.0, 4.0));
        assert!(rounded_rect(RectF::new(0.0, 0.0, f32::NAN, 4.0), 1.0).is_none());
        assert!(ellipse(RectF::new(0.0, 0.0, 10.0, 4.0)).is_some());
    }

    #[test]
    fn balloon_outline_includes_tail_tip_on_every_side() {
        for tip in [
            PointF::new(50.0, -40.0),
            PointF::new(150.0, 25.0),
            PointF::new(50.0, 90.0),
            PointF::new(-50.0, 25.0),
        ] {
            let bal = BalloonShape {
                rect: RectF::new(0.0, 0.0, 100.0, 50.0),
                tail: tip,
                ..BalloonShape::default()
            };
            let p = balloon_outline(&bal, 10.0).unwrap();
            let r = p.bounds();
            assert!(r.left() <= tip.x + 0.01 && r.right() >= tip.x - 0.01, "{tip:?}");
            assert!(r.top() <= tip.y + 0.01 && r.bottom() >= tip.y - 0.01, "{tip:?}");
        }
        // A tip inside the body gives a plain rounded rectangle.
        let inside = BalloonShape {
            rect: RectF::new(0.0, 0.0, 100.0, 50.0),
            tail: PointF::new(50.0, 25.0),
            ..BalloonShape::default()
        };
        assert_eq!(b(&balloon_outline(&inside, 10.0).unwrap()), (0.0, 0.0, 100.0, 50.0));
    }

    #[test]
    fn filled_head_points_at_tip_and_shortens_shaft() {
        let tip = PointF::new(100.0, 0.0);
        let g = head_geometry(HeadStyle::Filled, tip, PointF::new(0.0, 0.0), 20.0, 25.0);
        let r = g.fill.unwrap().bounds();
        assert_eq!(r.right(), 100.0);
        assert!((r.left() - 80.0).abs() < 1e-4);
        assert!(g.shaft_end.x < 100.0 && g.shaft_end.x > 80.0);
        // Open head strokes two wings meeting at the tip.
        let o = head_geometry(HeadStyle::Open, tip, PointF::new(0.0, 0.0), 20.0, 25.0);
        assert!(o.fill.is_none() && o.stroke.is_some());
        assert_eq!(o.shaft_end, tip);
        // Degenerate inputs.
        let same = head_geometry(HeadStyle::Filled, tip, tip, 20.0, 25.0);
        assert!(same.fill.is_none());
        assert!(head_geometry(HeadStyle::None, tip, PointF::default(), 20.0, 25.0).fill.is_none());
        for s in [HeadStyle::Diamond, HeadStyle::Round, HeadStyle::Bar] {
            let h = head_geometry(s, tip, PointF::default(), 20.0, 25.0);
            assert!(h.fill.is_some() || h.stroke.is_some(), "{s:?}");
        }
    }

    #[test]
    fn direction_ref_skips_jitter() {
        let pts = [
            PointF::new(0.0, 0.0),
            PointF::new(50.0, 0.0),
            PointF::new(99.0, 0.5),
            PointF::new(100.0, 0.0),
        ];
        let r = direction_ref(&pts, true, 10.0).unwrap();
        assert_eq!(r, PointF::new(50.0, 0.0));
        assert_eq!(direction_ref(&pts, false, 10.0).unwrap(), PointF::new(50.0, 0.0));
        assert!(direction_ref(&[PointF::new(1.0, 1.0)], true, 5.0).is_none());
        assert!(direction_ref(&[], true, 5.0).is_none());
    }

    #[test]
    fn freehand_paths() {
        let pts: Vec<PointF> =
            (0..20).map(|i| PointF::new(i as f32 * 5.0, (i as f32 * 0.5).sin() * 10.0)).collect();
        let smooth = freehand_path(&pts, true).unwrap();
        let poly = freehand_path(&pts, false).unwrap();
        assert!(smooth.len() > poly.len() / 2);
        let (sb, pb) = (smooth.bounds(), poly.bounds());
        assert!((sb.left() - pb.left()).abs() < 1e-3 && (sb.right() - pb.right()).abs() < 1e-3);
        assert!(freehand_path(&[], true).is_none());
        assert!(freehand_path(&[PointF::new(1.0, 1.0)], true).is_none());
        assert!(freehand_path(&[PointF::new(1.0, 1.0), PointF::new(1.0, 1.0)], true).is_none());
        assert!(freehand_path(&[PointF::new(0.0, 0.0), PointF::new(5.0, 5.0)], true).is_some());
        assert!(
            freehand_path(&[PointF::new(f32::NAN, 0.0), PointF::new(5.0, 5.0)], true).is_none()
        );
    }

    #[test]
    fn catmull_rom_passes_through_points() {
        // A spline segment starts and ends exactly on the input points; the bounding box of a
        // straight line of points stays a line.
        let pts = [
            PointF::new(0.0, 0.0),
            PointF::new(10.0, 0.0),
            PointF::new(20.0, 0.0),
            PointF::new(30.0, 0.0),
        ];
        let p = freehand_path(&pts, true).unwrap();
        let r = p.bounds();
        assert_eq!((r.left(), r.right(), r.top(), r.bottom()), (0.0, 30.0, 0.0, 0.0));
    }

    #[test]
    fn thin_points_keeps_ends() {
        let pts: Vec<PointF> = (0..100).map(|i| PointF::new(i as f32 * 0.1, 0.0)).collect();
        let t = thin_points(&pts, 1.0);
        assert!(t.len() < 15);
        assert_eq!(t[0], pts[0]);
        assert_eq!(*t.last().unwrap(), *pts.last().unwrap());
        assert!(thin_points(&[], 1.0).is_empty());
    }

    #[test]
    fn stickers_fit_the_unit_square() {
        for w in [
            BuiltinSticker::Check,
            BuiltinSticker::Cross,
            BuiltinSticker::Star,
            BuiltinSticker::Heart,
            BuiltinSticker::Exclamation,
            BuiltinSticker::Plus,
            BuiltinSticker::Minus,
            BuiltinSticker::ArrowRight,
            BuiltinSticker::Bolt,
        ] {
            let (p, _) = builtin_sticker(w).unwrap_or_else(|| panic!("{w:?}"));
            let r = p.bounds();
            assert!(
                r.left() >= -0.01 && r.top() >= -0.01 && r.right() <= 1.01 && r.bottom() <= 1.01,
                "{w:?} {r:?}"
            );
            assert!(r.width() > 0.3 && r.height() > 0.15, "{w:?}");
        }
    }

    #[test]
    fn cursor_paths_exist() {
        let (fill, stroke) = cursor_paths(CursorKind::Arrow, PointF::new(10.0, 10.0), 32.0);
        assert!(fill.is_some() && stroke.is_some());
        let r = fill.unwrap().bounds();
        assert_eq!((r.left(), r.top()), (10.0, 10.0));
        for k in [CursorKind::IBeam, CursorKind::Crosshair] {
            let (f, s) = cursor_paths(k, PointF::new(50.0, 50.0), 32.0);
            assert!(f.is_none() && s.is_some());
        }
    }

    #[test]
    fn grid_and_hatch_segments_stay_inside_rect() {
        let r = RectF::new(10.0, 20.0, 100.0, 60.0);
        for pat in [
            GridPattern::Grid,
            GridPattern::HatchForward,
            GridPattern::HatchBackward,
            GridPattern::CrossHatch,
        ] {
            let segs = grid_segments(r, pat, 10.0);
            assert!(!segs.is_empty(), "{pat:?}");
            for (a, b) in &segs {
                for p in [a, b] {
                    assert!(p.x >= 10.0 - 1e-3 && p.x <= 110.0 + 1e-3, "{pat:?} {p:?}");
                    assert!(p.y >= 20.0 - 1e-3 && p.y <= 80.0 + 1e-3, "{pat:?} {p:?}");
                }
            }
        }
        assert_eq!(grid_segments(r, GridPattern::Grid, 10.0).len(), 11 + 7);
        // Forward hatch segments really run "/": y decreases as x increases.
        for (a, b) in grid_segments(r, GridPattern::HatchForward, 10.0) {
            assert!(b.x > a.x && b.y < a.y);
        }
        assert!(grid_segments(RectF::new(0.0, 0.0, 0.0, 5.0), GridPattern::Grid, 10.0).is_empty());
        assert!(grid_segments(r, GridPattern::Grid, 0.0001).len() <= MAX_SEGMENTS);
        assert!(grid_segments(r, GridPattern::Dots, 10.0).is_empty());
        assert_eq!(grid_dots(r, 10.0).len(), 10 * 6);
    }
}
