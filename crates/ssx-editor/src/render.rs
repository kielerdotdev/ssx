//! The renderer: `Document` → pixels.
//!
//! # Pipeline
//!
//! 1. Work out the output rectangle (whole canvas or a viewport) in *output pixels*
//!    (`canvas pixels × scale`, origin = top-left of the padded canvas).
//! 2. Grow it to the *work rectangle*: effect objects (blur, pixelate, magnifier) read the
//!    pixels beneath them, so everything they need must be painted even if it lies outside
//!    the viewport. Iterated to a fixed point.
//! 3. Paint into a premultiplied `tiny-skia` pixmap: background, base image, then objects
//!    bottom to top. Objects that need opacity, a shadow or a blend mode are painted into
//!    a private layer and composited; the rest paint directly.
//! 4. Un-premultiply the requested part into an RGBA8 [`Frame`].
//!
//! At scale 1 the base image is copied, not resampled, and everything is deterministic
//! (pure Rust, no system fonts), so exports are identical on every OS up to the
//! rasteriser's own float determinism (golden tests use a tiny tolerance for that reason).

use std::{cell::RefCell, sync::Arc};

use rayon::prelude::*;
use ssx_imgfx::{BlurMethod, Lens, LensShape};
use ssx_types::{Frame, Rect};
use tiny_skia::{
    BlendMode as SkBlend, FillRule, FilterQuality, GradientStop, LineCap, LineJoin, LinearGradient,
    Paint, Path, PathBuilder, Pattern, Pixmap, PixmapPaint, Shader, SpreadMode, Stroke,
    StrokeDash, Transform,
};

use crate::{
    doc::Document,
    geom::{Color, PointF, RectF},
    object::{
        ArrowHeads, BuiltinSticker, EffectBox, GridPattern, HeadStyle, Object, ObjectKind,
        StickerSource, TextContent,
    },
    shapes,
    style::{BlendMode, Fill, Style},
    text::TextEngine,
};

/// How the base image is resampled when the scale is not 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BaseFilter {
    /// Nearest when zoomed in by 2× or more, bilinear otherwise.
    #[default]
    Auto,
    /// Crisp pixels.
    Nearest,
    /// Bilinear.
    Bilinear,
    /// Bicubic.
    Bicubic,
}

/// Options for [`Renderer::render`].
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Output pixels per image pixel (zoom). 1.0 gives the pixel-exact export.
    pub scale: f32,
    /// Only render this part of the (scaled) canvas, in output pixels relative to the
    /// top-left of the padded canvas. The returned frame has exactly this size; parts
    /// outside the canvas are transparent. `None` renders the whole canvas.
    pub viewport: Option<Rect>,
    /// Draw thin dotted outlines around every object's bounds (debug aid; off for exports).
    pub include_guides: bool,
    /// Base-image resampling.
    pub base_filter: BaseFilter,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { scale: 1.0, viewport: None, include_guides: false, base_filter: BaseFilter::Auto }
    }
}

struct BaseCache {
    frame: Arc<Frame>,
    rev: u64,
    pixmap: Arc<Pixmap>,
    opaque: bool,
}

/// Paints documents. Owns the [`TextEngine`] and caches converted images.
pub struct Renderer {
    text: TextEngine,
    base: Option<BaseCache>,
    images: Vec<(Arc<Frame>, Arc<Pixmap>)>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static THREAD_RENDERER: RefCell<Renderer> = RefCell::new(Renderer::new());
}

/// Renders with a per-thread [`Renderer`] (convenient for one-off exports).
pub fn render(doc: &Document, opts: &RenderOptions) -> Frame {
    THREAD_RENDERER.with(|r| r.borrow_mut().render(doc, opts))
}

/// Runs `f` with the per-thread text engine (for measuring text without a session).
pub fn with_text_engine<R>(f: impl FnOnce(&mut TextEngine) -> R) -> R {
    THREAD_RENDERER.with(|r| f(&mut r.borrow_mut().text))
}

struct Ctx {
    scale: f32,
    work: Rect,
    /// Canvas offset of the image origin in output pixels (padding × scale).
    off: (f32, f32),
    view: Transform,
}

impl Ctx {
    /// Image-space rectangle → work-pixmap pixels, rounded outward.
    fn dev(&self, r: RectF) -> Rect {
        RectF::new(
            r.x * self.scale + self.off.0 - self.work.x as f32,
            r.y * self.scale + self.off.1 - self.work.y as f32,
            r.w * self.scale,
            r.h * self.scale,
        )
        .to_outer_rect()
    }

    fn pt(&self, p: PointF) -> PointF {
        PointF::new(
            p.x * self.scale + self.off.0 - self.work.x as f32,
            p.y * self.scale + self.off.1 - self.work.y as f32,
        )
    }
}

fn solid(color: Color) -> Paint<'static> {
    Paint {
        shader: Shader::SolidColor(color.to_skia()),
        blend_mode: SkBlend::SourceOver,
        anti_alias: true,
        force_hq_pipeline: false,
    }
}

