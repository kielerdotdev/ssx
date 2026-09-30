//! A tiny integer software canvas: alpha-blended rectangles, bitmap text, thick lines.
//!
//! Deliberately not an anti-aliasing vector rasteriser. The overlay's incremental redraw is
//! only sound if a pixel's value is a pure function of the scene, independent of which dirty
//! rectangle it was rendered inside; anti-aliased rasterisers (tiny-skia included) clip paths
//! against the target rectangle and round differently at the clip edge, which showed up as
//! stray ±10 level pixels in the property tests. Integer rasterisation is exact, and for
//! ellipse/freeform selections it is also *truthful*: the bright pixels are exactly the
//! pixels of the returned mask.
//!
//! Buffers are **BGRA byte order** (what X11 `ZPixmap` 24/32 bit, `wl_shm` `ARGB8888`/`XRGB8888`
//! on little-endian and Win32 DIBs want), so presenting is a plain copy.

use font8x8::legacy::BASIC_LEGACY;

use crate::model::scene::GLYPH;

/// An RGBA colour in *logical* channel order.
#[derive(Debug, Clone, Copy)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

/// Accent colour of borders and handles.
pub const ACCENT: Rgba = Rgba(0x2d, 0x8c, 0xff, 0xff);
/// Hover-highlight colour.
pub const HIGHLIGHT: Rgba = Rgba(0x3c, 0xd2, 0x78, 0xff);
/// Panel background of labels and the loupe info box.
pub const PANEL: Rgba = Rgba(0x14, 0x14, 0x18, 0xd0);
/// Text colour.
pub const TEXT: Rgba = Rgba(0xff, 0xff, 0xff, 0xff);

/// A BGRA canvas over a byte buffer.
#[derive(Debug)]
pub struct Canvas<'a> {
    /// Width in pixels.
    pub w: i64,
    /// Height in pixels.
    pub h: i64,
    /// `w * h * 4` BGRA bytes.
    pub data: &'a mut [u8],
}

fn blend(dst: &mut [u8], c: Rgba) {
    let a = u32::from(c.3);
    let mix = |d: u8, s: u8| ((u32::from(s) * a + u32::from(d) * (255 - a) + 127) / 255) as u8;
    dst[0] = mix(dst[0], c.2);
    dst[1] = mix(dst[1], c.1);
    dst[2] = mix(dst[2], c.0);
}

impl Canvas<'_> {
    /// Fills a rectangle (canvas coordinates), clipped to the canvas, alpha-blended.
    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgba) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = (x.saturating_add(w).min(self.w), y.saturating_add(h).min(self.h));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let stride = self.w as usize * 4;
        for yy in y0..y1 {
            let row = &mut self.data
                [yy as usize * stride + x0 as usize * 4..yy as usize * stride + x1 as usize * 4];
            if c.3 == 255 {
                for px in row.chunks_exact_mut(4) {
                    px.copy_from_slice(&[c.2, c.1, c.0, 255]);
                }
            } else {
                for px in row.chunks_exact_mut(4) {
                    blend(px, c);
                }
            }
        }
    }

    /// Outline of a rectangle, `t` pixels thick, *inside* its bounds.
    pub fn stroke_inside(&mut self, x: i64, y: i64, w: i64, h: i64, t: i64, c: Rgba) {
        if w <= 0 || h <= 0 {
            return;
        }
        let t = t.min(w / 2).min(h / 2);
        if t <= 0 {
            self.fill_rect(x, y, w, h, c);
            return;
        }
        self.fill_rect(x, y, w, t, c);
        self.fill_rect(x, y + h - t, w, t, c);
        self.fill_rect(x, y + t, t, h - 2 * t, c);
        self.fill_rect(x + w - t, y + t, t, h - 2 * t, c);
    }

    /// Single-line text with the 8x8 bitmap font at integer magnification `s`. Non-ASCII
    /// characters render as `?`. Colour alpha is ignored (text is opaque).
    pub fn text(&mut self, x: i64, y: i64, text: &str, s: u32, c: Rgba) {
        let s = i64::from(s);
        let c = Rgba(c.0, c.1, c.2, 255);
        let mut cx = x;
        for ch in text.chars() {
            let idx = if ch.is_ascii() { ch as usize } else { usize::from(b'?') };
            for (row, bits) in BASIC_LEGACY[idx].iter().enumerate() {
                // Merge horizontal runs so a glyph is a handful of rectangles.
                let mut col = 0i64;
                while col < i64::from(GLYPH) {
                    if bits & (1 << col) == 0 {
                        col += 1;
                        continue;
                    }
                    let start = col;
                    while col < i64::from(GLYPH) && bits & (1 << col) != 0 {
                        col += 1;
                    }
                    self.fill_rect(cx + start * s, y + row as i64 * s, (col - start) * s, s, c);
                }
            }
            cx += i64::from(GLYPH) * s;
        }
    }

    /// A line of `thick`-pixel squares stamped along a Bresenham path (canvas coordinates).
    pub fn thick_line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, thick: i64, c: Rgba) {
        let half = thick / 2;
        let (minx, maxx) = (x0.min(x1) - thick, x0.max(x1) + thick);
        let (miny, maxy) = (y0.min(y1) - thick, y0.max(y1) + thick);
        if maxx < 0 || maxy < 0 || minx >= self.w || miny >= self.h {
            return;
        }
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.fill_rect(x - half, y - half, thick, thick, c);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}
