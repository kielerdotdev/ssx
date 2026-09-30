//! Dirty-rectangle computation: what changed between two [`Scene`]s.
//!
//! The rule is *soundness first*: every pixel that renders differently must lie inside the
//! returned rectangles (the renderer tests prove this by property: rendering scene B over a
//! render of scene A restricted to the damage equals a from-scratch render of B). Tightness
//! is where the performance comes from: dragging a large selection dirties only the strips
//! between the old and new edge plus the handle rings, not the selection interior.

use ssx_types::{Point, Rect};

use super::geometry::{bounding_points, coalesce, inflate_i64, ring, subtract};
use super::scene::{Cutout, Scene, crosshair_strips, decor_pad};

/// Maximum number of rectangles returned; more than this costs more in per-rect overhead
/// than it saves in area.
pub const MAX_RECTS: usize = 8;

/// Dirty rectangles (desktop pixels) that turn a render of `old` into a render of `new`.
/// `old == None` means nothing has been drawn yet: the whole desktop is dirty.
pub fn between(old: Option<&Scene>, new: &Scene) -> Vec<Rect> {
    let Some(old) = old else { return vec![new.bounds] };
    if old == new {
        return Vec::new();
    }
    let mut d: Vec<Rect> = Vec::new();
    let scale_changed = (old.ui_scale - new.ui_scale).abs() > f32::EPSILON;

    // Bright cut-out (selection, else highlight).
    let cut = |s: &Scene| s.selection.or(s.highlight);
    let (co, cn) = (cut(old), cut(new));
    if co != cn || old.cutout != new.cutout {
        match (co, cn) {
            (Some(a), Some(b)) if old.cutout == Cutout::Rect && new.cutout == Cutout::Rect => {
                d.extend(subtract(a, b));
                d.extend(subtract(b, a));
            }
            (a, b) => {
                d.extend(a);
                d.extend(b);
            }
        }
    }

    // Borders and handles of both selection and highlight.
    let pad = decor_pad(old.ui_scale.max(new.ui_scale));
    let decor = |s: &Scene, which: fn(&Scene) -> Option<Rect>, d: &mut Vec<Rect>| {
        if let Some(r) = which(s) {
            d.extend(ring(r, pad, pad));
        }
    };
    let sel = |s: &Scene| s.selection;
    let hi = |s: &Scene| s.highlight;
    if old.selection != new.selection
        || old.handles != new.handles
        || old.active_handle != new.active_handle
        || old.cutout != new.cutout
        || scale_changed
    {
        decor(old, sel, &mut d);
        decor(new, sel, &mut d);
    }
    if old.highlight != new.highlight || scale_changed {
        decor(old, hi, &mut d);
        decor(new, hi, &mut d);
    }

    // Freeform outline.
    if old.freeform != new.freeform {
        let fpad = i64::from(pad);
        let appended = !old.freeform.is_empty()
            && new.freeform.len() >= old.freeform.len()
            && new.freeform[..old.freeform.len()] == old.freeform[..];
        if appended {
            // The polygon's fill only changes inside the bounding box of the first vertex,
            // the old last vertex and the appended vertices.
            let mut pts: Vec<Point> = vec![old.freeform[0], old.freeform[old.freeform.len() - 1]];
            pts.extend_from_slice(&new.freeform[old.freeform.len()..]);
            d.extend(bounding_points(&pts).map(|r| inflate_i64(r, fpad)));
        } else {
            d.extend(bounding_points(&old.freeform).map(|r| inflate_i64(r, fpad)));
            d.extend(bounding_points(&new.freeform).map(|r| inflate_i64(r, fpad)));
        }
    }

    // Crosshair guides.
    if old.crosshair != new.crosshair || scale_changed {
        if let Some(p) = old.crosshair {
            d.extend(crosshair_strips(p, old.ui_scale, old.bounds));
        }
        if let Some(p) = new.crosshair {
            d.extend(crosshair_strips(p, new.ui_scale, new.bounds));
        }
    }

    // Label and loupe boxes.
    if old.label != new.label {
        d.extend(old.label.as_ref().map(|l| l.rect));
        d.extend(new.label.as_ref().map(|l| l.rect));
    }
    if old.loupe != new.loupe {
        d.extend(old.loupe.as_ref().map(|l| l.outer));
        d.extend(new.loupe.as_ref().map(|l| l.outer));
    }

    coalesce(d, new.bounds, MAX_RECTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        Scene::empty(Rect::new(0, 0, 1000, 800))
    }

    #[test]
    fn first_frame_is_everything() {
        assert_eq!(between(None, &scene()), vec![Rect::new(0, 0, 1000, 800)]);
    }

    #[test]
    fn identical_scenes_have_no_damage() {
        let s = scene();
        assert!(between(Some(&s), &s.clone()).is_empty());
    }

    #[test]
    fn dragging_a_large_selection_does_not_dirty_the_interior() {
        let mut a = scene();
        a.selection = Some(Rect::new(100, 100, 700, 500));
        let mut b = a.clone();
        b.selection = Some(Rect::new(100, 100, 710, 505));
        let dirty = between(Some(&a), &b);
        let area: u64 = dirty.iter().map(|r| r.area()).sum();
        assert!(area < 700 * 500 / 4, "damage {area} too large: {dirty:?}");
        assert!(!dirty.iter().any(|r| r.contains(Point::new(400, 300))));
    }

    #[test]
    fn crosshair_motion_dirties_only_strips() {
        let mut a = scene();
        a.crosshair = Some(Point::new(10, 10));
        let mut b = a.clone();
        b.crosshair = Some(Point::new(500, 400));
        let dirty = between(Some(&a), &b);
        let area: u64 = dirty.iter().map(|r| r.area()).sum();
        assert!(area <= 4 * 1000 * 12 + 4 * 800 * 12, "{dirty:?}");
        assert!(dirty.len() <= MAX_RECTS);
    }

    #[test]
    fn freeform_append_is_bounded_by_new_points() {
        let mut a = scene();
        a.freeform = vec![Point::new(0, 0), Point::new(100, 0), Point::new(100, 100)];
        let mut b = a.clone();
        b.freeform.push(Point::new(90, 110));
        let dirty = between(Some(&a), &b);
        let union = Rect::bounding(dirty.iter().copied()).unwrap();
        assert!(union.right() < 200 && union.bottom() < 200);
    }
}