fn fill_paint(fill: &Fill, bounds: RectF) -> Option<Paint<'static>> {
    match *fill {
        Fill::None => None,
        Fill::Solid { color } => (!color.is_transparent()).then(|| solid(color)),
        Fill::Gradient { from, to, angle } => {
            let c = bounds.center();
            let a = angle.to_radians();
            let (dx, dy) = (a.cos(), a.sin());
            let half = (bounds.w * dx.abs() + bounds.h * dy.abs()) / 2.0;
            let shader = LinearGradient::new(
                tiny_skia::Point::from_xy(c.x - dx * half, c.y - dy * half),
                tiny_skia::Point::from_xy(c.x + dx * half, c.y + dy * half),
                vec![GradientStop::new(0.0, from.to_skia()), GradientStop::new(1.0, to.to_skia())],
                SpreadMode::Pad,
                Transform::identity(),
            )?;
            Some(Paint {
                shader,
                blend_mode: SkBlend::SourceOver,
                anti_alias: true,
                force_hq_pipeline: false,
            })
        }
    }
}

fn stroke_of(style: &Style) -> Stroke {
    let sw = style.stroke_width.max(0.0);
    let dash = style
        .dash
        .pattern()
        .and_then(|p| StrokeDash::new(p.iter().map(|v| (v * sw).max(0.01)).collect(), 0.0));
    Stroke {
        width: sw,
        miter_limit: 4.0,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        dash,
    }
}

fn sk_blend(b: BlendMode) -> SkBlend {
    match b {
        BlendMode::Normal => SkBlend::SourceOver,
        BlendMode::Multiply => SkBlend::Multiply,
        BlendMode::Screen => SkBlend::Screen,
        BlendMode::Overlay => SkBlend::Overlay,
        BlendMode::Darken => SkBlend::Darken,
        BlendMode::Lighten => SkBlend::Lighten,
    }
}

fn premul(c: u8, a: u8) -> u8 {
    ((u32::from(c) * u32::from(a) + 127) / 255) as u8
}

fn unpremul(c: u8, a: u8) -> u8 {
    if a == 0 || a == 255 { c } else { ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8 }
}

fn frame_to_pixmap(f: &Frame) -> Option<Pixmap> {
    let size = tiny_skia::IntSize::from_wh(f.width(), f.height())?;
    let w4 = f.width() as usize * 4;
    let mut data = vec![0u8; w4 * f.height() as usize];
    data.par_chunks_mut(w4).enumerate().for_each(|(y, row)| {
        for (d, s) in row.chunks_exact_mut(4).zip(f.row(y as u32).chunks_exact(4)) {
            let a = s[3];
            d[0] = premul(s[0], a);
            d[1] = premul(s[1], a);
            d[2] = premul(s[2], a);
            d[3] = a;
        }
    });
    Pixmap::from_vec(data, size)
}

/// Copies `r` (inside the pixmap) out as a straight-alpha frame.
fn pixmap_region_to_frame(pm: &Pixmap, r: Rect) -> Frame {
    let w4 = r.width as usize * 4;
    let mut data = vec![0u8; w4 * r.height as usize];
    let stride = pm.width() as usize * 4;
    let src = pm.data();
    if w4 > 0 {
        data.par_chunks_mut(w4).enumerate().for_each(|(y, row)| {
            let o = (r.y as usize + y) * stride + r.x as usize * 4;
            for (d, s) in row.chunks_exact_mut(4).zip(src[o..o + w4].chunks_exact(4)) {
                let a = s[3];
                d[0] = unpremul(s[0], a);
                d[1] = unpremul(s[1], a);
                d[2] = unpremul(s[2], a);
                d[3] = a;
            }
        });
    }
    Frame::from_rgba8(r.width, r.height, data).expect("exact-size buffer")
}

/// Writes a straight-alpha frame back into the pixmap at `r`.
fn frame_to_pixmap_region(pm: &mut Pixmap, r: Rect, f: &Frame) {
    let stride = pm.width() as usize * 4;
    let w4 = r.width as usize * 4;
    if w4 == 0 {
        return;
    }
    let dst = pm.data_mut();
    for y in 0..r.height as usize {
        let o = (r.y as usize + y) * stride + r.x as usize * 4;
        for (d, s) in dst[o..o + w4].chunks_exact_mut(4).zip(f.row(y as u32).chunks_exact(4)) {
            let a = s[3];
            d[0] = premul(s[0], a);
            d[1] = premul(s[1], a);
            d[2] = premul(s[2], a);
            d[3] = a;
        }
    }
}

fn pm_bounds(pm: &Pixmap) -> Rect {
    Rect::new(0, 0, pm.width(), pm.height())
}

impl Renderer {
    /// Creates a renderer with a fresh text engine.
    pub fn new() -> Self {
        Self { text: TextEngine::new(), base: None, images: Vec::new() }
    }

    /// The text engine (for layout queries that must match what is painted).
    pub fn text_mut(&mut self) -> &mut TextEngine {
        &mut self.text
    }

