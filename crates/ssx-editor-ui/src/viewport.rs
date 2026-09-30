//! Zoom and pan maths for the canvas, free of any drawing code.
//!
//! # Units
//!
//! Everything here is in **screen units**, and the canvas widget chooses those to be
//! *physical pixels* (egui points multiplied by `pixels_per_point`). That makes `zoom` "device
//! pixels per canvas pixel" so 100 % really is one image pixel per screen pixel on a HiDPI
//! display, and the tile renderer can ask the engine for `scale = zoom` and blit the result
//! 1:1 without resampling. The conversion to points happens once, at the widget boundary.
//!
//! The viewport works in *canvas pixels* (origin = top-left of the padded canvas, as in
//! `ssx_editor::RenderOptions`); the caller adds the padding offset to get image coordinates.
//!
//! # Clamping
//!
//! Panning is *loose*: the content may leave the view as long as a strip of it (at most
//! [`MIN_VISIBLE`] units) stays visible, so the user can never lose the image. Loose clamping
//! has a useful property that [`Viewport::zoom_at`] relies on: zooming about a point that is on
//! the content keeps that point under the cursor exactly, because the content still covers it
//! afterwards and the clamp never has to move anything.

use egui::{Pos2, Rect, Vec2, pos2, vec2};

/// Smallest zoom (1 %).
pub const MIN_ZOOM: f32 = 0.01;
/// Largest zoom (6400 %).
pub const MAX_ZOOM: f32 = 64.0;
/// How much of the content must stay inside the view, in screen units.
pub const MIN_VISIBLE: f32 = 64.0;
/// Margin left around the content by [`Viewport::fit`].
pub const FIT_MARGIN: f32 = 24.0;
/// Zoom from which the pixel grid is drawn.
pub const GRID_ZOOM: f32 = 8.0;

/// The zoom ladder used by the zoom in/out commands (like most image editors).
pub const ZOOM_STEPS: [f32; 24] = [
    0.01, 0.02, 0.05, 0.0625, 0.125, 0.25, 0.333, 0.5, 0.667, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0,
    8.0, 12.0, 16.0, 24.0, 32.0, 48.0, 56.0, 64.0,
];

/// Zoom/pan state of the canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewport {
    zoom: f32,
    /// Screen position (relative to the widget's top-left) of the canvas' top-left corner.
    offset: Vec2,
    view: Vec2,
    content: Vec2,
    /// While set, resizing the widget or the content re-fits the picture.
    follow_fit: bool,
}

impl Viewport {
    /// A viewport over `content` (canvas size) shown in a widget of size `view`, fitted.
    pub fn new(view: Vec2, content: Vec2) -> Self {
        let mut v = Self { zoom: 1.0, offset: Vec2::ZERO, view, content, follow_fit: true };
        v.fit();
        v
    }

    /// Current zoom (screen units per canvas pixel).
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Zoom as a percentage for display.
    pub fn percent(&self) -> f32 {
        self.zoom * 100.0
    }

    /// Size of the widget.
    pub fn view(&self) -> Vec2 {
        self.view
    }

    /// Size of the content (canvas pixels).
    pub fn content(&self) -> Vec2 {
        self.content
    }

    /// `true` while the view follows "fit to window".
    pub fn is_fitted(&self) -> bool {
        self.follow_fit
    }

    /// Exact (fractional) screen position of the canvas origin.
    pub fn offset(&self) -> Vec2 {
        self.offset
    }

    /// Screen position of the canvas origin rounded to whole units, so tiles blit without
    /// resampling. All conversions below use this.
    pub fn origin(&self) -> Vec2 {
        vec2(self.offset.x.round(), self.offset.y.round())
    }

    /// Canvas pixels → screen units.
    pub fn canvas_to_screen(&self, p: Pos2) -> Pos2 {
        let o = self.origin();
        pos2(o.x + p.x * self.zoom, o.y + p.y * self.zoom)
    }

