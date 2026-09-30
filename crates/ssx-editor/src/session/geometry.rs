//! Pure geometry of interaction: handle layout, resize/rotate maths, constraints, snapping.
//!
//! Kept free of session state so each formula can be unit-tested with plain numbers.

use crate::{
    geom::{PointF, RectF},
    input::{CursorHint, Guide, Handle, HandleKind, Modifiers, OrientedRect},
    object::{Object, ObjectKind},
};

/// Distance of the rotation knob above the top edge, in screen pixels.
pub const ROTATE_KNOB_PX: f32 = 26.0;

/// Which handle set applies to a selection.
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    /// A single box-like object (rotatable frame).
    Box {
        /// Unrotated rectangle.
        rect: RectF,
        /// Rotation in radians.
        rotation: f32,
        /// Kind supports the rotate knob.
        rotatable: bool,
    },
    /// A single line or arrow: two endpoints.
    Segment {
        /// Start.
        a: PointF,
        /// End.
        b: PointF,
    },
    /// Axis-aligned bounding box of anything else (freehand, steps, multi-selection).
    Group {
        /// Union of bounds.
        bounds: RectF,
    },
}

/// Chooses the handle layout for a selection of objects.
pub fn layout_for(objs: &[&Object]) -> Option<Layout> {
    match objs {
        [] => None,
        [o] => Some(match &o.kind {
            ObjectKind::Line(l) => Layout::Segment { a: l.a, b: l.b },
            ObjectKind::Arrow(a) => Layout::Segment { a: a.a, b: a.b },
            k => match k.as_box() {
                Some((rect, rotation)) => Layout::Box {
                    rect: rect.normalized(),
                    rotation,
                    rotatable: k.supports_rotation(),
                },
                None => Layout::Group { bounds: o.bounds() },
            },
        }),
        many => {
            let b = many.iter().map(|o| o.bounds()).reduce(|a, b| a.union(&b))?;
            Some(Layout::Group { bounds: b })
        }
    }
}

/// Cursor for a resize handle whose frame is rotated by `rotation`.
pub fn resize_cursor(kind: HandleKind, rotation: f32) -> CursorHint {
    let Some((dx, dy)) = kind.direction() else { return CursorHint::Move };
    let angle = f32::from(dy).atan2(f32::from(dx)) + rotation;
    // 0 => east-west, 1 => nw-se, 2 => north-south, 3 => ne-sw (45° steps, mod 180°).
    let idx = ((angle / std::f32::consts::FRAC_PI_4).round() as i32).rem_euclid(4);
    match idx {
        0 => CursorHint::ResizeEw,
        1 => CursorHint::ResizeNwse,
        2 => CursorHint::ResizeNs,
        _ => CursorHint::ResizeNesw,
    }
}

fn handle_pos(rect: RectF, kind: HandleKind) -> PointF {
    let (dx, dy) = kind.direction().unwrap_or((0, 0));
    let c = rect.center();
    PointF::new(c.x + f32::from(dx) * rect.w / 2.0, c.y + f32::from(dy) * rect.h / 2.0)
}