    /// Renders the document. See [`RenderOptions`].
    pub fn render(&mut self, doc: &Document, opts: &RenderOptions) -> Frame {
        let scale = if opts.scale.is_finite() && opts.scale > 0.0 { opts.scale.min(64.0) } else { 1.0 };
        let (cw, ch) = doc.canvas_size();
        let fw = if cw == 0 { 0 } else { ((cw as f32 * scale).round() as u32).max(1) };
        let fh = if ch == 0 { 0 } else { ((ch as f32 * scale).round() as u32).max(1) };
        let full = Rect::new(0, 0, fw, fh);
        let out = opts.viewport.unwrap_or(full);
        let mut result = ssx_imgfx::solid_frame(out.width, out.height, [0; 4]);
        let Some(vis) = out.intersect(full) else { return result };
        let work = self.work_rect(doc, scale, vis, full);
        let Some(pm) = self.paint(doc, scale, work, opts) else { return result };
        // Copy the visible part into the result.
        let inner = Rect::new(vis.x - work.x, vis.y - work.y, vis.width, vis.height);
        let frame = pixmap_region_to_frame(&pm, inner);
        let (dx, dy) = ((vis.x - out.x) as usize * 4, (vis.y - out.y) as usize);
        let w4 = vis.width as usize * 4;
        let out_stride = result.stride();
        result
            .data_mut()
            .par_chunks_mut(out_stride)
            .enumerate()
            .skip(dy)
            .take(vis.height as usize)
            .for_each(|(y, row)| {
                row[dx..dx + w4].copy_from_slice(&frame.row((y - dy) as u32)[..w4]);
            });
        result
    }

    /// The rectangle (output pixels) that must be painted to render `vis` correctly.
    fn work_rect(&self, doc: &Document, scale: f32, vis: Rect, full: Rect) -> Rect {
        let (ox, oy) = doc.canvas_offset();
        let dev = |r: RectF| -> Rect {
            RectF::new((r.x + ox as f32) * scale, (r.y + oy as f32) * scale, r.w * scale, r.h * scale)
                .to_outer_rect()
        };
        let mut work = vis;
        for _ in 0..8 {
            let mut next = work;
            for o in doc.objects().iter().filter(|o| o.visible) {
                let needs: Vec<RectF> = match &o.kind {
                    ObjectKind::Blur(_) | ObjectKind::Pixelate(_) => vec![o.bounds().inflate(1.0)],
                    ObjectKind::Magnify(m) => {
                        vec![o.bounds().inflate(1.0), m.source_rect().inflate(2.0)]
                    }
                    _ => continue,
                };
                if dev(needs[0]).intersect(work).is_some() {
                    for n in needs {
                        next = next.union(dev(n));
                    }
                }
            }
            let next = next.intersect(full).unwrap_or(work);
            if next == work {
                break;
            }
            work = next;
        }
        work
    }

    fn base_pixmap(&mut self, doc: &Document) -> Option<(Arc<Pixmap>, bool)> {
        if let Some(c) = &self.base {
            if Arc::ptr_eq(&c.frame, doc.base()) && c.rev == doc.base_revision() {
                return Some((c.pixmap.clone(), c.opaque));
            }
        }
        let f = doc.base();
        let pm = Arc::new(frame_to_pixmap(f)?);
        let opaque = (0..f.height()).into_par_iter().all(|y| f.row(y).chunks_exact(4).all(|p| p[3] == 255));
        self.base = Some(BaseCache {
            frame: f.clone(),
            rev: doc.base_revision(),
            pixmap: pm.clone(),
            opaque,
        });
        Some((pm, opaque))
    }

    fn image_pixmap(&mut self, f: &Arc<Frame>) -> Option<Arc<Pixmap>> {
        if let Some((_, p)) = self.images.iter().find(|(k, _)| Arc::ptr_eq(k, f)) {
            return Some(p.clone());
        }
        let p = Arc::new(frame_to_pixmap(f)?);
        if self.images.len() >= 32 {
            self.images.remove(0);
        }
        self.images.push((f.clone(), p.clone()));
        Some(p)
    }

    fn paint(&mut self, doc: &Document, scale: f32, work: Rect, opts: &RenderOptions) -> Option<Pixmap> {
        let mut pm = Pixmap::new(work.width, work.height)?;
        let (ox, oy) = doc.canvas_offset();
        let off = (ox as f32 * scale, oy as f32 * scale);
        let ctx = Ctx {
            scale,
            work,
            off,
            view: Transform::from_row(scale, 0.0, 0.0, scale, off.0 - work.x as f32, off.1 - work.y as f32),
        };
        self.draw_base(doc, &ctx, &mut pm, opts);
        let mut spotlights_done = false;
        for o in doc.objects().iter().filter(|o| o.visible) {
            self.draw_object(doc, &ctx, &mut pm, o, &mut spotlights_done);
        }
        if opts.include_guides {
            for o in doc.objects().iter().filter(|o| o.visible) {
                draw_guide(&ctx, &mut pm, o.bounds());
            }
        }
        Some(pm)
    }