    /// Screen units → canvas pixels.
    pub fn screen_to_canvas(&self, p: Pos2) -> Pos2 {
        let o = self.origin();
        pos2((p.x - o.x) / self.zoom, (p.y - o.y) / self.zoom)
    }

    /// The content rectangle on screen.
    pub fn content_screen_rect(&self) -> Rect {
        Rect::from_min_size(self.origin().to_pos2(), self.content * self.zoom)
    }

    /// The part of the canvas that is on screen, in canvas pixels.
    pub fn visible_canvas_rect(&self) -> Rect {
        let view = Rect::from_min_size(Pos2::ZERO, self.view);
        let a = self.screen_to_canvas(view.min);
        let b = self.screen_to_canvas(view.max);
        Rect::from_min_max(a, b).intersect(Rect::from_min_size(Pos2::ZERO, self.content))
    }

    /// The part of the *output* (canvas × zoom) that is on screen, as whole pixels
    /// `(x0, y0, x1, y1)`, clipped to the content. Empty (`x1 <= x0`) when nothing is visible.
    pub fn visible_output_px(&self) -> (i32, i32, i32, i32) {
        let o = self.origin();
        let out = self.content * self.zoom;
        let x0 = (-o.x).max(0.0).floor();
        let y0 = (-o.y).max(0.0).floor();
        let x1 = (self.view.x - o.x).min(out.x.ceil()).ceil();
        let y1 = (self.view.y - o.y).min(out.y.ceil()).ceil();
        (x0 as i32, y0 as i32, x1 as i32, y1 as i32)
    }

    /// Size of the rendered canvas in output pixels at the current zoom.
    pub fn output_size(&self) -> (i32, i32) {
        let s = self.content * self.zoom;
        (s.x.ceil() as i32, s.y.ceil() as i32)
    }

    /// Updates the widget size. Re-fits while [`Self::is_fitted`], otherwise keeps the centre.
    pub fn set_view(&mut self, view: Vec2) {
        if view == self.view {
            return;
        }
        if self.follow_fit {
            self.view = view;
            self.fit();
        } else {
            let centre_canvas = self.screen_to_canvas((self.view / 2.0).to_pos2());
            self.view = view;
            self.offset = view / 2.0 - centre_canvas.to_vec2() * self.zoom;
            self.clamp();
        }
    }

    /// Updates the content size (after crop, rotate, resize...). Re-fits when following fit,
    /// otherwise keeps the top-left corner where it is.
    pub fn set_content(&mut self, content: Vec2) {
        if content == self.content {
            return;
        }
        self.content = content;
        if self.follow_fit {
            self.fit();
        } else {
            self.clamp();
        }
    }

    /// Fits the content into the view: as large as possible but never above 100 %.
    pub fn fit(&mut self) {
        self.fit_to(1.0);
    }

    /// Fits the content into the view allowing up to `max_zoom`.
    pub fn fit_to(&mut self, max_zoom: f32) {
        self.follow_fit = true;
        let avail = (self.view - Vec2::splat(2.0 * FIT_MARGIN)).max(Vec2::splat(1.0));
        let c = self.content.max(Vec2::splat(1.0));
        let z = (avail.x / c.x).min(avail.y / c.y).min(max_zoom);
        self.zoom = z.clamp(MIN_ZOOM, MAX_ZOOM);
        self.center();
    }

    /// Centres the content in the view at the current zoom.
    pub fn center(&mut self) {
        self.offset = (self.view - self.content * self.zoom) / 2.0;
    }

    /// Zooms to `zoom` keeping the canvas point under `anchor` (a screen position) fixed.
    pub fn zoom_to_at(&mut self, anchor: Pos2, zoom: f32) {
        self.follow_fit = false;
        let new = if zoom.is_finite() { zoom.clamp(MIN_ZOOM, MAX_ZOOM) } else { self.zoom };
        let k = new / self.zoom;
        // Start from the *displayed* (rounded) origin so the point under the cursor is stable
        // in what the user actually sees, not in the hidden fractional offset.
        self.offset = anchor.to_vec2() - (anchor.to_vec2() - self.origin()) * k;
        self.zoom = new;
        self.clamp();
    }

