//! The canvas: the picture, the overlay, and pointer input.
//!
//! # Rendering
//!
//! The document is never rendered as a whole. The canvas asks the engine for *tiles*
//! (`crate::tiles`) covering the visible viewport at the current zoom, in physical pixels, and
//! shows each as its own texture blitted 1:1. A dirty rectangle from the engine repaints just
//! its part of the tiles that show it (`TextureHandle::set_partial`). Rendering happens on the
//! UI thread but under a per-frame time budget: what does not fit is finished on the next
//! frame, and while a zoom change is being re-rendered the previous zoom's tiles are drawn
//! scaled underneath, so software rendering (llvmpipe) never shows a blank canvas.
//!
//! # Input
//!
//! Raw egui events are processed *in order* (a fast drag delivers several moves per frame and
//! freehand strokes should follow all of them). Positions are converted to image space through
//! the [`Viewport`] and forwarded to the `EditorSession`. Wheel = zoom about the pointer (mouse
//! wheels) or pan (touchpads), Ctrl+wheel / pinch = zoom, middle-drag or Space+drag = pan.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use egui::{
    Align2, Color32, ColorImage, CursorIcon, Event, FontId, Key, Mesh, MouseWheelUnit,
    PointerButton, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions,
    TextureWrapMode, Ui, Vec2, pos2, vec2,
};
use ssx_editor::{Document, Modifiers, ObjectKind, Overlay, PointF, RectF, RenderOptions};
use ssx_types::Frame;

use super::theme;
use crate::{
    action::Action,
    document::{EditorDoc, Pumped, merge},
    keymap,
    preview::Preview,
    props::ColorField,
    state::AppState,
    tiles::{IRect, TILE, TileKey, TilePlanner, visible_tiles},
    viewport::Viewport,
};

/// Time spent rendering tiles per frame before yielding to the UI.
const RENDER_BUDGET: Duration = Duration::from_millis(14);
/// Tiles kept in memory (512x512 RGBA = 1 MB each).
const MAX_TILES: usize = 80;
const _: () = assert!(TILE == 512);

/// Counters for benchmarks and the status line.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanvasStats {
    /// Tiles (or tile regions) rendered so far.
    pub regions_rendered: u64,
    /// Total time spent in the engine's renderer, in milliseconds.
    pub render_ms: f64,
    /// Engine time spent during the last frame, in milliseconds.
    pub last_frame_render_ms: f32,
    /// Regions rendered during the last frame.
    pub last_frame_regions: u32,
}

struct Stale {
    zoom: f32,
    tiles: Vec<(IRect, TextureHandle)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pan {
    Space,
    Middle,
}

/// Canvas state that outlives a frame.
pub struct Canvas {
    /// Zoom and pan.
    pub viewport: Viewport,
    /// What the tiles were rendered from (0 = the session, otherwise a preview revision).
    source_key: u64,
    planner: TilePlanner,
    textures: HashMap<TileKey, TextureHandle>,
    stale: Option<Stale>,
    checker: Option<TextureHandle>,
    captured: bool,
    panning: Option<Pan>,
    last_pointer: Option<Pos2>,
    pressure: Option<f32>,
    probe: Option<(u64, (i32, i32), [u8; 4])>,
    /// Performance counters.
    pub stats: CanvasStats,
    /// The screen rectangle the canvas occupied last frame (points).
    pub last_rect: Rect,
}

impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas").field("viewport", &self.viewport).finish_non_exhaustive()
    }
}

/// What [`Canvas::show`] needs from the app.
#[derive(Debug)]
pub struct CanvasInput<'a> {
    /// The document being edited.
    pub doc: &'a mut EditorDoc,
    /// Window state (tool, eyedropper, hover readout).
    pub state: &'a mut AppState,
    /// Effect preview, shown instead of the document while active.
    pub preview: &'a mut Preview,
    /// A popup menu was open when the frame started: a click then only closes it.
    pub swallow_press: bool,
}

fn frame_to_image(frame: &Frame, w: usize, h: usize) -> ColorImage {
    let tight = w * 4;
    if frame.stride() == tight && frame.data().len() >= tight * h {
        return ColorImage::from_rgba_unmultiplied([w, h], &frame.data()[..tight * h]);
    }
    let mut data = Vec::with_capacity(tight * h);
    for y in 0..frame.height().min(h as u32) {
        data.extend_from_slice(&frame.row(y)[..tight.min(frame.row(y).len())]);
    }
    data.resize(tight * h, 0);
    ColorImage::from_rgba_unmultiplied([w, h], &data)
}

fn pf(p: PointF) -> Pos2 {
    pos2(p.x, p.y)
}

impl Canvas {
    /// A canvas for a document of `content` canvas pixels.
    pub fn new(content: Vec2) -> Self {
        Self {
            viewport: Viewport::new(vec2(1000.0, 700.0), content),
            source_key: 0,
            planner: TilePlanner::new(1.0, (0, 0)),
            textures: HashMap::new(),
            stale: None,
            checker: None,
            captured: false,
            panning: None,
            last_pointer: None,
            pressure: None,
            probe: None,
            stats: CanvasStats::default(),
            last_rect: Rect::ZERO,
        }
    }