    fn draw_base(&mut self, doc: &Document, ctx: &Ctx, pm: &mut Pixmap, opts: &RenderOptions) {
        let (cw, ch) = doc.canvas_size();
        let s = ctx.scale;
        // Canvas background over the whole canvas.
        let canvas = RectF::new(-(ctx.work.x as f32), -(ctx.work.y as f32), cw as f32 * s, ch as f32 * s);
        if let (Some(paint), Some(r)) =
            (fill_paint(&doc.canvas().background, canvas), shapes::sk_rect(canvas))
        {
            let mut paint = paint;
            paint.anti_alias = false;
            fill_rect_clamped(pm, r, &paint);
        }
        let Some((base, opaque)) = self.base_pixmap(doc) else { return };
        let (w, h) = doc.image_size();
        // Fast path: pixel-exact 1× export of an opaque, unpadded image.
        if s == 1.0
            && opaque
            && ctx.work == Rect::new(0, 0, w, h)
            && doc.canvas().padding == crate::doc::Padding::default()
        {
            pm.data_mut().copy_from_slice(base.data());
            return;
        }
        let quality = match opts.base_filter {
            BaseFilter::Nearest => FilterQuality::Nearest,
            BaseFilter::Bilinear => FilterQuality::Bilinear,
            BaseFilter::Bicubic => FilterQuality::Bicubic,
            BaseFilter::Auto => {
                if s == 1.0 || s >= 2.0 {
                    FilterQuality::Nearest
                } else {
                    FilterQuality::Bilinear
                }
            }
        };
        let rect = RectF::new(ctx.off.0 - ctx.work.x as f32, ctx.off.1 - ctx.work.y as f32, w as f32 * s, h as f32 * s);
        let Some(r) = shapes::sk_rect(rect) else { return };
        let paint = Paint {
            shader: Pattern::new(
                Pixmap::as_ref(&base),
                SpreadMode::Pad,
                quality,
                1.0,
                Transform::from_row(s, 0.0, 0.0, s, rect.x, rect.y),
            ),
            blend_mode: SkBlend::SourceOver,
            anti_alias: false,
            force_hq_pipeline: false,
        };
        fill_rect_clamped(pm, r, &paint);
    }

    fn draw_object(
        &mut self,
        doc: &Document,
        ctx: &Ctx,
        pm: &mut Pixmap,
        o: &Object,
        spotlights_done: &mut bool,
    ) {
        match &o.kind {
            ObjectKind::Blur(e) => return self.effect_blur(ctx, pm, e),
            ObjectKind::Pixelate(e) => return self.effect_pixelate(ctx, pm, e),
            ObjectKind::Magnify(_) => return self.effect_magnify(ctx, pm, o),
            ObjectKind::Spotlight(_) => {
                if !*spotlights_done {
                    *spotlights_done = true;
                    self.effect_spotlights(doc, ctx, pm);
                }
                return;
            }
            ObjectKind::Unknown(_) => return,
            _ => {}
        }
        let st = &o.style;
        let layered = st.opacity < 1.0 || st.shadow.is_some() || st.blend != BlendMode::Normal;
        if !layered {
            self.paint_content(doc, ctx.view, pm, o);
            return;
        }
        let Some(bbox) = ctx.dev(o.render_bounds()).intersect(pm_bounds(pm)) else { return };
        let Some(mut layer) = Pixmap::new(bbox.width, bbox.height) else { return };
        let tf = ctx.view.post_translate(-(bbox.x as f32), -(bbox.y as f32));
        self.paint_content(doc, tf, &mut layer, o);
        composite_layer(pm, &layer, bbox, st, ctx.scale);
    }

