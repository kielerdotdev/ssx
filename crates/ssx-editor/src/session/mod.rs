//! The interactive editing session: a UI-agnostic state machine.
//!
//! A GUI owns one [`EditorSession`] per open image. It forwards pointer, key and text events,
//! and after every call reads [`EditorSession::take_events`] (what to repaint), the
//! [`EditorSession::overlay`] (what to draw on top) and [`EditorSession::cursor_hint`].
//!
//! # Design notes
//!
//! * **Every mutation is a command** (`history::Command`) executed through the history, so
//!   undo/redo, dirty rectangles and coalescing come for free. A drag executes one command per
//!   pointer move; the history merges them into a single undo step (creation and its first
//!   drag merge too, so "draw a rectangle" is one step).
//! * **Image space in, image space out.** Pointer positions are image-space points (the GUI
//!   divides by its zoom and subtracts its pan/padding offset); dirty rectangles come back in
//!   image space with [`crate::Document::image_rect_to_output`] to map them to viewports.
//! * **No hidden global state**: the text engine and renderer live in the session.

mod ops;
mod textedit;

pub mod geometry;

use ssx_types::{Frame, Rect};

use self::geometry::SnapTargets;
use crate::{
    doc::Document,
    geom::{PointF, RectF},
    history::{Command, CoalesceKey, History, HistoryLimits},
    input::{
        CropOverlay, CursorHint, Guide, Handle, HandleKind, Key, Modifiers, Overlay, SessionEvent,
    },
    object::{ArrowHeads, ImageData, Object, ObjectId, ObjectKind, StickerSource},
    render::{RenderOptions, Renderer},
    shapes,
    style::Style,
    tool::{Preset, StyleMemory, Tool},
};

pub use textedit::{TextEditState, sync_text_rect};

/// Pixels of hit slack around thin objects and handles, in *screen* pixels.
const HIT_SLACK_PX: f32 = 5.0;
/// Handle grab radius in screen pixels.
const HANDLE_RADIUS_PX: f32 = 8.0;
/// Snap distance in screen pixels.
const SNAP_PX: f32 = 6.0;
/// Drags shorter than this (screen px) create nothing.
const MIN_DRAG_PX: f32 = 3.0;
/// Freehand points closer than this (screen px) are dropped.
const FREEHAND_STEP_PX: f32 = 1.5;

/// A pending crop (rectangle, ellipse or freeform) awaiting Enter / `apply_crop`.
#[derive(Debug, Clone, PartialEq)]
pub struct CropPending {
    /// Bounding rectangle (image space).
    pub rect: RectF,
    /// Which shape.
    pub tool: Tool,
    /// Polygon for freeform crops.
    pub polygon: Vec<PointF>,
}

pub(crate) enum Drag {
    Create {
        id: ObjectId,
        tool: Tool,
        start: PointF,
        points: Vec<PointF>,
        key: CoalesceKey,
    },
    Move {
        originals: Vec<Object>,
        start: PointF,
        key: CoalesceKey,
    },
    ResizeBox {
        original: Object,
        handle: HandleKind,
        rect: RectF,
        rotation: f32,
        key: CoalesceKey,
    },
    ResizeGroup {
        originals: Vec<Object>,
        handle: HandleKind,
        bounds: RectF,
        key: CoalesceKey,
    },
    Rotate {
        original: Object,
        pivot: PointF,
        start_angle: f32,
        key: CoalesceKey,
    },
    Endpoint {
        original: Object,
        which: u8,
        key: CoalesceKey,
    },
    Tail {
        original: Object,
        key: CoalesceKey,
    },
    Source {
        original: Object,
        key: CoalesceKey,
    },
    Marquee {
        start: PointF,
        cur: PointF,
        base: Vec<ObjectId>,
    },
    Erase {
        marks: Vec<ObjectId>,
        last: PointF,
    },
    Crop {
        start: PointF,
        cur: PointF,
        points: Vec<PointF>,
    },
    CutOut {
        start: PointF,
        cur: PointF,
    },
    TextSelect,
}

/// The interactive editor. See the [module docs](self).
pub struct EditorSession {
    pub(crate) doc: Document,
    pub(crate) history: History,
    pub(crate) renderer: Renderer,
    pub(crate) styles: StyleMemory,
    pub(crate) tool: Tool,
    pub(crate) selection: Vec<ObjectId>,
    pub(crate) drag: Option<Drag>,
    pub(crate) drag_seq: u64,
    pub(crate) text: Option<textedit::TextEdit>,
    pub(crate) text_clip: String,
    pub(crate) clipboard: Vec<Object>,
    pub(crate) paste_serial: u32,
    pub(crate) pending_image: Option<Frame>,
    pub(crate) hover: PointF,
    pub(crate) view_scale: f32,
    pub(crate) cursor: CursorHint,
    pub(crate) events: Vec<SessionEvent>,
    pub(crate) dirty: Option<Rect>,
    pub(crate) guides: Vec<Guide>,
    pub(crate) crop: Option<CropPending>,
}

impl std::fmt::Debug for EditorSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorSession")
            .field("tool", &self.tool)
            .field("objects", &self.doc.objects().len())
            .field("selection", &self.selection)
            .field("history", &self.history)
            .finish_non_exhaustive()
    }
}

impl EditorSession {
    /// Starts a session on `doc`.
    pub fn new(doc: Document) -> Self {
        Self {
            doc,
            history: History::new(HistoryLimits::default()),
            renderer: Renderer::new(),
            styles: StyleMemory::new(),
            tool: Tool::Select,
            selection: Vec::new(),
            drag: None,
            drag_seq: 0,
            text: None,
            text_clip: String::new(),
            clipboard: Vec::new(),
            paste_serial: 0,
            pending_image: None,
            hover: PointF::default(),
            view_scale: 1.0,
            cursor: CursorHint::Default,
            events: Vec::new(),
            dirty: None,
            guides: Vec::new(),
            crop: None,
        }
    }

    /// Starts a session on a fresh document made from a captured frame.
    pub fn from_frame(frame: Frame) -> Result<Self, crate::DocError> {
        Ok(Self::new(Document::new(frame)?))
    }

    /// Replaces the history limits (entries / bytes).
    pub fn set_history_limits(&mut self, limits: HistoryLimits) {
        self.history = History::new(limits);
    }

    // ---------------------------------------------------------------------------------
    // Accessors
    // ---------------------------------------------------------------------------------