    /// Forgets every rendered tile (document replaced, canvas changed).
    pub fn reset_tiles(&mut self) {
        self.textures.clear();
        self.stale = None;
        self.planner = TilePlanner::new(self.viewport.zoom(), self.viewport.output_size());
    }

    /// Resets everything for a brand-new document.
    pub fn reset_for_new_document(&mut self, content: Vec2) {
        self.viewport = Viewport::new(self.viewport.view(), content);
        self.captured = false;
        self.panning = None;
        self.probe = None;
        self.reset_tiles();
    }

    fn checker_texture(&mut self, ctx: &egui::Context) -> egui::TextureId {
        let tex = self.checker.get_or_insert_with(|| {
            let a = Color32::from_rgb(112, 112, 118);
            let b = Color32::from_rgb(88, 88, 94);
            let img = ColorImage::new([2, 2], vec![a, b, b, a]);
            ctx.load_texture(
                "ssx-checker",
                img,
                TextureOptions {
                    magnification: egui::TextureFilter::Nearest,
                    minification: egui::TextureFilter::Nearest,
                    wrap_mode: TextureWrapMode::Repeat,
                    mipmap_mode: None,
                },
            )
        });
        tex.id()
    }

    fn canvas_offset(doc: &Document) -> (f32, f32) {
        let (ox, oy) = doc.canvas_offset();
        (ox as f32, oy as f32)
    }

    /// Converts a screen position (points) to image coordinates.
    pub fn to_image(&self, rect: Rect, ppp: f32, p: Pos2, doc: &Document) -> PointF {
        let phys = pos2((p.x - rect.min.x) * ppp, (p.y - rect.min.y) * ppp);
        let c = self.viewport.screen_to_canvas(phys);
        let (ox, oy) = Self::canvas_offset(doc);
        PointF::new(c.x - ox, c.y - oy)
    }

    /// Converts image coordinates to a screen position (points).
    pub fn to_screen(&self, rect: Rect, ppp: f32, p: PointF, doc: &Document) -> Pos2 {
        let (ox, oy) = Self::canvas_offset(doc);
        let s = self.viewport.canvas_to_screen(pos2(p.x + ox, p.y + oy));
        pos2(rect.min.x + s.x / ppp, rect.min.y + s.y / ppp)
    }

    /// Draws the canvas and processes pointer input. Returns what the session reported.
    pub fn show(&mut self, ui: &mut Ui, inp: CanvasInput<'_>) -> Pumped {
        let CanvasInput { doc, state, preview, swallow_press } = inp;
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        self.last_rect = rect;
        let previewing = preview.active();

        // --- geometry ---------------------------------------------------------------------
        let content_doc: &Document = if previewing {
            preview.doc().map_or_else(|| doc.doc(), |d| d.as_ref())
        } else {
            doc.doc()
        };
        let (cw, ch) = content_doc.canvas_size();
        let key = if previewing { preview.revision * 2 + 1 } else { 0 };
        if key != self.source_key {
            self.source_key = key;
            self.reset_tiles();
        }
        self.viewport.set_view(vec2(rect.width() * ppp, rect.height() * ppp));
        self.viewport.set_content(vec2(cw as f32, ch as f32));
        if !self.planner.matches(self.viewport.zoom(), self.viewport.output_size()) {
            self.retire_tiles();
        }
        doc.session.set_view_scale(self.viewport.zoom() / ppp);
        state.zoom_percent = self.viewport.percent();
        state.image_size = doc.doc().image_size();

        // --- input ------------------------------------------------------------------------
        let mut pumped = Pumped::default();
        let editing_text = doc.session.text_edit_state().is_some();
        let can_draw = !previewing && state.dialog.is_none();
        self.handle_input(
            ui,
            &ctx,
            rect,
            ppp,
            &resp,
            doc,
            state,
            can_draw,
            editing_text,
            swallow_press,
            &mut pumped,
        );
        merge(&mut pumped, doc.pump());
        let (dirty, canvas_changed) = doc.take_render_events();
        if canvas_changed && !previewing {
            let (cw, ch) = doc.doc().canvas_size();
            self.viewport.set_content(vec2(cw as f32, ch as f32));
            self.reset_tiles();
        } else if !previewing {
            let zoom = self.viewport.zoom();
            let view = self.view_irect();
            let mut dropped = Vec::new();
            for r in &dirty {
                let out = doc.doc().image_rect_to_output(
                    RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32),
                    zoom,
                );
                let ir =
                    IRect::new(out.x, out.y, out.x + out.width as i32, out.y + out.height as i32);
                dropped.extend(self.planner.mark_dirty(ir, view));
            }
            for k in dropped {
                self.textures.remove(&k);
            }
        }

        // --- tiles ------------------------------------------------------------------------
        self.render_tiles(&ctx, doc, preview, previewing);