    /// Paints one object's own pixels (no opacity/shadow/blend handling).
    fn paint_content(&mut self, doc: &Document, tf: Transform, pm: &mut Pixmap, o: &Object) {
        let st = &o.style;
        match &o.kind {
            ObjectKind::Rectangle(b) => {
                let t = rot_transform(tf, b.rect, b.rotation);
                if let Some(p) = shapes::rounded_rect(b.rect, st.corner_radius) {
                    fill_and_stroke(pm, &p, t, st, b.rect);
                }
            }
            ObjectKind::Ellipse(b) => {
                let t = rot_transform(tf, b.rect, b.rotation);
                if let Some(p) = shapes::ellipse(b.rect) {
                    fill_and_stroke(pm, &p, t, st, b.rect);
                }
            }
            ObjectKind::Line(l) => {
                let mut pb = PathBuilder::new();
                pb.move_to(l.a.x, l.a.y);
                pb.line_to(l.b.x, l.b.y);
                if let Some(p) = pb.finish() {
                    stroke_only(pm, &p, tf, st, st.stroke);
                }
                if l.a == l.b {
                    dot(pm, l.a, st, tf);
                }
            }
            ObjectKind::Arrow(a) => draw_arrow(pm, tf, st, a.a, a.b, &a.heads),
            ObjectKind::Freehand(f) => draw_freehand(pm, tf, st, &f.points, f.smooth, f.arrow.as_ref()),
            ObjectKind::Text(t) => {
                let c = &t.content;
                let pad = c.padding.max(0.0);
                let wrap = (!t.auto_width).then(|| (t.rect.w - 2.0 * pad).max(1.0));
                let layout = self.text.layout_content(c, wrap);
                let box_w = wrap.unwrap_or(layout.width) + 2.0 * pad;
                let box_h = layout.height + 2.0 * pad;
                let rect = RectF::new(t.rect.x, t.rect.y, box_w, box_h);
                let base = rot_transform(tf, t.rect, t.rotation);
                self.draw_text_box(pm, base, rect, st, c, &layout);
            }
            ObjectKind::Balloon(b) => {
                if let Some(p) = shapes::balloon_outline(b, st.corner_radius) {
                    fill_and_stroke(pm, &p, tf, st, b.rect.union(&RectF::new(b.tail.x, b.tail.y, 0.0, 0.0)));
                }
                let c = &b.content;
                let pad = c.padding.max(0.0);
                let wrap = (b.rect.w - 2.0 * pad).max(1.0);
                let layout = self.text.layout_content(c, Some(wrap));
                let origin = crate::text::balloon_text_origin(b, layout.height);
                self.draw_glyphs(pm, tf, origin, c, &layout);
            }
            ObjectKind::Step(s) => {
                let r = RectF::from_center_size(s.center, s.diameter, s.diameter);
                if let Some(p) = shapes::ellipse(r) {
                    fill_and_stroke(pm, &p, tf, st, r);
                }
                let n = doc.step_number(o.id).unwrap_or(1);
                let content = TextContent {
                    text: n.to_string(),
                    font: crate::object::FontSpec {
                        size: s.diameter * 0.52,
                        bold: true,
                        ..Default::default()
                    },
                    color: s.text_color,
                    ..TextContent::default()
                };
                let layout = self.text.layout_content(&content, None);
                let origin = PointF::new(
                    s.center.x - layout.width / 2.0,
                    s.center.y - layout.height / 2.0,
                );
                self.draw_glyphs(pm, tf, origin, &content, &layout);
            }
            ObjectKind::Highlight(h) => {
                let colour = st.solid_fill().unwrap_or(st.stroke);
                if h.points.is_empty() {
                    if let Some(r) = shapes::sk_rect(h.rect) {
                        pm.fill_rect(r, &solid(colour), tf, None);
                    }
                } else if let Some(p) = shapes::freehand_path(&h.points, true) {
                    let stroke = Stroke {
                        width: st.stroke_width.max(1.0),
                        line_cap: LineCap::Square,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    };
                    pm.stroke_path(&p, &solid(colour), &stroke, tf, None);
                }
            }
            ObjectKind::Image(i) => {
                if let Some(img) = self.image_pixmap(&i.image.0) {
                    draw_bitmap(pm, tf, i.rect, i.rotation, &img);
                }
            }
            ObjectKind::Sticker(s) => match &s.source {
                StickerSource::Builtin { which } => draw_builtin(pm, tf, st, s.rect, s.rotation, *which),
                StickerSource::Bitmap { image } => {
                    if let Some(img) = self.image_pixmap(&image.0) {
                        draw_bitmap(pm, tf, s.rect, s.rotation, &img);
                    }
                }
                StickerSource::Glyph { text } => {
                    let colour = st.solid_fill().unwrap_or(st.stroke);
                    let content = TextContent {
                        text: text.clone(),
                        font: crate::object::FontSpec {
                            size: s.rect.w.min(s.rect.h).max(1.0) * 0.8,
                            ..Default::default()
                        },
                        color: colour,
                        ..TextContent::default()
                    };
                    let layout = self.text.layout_content(&content, None);
                    let t = rot_transform(tf, s.rect, s.rotation);
                    let origin = PointF::new(
                        s.rect.x + (s.rect.w - layout.width) / 2.0,
                        s.rect.y + (s.rect.h - layout.height) / 2.0,
                    );
                    self.draw_glyphs(pm, t, origin, &content, &layout);
                }
            },
            ObjectKind::Cursor(c) => {
                let size = crate::object::CursorShape::BASE_SIZE * c.scale;
                let (fill, stroke) = shapes::cursor_paths(c.kind, c.pos, size);
                if let (Some(p), Some(colour)) = (fill, st.solid_fill()) {
                    pm.fill_path(&p, &solid(colour), FillRule::Winding, tf, None);
                }
                if let Some(p) = stroke {
                    let mut s2 = st.clone();
                    s2.stroke_width = st.stroke_width.max(1.0) * c.scale.max(0.25);
                    stroke_only(pm, &p, tf, &s2, st.stroke);
                }
            }
            ObjectKind::Grid(g) => draw_grid(pm, tf, st, g.rect, g.pattern, g.spacing),
            _ => {}
        }
    }

    fn draw_text_box(
        &mut self,
        pm: &mut Pixmap,
        tf: Transform,
        rect: RectF,
        st: &Style,
        c: &TextContent,
        layout: &crate::text::TextLayout,
    ) {
        if let Some(bg) = c.background.filter(|b| !b.is_transparent()) {
            if let Some(p) = shapes::rounded_rect(rect, st.corner_radius) {
                pm.fill_path(&p, &solid(bg), FillRule::Winding, tf, None);
            }
        } else if let Some(paint) = fill_paint(&st.fill, rect) {
            if let Some(p) = shapes::rounded_rect(rect, st.corner_radius) {
                pm.fill_path(&p, &paint, FillRule::Winding, tf, None);
            }
        }
        if st.has_stroke() {
            if let Some(p) = shapes::rounded_rect(rect, st.corner_radius) {
                stroke_only(pm, &p, tf, st, st.stroke);
            }
        }
        let pad = c.padding.max(0.0);
        self.draw_glyphs(pm, tf, PointF::new(rect.x + pad, rect.y + pad), c, layout);
    }

    fn draw_glyphs(
        &mut self,
        pm: &mut Pixmap,
        tf: Transform,
        origin: PointF,
        c: &TextContent,
        layout: &crate::text::TextLayout,
    ) {
        let Some(path) = self.text.text_path(layout) else { return };
        let t = tf.pre_translate(origin.x, origin.y);
        if let Some(o) = c.outline.filter(|o| o.width > 0.0 && !o.color.is_transparent()) {
            let stroke = Stroke {
                width: o.width * 2.0,
                line_join: LineJoin::Round,
                line_cap: LineCap::Round,
                ..Stroke::default()
            };
            pm.stroke_path(&path, &solid(o.color), &stroke, t, None);
        }
        if !c.color.is_transparent() {
            pm.fill_path(&path, &solid(c.color), FillRule::Winding, t, None);
        }
    }

