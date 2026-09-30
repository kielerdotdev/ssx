//! Which parts of the zoomed picture need (re)rendering, without any GPU or egui types.
//!
//! The canvas shows the document through a grid of fixed-size **tiles** in *output pixel*
//! space (`canvas pixels x zoom`, exactly what `ssx_editor::RenderOptions::viewport` addresses).
//! Tiles are rendered lazily when they scroll into view and repainted in place
//! (`TextureHandle::set_partial`) when the engine reports a dirty rectangle, so:
//!
//! * a 4K image only ever costs the visible ~1080p of rendering, at any zoom;
//! * dragging a rectangle re-renders just the tiles it touches, and within them just the
//!   dirty rectangle;
//! * off-screen tiles never do work: a dirty rectangle that touches an invisible tile simply
//!   drops that tile, and it is re-rendered from scratch if it ever comes back.
//!
//! [`TilePlanner`] is the bookkeeping: it decides *what* to render and in which order (nearest
//! to the centre of the view first) and forgets the rest; the canvas does the rendering and
//! texture upload. Keeping the bookkeeping separate makes the tricky part (dirty rectangles,
//! eviction, ordering) unit-testable.

use std::collections::HashMap;

/// Edge length of a tile in output pixels.
pub const TILE: i32 = 512;

/// An integer rectangle `[x0, x1) x [y0, y1)` in output pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IRect {
    /// Left.
    pub x0: i32,
    /// Top.
    pub y0: i32,
    /// Right (exclusive).
    pub x1: i32,
    /// Bottom (exclusive).
    pub y1: i32,
}

impl IRect {
    /// Builds a rectangle.
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// Width (0 when empty).
    pub fn width(&self) -> i32 {
        (self.x1 - self.x0).max(0)
    }

    /// Height (0 when empty).
    pub fn height(&self) -> i32 {
        (self.y1 - self.y0).max(0)
    }

    /// `true` when it covers no pixels.
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    /// Overlap of two rectangles (`None` when disjoint).
    pub fn intersect(&self, o: &IRect) -> Option<IRect> {
        let r =
            IRect::new(self.x0.max(o.x0), self.y0.max(o.y0), self.x1.min(o.x1), self.y1.min(o.y1));
        (!r.is_empty()).then_some(r)
    }

    /// Smallest rectangle covering both.
    pub fn union(&self, o: &IRect) -> IRect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        IRect::new(self.x0.min(o.x0), self.y0.min(o.y0), self.x1.max(o.x1), self.y1.max(o.y1))
    }

    /// Centre point (may be fractional; returned doubled to stay integral).
    fn centre2(&self) -> (i64, i64) {
        (i64::from(self.x0 + self.x1), i64::from(self.y0 + self.y1))
    }
}

/// A tile's grid coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    /// Column.
    pub tx: i32,
    /// Row.
    pub ty: i32,
}

impl TileKey {
    /// The tile's area in output pixels, clipped to the output size.
    pub fn rect(self, bounds: (i32, i32)) -> IRect {
        IRect::new(
            self.tx * TILE,
            self.ty * TILE,
            ((self.tx + 1) * TILE).min(bounds.0),
            ((self.ty + 1) * TILE).min(bounds.1),
        )
    }
}

/// Tiles that overlap `view` (clipped to `bounds`), nearest to the view's centre first.
pub fn visible_tiles(view: IRect, bounds: (i32, i32)) -> Vec<TileKey> {
    let Some(v) = view.intersect(&IRect::new(0, 0, bounds.0, bounds.1)) else { return Vec::new() };
    let (tx0, ty0) = (v.x0.div_euclid(TILE), v.y0.div_euclid(TILE));
    let (tx1, ty1) = ((v.x1 - 1).div_euclid(TILE), (v.y1 - 1).div_euclid(TILE));
    let mut out: Vec<TileKey> =
        (ty0..=ty1).flat_map(|ty| (tx0..=tx1).map(move |tx| TileKey { tx, ty })).collect();
    let (cx, cy) = view.centre2();
    out.sort_by_key(|k| {
        let (kx, ky) = k.rect(bounds).centre2();
        let (dx, dy) = ((kx - cx) / 2, (ky - cy) / 2);
        (dx * dx + dy * dy, k.ty, k.tx)
    });
    out
}