    /// The document (read-only; mutate through the session so undo stays consistent).
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Consumes the session and returns the document.
    pub fn into_document(self) -> Document {
        self.doc
    }

    /// The renderer (and, through it, the text engine).
    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Renders with the session's own renderer (keeps caches warm between frames).
    pub fn render(&mut self, opts: &RenderOptions) -> Frame {
        self.renderer.render(&self.doc, opts)
    }

    /// The active tool.
    pub fn tool(&self) -> Tool {
        self.tool
    }

    /// Currently selected object ids (z-order not guaranteed).
    pub fn selection(&self) -> &[ObjectId] {
        &self.selection
    }

    /// The per-tool style memory.
    pub fn styles(&self) -> &StyleMemory {
        &self.styles
    }

    /// Mutable style memory (to load persisted styles).
    pub fn styles_mut(&mut self) -> &mut StyleMemory {
        &mut self.styles
    }

    /// Undo/redo availability.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// See [`EditorSession::can_undo`].
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// The history (for diagnostics such as memory use).
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Tells the session the current zoom (screen pixels per image pixel) so hit slack, handle
    /// size and snapping stay constant on screen.
    pub fn set_view_scale(&mut self, zoom: f32) {
        if zoom.is_finite() && zoom > 0.0 {
            self.view_scale = zoom;
        }
    }

    /// Supplies the bitmap that the [`Tool::Image`] tool stamps on click.
    pub fn set_pending_image(&mut self, frame: Option<Frame>) {
        self.pending_image = frame;
    }

    /// Drains queued events.
    pub fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    /// The union of all dirty rectangles since the last call (image space), if any.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    // ---------------------------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------------------------

    pub(crate) fn obj(&self, id: ObjectId) -> Option<&Object> {
        self.doc.object(id)
    }

    fn next_key(&mut self, kind: &'static str) -> CoalesceKey {
        self.drag_seq += 1;
        CoalesceKey { kind, tag: self.drag_seq }
    }

    pub(crate) fn hit_slack(&self) -> f32 {
        HIT_SLACK_PX / self.view_scale
    }

    pub(crate) fn mark_dirty(&mut self, r: RectF) {
        let canvas = self.doc.canvas_rect();
        let expanded = self.doc.dirty_region(r);
        if let Some(clipped) = expanded.to_outer_rect().intersect(canvas) {
            self.dirty = Some(self.dirty.map_or(clipped, |d| d.union(clipped)));
            self.events.push(SessionEvent::Dirty(clipped));
        }
    }

    /// Records the effects of a command that has just been executed/undone/redone.
    pub(crate) fn after_change(&mut self, affected: Option<RectF>, canvas_changed: bool) {
        match affected {
            Some(r) => self.mark_dirty(r),
            None => {
                let c = RectF::from(self.doc.canvas_rect());
                self.mark_dirty(c);
            }
        }
        if canvas_changed {
            self.events.push(SessionEvent::CanvasChanged);
        }
        self.events.push(SessionEvent::HistoryChanged {
            can_undo: self.history.can_undo(),
            can_redo: self.history.can_redo(),
        });
        self.prune_selection();
        self.events.push(SessionEvent::OverlayChanged);
    }

    /// Executes a command through the history and reports what changed.
    pub(crate) fn exec(&mut self, cmd: Command, key: Option<CoalesceKey>) {
        let affected = cmd.affected();
        let canvas = matches!(cmd, Command::SetDocument { .. });
        self.history.execute(&mut self.doc, cmd, key);
        self.after_change(affected, canvas);
    }

    pub(crate) fn prune_selection(&mut self) {
        let before = self.selection.len();
        let doc = &self.doc;
        self.selection.retain(|id| doc.object(*id).is_some());
        if let Some(t) = &self.text {
            if self.doc.object(t.id).is_none() {
                self.text = None;
                self.events.push(SessionEvent::TextEditing(false));
            }
        }
        if before != self.selection.len() {
            self.events.push(SessionEvent::SelectionChanged);
        }
    }

    pub(crate) fn set_selection(&mut self, ids: Vec<ObjectId>) {
        let ids = self.expand_groups(ids);
        if ids != self.selection {
            self.selection = ids;
            self.events.push(SessionEvent::SelectionChanged);
            self.events.push(SessionEvent::OverlayChanged);
        }
    }

    fn expand_groups(&self, ids: Vec<ObjectId>) -> Vec<ObjectId> {
        let groups: Vec<u32> =
            ids.iter().filter_map(|id| self.doc.object(*id).and_then(|o| o.group)).collect();
        let mut out = ids;
        for o in self.doc.objects() {
            if o.group.is_some_and(|g| groups.contains(&g)) && !out.contains(&o.id) {
                out.push(o.id);
            }
        }
        out
    }

    /// Replaces `before` objects with `after` versions (skipped when identical).
    pub(crate) fn modify(&mut self, before: Vec<Object>, after: Vec<Object>, key: Option<CoalesceKey>) {
        if before == after {
            return;
        }
        self.exec(Command::Modify { before, after }, key);
    }

    pub(crate) fn snap_targets(&self, exclude: &[ObjectId]) -> SnapTargets {
        let mut t = SnapTargets::default();
        let (w, h) = self.doc.image_size();
        t.add_rect(RectF::new(0.0, 0.0, w as f32, h as f32));
        for o in self.doc.objects().iter().filter(|o| o.visible && !exclude.contains(&o.id)) {
            let b = o.bounds();
            if b.w > 0.0 || b.h > 0.0 {
                t.add_rect(b);
            }
        }
        t
    }

    fn snap_thr(&self) -> f32 {
        SNAP_PX / self.view_scale
    }

    fn extent(&self) -> RectF {
        RectF::from(self.doc.canvas_rect())
    }

    // ---------------------------------------------------------------------------------
    // Tools
    // ---------------------------------------------------------------------------------

    /// Switches tool. Finishes text editing, drops a pending crop and any snapping guides.
    pub fn set_tool(&mut self, tool: Tool) {
        if tool == self.tool {
            return;
        }
        self.commit_text_edit();
        self.cancel_drag();
        self.crop = None;
        self.guides.clear();
        self.tool = tool;
        self.events.push(SessionEvent::ToolChanged(tool));
        self.events.push(SessionEvent::OverlayChanged);
        self.update_cursor();
    }

    // ---------------------------------------------------------------------------------
    // Hit testing and handles
    // ---------------------------------------------------------------------------------