    // -------------------------------------------------------------------------------
    // Backdrop effects
    // -------------------------------------------------------------------------------

    fn effect_blur(&mut self, ctx: &Ctx, pm: &mut Pixmap, e: &EffectBox) {
        let Some(r) = ctx.dev(e.rect.normalized()).intersect(pm_bounds(pm)) else { return };
        let sigma = e.amount * ctx.scale;
        if sigma <= 0.0 {
            return;
        }
        let stride = pm.width() as usize * 4;
        let w4 = r.width as usize * 4;
        let mut buf = vec![0u8; w4 * r.height as usize];
        for y in 0..r.height as usize {
            let o = (r.y as usize + y) * stride + r.x as usize * 4;
            buf[y * w4..(y + 1) * w4].copy_from_slice(&pm.data()[o..o + w4]);
        }
        ssx_imgfx::gaussian_blur_premultiplied(
            &mut buf,
            r.width as usize,
            r.height as usize,
            sigma,
            BlurMethod::Auto,
        );
        for y in 0..r.height as usize {
            let o = (r.y as usize + y) * stride + r.x as usize * 4;
            pm.data_mut()[o..o + w4].copy_from_slice(&buf[y * w4..(y + 1) * w4]);
        }
    }

    fn effect_pixelate(&mut self, ctx: &Ctx, pm: &mut Pixmap, e: &EffectBox) {
        let Some(r) = ctx.dev(e.rect.normalized()).intersect(pm_bounds(pm)) else { return };
        let block = (e.amount * ctx.scale).round().max(1.0) as u32;
        let mut f = pixmap_region_to_frame(pm, r);
        if ssx_imgfx::pixelate(&mut f, None, block).is_ok() {
            frame_to_pixmap_region(pm, r, &f);
        }
    }

    fn effect_magnify(&mut self, ctx: &Ctx, pm: &mut Pixmap, o: &Object) {
        let ObjectKind::Magnify(m) = &o.kind else { return };
        let st = &o.style;
        let lens_dr = ctx.dev(m.rect.normalized());
        let bounds = pm_bounds(pm);
        let shape_rect = m.rect.normalized();
        // Shadow of the lens silhouette, beneath the lens.
        if let Some(sh) = st.shadow {
            if let Some(bbox) = ctx.dev(o.render_bounds()).intersect(bounds) {
                if let Some(mut layer) = Pixmap::new(bbox.width, bbox.height) {
                    let tf = ctx.view.post_translate(-(bbox.x as f32), -(bbox.y as f32));
                    let p = if m.circular {
                        shapes::ellipse(shape_rect)
                    } else {
                        shapes::rounded_rect(shape_rect, 0.0)
                    };
                    if let Some(p) = p {
                        layer.fill_path(&p, &solid(Color::BLACK), FillRule::Winding, tf, None);
                    }
                    draw_shadow(pm, &layer, bbox, &sh, ctx.scale, st.opacity);
                }
            }
        }
        let (Some(dst_r), Some(src_r)) = (
            lens_dr.intersect(bounds),
            ctx.dev(m.source_rect().inflate(2.0)).intersect(bounds),
        ) else {
            return;
        };
        let src = pixmap_region_to_frame(pm, src_r);
        let mut dst = pixmap_region_to_frame(pm, dst_r);
        let c = ctx.pt(m.source);
        let lens = Lens {
            dst: Rect::new(lens_dr.x - dst_r.x, lens_dr.y - dst_r.y, lens_dr.width, lens_dr.height),
            shape: if m.circular { LensShape::Ellipse } else { LensShape::Rect },
            source_center: ssx_imgfx::PointF::new(c.x - src_r.x as f32, c.y - src_r.y as f32),
            zoom: m.zoom,
        };
        if ssx_imgfx::magnify(&src, &mut dst, &lens).is_ok() {
            frame_to_pixmap_region(pm, dst_r, &dst);
        }
        // Border.
        if st.has_stroke() {
            let p = if m.circular {
                shapes::ellipse(shape_rect)
            } else {
                shapes::rounded_rect(shape_rect, 0.0)
            };
            if let Some(p) = p {
                stroke_only(pm, &p, ctx.view, st, st.stroke);
            }
        }
    }

    /// All visible spotlights together: dim everything except the union of their shapes.
    fn effect_spotlights(&mut self, doc: &Document, ctx: &Ctx, pm: &mut Pixmap) {
        let spots: Vec<&crate::object::SpotlightShape> = doc
            .objects()
            .iter()
            .filter(|o| o.visible)
            .filter_map(|o| match &o.kind {
                ObjectKind::Spotlight(s) => Some(s),
                _ => None,
            })
            .collect();
        let Some(first) = spots.first() else { return };
        let Some(mut mask) = Pixmap::new(pm.width(), pm.height()) else { return };
        for s in &spots {
            let p = if s.ellipse { shapes::ellipse(s.rect) } else { shapes::rounded_rect(s.rect, 0.0) };
            if let Some(p) = p {
                mask.fill_path(&p, &solid(Color::WHITE), FillRule::Winding, ctx.view, None);
            }
        }
        let feather = first.feather * ctx.scale;
        if feather > 0.0 {
            let (w, h) = (mask.width() as usize, mask.height() as usize);
            ssx_imgfx::gaussian_blur_premultiplied(mask.data_mut(), w, h, feather, BlurMethod::Auto);
        }
        let dim = first.dim;
        let da = f32::from(dim.a) / 255.0;
        let dc = [f32::from(dim.r), f32::from(dim.g), f32::from(dim.b)];
        pm.data_mut().par_chunks_mut(4096).zip(mask.data().par_chunks(4096)).for_each(|(px, mk)| {
            for (p, m) in px.chunks_exact_mut(4).zip(mk.chunks_exact(4)) {
                let k = da * (1.0 - f32::from(m[3]) / 255.0);
                if k <= 0.0 {
                    continue;
                }
                for c in 0..3 {
                    p[c] = (f32::from(p[c]) * (1.0 - k) + dc[c] * k).round().clamp(0.0, 255.0) as u8;
                }
                p[3] = (f32::from(p[3]) * (1.0 - k) + 255.0 * k).round().clamp(0.0, 255.0) as u8;
            }
        });
    }
}