    /// Multiplies the zoom by `factor` about `anchor`.
    pub fn zoom_at(&mut self, anchor: Pos2, factor: f32) {
        if factor.is_finite() && factor > 0.0 {
            self.zoom_to_at(anchor, self.zoom * factor);
        }
    }

    /// Zooms about the centre of the view.
    pub fn zoom_to_centered(&mut self, zoom: f32) {
        self.zoom_to_at((self.view / 2.0).to_pos2(), zoom);
    }

    /// The next entry of [`ZOOM_STEPS`] above (or below) `zoom`.
    pub fn next_step(zoom: f32, up: bool) -> f32 {
        let eps = zoom * 1e-3;
        if up {
            ZOOM_STEPS.iter().copied().find(|s| *s > zoom + eps).unwrap_or(MAX_ZOOM)
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|s| *s < zoom - eps).unwrap_or(MIN_ZOOM)
        }
    }

    /// One zoom step in or out about `anchor`.
    pub fn step_at(&mut self, anchor: Pos2, up: bool) {
        self.zoom_to_at(anchor, Self::next_step(self.zoom, up));
    }

    /// Scrolls the content by `delta` screen units.
    pub fn pan_by(&mut self, delta: Vec2) {
        if delta == Vec2::ZERO {
            return;
        }
        self.follow_fit = false;
        self.offset += delta;
        self.clamp();
    }

    /// Scrolls so the canvas point `p` is at the centre of the view.
    pub fn center_on(&mut self, p: Pos2) {
        self.follow_fit = false;
        self.offset = self.view / 2.0 - p.to_vec2() * self.zoom;
        self.clamp();
    }

    /// Keeps a strip of the content in view (see the module docs).
    pub fn clamp(&mut self) {
        let size = self.content * self.zoom;
        let keep_x = MIN_VISIBLE.min(size.x).min(self.view.x / 2.0).max(1.0);
        let keep_y = MIN_VISIBLE.min(size.y).min(self.view.y / 2.0).max(1.0);
        self.offset.x = self.offset.x.clamp(keep_x - size.x, self.view.x - keep_x);
        self.offset.y = self.offset.y.clamp(keep_y - size.y, self.view.y - keep_y);
    }

    /// `true` when the per-pixel grid should be drawn.
    pub fn show_pixel_grid(&self) -> bool {
        self.zoom >= GRID_ZOOM
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn vp() -> Viewport {
        Viewport::new(vec2(1000.0, 600.0), vec2(1920.0, 1080.0))
    }

    #[test]
    fn fit_centres_and_never_upscales() {
        let v = vp();
        assert!(v.is_fitted());
        let z = (1000.0f32 - 48.0) / 1920.0;
        assert!((v.zoom() - z.min((600.0 - 48.0) / 1080.0)).abs() < 1e-6);
        let r = v.content_screen_rect();
        assert!((r.center().x - 500.0).abs() < 1.0 && (r.center().y - 300.0).abs() < 1.0);

        let small = Viewport::new(vec2(1000.0, 600.0), vec2(100.0, 50.0));
        assert!((small.zoom() - 1.0).abs() < 1e-6, "small images are shown at 100 %");
        let mut big = Viewport::new(vec2(1000.0, 600.0), vec2(100.0, 50.0));
        big.fit_to(8.0);
        assert!(big.zoom() > 5.0);
    }

    #[test]
    fn fit_follows_resizes_until_the_user_pans() {
        let mut v = vp();
        v.set_view(vec2(500.0, 300.0));
        assert!(v.is_fitted());
        assert!(v.content_screen_rect().max.x <= 500.0);
        v.pan_by(vec2(10.0, 0.0));
        assert!(!v.is_fitted());
        let before = v.zoom();
        v.set_view(vec2(800.0, 300.0));
        assert!((v.zoom() - before).abs() < 1e-6, "manual zoom is kept on resize");
        v.fit();
        assert!(v.is_fitted());
    }

    #[test]
    fn zoom_keeps_the_anchor_fixed() {
        let mut v = vp();
        let anchor = pos2(400.0, 250.0);
        let before = v.screen_to_canvas(anchor);
        v.zoom_at(anchor, 4.0);
        let after = v.screen_to_canvas(anchor);
        // Whole-unit origin rounding moves the mapping by at most half a screen unit.
        let tol = 0.5 / v.zoom() + 0.01;
        assert!((before - after).length() < tol * 1.5, "{before:?} vs {after:?} tol {tol}");
        assert!(!v.is_fitted());
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = vp();
        v.zoom_at(pos2(100.0, 100.0), 1e9);
        assert_eq!(v.zoom(), MAX_ZOOM);
        v.zoom_at(pos2(100.0, 100.0), 1e-9);
        assert_eq!(v.zoom(), MIN_ZOOM);
        v.zoom_at(pos2(100.0, 100.0), f32::NAN);
        v.zoom_to_at(pos2(1.0, 1.0), f32::INFINITY);
        assert_eq!(v.zoom(), MIN_ZOOM, "non-finite input is ignored");
    }

    #[test]
    fn steps_walk_the_ladder() {
        assert_eq!(Viewport::next_step(1.0, true), 1.5);
        assert_eq!(Viewport::next_step(1.0, false), 0.75);
        assert_eq!(Viewport::next_step(0.4, true), 0.5);
        assert_eq!(Viewport::next_step(0.4, false), 0.333);
        assert_eq!(Viewport::next_step(64.0, true), MAX_ZOOM);
        assert_eq!(Viewport::next_step(0.001, false), MIN_ZOOM);
        let mut z = 0.01;
        for _ in 0..40 {
            z = Viewport::next_step(z, true);
        }
        assert_eq!(z, MAX_ZOOM);
    }

    #[test]
    fn pan_is_clamped_so_content_stays_reachable() {
        let mut v = vp();
        v.zoom_to_centered(2.0);
        v.pan_by(vec2(1e7, -1e7));
        let r = v.content_screen_rect();
        let view = Rect::from_min_size(Pos2::ZERO, v.view());
        let inter = r.intersect(view);
        assert!(inter.width() >= MIN_VISIBLE - 1.0 && inter.height() >= MIN_VISIBLE - 1.0);
        v.pan_by(vec2(-1e7, 1e7));
        let inter = v.content_screen_rect().intersect(view);
        assert!(inter.width() >= MIN_VISIBLE - 1.0 && inter.height() >= MIN_VISIBLE - 1.0);
    }

    #[test]
    fn visible_output_is_clipped_to_content_and_view() {
        let mut v = Viewport::new(vec2(800.0, 600.0), vec2(4000.0, 3000.0));
        v.zoom_to_at(pos2(0.0, 0.0), 1.0);
        v.center_on(pos2(2000.0, 1500.0));
        let (x0, y0, x1, y1) = v.visible_output_px();
        assert_eq!((x1 - x0, y1 - y0), (800, 600));
        assert_eq!((x0, y0), (1600, 1200));
        v.center_on(pos2(0.0, 0.0));
        let (x0, y0, ..) = v.visible_output_px();
        assert!(x0 == 0 && y0 == 0);
        let vis = v.visible_canvas_rect();
        assert!(vis.min.x >= 0.0 && vis.min.y >= 0.0);
    }

    #[test]
    fn set_content_refits_only_when_following() {
        let mut v = vp();
        v.set_content(vec2(100.0, 100.0));
        assert!(v.is_fitted());
        assert!((v.zoom() - 1.0).abs() < 1e-6);
        v.zoom_to_centered(3.0);
        v.set_content(vec2(50.0, 50.0));
        assert!(!v.is_fitted());
        assert_eq!(v.zoom(), 3.0);
    }

    #[test]
    fn origin_is_whole_units() {
        let mut v = vp();
        v.pan_by(vec2(0.37, 0.61));
        let o = v.origin();
        assert_eq!(o.x, o.x.round());
        assert_eq!(o.y, o.y.round());
    }

    #[test]
    fn grid_appears_when_zoomed_in() {
        let mut v = vp();
        assert!(!v.show_pixel_grid());
        v.zoom_to_centered(8.0);
        assert!(v.show_pixel_grid());
    }

    proptest! {
        /// Zooming about a point on the content leaves that content point under the cursor.
        #[test]
        fn zoom_about_cursor_is_stable(
            zoom in 0.02f32..32.0,
            factor in 0.2f32..8.0,
            fx in 0.05f32..0.95,
            fy in 0.05f32..0.95,
            panx in -800.0f32..800.0,
            pany in -800.0f32..800.0,
            cw in 50.0f32..6000.0,
            ch in 50.0f32..4000.0,
        ) {
            let mut v = Viewport::new(vec2(1200.0, 800.0), vec2(cw, ch));
            v.zoom_to_centered(zoom);
            v.pan_by(vec2(panx, pany));
            // Pick an anchor that is on the content and on screen.
            let vis = v.content_screen_rect().intersect(Rect::from_min_size(Pos2::ZERO, v.view()));
            prop_assume!(vis.width() > 4.0 && vis.height() > 4.0);
            let anchor = pos2(vis.min.x + vis.width() * fx, vis.min.y + vis.height() * fy);
            let anchor = pos2(anchor.x.round(), anchor.y.round());
            let before = v.screen_to_canvas(anchor);
            let z0 = v.zoom();
            v.zoom_at(anchor, factor);
            prop_assert!(v.zoom() >= MIN_ZOOM && v.zoom() <= MAX_ZOOM);
            let after = v.screen_to_canvas(anchor);
            // The rounded origin moves the mapping by at most half a screen unit.
            let tol = 1.0 / v.zoom() + 0.05;
            let _ = z0;
            prop_assert!((before - after).length() <= tol, "{before:?} {after:?} tol {tol}");
        }

        /// Whatever the operations, the content never leaves the view entirely and the
        /// zoom stays in range.
        #[test]
        fn clamp_invariants(
            ops in proptest::collection::vec((0u8..4, -3000.0f32..3000.0, 0.1f32..6.0), 1..30),
        ) {
            let mut v = Viewport::new(vec2(900.0, 700.0), vec2(2000.0, 1000.0));
            for (kind, a, k) in ops {
                match kind {
                    0 => v.pan_by(vec2(a, -a / 2.0)),
                    1 => v.zoom_at(pos2(450.0 + a / 10.0, 350.0), k),
                    2 => v.fit(),
                    _ => v.step_at(pos2(100.0, 100.0), a > 0.0),
                }
                prop_assert!(v.zoom() >= MIN_ZOOM && v.zoom() <= MAX_ZOOM);
                let vis = v.content_screen_rect().intersect(Rect::from_min_size(Pos2::ZERO, v.view()));
                let size = v.content() * v.zoom();
                let need_x = MIN_VISIBLE.min(size.x).min(v.view().x / 2.0) - 1.5;
                let need_y = MIN_VISIBLE.min(size.y).min(v.view().y / 2.0) - 1.5;
                prop_assert!(vis.width() >= need_x, "{vis:?} need {need_x}");
                prop_assert!(vis.height() >= need_y, "{vis:?} need {need_y}");
            }
        }

        #[test]
        fn screen_canvas_round_trip(
            zoom in 0.01f32..64.0, x in -5000.0f32..5000.0, y in -5000.0f32..5000.0,
        ) {
            let mut v = vp();
            v.zoom_to_centered(zoom);
            let p = pos2(x, y);
            let back = v.screen_to_canvas(v.canvas_to_screen(p));
            prop_assert!((back - p).length() < 0.05 + p.to_vec2().length() * 1e-5);
        }
    }
}
