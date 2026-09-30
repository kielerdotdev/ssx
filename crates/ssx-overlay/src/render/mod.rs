//! CPU renderer: turns a [`Scene`] into BGRA pixels, one dirty rectangle at a time.
//!
//! Why CPU instead of wgpu: an overlay must appear instantly and work on every driver stack,
//! under Xvfb and headless compositors alike. Everything expensive is done **once** at
//! start-up (convert to BGRA, pre-dim), so a redraw is: memcpy the dimmed background for the
//! dirty rectangle, memcpy the bright pixels inside the selection, draw the handful of
//! overlays into a small scratch canvas, copy to the target. The cost is proportional to the
//! dirty area, never to the desktop size.
//!
//! Rendering an area is a pure function of `(scene, area)`: the result for a pixel does not
//! depend on which rectangle it was rendered as part of. The property tests in `tests` prove
//! that damage tracking built on this is sound (incremental == from scratch). That is why
//! the drawing primitives in [`paint`] are integer-only.

pub mod paint;

use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Rect, Size};

use self::paint::{ACCENT, Canvas, HIGHLIGHT, PANEL, Rgba, TEXT};
use crate::error::OverlayError;
use crate::model::geometry::{ellipse_span, handle_rect, polygon_spans, visible_handles};
use crate::model::scene::{Cutout, LabelScene, LoupeScene, Scene, handle_size, loupe_info, text_scale};

/// A BGRA pixel buffer the renderer draws into. `data` is tightly packed
/// (`stride == width * 4`) and its top-left pixel is desktop position `origin`.
#[derive(Debug)]
pub struct TargetBuf<'a> {
    /// Desktop position of the buffer's first pixel.
    pub origin: Point,
    /// Size in pixels.
    pub size: Size,
    /// BGRA bytes, `size.width * size.height * 4` long.
    pub data: &'a mut [u8],
}

impl TargetBuf<'_> {
    /// Desktop rectangle covered.
    pub fn rect(&self) -> Rect {
        Rect::from_origin_size(self.origin, self.size)
    }
}

/// Holds the frozen desktop (bright and pre-dimmed, BGRA) and draws scenes over it.
pub struct Renderer {
    bounds: Rect,
    width: usize,
    base: Vec<u8>,
    dimmed: Vec<u8>,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").field("bounds", &self.bounds).finish_non_exhaustive()
    }
}