/// `fill_rect` with the rectangle clipped to the pixmap first. tiny-skia's non-AA rectangle
/// fill covers one column too many when the rectangle starts left of the pixmap (negative x),
/// which would smear the base image's edge pixel into the padding in viewport renders.
fn fill_rect_clamped(pm: &mut Pixmap, r: tiny_skia::Rect, paint: &Paint) {
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    let (l, t, rt, b) = (r.left().max(0.0), r.top().max(0.0), r.right().min(w), r.bottom().min(h));
    if let Some(c) = tiny_skia::Rect::from_ltrb(l, t, rt, b) {
        pm.fill_rect(c, paint, Transform::identity(), None);
    }
}

fn rot_transform(tf: Transform, rect: RectF, rotation: f32) -> Transform {
    if rotation == 0.0 {
        tf
    } else {
        let c = rect.center();
        tf.pre_concat(Transform::from_rotate_at(rotation.to_degrees(), c.x, c.y))
    }
}

fn fill_and_stroke(pm: &mut Pixmap, path: &Path, tf: Transform, st: &Style, bounds: RectF) {
    if let Some(paint) = fill_paint(&st.fill, bounds) {
        pm.fill_path(path, &paint, FillRule::Winding, tf, None);
    }
    if st.has_stroke() {
        stroke_only(pm, path, tf, st, st.stroke);
    }
}

fn stroke_only(pm: &mut Pixmap, path: &Path, tf: Transform, st: &Style, colour: Color) {
    if st.stroke_width <= 0.0 || colour.is_transparent() {
        return;
    }
    pm.stroke_path(path, &solid(colour), &stroke_of(st), tf, None);
}

fn dot(pm: &mut Pixmap, p: PointF, st: &Style, tf: Transform) {
    if let Some(c) = PathBuilder::from_circle(p.x, p.y, (st.stroke_width / 2.0).max(0.5)) {
        pm.fill_path(&c, &solid(st.stroke), FillRule::Winding, tf, None);
    }
}

fn draw_arrow(pm: &mut Pixmap, tf: Transform, st: &Style, a: PointF, b: PointF, h: &ArrowHeads) {
    let len = h.length(st.stroke_width);
    let end = shapes::head_geometry(h.end, b, a, len, h.angle);
    let start = shapes::head_geometry(h.start, a, b, len, h.angle);
    // Shorten the shaft so it does not poke through filled heads.
    let (sa, sb) = (start.shaft_end, end.shaft_end);
    if sa.distance(sb) > 1e-3 && (sb - sa).dot(b - a) > 0.0 {
        let mut pb = PathBuilder::new();
        pb.move_to(sa.x, sa.y);
        pb.line_to(sb.x, sb.y);
        if let Some(p) = pb.finish() {
            stroke_only(pm, &p, tf, st, st.stroke);
        }
    } else if a.distance(b) > 1e-3 && h.end == HeadStyle::None && h.start == HeadStyle::None {
        dot(pm, a, st, tf);
    }
    draw_heads(pm, tf, st, &[start, end]);
}

fn draw_heads(pm: &mut Pixmap, tf: Transform, st: &Style, heads: &[shapes::HeadGeom]) {
    for g in heads {
        if let Some(p) = &g.fill {
            pm.fill_path(p, &solid(st.stroke), FillRule::Winding, tf, None);
        }
        if let Some(p) = &g.stroke {
            let mut s2 = st.clone();
            s2.dash = crate::style::DashStyle::Solid;
            stroke_only(pm, p, tf, &s2, st.stroke);
        }
    }
}

fn draw_freehand(
    pm: &mut Pixmap,
    tf: Transform,
    st: &Style,
    pts: &[PointF],
    smooth: bool,
    arrow: Option<&ArrowHeads>,
) {
    let Some(first) = pts.first() else { return };
    let Some(path) = shapes::freehand_path(pts, smooth) else {
        dot(pm, *first, st, tf);
        return;
    };
    stroke_only(pm, &path, tf, st, st.stroke);
    if let Some(h) = arrow {
        let len = h.length(st.stroke_width);
        let mut geoms = Vec::new();
        for (style, at_end) in [(h.end, true), (h.start, false)] {
            let tip = if at_end { pts[pts.len() - 1] } else { pts[0] };
            if let Some(r) = shapes::direction_ref(pts, at_end, len * 0.6) {
                geoms.push(shapes::head_geometry(style, tip, r, len, h.angle));
            }
        }
        draw_heads(pm, tf, st, &geoms);
    }
}