        // --- paint ------------------------------------------------------------------------
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, theme::CANVAS_BG);
        let zoom = self.viewport.zoom();
        let origin = self.viewport.origin();
        let crect = Rect::from_min_size(
            rect.min + vec2(origin.x / ppp, origin.y / ppp),
            vec2(cw as f32 * zoom / ppp, ch as f32 * zoom / ppp),
        );
        // Soft shadow so the picture floats above the surround.
        painter.add(
            egui::Shadow {
                offset: [0, 3],
                blur: 18,
                spread: 0,
                color: Color32::from_black_alpha(120),
            }
            .as_shape(crect, 0.0),
        );
        let visible_c = crect.intersect(rect);
        if visible_c.is_positive() {
            let tex = self.checker_texture(&ctx);
            let cell_pts = 8.0;
            let uv = Rect::from_min_max(
                pos2(
                    (visible_c.min.x - crect.min.x) / (2.0 * cell_pts),
                    (visible_c.min.y - crect.min.y) / (2.0 * cell_pts),
                ),
                pos2(
                    (visible_c.max.x - crect.min.x) / (2.0 * cell_pts),
                    (visible_c.max.y - crect.min.y) / (2.0 * cell_pts),
                ),
            );
            let mut m = Mesh::with_texture(tex);
            m.add_rect_with_uv(visible_c, uv, Color32::WHITE);
            painter.add(Shape::mesh(m));
        }
        if let Some(s) = &self.stale {
            let k = zoom / s.zoom;
            for (r, tex) in &s.tiles {
                let min = rect.min
                    + vec2((origin.x + r.x0 as f32 * k) / ppp, (origin.y + r.y0 as f32 * k) / ppp);
                let size = vec2(r.width() as f32 * k / ppp, r.height() as f32 * k / ppp);
                let tr = Rect::from_min_size(min, size);
                if tr.intersects(rect) {
                    painter.image(
                        tex.id(),
                        tr,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            }
        }
        let bounds = self.planner.bounds();
        for (key, tex) in &self.textures {
            if !self.planner.is_rendered(*key) {
                continue;
            }
            let r = key.rect(bounds);
            let min =
                rect.min + vec2((origin.x + r.x0 as f32) / ppp, (origin.y + r.y0 as f32) / ppp);
            let tr =
                Rect::from_min_size(min, vec2(r.width() as f32 / ppp, r.height() as f32 / ppp));
            if tr.intersects(rect) {
                painter.image(
                    tex.id(),
                    tr,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }
        if self.viewport.show_pixel_grid() && state.prefs.pixel_grid {
            self.paint_pixel_grid(&painter, rect, ppp);
        }
        if !previewing {
            let overlay = doc.session.overlay();
            self.paint_overlay(ui, &painter, rect, ppp, &overlay, doc, state);
        }
        self.paint_eyedropper(&painter, rect, ppp, &resp, doc, state);
        if previewing {
            painter.text(
                rect.left_top() + vec2(12.0, 10.0),
                Align2::LEFT_TOP,
                "Effect preview",
                FontId::proportional(12.0),
                Color32::from_white_alpha(170),
            );
        }

        // --- cursor -----------------------------------------------------------------------
        if resp.hovered() || self.captured || self.panning.is_some() {
            let icon = if self.panning.is_some() {
                CursorIcon::Grabbing
            } else if state.eyedropper.is_some() {
                CursorIcon::Crosshair
            } else if previewing {
                CursorIcon::Default
            } else if ctx.input(|i| i.key_down(Key::Space)) && !editing_text {
                CursorIcon::Grab
            } else {
                keymap::cursor_icon(doc.session.cursor_hint())
            };
            ctx.set_cursor_icon(icon);
        }
        if doc.session.text_edit_state().is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        pumped
    }

    fn view_irect(&self) -> IRect {
        let (x0, y0, x1, y1) = self.viewport.visible_output_px();
        IRect::new(x0, y0, x1, y1)
    }

    /// Moves the current textures into the stale layer and starts a fresh planner (zoom or
    /// canvas size changed).
    fn retire_tiles(&mut self) {
        let bounds = self.planner.bounds();
        let old_zoom = self.planner.zoom();
        let mut tiles = Vec::new();
        if bounds.0 > 0 {
            for (k, tex) in self.textures.drain() {
                if self.planner.is_rendered(k) {
                    tiles.push((k.rect(bounds), tex));
                }
            }
        } else {
            self.textures.clear();
        }
        // Keep the previous stale layer only when nothing better exists.
        if tiles.is_empty() {
            // nothing new to show; keep whatever was stale
        } else {
            self.stale = Some(Stale { zoom: old_zoom, tiles });
        }
        self.planner = TilePlanner::new(self.viewport.zoom(), self.viewport.output_size());
    }

    #[allow(clippy::too_many_arguments)] // one call site; a context struct would only rename them
    fn handle_input(
        &mut self,
        ui: &Ui,
        ctx: &egui::Context,
        rect: Rect,
        ppp: f32,
        resp: &egui::Response,
        doc: &mut EditorDoc,
        state: &mut AppState,
        can_draw: bool,
        editing_text: bool,
        swallow_press: bool,
        pumped: &mut Pumped,
    ) {
        let events = ui.input(|i| i.events.clone());
        let space = ui.input(|i| i.key_down(Key::Space)) && !editing_text;
        let live_mods = ui.input(|i| i.modifiers);
        let hover = ui.input(|i| i.pointer.hover_pos());
        let top_layer = |p: Pos2| ctx.layer_id_at(p) == Some(ui.layer_id());
        let to_img = |s: &Self, p: Pos2, d: &EditorDoc| s.to_image(rect, ppp, p, d.doc());

        // Zoom gestures (pinch, ctrl+wheel) first: egui already merged them.
        if resp.hovered() || self.panning.is_some() {
            let z = ui.input(|i| i.zoom_delta());
            if (z - 1.0).abs() > f32::EPSILON {
                if let Some(h) = hover {
                    self.viewport
                        .zoom_at(pos2((h.x - rect.min.x) * ppp, (h.y - rect.min.y) * ppp), z);
                }
            }
        }

        for ev in &events {
            match ev {
                Event::Touch { force, .. } => self.pressure = *force,
                Event::MouseWheel { unit, delta, modifiers, .. } => {
                    let over = hover.is_some_and(|h| rect.contains(h) && top_layer(h));
                    if !over || modifiers.command {
                        continue;
                    }
                    let h = hover.unwrap_or(rect.center());
                    let anchor = pos2((h.x - rect.min.x) * ppp, (h.y - rect.min.y) * ppp);
                    match unit {
                        MouseWheelUnit::Line | MouseWheelUnit::Page if !modifiers.shift => {
                            let notches =
                                if *unit == MouseWheelUnit::Page { delta.y * 3.0 } else { delta.y };
                            self.viewport.zoom_at(anchor, 1.15f32.powf(notches));
                        }
                        MouseWheelUnit::Line | MouseWheelUnit::Page => {
                            self.viewport.pan_by(vec2(delta.y * 60.0 * ppp, delta.x * 60.0 * ppp));
                        }
                        MouseWheelUnit::Point => {
                            self.viewport.pan_by(vec2(delta.x * ppp, delta.y * ppp));
                        }
                    }
                }
                Event::PointerMoved(p) => {
                    if self.panning.is_some() {
                        if let Some(last) = self.last_pointer {
                            self.viewport.pan_by(vec2((p.x - last.x) * ppp, (p.y - last.y) * ppp));
                        }
                        self.last_pointer = Some(*p);
                    } else if self.captured {
                        let img = to_img(self, *p, doc);
                        doc.session.pointer_move(img, ssx_mods(live_mods), self.pressure);
                    } else if rect.contains(*p) && can_draw {
                        let img = to_img(self, *p, doc);
                        doc.session.pointer_move(img, ssx_mods(live_mods), None);
                    }
                }
                Event::PointerButton { pos, button, pressed, modifiers } => {
                    let inside = rect.contains(*pos) && top_layer(*pos);
                    match (*button, *pressed) {
                        (PointerButton::Middle, true) if inside => {
                            self.panning = Some(Pan::Middle);
                            self.last_pointer = Some(*pos);
                        }
                        (PointerButton::Primary, true) if inside && !swallow_press => {
                            if space {
                                self.panning = Some(Pan::Space);
                                self.last_pointer = Some(*pos);
                            } else if let Some(field) = state.eyedropper {
                                self.pick_color(ppp, rect, *pos, doc, state, field);
                            } else if can_draw {
                                self.captured = true;
                                let img = to_img(self, *pos, doc);
                                doc.session.pointer_down(img, ssx_mods(*modifiers), self.pressure);
                            }
                        }
                        (PointerButton::Middle, false) if self.panning == Some(Pan::Middle) => {
                            self.panning = None;
                        }
                        (PointerButton::Primary, false) => {
                            if self.panning == Some(Pan::Space) {
                                self.panning = None;
                            }
                            if self.captured {
                                self.captured = false;
                                let img = to_img(self, *pos, doc);
                                doc.session.pointer_up(img, ssx_mods(*modifiers));
                            }
                        }
                        _ => {}
                    }
                }
                Event::PointerGone => {
                    if !self.captured {
                        state.hover = crate::state::Hover::default();
                    }
                }
                _ => {}
            }
        }
        if can_draw && resp.double_clicked() {
            if let Some(p) = resp.interact_pointer_pos().or(hover) {
                let img = to_img(self, p, doc);
                doc.session.double_click(img, ssx_mods(live_mods));
            }
        }

        // Hover readout for the status bar.
        if let Some(h) = hover.filter(|h| rect.contains(*h) && top_layer(*h)) {
            let img = to_img(self, h, doc);
            let (w, hh) = doc.doc().image_size();
            let (px, py) = (img.x.floor() as i32, img.y.floor() as i32);
            let inside = px >= 0 && py >= 0 && px < w as i32 && py < hh as i32;
            state.hover.pixel = inside.then_some((px, py));
            state.hover.color =
                if inside && !self.captured { Some(self.probe_color(doc, px, py)) } else { None };
        } else if !self.captured {
            state.hover = crate::state::Hover::default();
        }
        let _ = pumped;
    }

    fn pick_color(
        &mut self,
        ppp: f32,
        rect: Rect,
        pos: Pos2,
        doc: &EditorDoc,
        state: &mut AppState,
        field: ColorField,
    ) {
        let img = self.to_image(rect, ppp, pos, doc.doc());
        let (px, py) = (img.x.floor() as i32, img.y.floor() as i32);
        let [r, g, b, a] = self.probe_color(doc, px, py);
        state.picked = Some((field, ssx_editor::Color::rgba(r, g, b, a)));
        state.eyedropper = None;
    }

    /// The composited colour at an image pixel (transparent outside the canvas).
    fn probe_color(&mut self, doc: &EditorDoc, x: i32, y: i32) -> [u8; 4] {
        let rev = doc.revision();
        if let Some((r, p, c)) = self.probe {
            if r == rev && p == (x, y) {
                return c;
            }
        }
        let d = doc.doc();
        let (ox, oy) = d.canvas_offset();
        let px = x + ox as i32;
        let py = y + oy as i32;
        let (cw, chh) = d.canvas_size();
        let c = if px < 0 || py < 0 || px >= cw as i32 || py >= chh as i32 {
            [0; 4]
        } else {
            let f = ssx_editor::render(
                d,
                &RenderOptions {
                    viewport: Some(ssx_types::Rect::new(px, py, 1, 1)),
                    ..RenderOptions::default()
                },
            );
            let dd = f.data();
            if dd.len() >= 4 { [dd[0], dd[1], dd[2], dd[3]] } else { [0; 4] }
        };
        self.probe = Some((rev, (x, y), c));
        c
    }

    fn render_tiles(
        &mut self,
        ctx: &egui::Context,
        doc: &mut EditorDoc,
        preview: &mut Preview,
        previewing: bool,
    ) {
        let view = self.view_irect();
        let zoom = self.viewport.zoom();
        let bounds = self.planner.bounds();
        self.stats.last_frame_render_ms = 0.0;
        self.stats.last_frame_regions = 0;
        if bounds.0 <= 0 || bounds.1 <= 0 || view.is_empty() {
            return;
        }
        let work = self.planner.work(view);
        let start = Instant::now();
        let opts_base = RenderOptions { scale: zoom, ..RenderOptions::default() };
        let mut done_all = true;
        for (n, w) in work.iter().enumerate() {
            if n > 0 && start.elapsed() > RENDER_BUDGET {
                done_all = false;
                break;
            }
            let (rw, rh) = (w.rect.width(), w.rect.height());
            if rw <= 0 || rh <= 0 {
                continue;
            }
            let opts = RenderOptions {
                viewport: Some(ssx_types::Rect::new(w.rect.x0, w.rect.y0, rw as u32, rh as u32)),
                ..opts_base.clone()
            };
            let t0 = Instant::now();
            let frame = if previewing {
                match preview.doc().cloned() {
                    Some(d) => preview.renderer.render(&d, &opts),
                    None => break,
                }
            } else {
                doc.session.render(&opts)
            };
            let dt = t0.elapsed();
            self.stats.render_ms += dt.as_secs_f64() * 1000.0;
            self.stats.last_frame_render_ms += dt.as_secs_f32() * 1000.0;
            self.stats.regions_rendered += 1;
            self.stats.last_frame_regions += 1;
            let image = frame_to_image(&frame, rw as usize, rh as usize);
            let tile_rect = w.key.rect(bounds);
            let nearest = TextureOptions::NEAREST;
            if w.full {
                let name = format!("ssx-tile-{}-{}", w.key.tx, w.key.ty);
                let tex = ctx.load_texture(name, image, nearest);
                self.textures.insert(w.key, tex);
            } else if let Some(tex) = self.textures.get_mut(&w.key) {
                tex.set_partial(
                    [(w.rect.x0 - tile_rect.x0) as usize, (w.rect.y0 - tile_rect.y0) as usize],
                    image,
                    nearest,
                );
            } else {
                // The texture vanished (evicted); the planner will ask for it in full.
                continue;
            }
            self.planner.done(w);
        }
        if !done_all {
            ctx.request_repaint();
        }
        // The stale layer has served its purpose once every visible tile is fresh.
        if self.stale.is_some()
            && visible_tiles(view, bounds).iter().all(|k| self.planner.is_fresh(*k))
        {
            self.stale = None;
        }
        for k in self.planner.evict(view, MAX_TILES) {
            self.textures.remove(&k);
        }
    }

    fn paint_pixel_grid(&self, painter: &egui::Painter, rect: Rect, ppp: f32) {
        let z = self.viewport.zoom();
        let vis = self.viewport.visible_canvas_rect();
        if vis.width() <= 0.0 || vis.height() <= 0.0 {
            return;
        }
        let stroke = Stroke::new(1.0 / ppp, Color32::from_white_alpha(46));
        let dark = Stroke::new(1.0 / ppp, Color32::from_black_alpha(46));
        let (x0, x1) = (vis.min.x.floor() as i32, vis.max.x.ceil() as i32);
        let (y0, y1) = (vis.min.y.floor() as i32, vis.max.y.ceil() as i32);
        if (x1 - x0).max(y1 - y0) > 1400 {
            return;
        }
        let top = self.viewport.canvas_to_screen(pos2(0.0, y0 as f32));
        let bot = self.viewport.canvas_to_screen(pos2(0.0, y1 as f32));
        for x in x0..=x1 {
            let sx = self.viewport.canvas_to_screen(pos2(x as f32, 0.0)).x;
            let px = rect.min.x + sx / ppp;
            painter.line_segment(
                [pos2(px, rect.min.y + top.y / ppp), pos2(px, rect.min.y + bot.y / ppp)],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(px + 1.0 / ppp, rect.min.y + top.y / ppp),
                    pos2(px + 1.0 / ppp, rect.min.y + bot.y / ppp),
                ],
                dark,
            );
        }
        let left = self.viewport.canvas_to_screen(pos2(x0 as f32, 0.0));
        let right = self.viewport.canvas_to_screen(pos2(x1 as f32, 0.0));
        for y in y0..=y1 {
            let sy = self.viewport.canvas_to_screen(pos2(0.0, y as f32)).y;
            let py = rect.min.y + sy / ppp;
            painter.line_segment(
                [pos2(rect.min.x + left.x / ppp, py), pos2(rect.min.x + right.x / ppp, py)],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(rect.min.x + left.x / ppp, py + 1.0 / ppp),
                    pos2(rect.min.x + right.x / ppp, py + 1.0 / ppp),
                ],
                dark,
            );
        }
        let _ = z;
    }

    #[allow(clippy::too_many_arguments)] // drawing helper with one caller
    fn paint_overlay(
        &self,
        ui: &Ui,
        painter: &egui::Painter,
        rect: Rect,
        ppp: f32,
        ov: &Overlay,
        doc: &EditorDoc,
        state: &mut AppState,
    ) {
        let d = doc.doc();
        let sp = |p: PointF| self.to_screen(rect, ppp, p, d);
        let halo = Stroke::new(3.0, Color32::from_black_alpha(90));
        let line = Stroke::new(1.4, theme::ACCENT);
        let rotated = |r: RectF, rot: f32, pivot: PointF| -> Vec<Pos2> {
            r.corners().iter().map(|c| sp(c.rotate_about(pivot, rot))).collect()
        };

        // Crop.
        if let Some(c) = &ov.crop {
            self.paint_crop(painter, rect, ppp, c, d, ui);
        }
        if let Some(s) = ov.cut_strip {
            let a = sp(PointF::new(s.x, s.y));
            let b = sp(PointF::new(s.right(), s.bottom()));
            let r = Rect::from_two_pos(a, b);
            painter.rect_filled(r, 0.0, theme::DANGER.gamma_multiply(0.28));
            painter.rect_stroke(r, 0.0, Stroke::new(1.2, theme::DANGER), StrokeKind::Inside);
        }
        // Objects the eraser is about to delete.
        for id in &ov.erase_marks {
            if let Some(o) = d.object(*id) {
                let b = o.bounds();
                let r = Rect::from_two_pos(
                    sp(PointF::new(b.x, b.y)),
                    sp(PointF::new(b.right(), b.bottom())),
                );
                painter.rect_filled(r, 0.0, theme::DANGER.gamma_multiply(0.22));
                painter.rect_stroke(r, 0.0, Stroke::new(1.2, theme::DANGER), StrokeKind::Outside);
            }
        }
        // Selection outlines.
        for o in &ov.selection {
            let pivot = o.rect.center();
            let pts = rotated(o.rect, o.rotation, pivot);
            painter.add(Shape::closed_line(pts.clone(), halo));
            painter.add(Shape::closed_line(pts, line));
        }
        // Handles.
        let north =
            ov.handles.iter().find(|h| h.kind == ssx_editor::HandleKind::North).map(|h| sp(h.pos));
        for h in &ov.handles {
            let p = sp(h.pos);
            match h.kind {
                ssx_editor::HandleKind::Rotate => {
                    if let Some(n) = north {
                        painter.line_segment([n, p], Stroke::new(1.0, theme::ACCENT));
                    }
                    painter.circle(p, 5.0, Color32::WHITE, Stroke::new(1.5, theme::ACCENT));
                }
                ssx_editor::HandleKind::Endpoint(_)
                | ssx_editor::HandleKind::Tail
                | ssx_editor::HandleKind::Source => {
                    painter.circle(p, 5.5, Color32::WHITE, Stroke::new(1.5, theme::ACCENT));
                    painter.circle_filled(p, 2.0, theme::ACCENT);
                }
                _ => {
                    let r = Rect::from_center_size(p, Vec2::splat(8.0));
                    painter.rect(
                        r,
                        1.5,
                        Color32::WHITE,
                        Stroke::new(1.5, theme::ACCENT),
                        StrokeKind::Middle,
                    );
                }
            }
        }
        if let Some(m) = ov.marquee {
            let r = Rect::from_two_pos(
                sp(PointF::new(m.x, m.y)),
                sp(PointF::new(m.right(), m.bottom())),
            );
            painter.rect_filled(r, 0.0, theme::ACCENT.gamma_multiply(0.16));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
        }
        for g in &ov.guides {
            let (a, b) = if g.vertical {
                (sp(PointF::new(g.position, g.from)), sp(PointF::new(g.position, g.to)))
            } else {
                (sp(PointF::new(g.from, g.position)), sp(PointF::new(g.to, g.position)))
            };
            painter.line_segment([a, b], Stroke::new(1.0, theme::GUIDE));
        }
        // Text caret and IME.
        if let Some(c) = &ov.caret {
            for s in &c.selection {
                let pts = rotated(*s, c.rotation, c.pivot);
                painter.add(Shape::convex_polygon(
                    pts,
                    theme::ACCENT.gamma_multiply(0.45),
                    Stroke::NONE,
                ));
            }
            // The caret is solid right after any movement or typing and blinks after that.
            let now = ui.input(|i| i.time);
            let stamp = (c.caret.x.to_bits(), c.caret.y.to_bits(), c.selection.len());
            let key = egui::Id::new("ssx-caret-epoch");
            let epoch = ui.data_mut(|d| {
                let e: &mut (f64, (u32, u32, usize)) = d.get_temp_mut_or_insert_with(key, || (now, stamp));
                if e.1 != stamp {
                    *e = (now, stamp);
                }
                e.0
            });
            let on = (((now - epoch) * 2.0) as i64) % 2 == 0;
            let top = pf(PointF::new(c.caret.x, c.caret.y));
            let bottom = pf(PointF::new(c.caret.x, c.caret.bottom()));
            let pivot = pf(c.pivot);
            let rot = |p: Pos2| {
                let v = p - pivot;
                let (s, co) = c.rotation.sin_cos();
                pivot + vec2(v.x * co - v.y * s, v.x * s + v.y * co)
            };
            let (a, b) = (
                sp(PointF::new(rot(top).x, rot(top).y)),
                sp(PointF::new(rot(bottom).x, rot(bottom).y)),
            );
            if on {
                painter.line_segment([a, b], Stroke::new(3.0, Color32::from_black_alpha(140)));
                painter.line_segment([a, b], Stroke::new(1.5, Color32::WHITE));
            }
            if let Some(pre) = &c.preedit {
                let size = ((b.y - a.y).abs()).max(10.0);
                let galley = painter.layout_no_wrap(
                    pre.clone(),
                    FontId::proportional(size * 0.8),
                    Color32::WHITE,
                );
                let r = Rect::from_min_size(a, galley.size());
                painter.rect_filled(r.expand(1.0), 2.0, Color32::from_black_alpha(190));
                painter.galley(a, galley.clone(), Color32::WHITE);
                painter.line_segment(
                    [r.left_bottom(), r.right_bottom()],
                    Stroke::new(1.5, Color32::WHITE),
                );
            }
            let cursor_rect = Rect::from_two_pos(a, b).expand2(vec2(1.0, 0.0));
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                });
            });
        }
        let _ = state;
    }

    fn paint_crop(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        ppp: f32,
        c: &ssx_editor::input::CropOverlay,
        d: &Document,
        ui: &Ui,
    ) {
        let sp = |p: PointF| self.to_screen(rect, ppp, p, d);
        let r = Rect::from_two_pos(
            sp(PointF::new(c.rect.x, c.rect.y)),
            sp(PointF::new(c.rect.right(), c.rect.bottom())),
        );
        let white = Stroke::new(1.5, Color32::WHITE);
        let black = Stroke::new(3.0, Color32::from_black_alpha(120));
        if !c.ellipse && c.polygon.is_empty() {
            // Dim everything outside the rectangle.
            let dim = Color32::from_black_alpha(130);
            let full = self.to_screen(rect, ppp, PointF::new(-1e5, -1e5), d);
            let _ = full;
            let clip = rect;
            let top = Rect::from_min_max(
                clip.min,
                pos2(clip.max.x, r.min.y.clamp(clip.min.y, clip.max.y)),
            );
            let bottom = Rect::from_min_max(
                pos2(clip.min.x, r.max.y.clamp(clip.min.y, clip.max.y)),
                clip.max,
            );
            let mid_y0 = top.max.y;
            let mid_y1 = bottom.min.y;
            let left = Rect::from_min_max(
                pos2(clip.min.x, mid_y0),
                pos2(r.min.x.clamp(clip.min.x, clip.max.x), mid_y1),
            );
            let right = Rect::from_min_max(
                pos2(r.max.x.clamp(clip.min.x, clip.max.x), mid_y0),
                pos2(clip.max.x, mid_y1),
            );
            for q in [top, bottom, left, right] {
                if q.is_positive() {
                    painter.rect_filled(q, 0.0, dim);
                }
            }
            painter.rect_stroke(r, 0.0, black, StrokeKind::Outside);
            painter.rect_stroke(r, 0.0, white, StrokeKind::Inside);
            let third = Stroke::new(1.0, Color32::from_white_alpha(70));
            for i in 1..3 {
                let x = r.min.x + r.width() * i as f32 / 3.0;
                let y = r.min.y + r.height() * i as f32 / 3.0;
                painter.line_segment([pos2(x, r.min.y), pos2(x, r.max.y)], third);
                painter.line_segment([pos2(r.min.x, y), pos2(r.max.x, y)], third);
            }
        } else if c.ellipse {
            let pts: Vec<Pos2> = (0..64)
                .map(|i| {
                    let a = i as f32 / 64.0 * std::f32::consts::TAU;
                    r.center() + vec2(a.cos() * r.width() / 2.0, a.sin() * r.height() / 2.0)
                })
                .collect();
            painter.add(Shape::convex_polygon(
                pts.clone(),
                theme::ACCENT.gamma_multiply(0.14),
                Stroke::NONE,
            ));
            painter.add(Shape::closed_line(pts.clone(), black));
            painter.add(Shape::closed_line(pts, white));
        } else {
            let pts: Vec<Pos2> = c.polygon.iter().map(|p| sp(*p)).collect();
            if pts.len() > 1 {
                painter.add(Shape::closed_line(pts.clone(), black));
                painter.add(Shape::closed_line(pts, white));
            }
        }
        // Size label.
        let label = format!("{} x {}", c.rect.w.round() as i32, c.rect.h.round() as i32);
        let galley = painter.layout_no_wrap(label, FontId::proportional(11.5), Color32::WHITE);
        let pos = pos2(r.min.x, (r.min.y - galley.size().y - 6.0).max(rect.min.y + 2.0));
        painter.rect_filled(
            Rect::from_min_size(pos - vec2(4.0, 2.0), galley.size() + vec2(8.0, 4.0)),
            4.0,
            Color32::from_black_alpha(190),
        );
        painter.galley(pos, galley, Color32::WHITE);
        let _ = ui;
    }

    fn paint_eyedropper(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        ppp: f32,
        resp: &egui::Response,
        doc: &EditorDoc,
        state: &AppState,
    ) {
        if state.eyedropper.is_none() || !resp.hovered() {
            return;
        }
        let Some(h) = resp.hover_pos() else { return };
        let img = self.to_image(rect, ppp, h, doc.doc());
        let [r, g, b, a] = state.hover.color.unwrap_or([0; 4]);
        let _ = img;
        let box_rect = Rect::from_min_size(h + vec2(16.0, 16.0), vec2(112.0, 34.0));
        painter.rect_filled(box_rect, 6.0, Color32::from_black_alpha(220));
        let sw = Rect::from_min_size(box_rect.min + vec2(6.0, 6.0), vec2(22.0, 22.0));
        painter.rect_filled(sw, 4.0, Color32::from_rgba_unmultiplied(r, g, b, a));
        painter.rect_stroke(sw, 4.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
        painter.text(
            box_rect.min + vec2(36.0, 17.0),
            Align2::LEFT_CENTER,
            format!("#{r:02x}{g:02x}{b:02x}"),
            FontId::monospace(12.5),
            Color32::WHITE,
        );
    }

    /// Draws the floating Apply/Cancel buttons of a pending crop; call after [`Self::show`].
    pub fn crop_buttons(&self, ui: &mut Ui, doc: &EditorDoc, state: &mut AppState) {
        let Some(c) = doc.session.pending_crop() else { return };
        let ppp = ui.ctx().pixels_per_point();
        let br = self.to_screen(
            self.last_rect,
            ppp,
            PointF::new(c.rect.right(), c.rect.bottom()),
            doc.doc(),
        );
        let pos = pos2(
            (br.x - 176.0).clamp(self.last_rect.min.x + 4.0, self.last_rect.max.x - 180.0),
            (br.y + 8.0).clamp(self.last_rect.min.y + 4.0, self.last_rect.max.y - 40.0),
        );
        egui::Area::new(egui::Id::new("ssx-crop-buttons"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if super::widgets::accent_button(
                            ui,
                            "Apply",
                            Some(crate::icons::Icon::Check),
                        )
                        .clicked()
                        {
                            state.push(Action::ApplyPendingCrop);
                        }
                        if ui.button("Cancel").clicked() {
                            state.push(Action::CancelPendingCrop);
                        }
                    });
                });
            });
    }
}

fn ssx_mods(m: impl Into<ModsIn>) -> Modifiers {
    let m: ModsIn = m.into();
    Modifiers { shift: m.0.shift, ctrl: m.0.command, alt: m.0.alt }
}

/// Newtype so `ssx_mods` accepts both `Modifiers` and `&Modifiers`.
struct ModsIn(egui::Modifiers);

impl From<egui::Modifiers> for ModsIn {
    fn from(m: egui::Modifiers) -> Self {
        ModsIn(m)
    }
}

impl From<&egui::Modifiers> for ModsIn {
    fn from(m: &egui::Modifiers) -> Self {
        ModsIn(*m)
    }
}

/// Is the object kind one whose dragging creates a rectangle (used for tests and hints)?
pub fn is_box_kind(k: &ObjectKind) -> bool {
    matches!(k, ObjectKind::Rectangle(_) | ObjectKind::Ellipse(_))
}