impl Renderer {
    /// Builds the renderer from an 8-bit sRGB frame. `dim` is 0.0..=1.0.
    ///
    /// Converts to BGRA/opaque and pre-dims in a single multi-threaded pass over the frame.
    pub fn new(frame: &Frame, dim: f32) -> Result<Self, OverlayError> {
        if frame.width() == 0 || frame.height() == 0 {
            return Err(OverlayError::InvalidInput("the desktop frame is empty".into()));
        }
        if !frame.is_sdr8() || frame.color_space() != ColorSpace::Srgb {
            return Err(OverlayError::InvalidInput(format!(
                "the overlay needs an 8-bit sRGB frame, got {:?}/{:?}; tone-map it first",
                frame.format(),
                frame.color_space()
            )));
        }
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let n = w
            .checked_mul(h)
            .and_then(|p| p.checked_mul(4))
            .ok_or_else(|| OverlayError::InvalidInput("frame too large".into()))?;
        let mut base = vec![0u8; n];
        let mut dimmed = vec![0u8; n];
        let keep = (1.0 - dim.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        let mut lut = [0u8; 256];
        for (v, o) in lut.iter_mut().enumerate() {
            *o = (v as f32 * keep + 0.5) as u8;
        }
        let swap = frame.format() == PixelFormat::Rgba8;
        let threads = std::thread::available_parallelism().map_or(1, usize::from).clamp(1, 8);
        let rows_per = h.div_ceil(threads).max(1);
        std::thread::scope(|s| {
            let stride = w * 4;
            for (ci, (b, d)) in
                base.chunks_mut(rows_per * stride).zip(dimmed.chunks_mut(rows_per * stride)).enumerate()
            {
                let lut = &lut;
                s.spawn(move || {
                    for (ri, (brow, drow)) in b.chunks_exact_mut(stride).zip(d.chunks_exact_mut(stride)).enumerate() {
                        let src = frame.row((ci * rows_per + ri) as u32);
                        for ((bp, dp), sp) in brow
                            .chunks_exact_mut(4)
                            .zip(drow.chunks_exact_mut(4))
                            .zip(src.chunks_exact(4))
                        {
                            let (c0, c1, c2) = if swap { (sp[2], sp[1], sp[0]) } else { (sp[0], sp[1], sp[2]) };
                            bp.copy_from_slice(&[c0, c1, c2, 255]);
                            dp.copy_from_slice(&[lut[usize::from(c0)], lut[usize::from(c1)], lut[usize::from(c2)], 255]);
                        }
                    }
                });
            }
        });
        Ok(Self {
            bounds: frame.rect(),
            width: w,
            base,
            dimmed,
            scratch: Vec::new(),
        })
    }

    /// Desktop rectangle the renderer covers.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The (bright) sRGB pixel at a desktop position.
    pub fn pixel(&self, p: Point) -> Option<[u8; 3]> {
        if !self.bounds.contains(p) {
            return None;
        }
        let x = (i64::from(p.x) - i64::from(self.bounds.x)) as usize;
        let y = (i64::from(p.y) - i64::from(self.bounds.y)) as usize;
        let o = (y * self.width + x) * 4;
        Some([self.base[o + 2], self.base[o + 1], self.base[o]])
    }

    /// Bright BGRA row segment of the original desktop (for tests and zero-copy presenters).
    pub fn base(&self) -> &[u8] {
        &self.base
    }

    /// Renders `scene` for the part of `area` (desktop pixels) that lies inside `target`.
    pub fn render(&mut self, scene: &Scene, area: Rect, target: &mut TargetBuf<'_>) {
        let Some(a) = area.intersect(target.rect()).and_then(|r| r.intersect(self.bounds)) else {
            return;
        };
        let (aw, ah) = (a.width as usize, a.height as usize);
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        buf.resize(aw * ah * 4, 0);
        self.compose_background(scene, a, &mut buf);
        {
            let mut cv = Canvas { w: i64::from(a.width), h: i64::from(a.height), data: &mut buf };
            self.draw_overlays(scene, a, &mut cv);
        }
        let tw = target.size.width as usize;
        let row_bytes = aw * 4;
        for row in 0..ah {
            let ty = (i64::from(a.y) - i64::from(target.origin.y)) as usize + row;
            let tx = (i64::from(a.x) - i64::from(target.origin.x)) as usize;
            let dst = (ty * tw + tx) * 4;
            target.data[dst..dst + row_bytes]
                .copy_from_slice(&buf[row * row_bytes..(row + 1) * row_bytes]);
        }
        self.scratch = buf;
    }

    /// Renders the whole scene to a new RGBA [`Frame`] (tests, golden images, screenshots).
    pub fn render_to_frame(&mut self, scene: &Scene) -> Frame {
        let b = self.bounds;
        let mut data = vec![0u8; b.width as usize * b.height as usize * 4];
        {
            let mut t = TargetBuf { origin: b.origin(), size: b.size(), data: &mut data };
            self.render(scene, b, &mut t);
        }
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let mut f =
            Frame::from_rgba8(b.width, b.height, data).expect("buffer sized by construction");
        f.origin = b.origin();
        f
    }

    /// Byte offset of desktop pixel (`x`,`y`) in `base`/`dimmed`.
    fn offset(&self, x: i64, y: i64) -> usize {
        (((y - i64::from(self.bounds.y)) as usize) * self.width
            + (x - i64::from(self.bounds.x)) as usize)
            * 4
    }

    /// Copies the bright pixels of desktop row `y`, columns `x0..x1`, into `buf` (an
    /// `a`-sized scratch buffer). Columns are clipped to `a`.
    fn copy_bright(&self, buf: &mut [u8], a: Rect, y: i64, x0: i64, x1: i64) {
        let (x0, x1) = (x0.max(i64::from(a.x)), x1.min(a.right()));
        if x1 <= x0 || y < i64::from(a.y) || y >= a.bottom() {
            return;
        }
        let src = self.offset(x0, y);
        let n = (x1 - x0) as usize * 4;
        let dst = (((y - i64::from(a.y)) as usize) * a.width as usize
            + (x0 - i64::from(a.x)) as usize)
            * 4;
        buf[dst..dst + n].copy_from_slice(&self.base[src..src + n]);
    }

    fn compose_background(&self, scene: &Scene, a: Rect, buf: &mut [u8]) {
        let row_bytes = a.width as usize * 4;
        for row in 0..a.height as usize {
            let src = self.offset(i64::from(a.x), i64::from(a.y) + row as i64);
            buf[row * row_bytes..(row + 1) * row_bytes]
                .copy_from_slice(&self.dimmed[src..src + row_bytes]);
        }
        let rows = |r: Rect| i64::from(r.y).max(i64::from(a.y))..r.bottom().min(a.bottom());
        // Bright polygon of a freeform stroke.
        if scene.freeform.len() >= 3 {
            let Some(bb) = crate::model::geometry::bounding_points(&scene.freeform) else {
                return;
            };
            for y in rows(bb) {
                for (x0, x1) in polygon_spans(&scene.freeform, y) {
                    self.copy_bright(buf, a, y, x0, x1);
                }
            }
            return;
        }
        match (scene.selection, scene.highlight) {
            (Some(sel), _) if scene.cutout == Cutout::Ellipse => {
                for y in rows(sel) {
                    if let Some((x0, x1)) = ellipse_span(sel, y) {
                        self.copy_bright(buf, a, y, x0, x1);
                    }
                }
            }
            (sel, hi) => {
                if let Some(r) = sel.or(hi) {
                    for y in rows(r) {
                        self.copy_bright(buf, a, y, i64::from(r.x), r.right());
                    }
                }
            }
        }
    }

    fn draw_overlays(&self, scene: &Scene, a: Rect, cv: &mut Canvas<'_>) {
        let s = i64::from(text_scale(scene.ui_scale));
        let (ax, ay) = (i64::from(a.x), i64::from(a.y));

        // Freeform outline.
        for w in scene.freeform.windows(2) {
            cv.thick_line(
                i64::from(w[0].x) - ax,
                i64::from(w[0].y) - ay,
                i64::from(w[1].x) - ax,
                i64::from(w[1].y) - ay,
                2 * s,
                ACCENT,
            );
        }

        // Hover highlight: border inside the rectangle.
        if let Some(h) = scene.highlight {
            cv.stroke_inside(
                i64::from(h.x) - ax,
                i64::from(h.y) - ay,
                i64::from(h.width),
                i64::from(h.height),
                2 * s,
                HIGHLIGHT,
            );
        }

        // Selection border (outside the selected pixels) and handles.
        if let Some(sel) = scene.selection {
            let (x, y, w, h) = (
                i64::from(sel.x) - ax,
                i64::from(sel.y) - ay,
                i64::from(sel.width),
                i64::from(sel.height),
            );
            if scene.cutout == Cutout::Ellipse {
                Self::ellipse_outline(cv, sel, a, s);
                // Faint bounding box so the resize handles stay attached to something.
                cv.stroke_inside(x - s, y - s, w + 2 * s, h + 2 * s, 1, Rgba(255, 255, 255, 110));
            } else {
                cv.stroke_inside(x - s, y - s, w + 2 * s, h + 2 * s, s, ACCENT);
            }
            if scene.handles {
                let size = handle_size(scene.ui_scale);
                for hnd in visible_handles(sel, size) {
                    let hr = handle_rect(sel, hnd, size);
                    let (hx, hy) = (i64::from(hr.x) - ax, i64::from(hr.y) - ay);
                    let (hw, hh) = (i64::from(hr.width), i64::from(hr.height));
                    cv.fill_rect(hx, hy, hw, hh, ACCENT);
                    let inner = if scene.active_handle == Some(hnd) { ACCENT } else { TEXT };
                    cv.fill_rect(hx + s, hy + s, hw - 2 * s, hh - 2 * s, inner);
                }
            }
        }

        // Crosshair guides: dark halo, then a light line, through the pointer pixel.
        if let Some(c) = scene.crosshair {
            let b = scene.bounds;
            let (cx, cy) = (i64::from(c.x) - ax, i64::from(c.y) - ay);
            let (bx, by) = (i64::from(b.x) - ax, i64::from(b.y) - ay);
            let halo = Rgba(0, 0, 0, 110);
            let line = Rgba(255, 255, 255, 215);
            cv.fill_rect(bx, cy - s, i64::from(b.width), 3 * s, halo);
            cv.fill_rect(cx - s, by, 3 * s, i64::from(b.height), halo);
            cv.fill_rect(bx, cy, i64::from(b.width), s, line);
            cv.fill_rect(cx, by, s, i64::from(b.height), line);
        }

        if let Some(l) = &scene.label {
            Self::draw_label(cv, a, l, scene.ui_scale);
        }
        if let Some(l) = &scene.loupe {
            self.draw_loupe(cv, a, l, scene.ui_scale);
        }
    }

    /// Outline `s` pixels thick, inside the ellipse: pixels of the ellipse that have a
    /// non-ellipse pixel `s` steps away horizontally or vertically.
    fn ellipse_outline(cv: &mut Canvas<'_>, sel: Rect, a: Rect, s: i64) {
        let (ax, ay) = (i64::from(a.x), i64::from(a.y));
        for y in i64::from(sel.y).max(i64::from(a.y))..sel.bottom().min(a.bottom()) {
            let Some((l, r)) = ellipse_span(sel, y) else { continue };
            let inner = ellipse_span(sel, y - s).zip(ellipse_span(sel, y + s)).and_then(|(u, d)| {
                let lo = (l + s).max(u.0).max(d.0);
                let hi = (r - s).min(u.1).min(d.1);
                (hi > lo).then_some((lo, hi))
            });
            let mut run = |x0: i64, x1: i64| {
                if x1 > x0 {
                    cv.fill_rect(x0 - ax, y - ay, x1 - x0, 1, ACCENT);
                }
            };
            match inner {
                Some((lo, hi)) => {
                    run(l, lo);
                    run(hi, r);
                }
                None => run(l, r),
            }
        }
    }

    fn draw_label(cv: &mut Canvas<'_>, a: Rect, l: &LabelScene, ui: f32) {
        let s = text_scale(ui);
        let (x, y) = (i64::from(l.rect.x) - i64::from(a.x), i64::from(l.rect.y) - i64::from(a.y));
        cv.fill_rect(x, y, i64::from(l.rect.width), i64::from(l.rect.height), PANEL);
        let pad = i64::from(3 * s);
        let line_h = i64::from(10 * s);
        for (i, line) in l.lines.iter().enumerate() {
            cv.text(x + pad, y + pad + i as i64 * line_h, line, s, TEXT);
        }
    }

    fn draw_loupe(&self, cv: &mut Canvas<'_>, a: Rect, l: &LoupeScene, ui: f32) {
        let s = i64::from(text_scale(ui));
        let (ox, oy) =
            (i64::from(l.outer.x) - i64::from(a.x), i64::from(l.outer.y) - i64::from(a.y));
        cv.fill_rect(
            ox,
            oy,
            i64::from(l.outer.width),
            i64::from(l.outer.height),
            Rgba(0x14, 0x14, 0x18, 255),
        );

        // Zoomed pixels (nearest neighbour). Cells outside the desktop are a dark checker.
        let half = (l.cells / 2) as i32;
        let cell = i64::from(l.cell_px);
        for j in 0..l.cells as i32 {
            for i in 0..l.cells as i32 {
                let p = Point::new(
                    l.centre.x.saturating_add(i - half),
                    l.centre.y.saturating_add(j - half),
                );
                let c = match self.pixel(p) {
                    Some([r, g, b]) => Rgba(r, g, b, 255),
                    None if (i + j) % 2 == 0 => Rgba(0x30, 0x30, 0x30, 255),
                    None => Rgba(0x20, 0x20, 0x20, 255),
                };
                cv.fill_rect(ox + i64::from(i) * cell, oy + i64::from(j) * cell, cell, cell, c);
            }
        }
        // Centre marker: dark outline around a light one.
        let (cx, cy) = (ox + i64::from(half) * cell, oy + i64::from(half) * cell);
        cv.stroke_inside(cx - s, cy - s, cell + 2 * s, cell + 2 * s, s, Rgba(0, 0, 0, 255));
        cv.stroke_inside(cx, cy, cell, cell, s, Rgba(255, 255, 255, 255));
        cv.stroke_inside(ox, oy, i64::from(l.outer.width), i64::from(l.outer.height), s, ACCENT);

        // Info panel: swatch, hex, rgb, position.
        let rgb = self.pixel(l.centre);
        let info_y = oy + i64::from(l.grid.height);
        let (pad, line_h) = (3 * s, 10 * s);
        for (i, line) in loupe_info(l.centre, rgb).iter().enumerate() {
            let ty = info_y + pad + i as i64 * line_h;
            let mut tx = ox + pad;
            if let (0, Some([r, g, b])) = (i, rgb) {
                let sw = 8 * s;
                cv.stroke_inside(tx - 1, ty - 1, sw + 2, sw + 2, 1, TEXT);
                cv.fill_rect(tx, ty, sw, sw, Rgba(r, g, b, 255));
                tx += sw + 2 * s;
            }
            cv.text(tx, ty, line, s as u32, TEXT);
        }
    }
}

#[cfg(test)]
mod tests;