    /// The topmost visible, unlocked object under `p` (image space).
    pub fn hit_test(&self, p: PointF) -> Option<ObjectId> {
        let tol = self.hit_slack();
        self.doc
            .objects()
            .iter()
            .rev()
            .find(|o| o.visible && !o.locked && !matches!(o.kind, ObjectKind::Spotlight(_)) && o.hit_test(p, tol))
            .map(|o| o.id)
            .or_else(|| {
                // Spotlights are hit last (they are big and dim everything else).
                self.doc
                    .objects()
                    .iter()
                    .rev()
                    .find(|o| o.visible && !o.locked && matches!(o.kind, ObjectKind::Spotlight(_)) && o.hit_test(p, tol))
                    .map(|o| o.id)
            })
    }

    /// Handles of the current selection.
    pub fn handles(&self) -> Vec<Handle> {
        let objs: Vec<&Object> = self.selection.iter().filter_map(|id| self.doc.object(*id)).collect();
        if objs.iter().any(|o| o.locked) {
            return Vec::new();
        }
        let Some(layout) = geometry::layout_for(&objs) else { return Vec::new() };
        let mut extras = Vec::new();
        if let [o] = objs.as_slice() {
            match &o.kind {
                ObjectKind::Balloon(b) => extras.push((HandleKind::Tail, b.tail)),
                ObjectKind::Magnify(m) => extras.push((HandleKind::Source, m.source)),
                _ => {}
            }
        }
        geometry::handles(&layout, self.view_scale, &extras)
    }

    fn hit_handle(&self, p: PointF) -> Option<Handle> {
        geometry::hit_handle(&self.handles(), p, HANDLE_RADIUS_PX / self.view_scale)
    }

    /// The cursor the GUI should show right now.
    pub fn cursor_hint(&self) -> CursorHint {
        if let Some(d) = &self.drag {
            return match d {
                Drag::Create { .. } | Drag::Crop { .. } | Drag::CutOut { .. } => CursorHint::Crosshair,
                Drag::Move { .. } => CursorHint::Grabbing,
                Drag::ResizeBox { handle, rotation, .. } => geometry::resize_cursor(*handle, *rotation),
                Drag::ResizeGroup { handle, .. } => geometry::resize_cursor(*handle, 0.0),
                Drag::Rotate { .. } => CursorHint::Rotate,
                Drag::Endpoint { .. } | Drag::Tail { .. } | Drag::Source { .. } => CursorHint::Crosshair,
                Drag::Marquee { .. } => CursorHint::Default,
                Drag::Erase { .. } => CursorHint::Eraser,
                Drag::TextSelect => CursorHint::Text,
            };
        }
        if let Some(t) = &self.text {
            if self.obj(t.id).is_some_and(|o| o.hit_test(self.hover, self.hit_slack())) {
                return CursorHint::Text;
            }
        }
        if let Some(h) = self.hit_handle(self.hover) {
            return h.cursor;
        }
        match self.tool {
            Tool::Select => {
                if self.hit_test(self.hover).is_some() {
                    CursorHint::Move
                } else {
                    CursorHint::Default
                }
            }
            Tool::Text => CursorHint::Text,
            Tool::Eraser => CursorHint::Eraser,
            _ => CursorHint::Crosshair,
        }
    }

    pub(crate) fn update_cursor(&mut self) {
        let c = self.cursor_hint();
        if c != self.cursor {
            self.cursor = c;
            self.events.push(SessionEvent::CursorChanged(c));
        }
    }

    /// Everything the GUI should draw over the rendered document.
    pub fn overlay(&mut self) -> Overlay {
        let objs: Vec<&Object> = self.selection.iter().filter_map(|id| self.doc.object(*id)).collect();
        let mut o = Overlay {
            selection: geometry::outlines(&objs),
            handles: if self.drag.is_none() || matches!(self.drag, Some(Drag::TextSelect)) {
                self.handles()
            } else {
                Vec::new()
            },
            guides: self.guides.clone(),
            caret: None,
            ..Overlay::default()
        };
        o.caret = self.caret_overlay();
        match &self.drag {
            Some(Drag::Marquee { start, cur, .. }) => o.marquee = Some(RectF::from_points(*start, *cur)),
            Some(Drag::Erase { marks, .. }) => o.erase_marks = marks.clone(),
            Some(Drag::CutOut { start, cur }) => o.cut_strip = Some(self.cut_strip(*start, *cur)),
            Some(Drag::Crop { start, cur, points }) => {
                o.crop = Some(self.crop_overlay(self.tool, *start, *cur, points));
            }
            _ => {}
        }
        if let Some(c) = &self.crop {
            o.crop = Some(CropOverlay {
                rect: c.rect,
                polygon: c.polygon.clone(),
                ellipse: c.tool == Tool::CropEllipse,
            });
        }
        o
    }

    fn crop_overlay(&self, tool: Tool, start: PointF, cur: PointF, points: &[PointF]) -> CropOverlay {
        let rect = if tool == Tool::CropFreeform {
            RectF::bounding(points).unwrap_or_default()
        } else {
            RectF::from_points(start, cur)
        };
        CropOverlay {
            rect,
            polygon: if tool == Tool::CropFreeform { points.to_vec() } else { Vec::new() },
            ellipse: tool == Tool::CropEllipse,
        }
    }

    /// The strip a cut-out drag from `start` to `cur` would remove (image space, full extent
    /// along the other axis).
    fn cut_strip(&self, start: PointF, cur: PointF) -> RectF {
        let (w, h) = self.doc.image_size();
        let d = cur - start;
        if d.x.abs() >= d.y.abs() {
            let (a, b) = (start.x.min(cur.x), start.x.max(cur.x));
            RectF::new(a, 0.0, b - a, h as f32)
        } else {
            let (a, b) = (start.y.min(cur.y), start.y.max(cur.y));
            RectF::new(0.0, a, w as f32, b - a)
        }
    }

    // ---------------------------------------------------------------------------------
    // Pointer input
    // ---------------------------------------------------------------------------------

