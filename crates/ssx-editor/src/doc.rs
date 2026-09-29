//! The document: base image + canvas + ordered objects, and the global operations.
//!
//! # Coordinate system
//!
//! Objects live in **image space**: `(0, 0)` is the top-left pixel of the *base image*.
//! The canvas may extend beyond it (padding), so canvas pixels can have negative image-space
//! coordinates. Keeping objects in image space means changing the padding never moves
//! annotations relative to the picture.
//!
//! Global operations (crop, cut-out, rotate...) are pure `Document -> Document` edits. They
//! are undoable because the base image is an `Arc<Frame>`: a snapshot of the whole document
//! shares the pixels, so a "before" copy costs no image memory and the history can hold
//! many of them.

use std::sync::Arc;

use ssx_imgfx::{Effect, Placed, ResizeFilter};
use ssx_types::{Frame, Point, Rect};

use crate::{
    geom::{Color, PointF, RectF},
    object::{Axis, Object, ObjectId, ObjectKind, Orient},
    style::Fill,
};

/// Errors from document-level operations.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// The requested crop/canvas rectangle does not overlap the base image.
    #[error("the rectangle {0:?} does not overlap the image; nothing would be left")]
    OutsideImage(Rect),
    /// A cut-out strip is empty or would remove the entire image.
    #[error("invalid cut-out strip {start}..{end} for an image {len} pixels long")]
    InvalidStrip {
        /// Strip start.
        start: i32,
        /// Strip end.
        end: i32,
        /// Image extent along the cut axis.
        len: u32,
    },
    /// The requested size is zero.
    #[error("the target size {0}x{1} is empty")]
    EmptySize(u32, u32),
    /// The image is too large for the editor's `i32` coordinates.
    #[error("image dimensions exceed the supported range")]
    TooLarge,
    /// An image effect failed.
    #[error(transparent)]
    Fx(#[from] ssx_imgfx::FxError),
    /// Frame handling failed.
    #[error(transparent)]
    Frame(#[from] ssx_types::FrameError),
    /// No object with this id.
    #[error("no object with id {0:?}")]
    NoSuchObject(ObjectId),
}

/// Extra canvas around the base image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Padding {
    /// Left.
    pub left: u32,
    /// Top.
    pub top: u32,
    /// Right.
    pub right: u32,
    /// Bottom.
    pub bottom: u32,
}

impl Padding {
    /// Same padding on all four sides.
    pub const fn uniform(v: u32) -> Self {
        Self { left: v, top: v, right: v, bottom: v }
    }
}

/// Canvas around the base image: padding plus what fills it (and shows through transparent
/// base pixels).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Canvas {
    /// Padding around the base image.
    pub padding: Padding,
    /// Background fill ([`Fill::None`] = transparent).
    pub background: Fill,
}

impl Default for Canvas {
    fn default() -> Self {
        Self { padding: Padding::default(), background: Fill::Solid { color: Color::WHITE } }
    }
}

/// The editable document.
#[derive(Debug, Clone)]
pub struct Document {
    pub(crate) base: Arc<Frame>,
    /// Bumped whenever the base pixels change; lets caches key on it instead of pointers.
    pub(crate) base_rev: u64,
    pub(crate) canvas: Canvas,
    pub(crate) objects: Vec<Object>,
    pub(crate) next_id: u64,
    pub(crate) step_start: u32,
}

impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        (Arc::ptr_eq(&self.base, &other.base) || *self.base == *other.base)
            && self.canvas == other.canvas
            && self.objects == other.objects
            && self.next_id == other.next_id
            && self.step_start == other.step_start
    }
}