/// All handles for a layout. `zoom` converts screen pixels to image pixels (the rotate knob
/// keeps a constant on-screen distance). `extras` adds balloon-tail / magnifier-source knobs.
pub fn handles(layout: &Layout, zoom: f32, extras: &[(HandleKind, PointF)]) -> Vec<Handle> {
    let mut out = Vec::new();
    match layout {
        Layout::Box { rect, rotation, rotatable } => {
            let c = rect.center();
            for k in HandleKind::RESIZE {
                let p = handle_pos(*rect, k).rotate_about(c, *rotation);
                out.push(Handle { kind: k, pos: p, cursor: resize_cursor(k, *rotation) });
            }
            if *rotatable {
                let knob = PointF::new(c.x, rect.y - ROTATE_KNOB_PX / zoom.max(1e-3))
                    .rotate_about(c, *rotation);
                out.push(Handle { kind: HandleKind::Rotate, pos: knob, cursor: CursorHint::Rotate });
            }
        }
        Layout::Segment { a, b } => {
            out.push(Handle { kind: HandleKind::Endpoint(0), pos: *a, cursor: CursorHint::Crosshair });
            out.push(Handle { kind: HandleKind::Endpoint(1), pos: *b, cursor: CursorHint::Crosshair });
        }
        Layout::Group { bounds } => {
            if bounds.w > 0.0 || bounds.h > 0.0 {
                for k in HandleKind::RESIZE {
                    out.push(Handle {
                        kind: k,
                        pos: handle_pos(*bounds, k),
                        cursor: resize_cursor(k, 0.0),
                    });
                }
            }
        }
    }
    for (kind, pos) in extras {
        out.push(Handle { kind: *kind, pos: *pos, cursor: CursorHint::Crosshair });
    }
    out
}

/// Outline rectangles to draw around a selection.
pub fn outlines(objs: &[&Object]) -> Vec<OrientedRect> {
    objs.iter()
        .map(|o| match o.kind.as_box() {
            Some((r, rot)) if !matches!(o.kind, ObjectKind::Balloon(_)) => {
                OrientedRect { rect: r.normalized(), rotation: rot }
            }
            _ => OrientedRect { rect: o.bounds(), rotation: 0.0 },
        })
        .collect()
}