    /// Primary button pressed at `pos` (image space). `pressure` (pen/tablet, 0–1) is accepted
    /// for API completeness; strokes are deliberately pressure-less.
    pub fn pointer_down(&mut self, pos: PointF, mods: Modifiers, _pressure: Option<f32>) {
        if !pos.is_finite() {
            return;
        }
        if self.drag.is_some() {
            self.pointer_up(pos, mods);
        }
        self.hover = pos;
        self.guides.clear();
        if self.text.is_some() && self.text_pointer_down(pos) {
            self.update_cursor();
            return;
        }
        self.commit_text_edit();
        match self.tool {
            Tool::Select => self.down_select(pos, mods),
            Tool::Eraser => {
                self.drag = Some(Drag::Erase { marks: Vec::new(), last: pos });
                self.erase_sample(pos, pos);
            }
            Tool::Crop | Tool::CropEllipse | Tool::CropFreeform => {
                self.crop = None;
                self.drag = Some(Drag::Crop { start: pos, cur: pos, points: vec![pos] });
            }
            Tool::CutOut => self.drag = Some(Drag::CutOut { start: pos, cur: pos }),
            _ => {
                // Creation tools: grabbing a handle of the selected object wins.
                if let Some(h) = self.hit_handle(pos) {
                    self.begin_handle_drag(h, pos);
                } else {
                    self.begin_create(pos, mods);
                }
            }
        }
        self.events.push(SessionEvent::OverlayChanged);
        self.update_cursor();
    }

    /// Pointer moved (with or without the button held).
    pub fn pointer_move(&mut self, pos: PointF, mods: Modifiers, _pressure: Option<f32>) {
        if !pos.is_finite() {
            return;
        }
        let prev = self.hover;
        self.hover = pos;
        if let Some(d) = self.drag.take() {
            let d = self.drag_move(d, pos, prev, mods);
            self.drag = Some(d);
            self.events.push(SessionEvent::OverlayChanged);
        }
        self.update_cursor();
    }

    /// Primary button released.
    pub fn pointer_up(&mut self, pos: PointF, mods: Modifiers) {
        if pos.is_finite() && self.drag.is_some() {
            self.pointer_move(pos, mods, None);
        }
        let Some(d) = self.drag.take() else { return };
        self.guides.clear();
        match d {
            Drag::Create { id, tool, start, .. } => self.end_create(id, tool, start),
            Drag::Move { .. }
            | Drag::ResizeBox { .. }
            | Drag::ResizeGroup { .. }
            | Drag::Rotate { .. }
            | Drag::Endpoint { .. }
            | Drag::Tail { .. }
            | Drag::Source { .. } => self.history.seal(),
            Drag::Marquee { start, cur, base } => {
                let r = RectF::from_points(start, cur);
                let mut ids = base;
                if r.w > MIN_DRAG_PX / self.view_scale || r.h > MIN_DRAG_PX / self.view_scale {
                    for o in self.doc.objects().iter().filter(|o| o.visible && !o.locked) {
                        if o.bounds().intersects(&r) && !ids.contains(&o.id) {
                            ids.push(o.id);
                        }
                    }
                }
                self.set_selection(ids);
            }
            Drag::Erase { marks, .. } => self.finish_erase(marks),
            Drag::Crop { start, cur, points } => self.finish_crop_drag(start, cur, points),
            Drag::CutOut { start, cur } => {
                let strip = self.cut_strip(start, cur);
                let vertical = strip.h >= self.doc.image_size().1 as f32 - 0.5
                    && (cur.x - start.x).abs() >= (cur.y - start.y).abs();
                let (axis, a, b) = if vertical {
                    (crate::object::Axis::X, strip.x, strip.right())
                } else {
                    (crate::object::Axis::Y, strip.y, strip.bottom())
                };
                if (b - a) >= 1.0 {
                    let _ = self.cut_out(axis, a.round() as i32, b.round() as i32);
                }
            }
            Drag::TextSelect => {}
        }
        self.events.push(SessionEvent::OverlayChanged);
        self.update_cursor();
    }

    /// Double click: enters text editing on a text/balloon under the pointer.
    pub fn double_click(&mut self, pos: PointF, _mods: Modifiers) {
        if let Some(id) = self.hit_test(pos) {
            if self.obj(id).is_some_and(|o| o.kind.text_content().is_some()) {
                self.cancel_drag();
                self.set_selection(vec![id]);
                self.begin_text_edit(id);
            }
        }
    }