fn i32_of(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

impl Document {
    /// A document around `base` with a white canvas and no padding. Non-RGBA8 frames are
    /// converted (float/HDR frames must be tonemapped by the caller).
    pub fn new(base: Frame) -> Result<Self, DocError> {
        let base = base.into_rgba8()?;
        Ok(Self::from_arc(Arc::new(base)))
    }

    /// Wraps an already shared RGBA8 frame.
    pub fn from_arc(base: Arc<Frame>) -> Self {
        Self {
            base,
            base_rev: 0,
            canvas: Canvas::default(),
            objects: Vec::new(),
            next_id: 1,
            step_start: 1,
        }
    }

    /// The base image.
    pub fn base(&self) -> &Arc<Frame> {
        &self.base
    }

    /// Revision counter of the base pixels (for render caches).
    pub fn base_revision(&self) -> u64 {
        self.base_rev
    }

    /// Canvas settings.
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// The objects, bottom (first) to top (last).
    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    /// Looks up an object.
    pub fn object(&self, id: ObjectId) -> Option<&Object> {
        self.objects.iter().find(|o| o.id == id)
    }

    /// Mutable lookup. This **bypasses undo**; the interactive session never uses it.
    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.objects.iter_mut().find(|o| o.id == id)
    }

    /// Z-order index of `id` (0 = bottom).
    pub fn index_of(&self, id: ObjectId) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    /// Number shown in the first step marker.
    pub fn step_start(&self) -> u32 {
        self.step_start
    }

    /// The next id that will be handed out.
    pub fn peek_next_id(&self) -> ObjectId {
        ObjectId(self.next_id)
    }

    /// Reserves and returns a fresh id.
    pub fn alloc_id(&mut self) -> ObjectId {
        let id = ObjectId(self.next_id);
        self.next_id += 1;
        id
    }

    /// A group id not used by any object (computed, so allocating one changes no state and
    /// undo restores documents exactly).
    pub fn fresh_group_id(&self) -> u32 {
        self.objects.iter().filter_map(|o| o.group).max().unwrap_or(0) + 1
    }

    /// Inserts an object at `index` (clamped). Bypasses undo.
    pub fn insert_object(&mut self, index: usize, obj: Object) {
        self.next_id = self.next_id.max(obj.id.0 + 1);
        let i = index.min(self.objects.len());
        self.objects.insert(i, obj);
    }

    /// Removes and returns an object. Bypasses undo.
    pub fn remove_object(&mut self, id: ObjectId) -> Option<(usize, Object)> {
        let i = self.index_of(id)?;
        Some((i, self.objects.remove(i)))
    }

    /// Current z-order as ids.
    pub fn order(&self) -> Vec<ObjectId> {
        self.objects.iter().map(|o| o.id).collect()
    }

    /// Reorders objects to match `order`; ids not listed keep their relative order at the
    /// bottom, unknown ids are ignored.
    pub fn set_order(&mut self, order: &[ObjectId]) {
        let mut rest: Vec<Object> = Vec::with_capacity(self.objects.len());
        let mut taken: Vec<Option<Object>> = Vec::new();
        let mut map = std::collections::HashMap::new();
        for o in self.objects.drain(..) {
            if order.contains(&o.id) {
                map.insert(o.id, taken.len());
                taken.push(Some(o));
            } else {
                rest.push(o);
            }
        }
        for id in order {
            if let Some(&i) = map.get(id) {
                if let Some(o) = taken[i].take() {
                    rest.push(o);
                }
            }
        }
        self.objects = rest;
    }

    /// The step number displayed by the step object `id`: its rank among all step objects in
    /// z-order plus [`Document::step_start`], unless it has a manual override. Because the
    /// number is *derived*, deleting or reordering steps renumbers the rest automatically.
    pub fn step_number(&self, id: ObjectId) -> Option<u32> {
        let mut n = self.step_start;
        for o in &self.objects {
            if let ObjectKind::Step(s) = &o.kind {
                if o.id == id {
                    return Some(s.manual.unwrap_or(n));
                }
                n += 1;
            }
        }
        None
    }

    /// Size of the base image in image pixels.
    pub fn image_size(&self) -> (u32, u32) {
        (self.base.width(), self.base.height())
    }

    /// The canvas in image space (may have a negative origin).
    pub fn canvas_rect(&self) -> Rect {
        let p = self.canvas.padding;
        Rect::new(
            -i32_of(p.left),
            -i32_of(p.top),
            self.base.width() + p.left + p.right,
            self.base.height() + p.top + p.bottom,
        )
    }

    /// Output size of a 1× export.
    pub fn canvas_size(&self) -> (u32, u32) {
        let r = self.canvas_rect();
        (r.width, r.height)
    }

    /// Image-space → canvas-pixel offset: add this to an image-space point to get its
    /// position from the top-left of the exported canvas.
    pub fn canvas_offset(&self) -> (u32, u32) {
        (self.canvas.padding.left, self.canvas.padding.top)
    }

    /// Converts an image-space rectangle to output pixels at `scale` (outward rounded),
    /// i.e. the rectangle to pass as [`crate::RenderOptions::viewport`] to repaint it.
    pub fn image_rect_to_output(&self, r: RectF, scale: f32) -> Rect {
        let (ox, oy) = self.canvas_offset();
        RectF::new((r.x + ox as f32) * scale, (r.y + oy as f32) * scale, r.w * scale, r.h * scale)
            .to_outer_rect()
    }

    /// Expands a changed image-space rectangle to everything that must be repainted:
    /// objects above it that read the pixels beneath them (blur, pixelate, magnifier) and,
    /// for spotlights, the whole canvas. Iterates to a fixed point.
    pub fn dirty_region(&self, changed: RectF) -> RectF {
        let mut dirty = changed;
        loop {
            let before = dirty;
            for o in &self.objects {
                if !o.visible {
                    continue;
                }
                match &o.kind {
                    ObjectKind::Spotlight(_) => {
                        return RectF::from(self.canvas_rect());
                    }
                    ObjectKind::Blur(_) | ObjectKind::Pixelate(_) => {
                        if o.bounds().intersects(&dirty) {
                            dirty = dirty.union(&o.render_bounds());
                        }
                    }
                    ObjectKind::Magnify(m) => {
                        if m.source_rect().intersects(&dirty) || o.bounds().intersects(&dirty) {
                            dirty = dirty.union(&o.render_bounds());
                        }
                    }
                    _ => {}
                }
            }
            if dirty == before {
                return dirty;
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Global operations
    // -----------------------------------------------------------------------------------

    fn set_base(&mut self, frame: Frame) {
        self.base = Arc::new(frame);
        self.base_rev += 1;
    }

    fn translate_all(&mut self, dx: f32, dy: f32) {
        if dx != 0.0 || dy != 0.0 {
            for o in &mut self.objects {
                o.translate_with_source(dx, dy);
            }
        }
    }

    /// Makes `rect` (image space) the new canvas: the base image is cropped to its overlap
    /// with `rect`, and any part of `rect` outside the image becomes padding. This one
    /// primitive implements crop, canvas grow/shrink and auto-crop. Objects keep their
    /// position relative to the picture.
    pub fn set_canvas_rect(&mut self, rect: Rect) -> Result<(), DocError> {
        let base_rect = Rect::new(0, 0, self.base.width(), self.base.height());
        let inter = rect.intersect(base_rect).ok_or(DocError::OutsideImage(rect))?;
        let cropped =
            if inter == base_rect { None } else { Some(ssx_imgfx::crop(&self.base, inter)?) };
        if let Some(c) = cropped {
            self.set_base(c);
        }
        let to_u32 = |v: i64| u32::try_from(v.max(0)).map_err(|_| DocError::TooLarge);
        self.canvas.padding = Padding {
            left: to_u32(i64::from(inter.x) - i64::from(rect.x))?,
            top: to_u32(i64::from(inter.y) - i64::from(rect.y))?,
            right: to_u32(rect.right() - inter.right())?,
            bottom: to_u32(rect.bottom() - inter.bottom())?,
        };
        self.translate_all(-(inter.x as f32), -(inter.y as f32));
        Ok(())
    }

    /// Crops the canvas to `rect` (image space). Alias of [`Document::set_canvas_rect`].
    pub fn crop(&mut self, rect: Rect) -> Result<(), DocError> {
        self.set_canvas_rect(rect)
    }

    /// Grows (positive) or shrinks (negative) the canvas on each side by the given amounts.
    pub fn resize_canvas(
        &mut self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> Result<(), DocError> {
        let c = self.canvas_rect();
        let x = i64::from(c.x) - i64::from(left);
        let y = i64::from(c.y) - i64::from(top);
        let w = i64::from(c.width) + i64::from(left) + i64::from(right);
        let h = i64::from(c.height) + i64::from(top) + i64::from(bottom);
        let bounded = |v: i64| i32::try_from(v).map_err(|_| DocError::TooLarge);
        let w = u32::try_from(w.max(0)).map_err(|_| DocError::TooLarge)?;
        let h = u32::try_from(h.max(0)).map_err(|_| DocError::TooLarge)?;
        self.set_canvas_rect(Rect::new(bounded(x)?, bounded(y)?, w, h))
    }

    /// Sets the padding directly, keeping the base image (annotations stay put).
    pub fn set_padding(&mut self, padding: Padding) {
        self.canvas.padding = padding;
    }

    /// Sets the canvas background.
    pub fn set_background(&mut self, background: Fill) {
        self.canvas.background = background;
    }

    /// Sets the number shown by the first step marker.
    pub fn set_step_start(&mut self, start: u32) {
        self.step_start = start;
    }

    /// Crops away uniform borders (colour of the top-left pixel, `tolerance` per channel).
    /// Returns `false` (and changes nothing) when the whole image is uniform.
    pub fn auto_crop(&mut self, tolerance: u8) -> Result<bool, DocError> {
        match ssx_imgfx::auto_crop_bounds(&self.base, tolerance)? {
            Some(r) if r != Rect::new(0, 0, self.base.width(), self.base.height()) => {
                self.set_canvas_rect(r)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Removes the band `start..end` (image pixels along `axis`) from the base image and
    /// joins the two remaining parts. Annotations after the band shift over it; ones that
    /// straddle it shrink.
    pub fn cut_out(&mut self, axis: Axis, start: i32, end: i32) -> Result<(), DocError> {
        let (w, h) = self.image_size();
        let len = if axis == Axis::X { w } else { h };
        let lo = start.min(end).clamp(0, i32_of(len));
        let hi = start.max(end).clamp(0, i32_of(len));
        if lo >= hi || (hi - lo) as u32 >= len {
            return Err(DocError::InvalidStrip { start, end, len });
        }
        let (lo_u, hi_u) = (lo as u32, hi as u32);
        let joined = match axis {
            Axis::X => {
                let left = ssx_imgfx::crop(&self.base, Rect::new(0, 0, lo_u, h))?;
                let right = ssx_imgfx::crop(&self.base, Rect::new(hi, 0, w - hi_u, h))?;
                join(&left, &right, Axis::X)?
            }
            Axis::Y => {
                let top = ssx_imgfx::crop(&self.base, Rect::new(0, 0, w, lo_u))?;
                let bottom = ssx_imgfx::crop(&self.base, Rect::new(0, hi, w, h - hi_u))?;
                join(&top, &bottom, Axis::Y)?
            }
        };
        self.set_base(joined);
        for o in &mut self.objects {
            o.cut(axis, lo as f32, hi as f32);
        }
        Ok(())
    }

    /// Rotates or flips the whole document (base image, padding and annotations).
    pub fn orient(&mut self, o: Orient) -> Result<(), DocError> {
        let (w, h) = self.image_size();
        let out = match o {
            Orient::Rotate90 => ssx_imgfx::rotate_90(&self.base)?,
            Orient::Rotate180 => ssx_imgfx::rotate_180(&self.base)?,
            Orient::Rotate270 => ssx_imgfx::rotate_270(&self.base)?,
            Orient::FlipH => ssx_imgfx::flip_horizontal(&self.base)?,
            Orient::FlipV => ssx_imgfx::flip_vertical(&self.base)?,
        };
        self.set_base(out);
        let p = self.canvas.padding;
        self.canvas.padding = match o {
            Orient::Rotate90 => Padding { left: p.bottom, top: p.left, right: p.top, bottom: p.right },
            Orient::Rotate180 => Padding { left: p.right, top: p.bottom, right: p.left, bottom: p.top },
            Orient::Rotate270 => Padding { left: p.top, top: p.right, right: p.bottom, bottom: p.left },
            Orient::FlipH => Padding { left: p.right, right: p.left, ..p },
            Orient::FlipV => Padding { top: p.bottom, bottom: p.top, ..p },
        };
        for obj in &mut self.objects {
            obj.orient(o, w as f32, h as f32);
        }
        Ok(())
    }

    /// Scales the whole document to `new_w`×`new_h` base pixels (annotations, padding, stroke
    /// widths and fonts scale with it).
    pub fn resize(&mut self, new_w: u32, new_h: u32, filter: ResizeFilter) -> Result<(), DocError> {
        if new_w == 0 || new_h == 0 {
            return Err(DocError::EmptySize(new_w, new_h));
        }
        let (w, h) = self.image_size();
        let resized = ssx_imgfx::resize(&self.base, new_w, new_h, filter)?;
        self.set_base(resized);
        let (sx, sy) = (new_w as f32 / w.max(1) as f32, new_h as f32 / h.max(1) as f32);
        let k = (sx * sy).sqrt();
        for o in &mut self.objects {
            o.scale_about(PointF::new(0.0, 0.0), sx, sy);
            o.scale_style(k);
        }
        let p = self.canvas.padding;
        let sc = |v: u32, s: f32| (v as f32 * s).round() as u32;
        self.canvas.padding =
            Padding { left: sc(p.left, sx), top: sc(p.top, sy), right: sc(p.right, sx), bottom: sc(p.bottom, sy) };
        Ok(())
    }

    /// Applies an image effect to the **base image** (annotations are untouched, so they
    /// stay editable). `region` is in image space. Effects that grow the image shift the
    /// annotations so they keep their place on the picture.
    pub fn apply_effect(&mut self, effect: &Effect, region: Option<Rect>) -> Result<(), DocError> {
        let Placed { frame, origin } = effect.apply(&self.base, region)?;
        self.set_base(frame);
        let Point { x, y } = origin;
        self.translate_all(x as f32, y as f32);
        Ok(())
    }

    /// Replaces the base image with `frame` of the same size (used by tools that bake
    /// pixels, such as flattening). Increments the base revision.
    pub fn replace_base(&mut self, frame: Frame) -> Result<(), DocError> {
        let frame = frame.into_rgba8()?;
        self.set_base(frame);
        Ok(())
    }

    /// Removes all objects and padding, keeping only the base (used after flattening).
    pub fn clear_annotations(&mut self) {
        self.objects.clear();
        self.canvas.padding = Padding::default();
    }
}

/// Joins two frames along `axis` (they share the other dimension).
fn join(a: &Frame, b: &Frame, axis: Axis) -> Result<Frame, DocError> {
    let out = match axis {
        Axis::X => {
            let mut canvas = ssx_imgfx::solid_frame(a.width() + b.width(), a.height(), [0; 4]);
            paste(&mut canvas, a, Point::new(0, 0))?;
            paste(&mut canvas, b, Point::new(i32_of(a.width()), 0))?;
            canvas
        }
        Axis::Y => {
            let mut canvas = ssx_imgfx::solid_frame(a.width(), a.height() + b.height(), [0; 4]);
            paste(&mut canvas, a, Point::new(0, 0))?;
            paste(&mut canvas, b, Point::new(0, i32_of(a.height())))?;
            canvas
        }
    };
    Ok(out)
}

/// Copies `src` over `dst` at `at` without blending (exact pixel copy, alpha included).
fn paste(dst: &mut Frame, src: &Frame, at: Point) -> Result<(), DocError> {
    // Compositing over a fully transparent backdrop with Normal blend reproduces the
    // source exactly for opaque pixels; for translucent pixels `blend_pixel` returns the
    // source too (αb = 0), so the result is exact.
    ssx_imgfx::composite_over(dst, src, at, 1.0, ssx_imgfx::BlendMode::Normal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{BoxShape, LineShape, StepShape};
    use crate::style::Style;
    use ssx_imgfx::solid_frame;

    fn doc(w: u32, h: u32) -> Document {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        Document::new(Frame::from_rgba8(w, h, px).unwrap()).unwrap()
    }

    fn add_rect(d: &mut Document, r: RectF) -> ObjectId {
        let id = d.alloc_id();
        d.insert_object(
            usize::MAX,
            Object::new(id, Style::default(), ObjectKind::Rectangle(BoxShape { rect: r, rotation: 0.0 })),
        );
        id
    }

    #[test]
    fn crop_shifts_objects_and_keeps_pixels() {
        let mut d = doc(20, 10);
        let id = add_rect(&mut d, RectF::new(10.0, 4.0, 5.0, 5.0));
        d.crop(Rect::new(5, 2, 10, 6)).unwrap();
        assert_eq!(d.image_size(), (10, 6));
        assert_eq!(d.canvas_rect(), Rect::new(0, 0, 10, 6));
        let r = d.object(id).unwrap().kind.as_box().unwrap().0;
        assert_eq!((r.x, r.y), (5.0, 2.0));
        // Pixel (0,0) of the crop was (5,2) of the original.
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 0, 0), [5, 2, 7, 255]);
    }

    #[test]
    fn canvas_growth_becomes_padding_and_round_trips() {
        let mut d = doc(20, 10);
        let id = add_rect(&mut d, RectF::new(1.0, 1.0, 2.0, 2.0));
        let before = d.clone();
        d.resize_canvas(5, 6, 7, 8).unwrap();
        assert_eq!(d.canvas().padding, Padding { left: 5, top: 6, right: 7, bottom: 8 });
        assert_eq!(d.image_size(), (20, 10));
        assert_eq!(d.canvas_size(), (32, 24));
        assert_eq!(d.canvas_rect(), Rect::new(-5, -6, 32, 24));
        // Objects keep image-space coordinates (padding does not shift them).
        assert_eq!(d.object(id).unwrap().bounds().x, 1.0);
        d.resize_canvas(-5, -6, -7, -8).unwrap();
        assert_eq!(d, before);
    }

    #[test]
    fn shrinking_into_the_image_crops() {
        let mut d = doc(20, 10);
        d.resize_canvas(-2, -1, -3, -4).unwrap();
        assert_eq!(d.image_size(), (15, 5));
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 0, 0), [2, 1, 7, 255]);
    }

    #[test]
    fn crop_outside_is_an_error() {
        let mut d = doc(10, 10);
        assert!(matches!(d.crop(Rect::new(20, 20, 5, 5)), Err(DocError::OutsideImage(_))));
        assert!(d.crop(Rect::new(0, 0, 0, 0)).is_err());
        assert_eq!(d.image_size(), (10, 10));
    }

    #[test]
    fn cut_out_vertical_strip_joins_and_shifts() {
        let mut d = doc(20, 4);
        let line = d.alloc_id();
        d.insert_object(
            0,
            Object::new(
                line,
                Style::default(),
                ObjectKind::Line(LineShape { a: PointF::new(2.0, 1.0), b: PointF::new(18.0, 1.0) }),
            ),
        );
        d.cut_out(Axis::X, 5, 10).unwrap();
        assert_eq!(d.image_size(), (15, 4));
        // Columns 0..5 kept, then original column 10 follows.
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 4, 0)[0], 4);
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 5, 0)[0], 10);
        match &d.object(line).unwrap().kind {
            ObjectKind::Line(l) => assert_eq!((l.a.x, l.b.x), (2.0, 13.0)),
            _ => unreachable!(),
        }
        d.cut_out(Axis::Y, 1, 3).unwrap();
        assert_eq!(d.image_size(), (15, 2));
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 0, 1)[1], 3);
    }

    #[test]
    fn cut_out_validation() {
        let mut d = doc(10, 10);
        assert!(d.cut_out(Axis::X, 3, 3).is_err());
        assert!(d.cut_out(Axis::X, -5, 50).is_err(), "removing everything is refused");
        d.cut_out(Axis::X, 8, 100).unwrap();
        assert_eq!(d.image_size(), (8, 10));
        d.cut_out(Axis::Y, 6, 2).unwrap();
        assert_eq!(d.image_size(), (8, 6));
    }

    #[test]
    fn orient_four_times_is_identity() {
        let mut d = doc(7, 3);
        let id = add_rect(&mut d, RectF::new(1.0, 1.0, 3.0, 1.0));
        d.set_padding(Padding { left: 1, top: 2, right: 3, bottom: 4 });
        let before = d.clone();
        for _ in 0..4 {
            d.orient(Orient::Rotate90).unwrap();
        }
        assert_eq!(d, before);
        d.orient(Orient::FlipH).unwrap();
        d.orient(Orient::FlipH).unwrap();
        d.orient(Orient::FlipV).unwrap();
        d.orient(Orient::FlipV).unwrap();
        assert_eq!(d, before);
        d.orient(Orient::Rotate90).unwrap();
        assert_eq!(d.image_size(), (3, 7));
        assert_eq!(d.canvas().padding, Padding { left: 4, top: 1, right: 2, bottom: 3 });
        let _ = id;
    }

    #[test]
    fn resize_scales_everything() {
        let mut d = doc(10, 10);
        let id = add_rect(&mut d, RectF::new(2.0, 2.0, 4.0, 4.0));
        d.resize(20, 30, ResizeFilter::Bilinear).unwrap();
        assert_eq!(d.image_size(), (20, 30));
        let o = d.object(id).unwrap();
        assert_eq!(o.kind.as_box().unwrap().0, RectF::new(4.0, 6.0, 8.0, 12.0));
        assert!(o.style.stroke_width > 4.0);
        assert!(d.resize(0, 5, ResizeFilter::Nearest).is_err());
    }

    #[test]
    fn effect_that_grows_shifts_objects() {
        let mut d = doc(10, 10);
        let id = add_rect(&mut d, RectF::new(2.0, 2.0, 4.0, 4.0));
        d.apply_effect(&Effect::Border { width: 3, color: [0, 0, 0, 255] }, None).unwrap();
        assert_eq!(d.image_size(), (16, 16));
        assert_eq!(d.object(id).unwrap().kind.as_box().unwrap().0.x, 5.0);
        d.apply_effect(&Effect::Invert, Some(Rect::new(0, 0, 4, 4))).unwrap();
        assert_eq!(ssx_imgfx::get_pixel(d.base(), 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn auto_crop_removes_uniform_border() {
        let mut f = solid_frame(20, 20, [255, 255, 255, 255]);
        for y in 5..9usize {
            for x in 6..12usize {
                f.data_mut()[(y * 20 + x) * 4..(y * 20 + x) * 4 + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        let mut d = Document::new(f).unwrap();
        assert!(d.auto_crop(0).unwrap());
        assert_eq!(d.image_size(), (6, 4));
        assert!(!d.auto_crop(0).unwrap(), "already tight");
        let mut u = Document::new(solid_frame(5, 5, [1, 2, 3, 255])).unwrap();
        assert!(!u.auto_crop(0).unwrap());
    }

    #[test]
    fn step_numbers_follow_z_order_and_renumber() {
        let mut d = doc(10, 10);
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = d.alloc_id();
            ids.push(id);
            d.insert_object(
                usize::MAX,
                Object::new(
                    id,
                    Style::default(),
                    ObjectKind::Step(StepShape { center: PointF::new(i as f32, 0.0), ..StepShape::default() }),
                ),
            );
        }
        assert_eq!(ids.iter().map(|&i| d.step_number(i).unwrap()).collect::<Vec<_>>(), vec![1, 2, 3]);
        d.remove_object(ids[0]);
        assert_eq!(d.step_number(ids[1]), Some(1));
        assert_eq!(d.step_number(ids[2]), Some(2));
        d.set_order(&[ids[2], ids[1]]);
        assert_eq!(d.step_number(ids[2]), Some(1));
        d.set_step_start(10);
        assert_eq!(d.step_number(ids[1]), Some(11));
        assert_eq!(d.step_number(ObjectId(999)), None);
    }

    #[test]
    fn set_order_keeps_unlisted_at_bottom() {
        let mut d = doc(4, 4);
        let a = add_rect(&mut d, RectF::new(0.0, 0.0, 1.0, 1.0));
        let b = add_rect(&mut d, RectF::new(0.0, 0.0, 1.0, 1.0));
        let c = add_rect(&mut d, RectF::new(0.0, 0.0, 1.0, 1.0));
        d.set_order(&[a, c]);
        assert_eq!(d.order(), vec![b, a, c]);
        d.set_order(&[ObjectId(99), b]);
        assert_eq!(d.order(), vec![a, c, b]);
    }

    #[test]
    fn dirty_region_expands_through_effect_objects() {
        let mut d = doc(100, 100);
        let blur = d.alloc_id();
        d.insert_object(
            0,
            Object::new(
                blur,
                Style::default(),
                ObjectKind::Blur(crate::object::EffectBox { rect: RectF::new(40.0, 40.0, 20.0, 20.0), amount: 5.0 }),
            ),
        );
        let hit = d.dirty_region(RectF::new(45.0, 45.0, 2.0, 2.0));
        assert!(hit.contains(PointF::new(40.0, 40.0)) && hit.contains(PointF::new(60.0, 60.0)));
        let miss = d.dirty_region(RectF::new(0.0, 0.0, 2.0, 2.0));
        assert_eq!(miss, RectF::new(0.0, 0.0, 2.0, 2.0));
    }

    #[test]
    fn image_rect_to_output_accounts_for_padding_and_scale() {
        let mut d = doc(10, 10);
        d.set_padding(Padding::uniform(4));
        let r = d.image_rect_to_output(RectF::new(1.0, 1.0, 2.0, 2.0), 2.0);
        assert_eq!(r, Rect::new(10, 10, 4, 4));
    }
}