fn draw_bitmap(pm: &mut Pixmap, tf: Transform, rect: RectF, rotation: f32, img: &Pixmap) {
    let r = rect.normalized();
    if r.is_empty() {
        return;
    }
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let t = rot_transform(tf, r, rotation)
        .pre_translate(r.x, r.y)
        .pre_scale(r.w / iw, r.h / ih);
    let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, iw, ih) else { return };
    let paint = Paint {
        shader: Pattern::new(img.as_ref(), SpreadMode::Pad, FilterQuality::Bicubic, 1.0, Transform::identity()),
        blend_mode: SkBlend::SourceOver,
        anti_alias: true,
        force_hq_pipeline: false,
    };
    pm.fill_path(&PathBuilder::from_rect(rect), &paint, FillRule::Winding, t, None);
}

fn draw_builtin(
    pm: &mut Pixmap,
    tf: Transform,
    st: &Style,
    rect: RectF,
    rotation: f32,
    which: BuiltinSticker,
) {
    let r = rect.normalized();
    let Some((path, rule)) = shapes::builtin_sticker(which) else { return };
    let colour = st.solid_fill().unwrap_or(st.stroke);
    let t = rot_transform(tf, r, rotation).pre_translate(r.x, r.y).pre_scale(r.w, r.h);
    pm.fill_path(&path, &solid(colour), rule, t, None);
}

fn draw_grid(pm: &mut Pixmap, tf: Transform, st: &Style, rect: RectF, pat: GridPattern, spacing: f32) {
    if let (Some(paint), Some(r)) = (fill_paint(&st.fill, rect), shapes::sk_rect(rect)) {
        pm.fill_rect(r, &paint, tf, None);
    }
    if !st.has_stroke() {
        return;
    }
    if pat == GridPattern::Dots {
        let mut pb = PathBuilder::new();
        let r = (st.stroke_width / 2.0).max(0.5);
        for p in shapes::grid_dots(rect, spacing) {
            pb.push_circle(p.x, p.y, r);
        }
        if let Some(p) = pb.finish() {
            pm.fill_path(&p, &solid(st.stroke), FillRule::Winding, tf, None);
        }
        return;
    }
    let mut pb = PathBuilder::new();
    for (a, b) in shapes::grid_segments(rect, pat, spacing) {
        pb.move_to(a.x, a.y);
        pb.line_to(b.x, b.y);
    }
    if let Some(p) = pb.finish() {
        let mut s2 = st.clone();
        s2.dash = crate::style::DashStyle::Solid;
        let stroke = Stroke { line_cap: LineCap::Butt, ..stroke_of(&s2) };
        pm.stroke_path(&p, &solid(st.stroke), &stroke, tf, None);
    }
}

fn draw_guide(ctx: &Ctx, pm: &mut Pixmap, bounds: RectF) {
    let Some(p) = shapes::rounded_rect(bounds, 0.0) else { return };
    let stroke = Stroke {
        width: 1.0,
        dash: StrokeDash::new(vec![3.0, 3.0], 0.0),
        ..Stroke::default()
    };
    pm.stroke_path(&p, &solid(Color::rgb(0, 160, 255)), &stroke, ctx.view, None);
}

/// Composites a private object layer onto the canvas with the object's opacity, blend mode
/// and (beneath it) its soft shadow.
fn composite_layer(pm: &mut Pixmap, layer: &Pixmap, bbox: Rect, st: &Style, scale: f32) {
    if let Some(sh) = st.shadow {
        draw_shadow(pm, layer, bbox, &sh, scale, st.opacity);
    }
    let paint = PixmapPaint {
        opacity: st.opacity.clamp(0.0, 1.0),
        blend_mode: sk_blend(st.blend),
        quality: FilterQuality::Nearest,
    };
    pm.draw_pixmap(bbox.x, bbox.y, layer.as_ref(), &paint, Transform::identity(), None);
}

fn draw_shadow(
    pm: &mut Pixmap,
    layer: &Pixmap,
    bbox: Rect,
    sh: &crate::style::Shadow,
    scale: f32,
    opacity: f32,
) {
    let Some(mut tint) = Pixmap::new(layer.width(), layer.height()) else { return };
    let col = sh.color;
    for (d, s) in tint.data_mut().chunks_exact_mut(4).zip(layer.data().chunks_exact(4)) {
        let a = ((u32::from(s[3]) * u32::from(col.a) + 127) / 255) as u8;
        d[0] = premul(col.r, a);
        d[1] = premul(col.g, a);
        d[2] = premul(col.b, a);
        d[3] = a;
    }
    let sigma = sh.blur * scale;
    if sigma > 0.0 {
        let (w, h) = (tint.width() as usize, tint.height() as usize);
        ssx_imgfx::gaussian_blur_premultiplied(tint.data_mut(), w, h, sigma, BlurMethod::Auto);
    }
    let paint = PixmapPaint {
        opacity: opacity.clamp(0.0, 1.0),
        blend_mode: SkBlend::SourceOver,
        quality: FilterQuality::Nearest,
    };
    pm.draw_pixmap(
        bbox.x + (sh.dx * scale).round() as i32,
        bbox.y + (sh.dy * scale).round() as i32,
        tint.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
}
