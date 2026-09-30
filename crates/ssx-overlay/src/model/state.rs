//! The selection state machine.
//!
//! [`SelectionModel`] is a pure function of the events fed to it: no windowing, no clock (the
//! backend supplies timestamps), no pixels. That is what lets every interaction rule be unit
//! and property tested. See the crate README for the interaction table.
//!
//! Design notes:
//!
//! * A left press does not immediately start a new rectangle: it enters `Pending`, and only
//!   after the pointer has travelled a few pixels does it become `Creating`. A plain click
//!   therefore never destroys an existing selection and can act as a "click the highlighted
//!   window" gesture.
//! * Rectangle edges are *corner coordinates* (a rectangle from x=10 to x=30 is 20 wide).
//!   The last pixel row/column of the desktop counts as the desktop edge, otherwise the
//!   bottom-right pixel could never be included.
//! * Every drag is recomputed from the drag origin and the last pointer position, never
//!   accumulated, so toggling Shift/Ctrl mid-drag reverts cleanly.

use ssx_types::{Monitor, Point, Rect, WindowInfo};

use super::events::{InputEvent, Key, KeyEvent, Modifiers, PointerButton, PointerEvent};
use super::geometry::{
    Handle, bounding_points, clamp_inside, handle_at, polygon_area2, rect_from_edges, snap_value,
};
use super::scene::{
    CursorHint, Cutout, Scene, handle_size, layout_loupe, place_label, text_scale,
};
use crate::types::{OverlayOptions, SelectMode, UiScale};

/// Pointer travel (desktop pixels) before a press becomes a drag.
const DRAG_THRESHOLD: i64 = 4;
/// Maximum time between clicks for a double-click.
const DOUBLE_CLICK_MS: u64 = 400;
/// Maximum distance between clicks for a double-click.
const DOUBLE_CLICK_DIST: i64 = 6;
/// Snap distance in UI units for Ctrl-snap.
const SNAP_DIST: i64 = 8;
/// Smallest and largest loupe zoom.
pub const LOUPE_ZOOM_RANGE: (u32, u32) = (2, 24);
/// Freeform vertices are only added when at least this far from the previous one.
const FREEFORM_MIN_STEP: i64 = 2;
/// Cap on freeform vertices, so a pathological input cannot grow without bound.
const FREEFORM_MAX_POINTS: usize = 20_000;

/// What the model asks the backend to do when finished.
#[derive(Debug, Clone, PartialEq)]
pub enum Finish {
    /// Nothing selected.
    Cancelled,
    /// A region.
    Selected {
        /// Bounding rectangle.
        rect: Rect,
        /// Mask shape.
        shape: FinishShape,
        /// Index into the window list the rectangle came from, if snapped.
        window: Option<usize>,
    },
    /// Index into the window list.
    Window(usize),
    /// Index into the monitor list.
    Monitor(usize),
    /// `C` was pressed at this pixel.
    PickColor(Point),
}

/// Mask shape of a finished selection.
#[derive(Debug, Clone, PartialEq)]
pub enum FinishShape {
    /// Rectangle.
    Rect,
    /// Ellipse.
    Ellipse,
    /// Polygon.
    Freeform(Vec<Point>),
}

/// Static inputs of the model.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// Desktop bounds.
    pub bounds: Rect,
    /// Monitors in desktop pixels (must not be empty; see [`ModelConfig::new`]).
    pub monitors: Vec<Monitor>,
    /// Windows front-to-back.
    pub windows: Vec<WindowInfo>,
    /// Options.
    pub options: OverlayOptions,
    /// Global UI scale (`Frame::scale_factor`).
    pub global_ui_scale: f32,
}