#[derive(Debug, Clone, Default)]
struct TileState {
    /// The tile has been rendered at least once.
    rendered: bool,
    /// Dirty region (output px) not yet re-rendered.
    pending: Option<IRect>,
}

/// One unit of rendering work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    /// The tile.
    pub key: TileKey,
    /// Region to render (whole tile when `full`).
    pub rect: IRect,
    /// The tile has no texture yet; the whole tile must be rendered and uploaded.
    pub full: bool,
}

/// Bookkeeping of tile freshness for one zoom level.
#[derive(Debug, Clone)]
pub struct TilePlanner {
    zoom: f32,
    bounds: (i32, i32),
    tiles: HashMap<TileKey, TileState>,
}

impl TilePlanner {
    /// An empty planner for `bounds` output pixels at `zoom`.
    pub fn new(zoom: f32, bounds: (i32, i32)) -> Self {
        Self { zoom, bounds, tiles: HashMap::new() }
    }

    /// The zoom this planner was created for.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// The output size in pixels.
    pub fn bounds(&self) -> (i32, i32) {
        self.bounds
    }

    /// Does this planner still describe `zoom` and `bounds`?
    pub fn matches(&self, zoom: f32, bounds: (i32, i32)) -> bool {
        (self.zoom - zoom).abs() <= f32::EPSILON * zoom.max(1.0) && self.bounds == bounds
    }