    /// Aborts an in-progress drag, undoing what it did so far (Escape).
    pub fn cancel_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        match d {
            Drag::Create { .. } => {
                self.history.discard_open(&mut self.doc, |c| matches!(c, Command::Add { .. }));
                self.after_change(None, false);
            }
            Drag::Move { originals, .. } => self.revert_to(&originals),
            Drag::ResizeBox { original, .. }
            | Drag::Rotate { original, .. }
            | Drag::Endpoint { original, .. }
            | Drag::Tail { original, .. }
            | Drag::Source { original, .. } => self.revert_to(std::slice::from_ref(&original)),
            Drag::ResizeGroup { originals, .. } => self.revert_to(&originals),
            _ => {}
        }
        self.guides.clear();
        self.history.seal();
        self.events.push(SessionEvent::OverlayChanged);
    }

    fn revert_to(&mut self, originals: &[Object]) {
        let cur: Vec<Object> = originals.iter().filter_map(|o| self.obj(o.id).cloned()).collect();
        // Drop the open (coalesced) drag entry so cancelling leaves no undo step.
        let dropped = self.history.discard_open(&mut self.doc, |c| matches!(c, Command::Modify { .. }));
        if dropped {
            self.after_change(None, false);
        } else if cur != originals {
            self.exec(Command::Modify { before: cur, after: originals.to_vec() }, None);
        }
    }

    // ---------------------------------------------------------------------------------
    // Select tool
    // ---------------------------------------------------------------------------------

    fn down_select(&mut self, pos: PointF, mods: Modifiers) {
        if let Some(h) = self.hit_handle(pos) {
            self.begin_handle_drag(h, pos);
            return;
        }
        match self.hit_test(pos) {
            Some(id) => {
                if mods.shift {
                    let mut sel = self.selection.clone();
                    if let Some(i) = sel.iter().position(|s| *s == id) {
                        sel.remove(i);
                        // Toggling a grouped object off removes its whole group.
                        let g = self.obj(id).and_then(|o| o.group);
                        if let Some(g) = g {
                            sel.retain(|s| self.obj(*s).and_then(|o| o.group) != Some(g));
                        }
                    } else {
                        sel.push(id);
                    }
                    self.set_selection(sel);
                    return;
                }
                if !self.selection.contains(&id) {
                    self.set_selection(vec![id]);
                }
                let originals: Vec<Object> =
                    self.selection.iter().filter_map(|i| self.obj(*i).cloned()).filter(|o| !o.locked).collect();
                if originals.is_empty() {
                    return;
                }
                let key = self.next_key("drag");
                self.drag = Some(Drag::Move { originals, start: pos, key });
            }
            None => {
                let base = if mods.shift { self.selection.clone() } else { Vec::new() };
                if !mods.shift {
                    self.set_selection(Vec::new());
                }
                self.drag = Some(Drag::Marquee { start: pos, cur: pos, base });
            }
        }
    }

    fn begin_handle_drag(&mut self, h: Handle, pos: PointF) {
        let objs: Vec<Object> = self.selection.iter().filter_map(|i| self.obj(*i).cloned()).collect();
        let key = self.next_key("drag");
        match (h.kind, objs.as_slice()) {
            (HandleKind::Rotate, [o]) => {
                let (r, rot) = o.kind.as_box().unwrap_or((o.bounds(), 0.0));
                let pivot = r.center();
                let _ = rot;
                self.drag = Some(Drag::Rotate {
                    original: o.clone(),
                    pivot,
                    start_angle: (pos - pivot).y.atan2((pos - pivot).x),
                    key,
                });
            }
            (HandleKind::Endpoint(w), [o]) => {
                self.drag = Some(Drag::Endpoint { original: o.clone(), which: w, key });
            }
            (HandleKind::Tail, [o]) => self.drag = Some(Drag::Tail { original: o.clone(), key }),
            (HandleKind::Source, [o]) => self.drag = Some(Drag::Source { original: o.clone(), key }),
            (kind, [o]) if kind.direction().is_some() => match o.kind.as_box() {
                Some((rect, rotation)) => {
                    self.drag = Some(Drag::ResizeBox {
                        original: o.clone(),
                        handle: kind,
                        rect: rect.normalized(),
                        rotation,
                        key,
                    });
                }
                None => {
                    self.drag = Some(Drag::ResizeGroup {
                        bounds: o.bounds(),
                        originals: objs.clone(),
                        handle: kind,
                        key,
                    });
                }
            },
            (kind, many) if kind.direction().is_some() && !many.is_empty() => {
                let bounds = many.iter().map(Object::bounds).reduce(|a, b| a.union(&b)).unwrap_or_default();
                self.drag = Some(Drag::ResizeGroup { bounds, originals: many.to_vec(), handle: kind, key });
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------------------------
    // Drag updates
    // ---------------------------------------------------------------------------------

    fn drag_move(&mut self, d: Drag, pos: PointF, prev: PointF, mods: Modifiers) -> Drag {
        match d {
            Drag::Create { id, tool, start, mut points, key } => {
                let cur = self.maybe_snap(pos, mods, &[id]);
                self.update_create(id, tool, start, cur, &mut points, mods, key);
                Drag::Create { id, tool, start, points, key }
            }
            Drag::Move { originals, start, key } => {
                let mut delta = pos - start;
                if mods.shift {
                    if delta.x.abs() >= delta.y.abs() {
                        delta.y = 0.0;
                    } else {
                        delta.x = 0.0;
                    }
                }
                if mods.ctrl {
                    let ids: Vec<ObjectId> = originals.iter().map(|o| o.id).collect();
                    let t = self.snap_targets(&ids);
                    let b = originals.iter().map(Object::bounds).reduce(|a, b| a.union(&b)).unwrap_or_default();
                    let (sx, sy, g) = geometry::snap_rect(b.translate(delta.x, delta.y), &t, self.snap_thr(), self.extent());
                    delta = delta + PointF::new(sx, sy);
                    self.guides = g;
                } else {
                    self.guides.clear();
                }
                let after: Vec<Object> = originals
                    .iter()
                    .map(|o| {
                        let mut n = o.clone();
                        n.translate_with_source(delta.x, delta.y);
                        n
                    })
                    .collect();
                self.modify(originals.clone(), after, Some(key));
                Drag::Move { originals, start, key }
            }
            Drag::ResizeBox { original, handle, rect, rotation, key } => {
                let p = self.maybe_snap(pos, mods, &[original.id]);
                let nr = geometry::resize_box(rect, rotation, handle, p, mods);
                let mut n = original.clone();
                if let Some(r) = n.kind.rect_mut() {
                    *r = nr;
                }
                if let ObjectKind::Text(t) = &mut n.kind {
                    if handle.direction().is_some_and(|(dx, _)| dx != 0) {
                        t.auto_width = false;
                    }
                }
                self.sync_text(&mut n);
                self.modify(vec![original.clone()], vec![n], Some(key));
                Drag::ResizeBox { original, handle, rect, rotation, key }
            }
            Drag::ResizeGroup { originals, handle, bounds, key } => {
                let p = self.maybe_snap(pos, mods, &originals.iter().map(|o| o.id).collect::<Vec<_>>());
                let (anchor, sx, sy) = geometry::group_scale(bounds, handle, p, mods);
                let after: Vec<Object> = originals
                    .iter()
                    .map(|o| {
                        let mut n = o.clone();
                        n.scale_about(anchor, sx, sy);
                        n
                    })
                    .collect();
                self.modify(originals.clone(), after, Some(key));
                Drag::ResizeGroup { originals, handle, bounds, key }
            }
            Drag::Rotate { original, pivot, start_angle, key } => {
                let now = (pos - pivot).y.atan2((pos - pivot).x);
                let mut rot = crate::object::normalize_angle(original.kind.rotation() + now - start_angle);
                if mods.shift {
                    let step = 15f32.to_radians();
                    rot = crate::object::normalize_angle((rot / step).round() * step);
                }
                let mut n = original.clone();
                n.kind.set_rotation(rot);
                self.modify(vec![original.clone()], vec![n], Some(key));
                Drag::Rotate { original, pivot, start_angle, key }
            }
            Drag::Endpoint { original, which, key } => {
                let mut p = self.maybe_snap(pos, mods, &[original.id]);
                let mut n = original.clone();
                let (a, b) = match &mut n.kind {
                    ObjectKind::Line(l) => (&mut l.a, &mut l.b),
                    ObjectKind::Arrow(a) => (&mut a.a, &mut a.b),
                    _ => return Drag::Endpoint { original, which, key },
                };
                let (moving, fixed) = if which == 0 { (a, *b) } else { (b, *a) };
                if mods.shift {
                    p = geometry::constrain_45(fixed, p);
                }
                *moving = p;
                self.modify(vec![original.clone()], vec![n], Some(key));
                Drag::Endpoint { original, which, key }
            }
            Drag::Tail { original, key } => {
                let p = self.maybe_snap(pos, mods, &[original.id]);
                let mut n = original.clone();
                if let ObjectKind::Balloon(b) = &mut n.kind {
                    b.tail = p;
                }
                self.modify(vec![original.clone()], vec![n], Some(key));
                Drag::Tail { original, key }
            }
            Drag::Source { original, key } => {
                let p = self.maybe_snap(pos, mods, &[original.id]);
                let mut n = original.clone();
                if let ObjectKind::Magnify(m) = &mut n.kind {
                    m.source = p;
                }
                self.modify(vec![original.clone()], vec![n], Some(key));
                Drag::Source { original, key }
            }
            Drag::Marquee { start, base, .. } => Drag::Marquee { start, cur: pos, base },
            Drag::Erase { mut marks, last } => {
                let _ = prev;
                self.erase_sample_into(last, pos, &mut marks);
                Drag::Erase { marks, last: pos }
            }
            Drag::Crop { start, mut points, .. } => {
                if self.tool == Tool::CropFreeform {
                    let step = FREEHAND_STEP_PX * 2.0 / self.view_scale;
                    if points.last().is_none_or(|l| l.distance(pos) >= step) {
                        points.push(pos);
                    }
                }
                Drag::Crop { start, cur: pos, points }
            }
            Drag::CutOut { start, .. } => Drag::CutOut { start, cur: pos },
            Drag::TextSelect => {
                self.text_drag_to(pos);
                Drag::TextSelect
            }
        }
    }

    /// With ctrl held, snaps a point to guides.
    fn maybe_snap(&mut self, p: PointF, mods: Modifiers, exclude: &[ObjectId]) -> PointF {
        if !mods.ctrl {
            self.guides.clear();
            return p;
        }
        let t = self.snap_targets(exclude);
        let (q, g) = geometry::snap_point(p, &t, self.snap_thr(), self.extent());
        self.guides = g;
        q
    }

    // ---------------------------------------------------------------------------------
    // Creating objects
    // ---------------------------------------------------------------------------------

    fn begin_create(&mut self, pos: PointF, mods: Modifiers) {
        let tool = self.tool;
        let Some(Preset { style, mut kind }) = self.styles.get(tool) else { return };
        if tool == Tool::Image {
            let Some(img) = self.pending_image.clone() else { return };
            if let ObjectKind::Image(i) = &mut kind {
                i.image = ImageData::new(img);
            }
        }
        let start = self.maybe_snap(pos, mods, &[]);
        let id = self.doc.peek_next_id();
        let mut obj = Object::new(id, style, kind);
        let mut points = vec![start];
        self.set_create_geometry(&mut obj, tool, start, start, &mut points, mods);
        self.sync_text(&mut obj);
        let key = if tool == Tool::Text {
            CoalesceKey { kind: "typing", tag: id.0 }
        } else {
            self.next_key("drag")
        };
        let cmd = Command::Add {
            items: vec![(self.doc.objects().len(), obj)],
            prev_next_id: self.doc.peek_next_id().0,
        };
        self.exec(cmd, Some(key));
        self.set_selection(Vec::new());
        self.drag = Some(Drag::Create { id, tool, start, points, key });
    }

    #[allow(clippy::too_many_arguments)] // one call site per drag event; a struct would only add noise
    fn update_create(
        &mut self,
        id: ObjectId,
        tool: Tool,
        start: PointF,
        cur: PointF,
        points: &mut Vec<PointF>,
        mods: Modifiers,
        key: CoalesceKey,
    ) {
        let Some(before) = self.obj(id).cloned() else { return };
        let mut n = before.clone();
        if matches!(tool, Tool::Freehand | Tool::FreehandArrow | Tool::HighlightPen) {
            let step = FREEHAND_STEP_PX / self.view_scale;
            if points.last().is_none_or(|l| l.distance(cur) >= step) {
                points.push(cur);
            }
        }
        self.set_create_geometry(&mut n, tool, start, cur, points, mods);
        self.sync_text(&mut n);
        self.modify(vec![before], vec![n], Some(key));
    }

    fn set_create_geometry(
        &self,
        obj: &mut Object,
        tool: Tool,
        start: PointF,
        cur: PointF,
        points: &mut Vec<PointF>,
        mods: Modifiers,
    ) {
        let dragged = start.distance(cur) * self.view_scale >= MIN_DRAG_PX;
        let end = if mods.shift && matches!(tool, Tool::Line | Tool::Arrow) {
            geometry::constrain_45(start, cur)
        } else {
            cur
        };
        match &mut obj.kind {
            ObjectKind::Line(l) => {
                let (a, b) = line_ends(start, end, mods);
                l.a = a;
                l.b = b;
            }
            ObjectKind::Arrow(ar) => {
                let (a, b) = line_ends(start, end, mods);
                ar.a = a;
                ar.b = b;
            }
            ObjectKind::Freehand(f) => {
                if mods.shift {
                    *points = vec![start, geometry::constrain_45(start, cur)];
                }
                f.points = shapes::thin_points(points, 0.0);
            }
            ObjectKind::Highlight(h) if tool == Tool::HighlightPen => {
                if mods.shift {
                    *points = vec![start, geometry::constrain_45(start, cur)];
                }
                h.points = points.clone();
            }
            ObjectKind::Step(s) => s.center = cur,
            ObjectKind::Cursor(c) => c.pos = cur,
            ObjectKind::Sticker(s) => {
                let size = 48.0;
                s.rect = RectF::from_center_size(cur, size, size);
            }
            ObjectKind::Image(i) => {
                let (w, h) = (i.image.0.width() as f32, i.image.0.height() as f32);
                let (cw, ch) = self.doc.image_size();
                let k = (cw as f32 * 0.8 / w.max(1.0)).min(ch as f32 * 0.8 / h.max(1.0)).min(1.0);
                i.rect = RectF::from_center_size(cur, w * k, h * k);
            }
            ObjectKind::Text(t) => {
                if dragged {
                    let r = geometry::drag_rect(start, cur, Modifiers::NONE);
                    t.rect = RectF::new(r.x, r.y, r.w.max(20.0), r.h);
                    t.auto_width = false;
                } else {
                    t.rect = RectF::new(start.x, start.y, 0.0, 0.0);
                    t.auto_width = true;
                }
            }
            ObjectKind::Balloon(b) => {
                let r = geometry::drag_rect(start, cur, mods);
                b.rect = r;
                b.tail = PointF::new(r.x + r.w * 0.3, r.bottom() + (r.h * 0.45).max(16.0));
            }
            ObjectKind::Magnify(m) => {
                let r = geometry::drag_rect(start, cur, mods);
                m.rect = r;
                m.source = r.center();
            }
            other => {
                if let Some(r) = other.rect_mut() {
                    *r = geometry::drag_rect(start, cur, mods);
                }
            }
        }
    }

    fn end_create(&mut self, id: ObjectId, tool: Tool, start: PointF) {
        let Some(mut obj) = self.obj(id).cloned() else { return };
        let b = obj.bounds();
        let min = MIN_DRAG_PX / self.view_scale;
        let tiny = b.w < min && b.h < min;
        match tool {
            Tool::Text => {
                self.begin_text_edit(id);
                return;
            }
            Tool::Balloon | Tool::Magnify if tiny => {
                let (w, h) = if tool == Tool::Balloon { (170.0, 70.0) } else { (140.0, 140.0) };
                let before = obj.clone();
                let r = RectF::new(start.x, start.y, w, h);
                match &mut obj.kind {
                    ObjectKind::Balloon(bl) => {
                        bl.rect = r;
                        bl.tail = PointF::new(r.x + r.w * 0.3, r.bottom() + 30.0);
                    }
                    ObjectKind::Magnify(m) => {
                        m.rect = r;
                        m.source = r.center();
                    }
                    _ => {}
                }
                let key = CoalesceKey { kind: "drag", tag: self.drag_seq };
                self.modify(vec![before], vec![obj], Some(key));
            }
            Tool::Rectangle
            | Tool::Ellipse
            | Tool::Line
            | Tool::Arrow
            | Tool::FreehandArrow
            | Tool::Blur
            | Tool::Pixelate
            | Tool::Highlight
            | Tool::Spotlight
            | Tool::Grid
            | Tool::Balloon
            | Tool::Magnify
                if tiny =>
            {
                self.history.discard_open(&mut self.doc, |c| matches!(c, Command::Add { .. }));
                self.after_change(None, false);
                return;
            }
            _ => {}
        }
        self.history.seal();
        let starts_editing = matches!(tool, Tool::Balloon);
        self.set_selection(vec![id]);
        if starts_editing {
            self.begin_text_edit(id);
        }
    }

    // ---------------------------------------------------------------------------------
    // Eraser
    // ---------------------------------------------------------------------------------

    fn erase_sample(&mut self, from: PointF, to: PointF) {
        let mut marks = match self.drag.take() {
            Some(Drag::Erase { marks, .. }) => marks,
            other => {
                self.drag = other;
                return;
            }
        };
        self.erase_sample_into(from, to, &mut marks);
        self.drag = Some(Drag::Erase { marks, last: to });
    }

    fn erase_sample_into(&self, from: PointF, to: PointF, marks: &mut Vec<ObjectId>) {
        let tol = self.hit_slack() + 2.0;
        let len = from.distance(to);
        let steps = ((len / (tol / 2.0).max(0.5)).ceil() as usize).clamp(1, 4000);
        for i in 0..=steps {
            let p = from.lerp(to, i as f32 / steps as f32);
            // Erase everything the stroke touches (top to bottom), not just the top object.
            for o in self.doc.objects().iter().rev() {
                if o.visible && !o.locked && !marks.contains(&o.id) && o.hit_test(p, tol) {
                    marks.push(o.id);
                }
            }
        }
    }

    fn finish_erase(&mut self, marks: Vec<ObjectId>) {
        if marks.is_empty() {
            return;
        }
        let mut items: Vec<(usize, Object)> = marks
            .iter()
            .filter_map(|id| self.doc.index_of(*id).map(|i| (i, self.doc.objects()[i].clone())))
            .collect();
        items.sort_by_key(|(i, _)| *i);
        self.exec(Command::Remove { items }, None);
    }

    // ---------------------------------------------------------------------------------
    // Crop tools
    // ---------------------------------------------------------------------------------

    fn finish_crop_drag(&mut self, start: PointF, cur: PointF, points: Vec<PointF>) {
        let canvas = self.extent();
        let (rect, polygon) = if self.tool == Tool::CropFreeform {
            (RectF::bounding(&points).unwrap_or_default(), points)
        } else {
            (RectF::from_points(start, cur), Vec::new())
        };
        let min = MIN_DRAG_PX / self.view_scale;
        if rect.w < min || rect.h < min {
            return;
        }
        let clipped = rect_intersection(rect, canvas);
        if let Some(r) = clipped {
            self.crop = Some(CropPending { rect: r, tool: self.tool, polygon });
        }
    }

    /// The pending crop, if any.
    pub fn pending_crop(&self) -> Option<&CropPending> {
        self.crop.as_ref()
    }

    /// Applies the pending crop (Enter). Rectangular crops keep annotations editable;
    /// elliptical and freeform crops flatten the picture (annotations are baked in, because a
    /// non-rectangular result cannot be expressed as canvas + objects).
    pub fn apply_crop(&mut self) -> Result<(), crate::DocError> {
        let Some(c) = self.crop.take() else { return Ok(()) };
        self.events.push(SessionEvent::OverlayChanged);
        let r = c.rect.to_outer_rect();
        match c.tool {
            Tool::CropEllipse | Tool::CropFreeform => self.crop_shaped(r, &c),
            _ => self.crop_rect(r),
        }
    }

    /// Cancels the pending crop (Escape).
    pub fn cancel_crop(&mut self) {
        if self.crop.take().is_some() {
            self.events.push(SessionEvent::OverlayChanged);
        }
    }

    // ---------------------------------------------------------------------------------
    // Keyboard
    // ---------------------------------------------------------------------------------

    /// A key was pressed. Returns `true` when the session used it.
    pub fn key_down(&mut self, key: Key, mods: Modifiers) -> bool {
        if self.text.is_some() && self.text_key(key, mods) {
            return true;
        }
        if mods.ctrl {
            if let Key::Char(c) = key {
                return self.shortcut(c.to_ascii_lowercase(), mods);
            }
        }
        match key {
            Key::Escape => {
                if self.drag.is_some() {
                    self.cancel_drag();
                } else if self.crop.is_some() {
                    self.cancel_crop();
                } else if !self.selection.is_empty() {
                    self.set_selection(Vec::new());
                } else {
                    return false;
                }
                true
            }
            Key::Delete | Key::Backspace => {
                if self.selection.is_empty() {
                    return false;
                }
                self.delete_selection();
                true
            }
            Key::Enter => {
                if self.crop.is_some() {
                    let _ = self.apply_crop();
                    return true;
                }
                if let [id] = self.selection.as_slice() {
                    let id = *id;
                    if self.obj(id).is_some_and(|o| o.kind.text_content().is_some()) {
                        self.begin_text_edit(id);
                        return true;
                    }
                }
                false
            }
            Key::Left | Key::Right | Key::Up | Key::Down => {
                if self.selection.is_empty() {
                    return false;
                }
                let step = if mods.shift { 10.0 } else { 1.0 };
                let (dx, dy) = match key {
                    Key::Left => (-step, 0.0),
                    Key::Right => (step, 0.0),
                    Key::Up => (0.0, -step),
                    _ => (0.0, step),
                };
                self.nudge(dx, dy);
                true
            }
            _ => false,
        }
    }

    fn shortcut(&mut self, c: char, mods: Modifiers) -> bool {
        match c {
            'z' if mods.shift => self.redo(),
            'z' => self.undo(),
            'y' => self.redo(),
            'a' => {
                let ids = self.doc.objects().iter().filter(|o| o.visible && !o.locked).map(|o| o.id).collect();
                self.set_selection(ids);
            }
            'c' => self.copy(),
            'x' => self.cut(),
            'v' => self.paste(),
            'd' => self.duplicate(),
            'g' if mods.shift => self.ungroup_selection(),
            'g' => self.group_selection(),
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------------------------------
    // Style and properties of the selection
    // ---------------------------------------------------------------------------------

    /// Edits the style of the selected objects (one undo step per gesture: repeated calls
    /// while dragging a slider coalesce). With nothing selected, edits the active tool's
    /// remembered style instead. The result is remembered per tool.
    pub fn set_style(&mut self, f: impl Fn(&mut Style)) {
        self.modify_selection("style", |o| f(&mut o.style));
    }

    /// Edits kind-specific properties (font, arrow heads, zoom...) of the selection, or of the
    /// tool preset when nothing is selected.
    pub fn set_kind_props(&mut self, f: impl Fn(&mut ObjectKind)) {
        self.modify_selection("props", |o| f(&mut o.kind));
    }

    /// Sets the arrow heads of selected arrows / freehand arrows.
    pub fn set_arrow_heads(&mut self, heads: ArrowHeads) {
        self.set_kind_props(move |k| match k {
            ObjectKind::Arrow(a) => a.heads = heads,
            ObjectKind::Freehand(f) if f.arrow.is_some() => f.arrow = Some(heads),
            _ => {}
        });
    }

    fn modify_selection(&mut self, kind: &'static str, f: impl Fn(&mut Object)) {
        if self.selection.is_empty() {
            if let Some(mut preset) = self.styles.get(self.tool) {
                let mut probe = Object::new(ObjectId(0), preset.style.clone(), preset.kind.clone());
                f(&mut probe);
                preset.style = probe.style;
                preset.kind = probe.kind.template();
                self.styles.remember(self.tool, preset);
            }
            return;
        }
        let before: Vec<Object> = self.selection.iter().filter_map(|i| self.obj(*i).cloned()).collect();
        let after: Vec<Object> = before
            .iter()
            .map(|o| {
                let mut n = o.clone();
                f(&mut n);
                self.sync_text(&mut n);
                n
            })
            .collect();
        for n in &after {
            self.styles.remember_object(n);
        }
        let tag = before.iter().fold(0u64, |a, o| a.wrapping_mul(31).wrapping_add(o.id.0));
        let key = CoalesceKey { kind, tag };
        // A style edit after other work starts a new step; repeated edits merge.
        self.modify(before, after, Some(key));
    }

    /// Inserts an image object at `at` (image-space centre) or the image centre.
    pub fn insert_image(&mut self, frame: Frame, at: Option<PointF>) {
        let (cw, ch) = self.doc.image_size();
        let c = at.unwrap_or(PointF::new(cw as f32 / 2.0, ch as f32 / 2.0));
        let Some(Preset { style, mut kind }) = self.styles.get(Tool::Image) else { return };
        let (w, h) = (frame.width() as f32, frame.height() as f32);
        let k = (cw as f32 * 0.8 / w.max(1.0)).min(ch as f32 * 0.8 / h.max(1.0)).min(1.0);
        if let ObjectKind::Image(i) = &mut kind {
            i.rect = RectF::from_center_size(c, w * k, h * k);
            i.image = ImageData::new(frame);
        }
        self.add_object(Object::new(self.doc.peek_next_id(), style, kind));
    }

    /// Inserts a sticker (built-in, glyph or bitmap) centred at `at`.
    pub fn insert_sticker(&mut self, source: StickerSource, at: PointF, size: f32) {
        let Some(Preset { style, mut kind }) = self.styles.get(Tool::Sticker) else { return };
        if let ObjectKind::Sticker(s) = &mut kind {
            s.rect = RectF::from_center_size(at, size, size);
            s.source = source;
        }
        self.add_object(Object::new(self.doc.peek_next_id(), style, kind));
    }

    /// Adds a fully formed object on top of the z-order and selects it (one undo step).
    /// The id inside `obj` is replaced by a fresh one.
    pub fn add_object(&mut self, mut obj: Object) -> ObjectId {
        obj.id = self.doc.peek_next_id();
        let id = obj.id;
        self.sync_text(&mut obj);
        let cmd = Command::Add {
            items: vec![(self.doc.objects().len(), obj)],
            prev_next_id: id.0,
        };
        self.exec(cmd, None);
        self.set_selection(vec![id]);
        id
    }
}

fn line_ends(start: PointF, end: PointF, mods: Modifiers) -> (PointF, PointF) {
    if mods.alt {
        (start - (end - start), end)
    } else {
        (start, end)
    }
}

fn rect_intersection(a: RectF, b: RectF) -> Option<RectF> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.right().min(b.right());
    let y1 = a.bottom().min(b.bottom());
    (x1 > x0 && y1 > y0).then(|| RectF::new(x0, y0, x1 - x0, y1 - y0))
}