impl ModelConfig {
    /// Builds a config; an empty monitor list becomes one monitor covering `bounds`.
    pub fn new(
        bounds: Rect,
        mut monitors: Vec<Monitor>,
        windows: Vec<WindowInfo>,
        options: OverlayOptions,
        global_ui_scale: f32,
    ) -> Self {
        if monitors.is_empty() {
            monitors.push(Monitor {
                id: "desktop".into(),
                name: "Desktop".into(),
                rect: bounds,
                scale_factor: f64::from(global_ui_scale),
                primary: true,
                refresh_hz: None,
                hdr: None,
            });
        }
        Self { bounds, monitors, windows, options, global_ui_scale }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hover {
    Window(usize),
    Monitor(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    /// Left is down, pointer has not moved far enough yet.
    Pending {
        down: Point,
    },
    Creating {
        anchor: Point,
        cur: Point,
    },
    Selected,
    Moving {
        grab: Point,
        orig: Rect,
    },
    Resizing {
        handle: Handle,
        orig: Rect,
    },
    Freeform,
    Done,
}

/// The selection state machine. Feed it [`InputEvent`]s, read [`SelectionModel::scene`] and
/// [`SelectionModel::finished`].
#[derive(Debug, Clone)]
pub struct SelectionModel {
    cfg: ModelConfig,
    /// Indices into `cfg.windows` that can be hovered (visible, non-empty, on the desktop).
    eligible: Vec<usize>,
    phase: Phase,
    sel: Option<Rect>,
    snapped: Option<usize>,
    cursor: Option<Point>,
    /// Pointer position used for the previous move, for Space-move deltas.
    last_ptr: Point,
    mods: Modifiers,
    space_down: bool,
    hover: Option<Hover>,
    mon_focus: Option<usize>,
    zoom: u32,
    last_click: Option<(Point, u64)>,
    points: Vec<Point>,
    finish: Option<Finish>,
}

impl SelectionModel {
    /// Creates a model. `options.initial` (clamped to the desktop) becomes the starting
    /// selection in rectangle/ellipse mode.
    pub fn new(cfg: ModelConfig) -> Self {
        let eligible = cfg
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.minimized && w.rect.intersect(cfg.bounds).is_some())
            .map(|(i, _)| i)
            .collect();
        let zoom = cfg.options.loupe_zoom.clamp(LOUPE_ZOOM_RANGE.0, LOUPE_ZOOM_RANGE.1);
        let mut m = Self {
            eligible,
            phase: Phase::Idle,
            sel: None,
            snapped: None,
            cursor: None,
            last_ptr: Point::default(),
            mods: Modifiers::default(),
            space_down: false,
            hover: None,
            mon_focus: None,
            zoom,
            last_click: None,
            points: Vec::new(),
            finish: None,
            cfg,
        };
        if m.is_region_mode()
            && let Some(init) = m.cfg.options.initial.and_then(|r| r.intersect(m.cfg.bounds))
        {
            m.sel = Some(init);
            m.phase = Phase::Selected;
        }
        m
    }

    fn is_region_mode(&self) -> bool {
        matches!(self.cfg.options.mode, SelectMode::Rect | SelectMode::Ellipse)
    }

    /// The result, once the interaction has ended.
    pub fn finished(&self) -> Option<&Finish> {
        self.finish.as_ref()
    }

    /// Forces the outcome to "cancelled" (timeout, window closed).
    pub fn cancel(&mut self) {
        self.end(Finish::Cancelled);
    }

    /// Current loupe zoom.
    pub fn zoom(&self) -> u32 {
        self.zoom
    }

    /// The pointer position, if it is over the overlay.
    pub fn cursor(&self) -> Option<Point> {
        self.cursor
    }

    /// The current selection rectangle, if any.
    pub fn selection(&self) -> Option<Rect> {
        self.sel
    }

    /// Desktop bounds.
    pub fn bounds(&self) -> Rect {
        self.cfg.bounds
    }

    /// The monitors the model works with (never empty).
    pub fn monitors(&self) -> &[Monitor] {
        &self.cfg.monitors
    }

    /// The window list.
    pub fn windows(&self) -> &[WindowInfo] {
        &self.cfg.windows
    }

    fn end(&mut self, f: Finish) {
        if self.finish.is_none() {
            self.finish = Some(f);
            self.phase = Phase::Done;
        }
    }

    // ---------------------------------------------------------------- geometry helpers

    /// UI scale (desktop pixels per UI unit) at `p`.
    pub fn ui_scale_at(&self, p: Option<Point>) -> f32 {
        match self.cfg.options.ui_scale {
            UiScale::Fixed(v) => v.max(0.5),
            UiScale::Auto => self.cfg.global_ui_scale.max(1.0),
            UiScale::PerMonitor => p
                .and_then(|p| self.monitor_at(p))
                .map_or(self.cfg.global_ui_scale, |i| self.cfg.monitors[i].scale_factor as f32)
                .max(1.0),
        }
    }

    fn ui(&self) -> f32 {
        self.ui_scale_at(self.cursor)
    }

    fn monitor_at(&self, p: Point) -> Option<usize> {
        self.cfg.monitors.iter().position(|m| m.rect.contains(p))
    }

    fn clamp_point(&self, p: Point) -> Point {
        let b = self.cfg.bounds;
        Point::new(
            i64::from(p.x).clamp(i64::from(b.x), b.right() - 1).max(i64::from(i32::MIN)) as i32,
            i64::from(p.y).clamp(i64::from(b.y), b.bottom() - 1).max(i64::from(i32::MIN)) as i32,
        )
    }

    /// Edge coordinate of a pointer position: the last pixel counts as the far edge.
    fn edge_x(&self, x: i32) -> i64 {
        let b = self.cfg.bounds;
        let x = i64::from(x);
        if x >= b.right() - 1 { b.right() } else { x.max(i64::from(b.x)) }
    }

    fn edge_y(&self, y: i32) -> i64 {
        let b = self.cfg.bounds;
        let y = i64::from(y);
        if y >= b.bottom() - 1 { b.bottom() } else { y.max(i64::from(b.y)) }
    }

    fn snap_lines(&self) -> (Vec<i64>, Vec<i64>) {
        let mut xs = vec![i64::from(self.cfg.bounds.x), self.cfg.bounds.right()];
        let mut ys = vec![i64::from(self.cfg.bounds.y), self.cfg.bounds.bottom()];
        for m in &self.cfg.monitors {
            xs.extend([i64::from(m.rect.x), m.rect.right()]);
            ys.extend([i64::from(m.rect.y), m.rect.bottom()]);
        }
        for &i in &self.eligible {
            let r = self.cfg.windows[i].rect;
            xs.extend([i64::from(r.x), r.right()]);
            ys.extend([i64::from(r.y), r.bottom()]);
        }
        (xs, ys)
    }

    fn snap_threshold(&self) -> i64 {
        SNAP_DIST * i64::from(text_scale(self.ui()))
    }

    fn snap_x(&self, v: i64) -> i64 {
        let (xs, _) = self.snap_lines();
        snap_value(v, &xs, self.snap_threshold()).unwrap_or(v)
    }

    fn snap_y(&self, v: i64) -> i64 {
        let (_, ys) = self.snap_lines();
        snap_value(v, &ys, self.snap_threshold()).unwrap_or(v)
    }

    fn window_at(&self, p: Point) -> Option<usize> {
        self.eligible.iter().copied().find(|&i| self.cfg.windows[i].rect.contains(p))
    }

    fn window_rect(&self, i: usize) -> Option<Rect> {
        self.cfg.windows.get(i)?.rect.intersect(self.cfg.bounds)
    }

    fn handle_metrics(&self) -> (u32, u32) {
        let ui = self.ui();
        (handle_size(ui), 4 * text_scale(ui))
    }

    fn handle_under(&self, p: Point) -> Option<Handle> {
        let sel = self.sel?;
        let (size, slop) = self.handle_metrics();
        handle_at(sel, p, size, slop)
    }

    fn inside_sel(&self, p: Point) -> bool {
        self.sel.is_some_and(|s| s.contains(p))
    }

    // ---------------------------------------------------------------- hover

    fn update_hover(&mut self) {
        let Some(c) = self.cursor else {
            self.hover = None;
            return;
        };
        self.hover = match self.cfg.options.mode {
            SelectMode::Window => self.window_at(c).map(Hover::Window),
            SelectMode::Monitor => self
                .mon_focus
                .or_else(|| self.monitor_at(c))
                .filter(|&i| i < self.cfg.monitors.len())
                .map(Hover::Monitor),
            SelectMode::Rect | SelectMode::Ellipse => {
                let idle = matches!(self.phase, Phase::Idle | Phase::Selected | Phase::Pending { .. });
                let over_sel = self.inside_sel(c) || self.handle_under(c).is_some();
                if self.cfg.options.snap_to_windows && idle && !over_sel {
                    self.window_at(c).map(Hover::Window)
                } else {
                    None
                }
            }
            SelectMode::Freeform => None,
        };
    }

    fn hover_rect(&self) -> Option<Rect> {
        match self.hover? {
            Hover::Window(i) => self.window_rect(i),
            Hover::Monitor(i) => self.cfg.monitors.get(i)?.rect.intersect(self.cfg.bounds),
        }
    }

    // ---------------------------------------------------------------- event entry

    /// Feeds one event. Returns `true` if anything observable may have changed.
    pub fn handle(&mut self, ev: InputEvent) -> bool {
        if self.finish.is_some() {
            return false;
        }
        match ev {
            InputEvent::Pointer(p) => self.on_pointer(p),
            InputEvent::Key(k) => self.on_key(k),
            InputEvent::Modifiers(m) => self.set_modifiers(m),
        }
        true
    }

    /// Replaces the modifier state (re-applies the drag in progress).
    pub fn set_modifiers(&mut self, m: Modifiers) {
        if m != self.mods {
            self.mods = m;
            self.reapply();
        }
    }

    fn reapply(&mut self) {
        if matches!(self.phase, Phase::Creating { .. } | Phase::Moving { .. } | Phase::Resizing { .. })
            && let Some(c) = self.cursor
        {
            self.drag_to(c, false);
        }
    }

    // ---------------------------------------------------------------- pointer

    fn on_pointer(&mut self, ev: PointerEvent) {
        match ev {
            PointerEvent::Leave => {
                self.cursor = None;
                self.update_hover();
            }
            PointerEvent::Wheel { delta } => {
                let z = i64::from(self.zoom) + i64::from(delta.signum());
                self.zoom = z.clamp(i64::from(LOUPE_ZOOM_RANGE.0), i64::from(LOUPE_ZOOM_RANGE.1))
                    as u32;
            }
            PointerEvent::Move { pos } => {
                let pos = self.clamp_point(pos);
                let moved = self.cursor != Some(pos);
                self.cursor = Some(pos);
                if moved {
                    self.mon_focus = None;
                }
                self.on_move(pos);
                self.last_ptr = pos;
                self.update_hover();
            }
            PointerEvent::Down { pos, button, time_ms } => {
                let pos = self.clamp_point(pos);
                self.cursor = Some(pos);
                self.last_ptr = pos;
                self.update_hover();
                match button {
                    PointerButton::Left => self.on_left_down(pos, time_ms),
                    PointerButton::Right => self.on_right_down(),
                    PointerButton::Middle => {}
                }
                self.update_hover();
            }
            PointerEvent::Up { pos, button } => {
                let pos = self.clamp_point(pos);
                self.cursor = Some(pos);
                if button == PointerButton::Left {
                    self.on_left_up(pos);
                }
                self.last_ptr = pos;
                self.update_hover();
            }
        }
    }

    fn on_move(&mut self, pos: Point) {
        match self.phase.clone() {
            Phase::Pending { down } => {
                let far = i64::from(pos.x - down.x).abs().max(i64::from(pos.y - down.y).abs())
                    >= DRAG_THRESHOLD;
                if far && self.is_region_mode() {
                    let anchor = Point::new(self.edge_x(down.x) as i32, self.edge_y(down.y) as i32);
                    self.phase = Phase::Creating { anchor, cur: anchor };
                    self.snapped = None;
                    self.drag_to(pos, true);
                }
            }
            Phase::Creating { .. } | Phase::Moving { .. } | Phase::Resizing { .. } => {
                self.drag_to(pos, true);
            }
            Phase::Freeform => self.freeform_add(pos),
            _ => {}
        }
    }

    fn on_left_down(&mut self, pos: Point, time_ms: u64) {
        let is_double = self.last_click.is_some_and(|(p, t)| {
            time_ms.saturating_sub(t) <= DOUBLE_CLICK_MS
                && i64::from(p.x - pos.x).abs().max(i64::from(p.y - pos.y).abs())
                    <= DOUBLE_CLICK_DIST
        });
        self.last_click = if is_double { None } else { Some((pos, time_ms)) };
        match self.cfg.options.mode {
            SelectMode::Freeform => {
                self.points.clear();
                self.points.push(Point::new(self.edge_x(pos.x) as i32, self.edge_y(pos.y) as i32));
                self.phase = Phase::Freeform;
            }
            SelectMode::Monitor | SelectMode::Window => {
                self.phase = Phase::Pending { down: pos };
            }
            SelectMode::Rect | SelectMode::Ellipse => {
                if is_double && self.inside_sel(pos) && matches!(self.phase, Phase::Selected) {
                    self.confirm();
                    return;
                }
                if let Phase::Selected = self.phase
                    && let Some(sel) = self.sel
                {
                    if let Some(h) = self.handle_under(pos) {
                        self.phase = Phase::Resizing { handle: h, orig: sel };
                        return;
                    }
                    if sel.contains(pos) {
                        self.phase = Phase::Moving { grab: pos, orig: sel };
                        return;
                    }
                }
                self.phase = Phase::Pending { down: pos };
            }
        }
    }

    fn on_right_down(&mut self) {
        let clearable = match self.cfg.options.mode {
            SelectMode::Rect | SelectMode::Ellipse => self.sel.is_some(),
            SelectMode::Freeform => matches!(self.phase, Phase::Freeform),
            SelectMode::Monitor | SelectMode::Window => false,
        };
        if clearable {
            self.sel = None;
            self.snapped = None;
            self.points.clear();
            self.phase = Phase::Idle;
        } else {
            self.end(Finish::Cancelled);
        }
    }

    fn on_left_up(&mut self, pos: Point) {
        match self.phase.clone() {
            Phase::Pending { .. } => self.on_click(pos),
            Phase::Creating { .. } => {
                if self.sel.is_some_and(Rect::is_empty) {
                    self.sel = None;
                    self.phase = Phase::Idle;
                } else {
                    self.phase = Phase::Selected;
                }
            }
            Phase::Moving { .. } | Phase::Resizing { .. } => {
                self.phase = if self.sel.is_some() { Phase::Selected } else { Phase::Idle };
            }
            Phase::Freeform => self.freeform_finish(),
            _ => {}
        }
    }

    /// A press-release without a drag.
    fn on_click(&mut self, pos: Point) {
        self.phase = if self.sel.is_some() { Phase::Selected } else { Phase::Idle };
        match self.cfg.options.mode {
            SelectMode::Monitor => {
                if let Some(Hover::Monitor(i)) = self.hover {
                    self.end(Finish::Monitor(i));
                }
            }
            SelectMode::Window => {
                if let Some(Hover::Window(i)) = self.hover {
                    self.end(Finish::Window(i));
                }
            }
            SelectMode::Rect | SelectMode::Ellipse => {
                if self.cfg.options.snap_to_windows
                    && let Some(i) = self.window_at(pos)
                    && let Some(r) = self.window_rect(i)
                {
                    self.sel = Some(r);
                    self.snapped = Some(i);
                    self.confirm();
                }
            }
            SelectMode::Freeform => {}
        }
    }

    // ---------------------------------------------------------------- dragging

    fn drag_to(&mut self, pos: Point, is_move_event: bool) {
        match self.phase.clone() {
            Phase::Creating { anchor, cur } => {
                if (self.space_down || self.mods.alt) && is_move_event {
                    self.drag_translate(anchor, cur, pos);
                } else {
                    self.drag_create(anchor, pos);
                }
            }
            Phase::Moving { grab, orig } => self.drag_move(grab, orig, pos),
            Phase::Resizing { handle, orig } => self.drag_resize(handle, orig, pos),
            _ => {}
        }
    }

    fn drag_create(&mut self, anchor: Point, pos: Point) {
        let (ax, ay) = (i64::from(anchor.x), i64::from(anchor.y));
        let mut cx = self.edge_x(pos.x);
        let mut cy = self.edge_y(pos.y);
        if self.mods.ctrl {
            cx = self.snap_x(cx);
            cy = self.snap_y(cy);
        }
        if self.mods.shift {
            let b = self.cfg.bounds;
            let (dx, dy) = (cx - ax, cy - ay);
            let (sx, sy) = (if dx < 0 { -1 } else { 1 }, if dy < 0 { -1 } else { 1 });
            let max_x = if sx > 0 { b.right() - ax } else { ax - i64::from(b.x) };
            let max_y = if sy > 0 { b.bottom() - ay } else { ay - i64::from(b.y) };
            let side = dx.abs().max(dy.abs()).min(max_x).min(max_y);
            cx = ax + sx * side;
            cy = ay + sy * side;
        }
        let cur = Point::new(cx as i32, cy as i32);
        self.phase = Phase::Creating { anchor, cur };
        self.sel = Some(rect_from_edges(ax, ay, cx, cy));
    }

    fn drag_translate(&mut self, anchor: Point, cur: Point, pos: Point) {
        // Edge coordinates on both ends so a pointer parked on the last pixel can push the
        // rectangle flush against the desktop edge.
        let (dx, dy) = (
            self.edge_x(pos.x) - self.edge_x(self.last_ptr.x),
            self.edge_y(pos.y) - self.edge_y(self.last_ptr.y),
        );
        let rect = rect_from_edges(
            i64::from(anchor.x),
            i64::from(anchor.y),
            i64::from(cur.x),
            i64::from(cur.y),
        );
        let moved = clamp_inside(
            rect.translate(dx.clamp(-1_000_000, 1_000_000) as i32, dy.clamp(-1_000_000, 1_000_000) as i32),
            self.cfg.bounds,
        );
        let (adx, ady) = (i64::from(moved.x) - i64::from(rect.x), i64::from(moved.y) - i64::from(rect.y));
        let shift = |p: Point| Point::new((i64::from(p.x) + adx) as i32, (i64::from(p.y) + ady) as i32);
        self.phase = Phase::Creating { anchor: shift(anchor), cur: shift(cur) };
        self.sel = Some(moved);
    }

    fn drag_move(&mut self, grab: Point, orig: Rect, pos: Point) {
        let (dx, dy) = (i64::from(pos.x - grab.x), i64::from(pos.y - grab.y));
        let mut r = clamp_inside(
            orig.translate(dx.clamp(-1_000_000, 1_000_000) as i32, dy.clamp(-1_000_000, 1_000_000) as i32),
            self.cfg.bounds,
        );
        if self.mods.ctrl {
            let (xs, ys) = self.snap_lines();
            let thr = self.snap_threshold();
            let best = |edges: [i64; 2], lines: &[i64]| -> Option<i64> {
                edges
                    .iter()
                    .filter_map(|&e| snap_value(e, lines, thr).map(|c| c - e))
                    .min_by_key(|d| d.abs())
            };
            let sdx = best([i64::from(r.x), r.right()], &xs).unwrap_or(0);
            let sdy = best([i64::from(r.y), r.bottom()], &ys).unwrap_or(0);
            r = clamp_inside(r.translate(sdx as i32, sdy as i32), self.cfg.bounds);
        }
        self.sel = Some(r);
    }

    fn drag_resize(&mut self, handle: Handle, orig: Rect, pos: Point) {
        let (ml, mt, mr, mb) = handle.moves();
        let (mut l, mut t, mut r, mut b) =
            (i64::from(orig.x), i64::from(orig.y), orig.right(), orig.bottom());
        let mut px = self.edge_x(pos.x);
        let mut py = self.edge_y(pos.y);
        if self.mods.ctrl {
            px = self.snap_x(px);
            py = self.snap_y(py);
        }
        if self.mods.shift && handle.is_corner() && orig.width > 0 && orig.height > 0 {
            let bnd = self.cfg.bounds;
            let fx = if ml { r } else { l };
            let fy = if mt { b } else { t };
            let (sx, sy) = (if px < fx { -1 } else { 1 }, if py < fy { -1 } else { 1 });
            let avail_w = if sx > 0 { bnd.right() - fx } else { fx - i64::from(bnd.x) };
            let avail_h = if sy > 0 { bnd.bottom() - fy } else { fy - i64::from(bnd.y) };
            let (ow, oh) = (f64::from(orig.width), f64::from(orig.height));
            let scale = ((px - fx).abs() as f64 / ow)
                .max((py - fy).abs() as f64 / oh)
                .min(avail_w as f64 / ow)
                .min(avail_h as f64 / oh);
            px = fx + sx * (ow * scale).round() as i64;
            py = fy + sy * (oh * scale).round() as i64;
        }
        if ml {
            l = px;
        }
        if mr {
            r = px;
        }
        if mt {
            t = py;
        }
        if mb {
            b = py;
        }
        self.sel = Some(rect_from_edges(l, t, r, b));
    }

    // ---------------------------------------------------------------- freeform

    fn freeform_add(&mut self, pos: Point) {
        let p = Point::new(self.edge_x(pos.x) as i32, self.edge_y(pos.y) as i32);
        let far = self.points.last().is_none_or(|q| {
            i64::from(q.x - p.x).abs().max(i64::from(q.y - p.y).abs()) >= FREEFORM_MIN_STEP
        });
        if far && self.points.len() < FREEFORM_MAX_POINTS {
            self.points.push(p);
        }
    }

    fn freeform_finish(&mut self) {
        let ok = self.points.len() >= 3 && polygon_area2(&self.points) > 0;
        match (ok, bounding_points(&self.points)) {
            (true, Some(rect)) if !rect.is_empty() => {
                let pts = std::mem::take(&mut self.points);
                self.end(Finish::Selected { rect, shape: FinishShape::Freeform(pts), window: None });
            }
            _ => {
                self.points.clear();
                self.phase = Phase::Idle;
            }
        }
    }

    // ---------------------------------------------------------------- keyboard

    fn on_key(&mut self, ev: KeyEvent) {
        match ev.key {
            Key::Shift | Key::Control | Key::Alt => {
                let mut m = self.mods;
                match ev.key {
                    Key::Shift => m.shift = ev.pressed,
                    Key::Control => m.ctrl = ev.pressed,
                    _ => m.alt = ev.pressed,
                }
                self.set_modifiers(m);
            }
            Key::Space => self.space_down = ev.pressed,
            _ if !ev.pressed => {}
            Key::Escape => self.end(Finish::Cancelled),
            Key::Enter => self.confirm_key(),
            Key::Tab => self.cycle_monitor(self.mods.shift),
            Key::Left | Key::Right | Key::Up | Key::Down => self.nudge(ev.key),
            Key::Char('c') if self.cfg.options.allow_color_pick => {
                if let Some(c) = self.cursor {
                    self.end(Finish::PickColor(c));
                }
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self) {
        match self.cfg.options.mode {
            SelectMode::Rect | SelectMode::Ellipse => {
                if self.sel.is_none()
                    && let Some(Hover::Window(i)) = self.hover
                    && let Some(r) = self.window_rect(i)
                {
                    self.sel = Some(r);
                    self.snapped = Some(i);
                }
                self.confirm();
            }
            SelectMode::Monitor => {
                if let Some(Hover::Monitor(i)) = self.hover {
                    self.end(Finish::Monitor(i));
                }
            }
            SelectMode::Window => {
                if let Some(Hover::Window(i)) = self.hover {
                    self.end(Finish::Window(i));
                }
            }
            SelectMode::Freeform => {}
        }
    }

    fn confirm(&mut self) {
        let Some(rect) = self.sel.filter(|r| !r.is_empty()) else { return };
        let shape = if self.cfg.options.mode == SelectMode::Ellipse {
            FinishShape::Ellipse
        } else {
            FinishShape::Rect
        };
        self.end(Finish::Selected { rect, shape, window: self.snapped });
    }

    fn cycle_monitor(&mut self, backwards: bool) {
        let n = self.cfg.monitors.len();
        let current = self.mon_focus.or_else(|| self.cursor.and_then(|c| self.monitor_at(c)));
        let next = match (current, backwards) {
            (Some(c), false) => (c + 1) % n,
            (Some(c), true) => (c + n - 1) % n,
            (None, false) => 0,
            (None, true) => n - 1,
        };
        self.mon_focus = Some(next);
        match self.cfg.options.mode {
            SelectMode::Monitor => self.update_hover(),
            SelectMode::Rect | SelectMode::Ellipse => {
                if let Some(r) = self.cfg.monitors[next].rect.intersect(self.cfg.bounds) {
                    self.sel = Some(r);
                    self.snapped = None;
                    self.phase = Phase::Selected;
                }
            }
            _ => {}
        }
    }

    fn nudge(&mut self, key: Key) {
        if !matches!(self.phase, Phase::Selected) || !self.is_region_mode() {
            return;
        }
        let Some(sel) = self.sel else { return };
        let step = if self.mods.shift { 10i64 } else { 1 };
        let (dx, dy) = match key {
            Key::Left => (-step, 0),
            Key::Right => (step, 0),
            Key::Up => (0, -step),
            _ => (0, step),
        };
        let b = self.cfg.bounds;
        self.snapped = None;
        if self.mods.ctrl {
            // Ctrl+arrow resizes the right/bottom edge.
            let w = (i64::from(sel.width) + dx).clamp(1, b.right() - i64::from(sel.x));
            let h = (i64::from(sel.height) + dy).clamp(1, b.bottom() - i64::from(sel.y));
            self.sel = Some(Rect::new(sel.x, sel.y, w as u32, h as u32));
        } else {
            self.sel = Some(clamp_inside(sel.translate(dx as i32, dy as i32), b));
        }
    }

    // ---------------------------------------------------------------- scene

    /// Snapshot of what should be on screen.
    pub fn scene(&self) -> Scene {
        let mut s = Scene::empty(self.cfg.bounds);
        let ui = self.ui();
        s.ui_scale = ui;
        let opts = &self.cfg.options;
        s.cutout = if opts.mode == SelectMode::Ellipse { Cutout::Ellipse } else { Cutout::Rect };
        let region = self.is_region_mode();
        if region {
            s.selection = self.sel;
        }
        s.handles = region && matches!(self.phase, Phase::Selected | Phase::Moving { .. } | Phase::Resizing { .. });
        if let Phase::Resizing { handle, .. } = self.phase {
            s.active_handle = Some(handle);
        }
        if matches!(self.phase, Phase::Freeform) {
            s.freeform.clone_from(&self.points);
        }
        s.highlight = self.hover_rect();
        let dragging = matches!(self.phase, Phase::Moving { .. } | Phase::Resizing { .. });
        let over_sel = self.cursor.is_some_and(|c| {
            region && matches!(self.phase, Phase::Selected) && (self.inside_sel(c) || self.handle_under(c).is_some())
        });
        let guides_mode = !matches!(opts.mode, SelectMode::Window | SelectMode::Monitor);
        s.crosshair = self.cursor.filter(|_| guides_mode && !dragging && !over_sel);
        s.cursor = match self.phase {
            Phase::Resizing { handle, .. } => CursorHint::Resize(handle),
            Phase::Moving { .. } => CursorHint::Move,
            _ => match self.cursor {
                Some(c) if region && matches!(self.phase, Phase::Selected) => {
                    self.handle_under(c).map_or_else(
                        || if self.inside_sel(c) { CursorHint::Move } else { CursorHint::Crosshair },
                        CursorHint::Resize,
                    )
                }
                _ => CursorHint::Crosshair,
            },
        };
        if opts.show_dimensions {
            let subject = if s.freeform.len() >= 2 {
                bounding_points(&s.freeform)
            } else {
                s.selection.or(s.highlight)
            };
            if let Some(r) = subject {
                let lines = vec![format!("{} x {}", r.width, r.height), format!("{}, {}", r.x, r.y)];
                let area = self
                    .cursor
                    .and_then(|c| self.monitor_at(c))
                    .map_or(self.cfg.bounds, |i| self.cfg.monitors[i].rect);
                s.label = Some(place_label(r, lines, ui, area));
            }
        }
        if opts.show_loupe && guides_mode {
            if let Some(c) = self.cursor {
                let area = self.monitor_at(c).map_or(self.cfg.bounds, |i| self.cfg.monitors[i].rect);
                s.loupe = Some(layout_loupe(c, self.zoom, ui, area));
            }
        }
        s
    }
}