    /// Tiles currently held.
    pub fn keys(&self) -> impl Iterator<Item = TileKey> + '_ {
        self.tiles.keys().copied()
    }

    /// Is `key` rendered and up to date?
    pub fn is_fresh(&self, key: TileKey) -> bool {
        self.tiles.get(&key).is_some_and(|t| t.rendered && t.pending.is_none())
    }

    /// Is `key` rendered (possibly with a pending refresh)?
    pub fn is_rendered(&self, key: TileKey) -> bool {
        self.tiles.get(&key).is_some_and(|t| t.rendered)
    }

    /// Records that `r` (output px) changed. Visible tiles refresh in place; invisible ones are
    /// dropped (`view` is what is currently on screen, with some slack if the caller wants).
    /// Returns the keys of dropped tiles so their textures can be freed.
    pub fn mark_dirty(&mut self, r: IRect, view: IRect) -> Vec<TileKey> {
        let Some(r) = r.intersect(&IRect::new(0, 0, self.bounds.0, self.bounds.1)) else {
            return Vec::new();
        };
        let mut dropped = Vec::new();
        for (key, state) in &mut self.tiles {
            let tr = key.rect(self.bounds);
            let Some(hit) = tr.intersect(&r) else { continue };
            if tr.intersect(&view).is_some() {
                state.pending = Some(state.pending.map_or(hit, |p| p.union(&hit)));
            } else {
                dropped.push(*key);
            }
        }
        for k in &dropped {
            self.tiles.remove(k);
        }
        dropped
    }

    /// Marks every tile as needing a full refresh of its visible part.
    pub fn mark_all_dirty(&mut self, view: IRect) -> Vec<TileKey> {
        self.mark_dirty(IRect::new(0, 0, self.bounds.0, self.bounds.1), view)
    }

    /// The rendering to do for the tiles in `view`, most central first.
    pub fn work(&self, view: IRect) -> Vec<Work> {
        visible_tiles(view, self.bounds)
            .into_iter()
            .filter_map(|key| {
                let full_rect = key.rect(self.bounds);
                match self.tiles.get(&key) {
                    Some(TileState { rendered: true, pending: None }) => None,
                    Some(TileState { rendered: true, pending: Some(p) }) => {
                        Some(Work { key, rect: p.intersect(&full_rect)?, full: false })
                    }
                    _ => Some(Work { key, rect: full_rect, full: true }),
                }
            })
            .collect()
    }

    /// Records that `w` has been rendered and uploaded.
    pub fn done(&mut self, w: &Work) {
        let st = self.tiles.entry(w.key).or_default();
        if w.full {
            st.rendered = true;
            st.pending = None;
        } else if let Some(p) = st.pending {
            // The region just rendered covers the pending rectangle (they are the same value),
            // unless new dirt arrived in between, which `mark_dirty` merges into `pending`.
            if w.rect.intersect(&p) == Some(p) {
                st.pending = None;
            }
        }
    }

    /// Drops tiles farthest from `view` until at most `max` remain; returns the dropped keys.
    pub fn evict(&mut self, view: IRect, max: usize) -> Vec<TileKey> {
        if self.tiles.len() <= max {
            return Vec::new();
        }
        let (cx, cy) = view.centre2();
        let mut keys: Vec<(i64, TileKey)> = self
            .tiles
            .keys()
            .map(|k| {
                let (kx, ky) = k.rect(self.bounds).centre2();
                let (dx, dy) = ((kx - cx) / 2, (ky - cy) / 2);
                (dx * dx + dy * dy, *k)
            })
            .collect();
        keys.sort_by(|a, b| b.0.cmp(&a.0));
        let drop_n = self.tiles.len() - max;
        let dropped: Vec<TileKey> = keys.into_iter().take(drop_n).map(|(_, k)| k).collect();
        for k in &dropped {
            self.tiles.remove(k);
        }
        dropped
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact values are what these tests assert
mod tests {
    use proptest::prelude::*;

    use super::*;

    const B: (i32, i32) = (2000, 1200);

    #[test]
    fn irect_basics() {
        let a = IRect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(&IRect::new(5, 5, 20, 20)), Some(IRect::new(5, 5, 10, 10)));
        assert_eq!(a.intersect(&IRect::new(10, 0, 20, 10)), None, "edges touching is not overlap");
        assert_eq!(a.union(&IRect::new(5, 5, 20, 8)), IRect::new(0, 0, 20, 10));
        assert!(IRect::new(3, 3, 3, 9).is_empty());
        assert_eq!(IRect::new(5, 5, 1, 1).width(), 0);
        assert_eq!(IRect::default_empty().union(&a), a);
    }

    impl IRect {
        fn default_empty() -> IRect {
            IRect::new(0, 0, 0, 0)
        }
    }

    #[test]
    fn tile_rects_are_clipped_to_the_output() {
        assert_eq!(TileKey { tx: 0, ty: 0 }.rect(B), IRect::new(0, 0, 512, 512));
        assert_eq!(TileKey { tx: 3, ty: 2 }.rect(B), IRect::new(1536, 1024, 2000, 1200));
    }

    #[test]
    fn visible_tiles_cover_the_view_nearest_first() {
        let view = IRect::new(400, 400, 1200, 900);
        let tiles = visible_tiles(view, B);
        // Columns 0..=2 and rows 0..=1.
        assert_eq!(tiles.len(), 6);
        let first = tiles[0].rect(B);
        assert!(
            first.intersect(&IRect::new(790, 640, 810, 660)).is_some(),
            "{first:?} should hold the centre"
        );
        assert!(visible_tiles(IRect::new(5000, 0, 6000, 100), B).is_empty());
        assert!(visible_tiles(IRect::new(0, 0, 0, 0), B).is_empty());
        assert!(visible_tiles(IRect::new(-500, -500, 10, 10), B).len() == 1);
    }

    #[test]
    fn fresh_planner_wants_every_visible_tile_once() {
        let mut p = TilePlanner::new(1.0, B);
        let view = IRect::new(0, 0, 1000, 600);
        let work = p.work(view);
        assert_eq!(work.len(), 4);
        assert!(work.iter().all(|w| w.full));
        for w in &work {
            p.done(w);
        }
        assert!(p.work(view).is_empty(), "nothing left to do");
        assert!(work.iter().all(|w| p.is_fresh(w.key)));
    }

    #[test]
    fn dirty_rects_refresh_only_the_dirty_part_of_visible_tiles() {
        let mut p = TilePlanner::new(1.0, B);
        let view = IRect::new(0, 0, 1000, 600);
        for w in p.work(view) {
            p.done(&w);
        }
        let dropped = p.mark_dirty(IRect::new(500, 100, 530, 140), view);
        assert!(dropped.is_empty());
        let work = p.work(view);
        // The rectangle straddles the boundary between tile columns 0 and 1.
        assert_eq!(work.len(), 2);
        assert!(work.iter().all(|w| !w.full));
        let total: i32 = work.iter().map(|w| w.rect.width() * w.rect.height()).sum();
        assert_eq!(total, 30 * 40, "exactly the dirty area is re-rendered");
        for w in &work {
            p.done(w);
        }
        assert!(p.work(view).is_empty());
    }

    #[test]
    fn repeated_dirt_merges_into_one_rect() {
        let mut p = TilePlanner::new(1.0, B);
        let view = IRect::new(0, 0, 500, 500);
        for w in p.work(view) {
            p.done(&w);
        }
        p.mark_dirty(IRect::new(10, 10, 20, 20), view);
        p.mark_dirty(IRect::new(100, 100, 120, 120), view);
        let work = p.work(view);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].rect, IRect::new(10, 10, 120, 120));
    }

    #[test]
    fn invisible_tiles_are_dropped_not_refreshed() {
        let mut p = TilePlanner::new(1.0, B);
        for w in p.work(IRect::new(0, 0, 2000, 1200)) {
            p.done(&w);
        }
        let all = p.keys().count();
        assert_eq!(all, 12);
        let view = IRect::new(0, 0, 500, 500);
        let dropped = p.mark_dirty(IRect::new(0, 0, 2000, 1200), view);
        assert_eq!(dropped.len(), 11, "only the visible tile survives, marked dirty");
        assert_eq!(p.keys().count(), 1);
        // Scrolling back later re-renders the dropped tiles in full.
        let work = p.work(IRect::new(0, 0, 2000, 1200));
        assert_eq!(work.iter().filter(|w| w.full).count(), 11);
        assert_eq!(work.iter().filter(|w| !w.full).count(), 1);
    }

    #[test]
    fn eviction_keeps_the_tiles_nearest_the_view() {
        let mut p = TilePlanner::new(1.0, (8000, 512));
        for w in p.work(IRect::new(0, 0, 8000, 512)) {
            p.done(&w);
        }
        assert_eq!(p.keys().count(), 16);
        let view = IRect::new(0, 0, 512, 512);
        let dropped = p.evict(view, 4);
        assert_eq!(dropped.len(), 12);
        assert!(p.is_rendered(TileKey { tx: 0, ty: 0 }));
        assert!(p.is_rendered(TileKey { tx: 3, ty: 0 }));
        assert!(!p.is_rendered(TileKey { tx: 15, ty: 0 }));
        assert!(p.evict(view, 100).is_empty());
    }

    #[test]
    fn matches_detects_zoom_and_size_changes() {
        let p = TilePlanner::new(0.5, B);
        assert!(p.matches(0.5, B));
        assert!(!p.matches(0.51, B));
        assert!(!p.matches(0.5, (1, 1)));
        assert_eq!(p.zoom(), 0.5);
        assert_eq!(p.bounds(), B);
    }

    proptest! {
        /// After doing all the work, no visible tile is stale, whatever the dirt.
        #[test]
        fn work_always_converges(
            dirt in proptest::collection::vec((0i32..2000, 0i32..1200, 1i32..600, 1i32..600), 0..12),
            vx in 0i32..1200, vy in 0i32..600,
        ) {
            let mut p = TilePlanner::new(1.0, B);
            let view = IRect::new(vx, vy, vx + 900, vy + 700);
            for w in p.work(view) { p.done(&w); }
            for (x, y, w, h) in dirt {
                p.mark_dirty(IRect::new(x, y, x + w, y + h), view);
                for w in p.work(view) { p.done(&w); }
            }
            prop_assert!(p.work(view).is_empty());
            for k in visible_tiles(view, B) {
                prop_assert!(p.is_fresh(k));
            }
        }
    }
}