/// Nearest handle to `p` within `radius` (image pixels).
pub fn hit_handle(handles: &[Handle], p: PointF, radius: f32) -> Option<Handle> {
    handles
        .iter()
        .filter(|h| h.pos.distance(p) <= radius)
        .min_by(|a, b| {
            a.pos.distance(p).partial_cmp(&b.pos.distance(p)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
}

/// New unrotated rectangle after dragging `handle` of a box with `rotation` to pointer `p`.
///
/// Works in the box's own (unrotated) frame, then rotates the new centre back so the
/// opposite edge stays put on screen. `shift` keeps the aspect ratio on corner handles,
/// `alt` resizes symmetrically about the centre. Sizes never drop below 1 px.
pub fn resize_box(orig: RectF, rotation: f32, handle: HandleKind, p: PointF, mods: Modifiers) -> RectF {
    let Some((dx, dy)) = handle.direction() else { return orig };
    let c = orig.center();
    let local = p.rotate_about(c, -rotation);
    let (mut l, mut t, mut r, mut b) = (orig.x, orig.y, orig.right(), orig.bottom());
    if dx < 0 {
        l = local.x;
    }
    if dx > 0 {
        r = local.x;
    }
    if dy < 0 {
        t = local.y;
    }
    if dy > 0 {
        b = local.y;
    }
    if mods.alt {
        if dx != 0 {
            let half = (local.x - c.x).abs();
            l = c.x - half;
            r = c.x + half;
        }
        if dy != 0 {
            let half = (local.y - c.y).abs();
            t = c.y - half;
            b = c.y + half;
        }
    }
    let (mut w, mut h) = ((r - l).abs().max(1.0), (b - t).abs().max(1.0));
    if mods.shift && dx != 0 && dy != 0 && orig.w > 0.0 && orig.h > 0.0 {
        let ratio = orig.w / orig.h;
        if w / ratio > h {
            h = w / ratio;
        } else {
            w = h * ratio;
        }
    }
    let (nx, ny);
    if mods.alt {
        nx = c.x - w / 2.0;
        ny = c.y - h / 2.0;
    } else {
        // Keep the fixed (opposite) edges where they were.
        nx = match dx {
            d if d < 0 => orig.right() - w,
            d if d > 0 => orig.x,
            _ => orig.x + (orig.w - w) / 2.0,
        };
        ny = match dy {
            d if d < 0 => orig.bottom() - h,
            d if d > 0 => orig.y,
            _ => orig.y + (orig.h - h) / 2.0,
        };
    }
    // Edge handles with no shift/alt: the moved edge follows the pointer exactly.
    let (nx, w) = if !mods.alt && !(mods.shift && dx != 0 && dy != 0) && dx != 0 {
        (l.min(r), (r - l).abs().max(1.0))
    } else {
        (nx, w)
    };
    let (ny, h) = if !mods.alt && !(mods.shift && dx != 0 && dy != 0) && dy != 0 {
        (t.min(b), (b - t).abs().max(1.0))
    } else {
        (ny, h)
    };
    let local_new = RectF::new(nx, ny, w, h);
    let world_c = local_new.center().rotate_about(c, rotation);
    RectF::from_center_size(world_c, w, h)
}

/// Scale factors and anchor for dragging `handle` of an axis-aligned group `bounds` to `p`.
pub fn group_scale(
    bounds: RectF,
    handle: HandleKind,
    p: PointF,
    mods: Modifiers,
) -> (PointF, f32, f32) {
    let Some((dx, dy)) = handle.direction() else { return (bounds.center(), 1.0, 1.0) };
    let c = bounds.center();
    let anchor = if mods.alt {
        c
    } else {
        PointF::new(
            match dx {
                d if d < 0 => bounds.right(),
                d if d > 0 => bounds.x,
                _ => c.x,
            },
            match dy {
                d if d < 0 => bounds.bottom(),
                d if d > 0 => bounds.y,
                _ => c.y,
            },
        )
    };
    let h0 = handle_pos(bounds, handle);
    let ratio = |cur: f32, anc: f32, orig: f32| {
        let d = orig - anc;
        if d.abs() < 1e-3 { 1.0 } else { ((cur - anc) / d).max(0.02) }
    };
    let mut sx = if dx == 0 { 1.0 } else { ratio(p.x, anchor.x, h0.x) };
    let mut sy = if dy == 0 { 1.0 } else { ratio(p.y, anchor.y, h0.y) };
    if mods.shift {
        if dx != 0 && dy != 0 {
            let s = if (sx - 1.0).abs() > (sy - 1.0).abs() { sx } else { sy };
            sx = s;
            sy = s;
        } else if dx != 0 {
            sy = sx;
        } else {
            sx = sy;
        }
    }
    (anchor, sx, sy)
}

/// Snaps `to` so the line from `from` is a multiple of 45°, keeping its length.
pub fn constrain_45(from: PointF, to: PointF) -> PointF {
    let d = to - from;
    let len = d.length();
    if len < 1e-4 {
        return to;
    }
    let step = std::f32::consts::FRAC_PI_4;
    let a = (d.y.atan2(d.x) / step).round() * step;
    PointF::new(from.x + len * a.cos(), from.y + len * a.sin())
}

/// Rectangle spanned by a drag from `start` to `cur`: `shift` makes it square, `alt` grows
/// it from `start` as the centre.
pub fn drag_rect(start: PointF, cur: PointF, mods: Modifiers) -> RectF {
    let mut d = cur - start;
    if mods.shift {
        let s = d.x.abs().max(d.y.abs());
        d = PointF::new(s.copysign(if d.x == 0.0 { 1.0 } else { d.x }), s.copysign(if d.y == 0.0 { 1.0 } else { d.y }));
    }
    if mods.alt {
        RectF::from_center_size(start, d.x.abs() * 2.0, d.y.abs() * 2.0)
    } else {
        RectF::from_points(start, start + d)
    }
}

/// Snapping candidates.
#[derive(Debug, Default, Clone)]
pub struct SnapTargets {
    /// x coordinates lines can snap to.
    pub xs: Vec<f32>,
    /// y coordinates lines can snap to.
    pub ys: Vec<f32>,
}

impl SnapTargets {
    /// Adds a rectangle's left/centre/right and top/middle/bottom lines.
    pub fn add_rect(&mut self, r: RectF) {
        self.xs.extend([r.x, r.center().x, r.right()]);
        self.ys.extend([r.y, r.center().y, r.bottom()]);
    }
}

fn nearest(v: f32, targets: &[f32], thr: f32) -> Option<f32> {
    targets
        .iter()
        .copied()
        .filter(|t| (t - v).abs() <= thr)
        .min_by(|a, b| (a - v).abs().partial_cmp(&(b - v).abs()).unwrap_or(std::cmp::Ordering::Equal))
}

/// Snaps a single point; returns the point and the guides that fired.
pub fn snap_point(p: PointF, t: &SnapTargets, thr: f32, extent: RectF) -> (PointF, Vec<Guide>) {
    let mut out = p;
    let mut guides = Vec::new();
    if let Some(x) = nearest(p.x, &t.xs, thr) {
        out.x = x;
        guides.push(Guide { vertical: true, position: x, from: extent.y, to: extent.bottom() });
    }
    if let Some(y) = nearest(p.y, &t.ys, thr) {
        out.y = y;
        guides.push(Guide { vertical: false, position: y, from: extent.x, to: extent.right() });
    }
    (out, guides)
}

/// Snaps a moving rectangle (by its left/centre/right and top/middle/bottom) and returns the
/// extra translation to apply plus the guides.
pub fn snap_rect(r: RectF, t: &SnapTargets, thr: f32, extent: RectF) -> (f32, f32, Vec<Guide>) {
    let mut guides = Vec::new();
    let best = |lines: [f32; 3], targets: &[f32]| -> Option<(f32, f32)> {
        lines
            .iter()
            .filter_map(|&l| nearest(l, targets, thr).map(|tv| (tv - l, tv)))
            .min_by(|a, b| a.0.abs().partial_cmp(&b.0.abs()).unwrap_or(std::cmp::Ordering::Equal))
    };
    let (mut sx, mut sy) = (0.0, 0.0);
    if let Some((d, pos)) = best([r.x, r.center().x, r.right()], &t.xs) {
        sx = d;
        guides.push(Guide { vertical: true, position: pos, from: extent.y, to: extent.bottom() });
    }
    if let Some((d, pos)) = best([r.y, r.center().y, r.bottom()], &t.ys) {
        sy = d;
        guides.push(Guide { vertical: false, position: pos, from: extent.x, to: extent.right() });
    }
    (sx, sy, guides)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn resize_cursors_follow_rotation() {
        assert_eq!(resize_cursor(HandleKind::East, 0.0), CursorHint::ResizeEw);
        assert_eq!(resize_cursor(HandleKind::North, 0.0), CursorHint::ResizeNs);
        assert_eq!(resize_cursor(HandleKind::SouthEast, 0.0), CursorHint::ResizeNwse);
        assert_eq!(resize_cursor(HandleKind::NorthEast, 0.0), CursorHint::ResizeNesw);
        // Rotated by 90°, an east handle points south.
        assert_eq!(resize_cursor(HandleKind::East, std::f32::consts::FRAC_PI_2), CursorHint::ResizeNs);
        assert_eq!(resize_cursor(HandleKind::SouthEast, std::f32::consts::FRAC_PI_2), CursorHint::ResizeNesw);
        assert_eq!(resize_cursor(HandleKind::Rotate, 0.0), CursorHint::Move);
    }

    #[test]
    fn box_handles_include_rotate_knob_and_rotate_with_box() {
        let l = Layout::Box { rect: RectF::new(0.0, 0.0, 100.0, 50.0), rotation: 0.0, rotatable: true };
        let hs = handles(&l, 1.0, &[]);
        assert_eq!(hs.len(), 9);
        let knob = hs.iter().find(|h| h.kind == HandleKind::Rotate).unwrap();
        assert_eq!(knob.pos, PointF::new(50.0, -ROTATE_KNOB_PX));
        let east = hs.iter().find(|h| h.kind == HandleKind::East).unwrap();
        assert_eq!(east.pos, PointF::new(100.0, 25.0));
        // Knob distance is constant on screen: at zoom 2 it is half as far in image space.
        let hs2 = handles(&l, 2.0, &[]);
        let k2 = hs2.iter().find(|h| h.kind == HandleKind::Rotate).unwrap();
        assert_eq!(k2.pos.y, -ROTATE_KNOB_PX / 2.0);
        let rot = Layout::Box {
            rect: RectF::new(0.0, 0.0, 100.0, 50.0),
            rotation: std::f32::consts::FRAC_PI_2,
            rotatable: false,
        };
        let hr = handles(&rot, 1.0, &[(HandleKind::Tail, PointF::new(1.0, 2.0))]);
        assert_eq!(hr.len(), 9, "8 resize + tail, no rotate knob");
        let e = hr.iter().find(|h| h.kind == HandleKind::East).unwrap();
        assert!((e.pos.x - 50.0).abs() < 1e-3 && (e.pos.y - 75.0).abs() < 1e-3, "{:?}", e.pos);
    }

    #[test]
    fn resize_unrotated_edges_and_corners() {
        let r = RectF::new(10.0, 10.0, 100.0, 50.0);
        let e = resize_box(r, 0.0, HandleKind::East, PointF::new(160.0, 999.0), NONE);
        assert_eq!(e, RectF::new(10.0, 10.0, 150.0, 50.0));
        let w = resize_box(r, 0.0, HandleKind::West, PointF::new(0.0, 0.0), NONE);
        assert_eq!(w, RectF::new(0.0, 10.0, 110.0, 50.0));
        let se = resize_box(r, 0.0, HandleKind::SouthEast, PointF::new(60.0, 40.0), NONE);
        assert_eq!(se, RectF::new(10.0, 10.0, 50.0, 30.0));
        // Dragging past the opposite edge flips instead of going negative.
        let flip = resize_box(r, 0.0, HandleKind::East, PointF::new(0.0, 0.0), NONE);
        assert_eq!((flip.x, flip.w), (0.0, 10.0));
        // Never collapses below 1 px.
        let tiny = resize_box(r, 0.0, HandleKind::East, PointF::new(10.0, 0.0), NONE);
        assert_eq!(tiny.w, 1.0);
    }

    #[test]
    fn resize_shift_keeps_aspect_and_alt_is_symmetric() {
        let r = RectF::new(0.0, 0.0, 100.0, 50.0);
        let s = resize_box(r, 0.0, HandleKind::SouthEast, PointF::new(200.0, 60.0), Modifiers::SHIFT);
        assert!((s.w / s.h - 2.0).abs() < 1e-4, "{s:?}");
        assert_eq!((s.x, s.y), (0.0, 0.0), "opposite corner fixed");
        let a = resize_box(r, 0.0, HandleKind::East, PointF::new(80.0, 25.0), Modifiers::ALT);
        assert_eq!(a.center(), r.center());
        assert_eq!(a.w, 60.0);
    }

    #[test]
    fn resize_rotated_keeps_opposite_edge_fixed_on_screen() {
        let r = RectF::new(0.0, 0.0, 100.0, 50.0);
        let rot = 0.6f32;
        let c = r.center();
        // World position of the west-middle point before.
        let west_before = PointF::new(0.0, 25.0).rotate_about(c, rot);
        // Drag the east handle outwards along the box's own x axis by 40.
        let east_now = PointF::new(140.0, 25.0).rotate_about(c, rot);
        let n = resize_box(r, rot, HandleKind::East, east_now, NONE);
        assert!((n.w - 140.0).abs() < 1e-3 && (n.h - 50.0).abs() < 1e-3);
        let west_after = PointF::new(n.x, n.y + 25.0).rotate_about(n.center(), rot);
        assert!(west_after.distance(west_before) < 1e-3, "{west_after:?} vs {west_before:?}");
    }

    #[test]
    fn group_scale_anchor_and_uniform() {
        let b = RectF::new(0.0, 0.0, 100.0, 100.0);
        let (a, sx, sy) = group_scale(b, HandleKind::SouthEast, PointF::new(200.0, 150.0), NONE);
        assert_eq!(a, PointF::new(0.0, 0.0));
        assert_eq!((sx, sy), (2.0, 1.5));
        let (_, ux, uy) = group_scale(b, HandleKind::SouthEast, PointF::new(200.0, 150.0), Modifiers::SHIFT);
        assert_eq!(ux, uy);
        assert_eq!(ux, 2.0);
        let (_, ex, ey) = group_scale(b, HandleKind::East, PointF::new(50.0, 0.0), NONE);
        assert_eq!((ex, ey), (0.5, 1.0));
        let (c, _, _) = group_scale(b, HandleKind::East, PointF::new(150.0, 0.0), Modifiers::ALT);
        assert_eq!(c, PointF::new(50.0, 50.0));
        // Dragging through the anchor never produces zero or negative scale.
        let (_, nx, _) = group_scale(b, HandleKind::East, PointF::new(-500.0, 0.0), NONE);
        assert!(nx > 0.0);
    }

    #[test]
    fn constrain_and_drag_rect() {
        let p = constrain_45(PointF::new(0.0, 0.0), PointF::new(100.0, 10.0));
        assert!((p.y).abs() < 1e-3 && (p.x - 100.499).abs() < 0.01, "{p:?}");
        let d = constrain_45(PointF::new(0.0, 0.0), PointF::new(50.0, 60.0));
        assert!((d.x - d.y).abs() < 1e-3);
        assert_eq!(constrain_45(PointF::new(1.0, 1.0), PointF::new(1.0, 1.0)), PointF::new(1.0, 1.0));
        let sq = drag_rect(PointF::new(10.0, 10.0), PointF::new(60.0, 30.0), Modifiers::SHIFT);
        assert_eq!((sq.w, sq.h), (50.0, 50.0));
        let neg = drag_rect(PointF::new(10.0, 10.0), PointF::new(-40.0, 30.0), Modifiers::SHIFT);
        assert_eq!(neg, RectF::new(-40.0, 10.0, 50.0, 50.0));
        let centred = drag_rect(PointF::new(50.0, 50.0), PointF::new(60.0, 70.0), Modifiers::ALT);
        assert_eq!(centred, RectF::new(40.0, 30.0, 20.0, 40.0));
    }

    #[test]
    fn snapping_points_and_rects() {
        let mut t = SnapTargets::default();
        t.add_rect(RectF::new(100.0, 100.0, 50.0, 50.0));
        let ext = RectF::new(0.0, 0.0, 400.0, 300.0);
        let (p, g) = snap_point(PointF::new(103.0, 300.0), &t, 5.0, ext);
        assert_eq!(p, PointF::new(100.0, 300.0));
        assert_eq!(g.len(), 1);
        assert!(g[0].vertical && g[0].position == 100.0);
        let (p, g) = snap_point(PointF::new(110.0, 110.0), &t, 5.0, ext);
        assert_eq!(p, PointF::new(110.0, 110.0));
        assert!(g.is_empty());
        // Moving rect whose right edge (170) is near target 150+... choose nearest line.
        let (dx, dy, g) = snap_rect(RectF::new(96.0, 400.0, 10.0, 10.0), &t, 5.0, ext);
        assert_eq!((dx, dy), (-1.0, 0.0)); // centre line 101 -> 100
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn hit_handle_prefers_nearest() {
        let l = Layout::Group { bounds: RectF::new(0.0, 0.0, 10.0, 10.0) };
        let hs = handles(&l, 1.0, &[]);
        let h = hit_handle(&hs, PointF::new(9.0, 9.0), 4.0).unwrap();
        assert_eq!(h.kind, HandleKind::SouthEast);
        assert!(hit_handle(&hs, PointF::new(-50.0, -50.0), 4.0).is_none());
    }
}
