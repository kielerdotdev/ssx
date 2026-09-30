//! Compositing the mouse cursor into a captured frame.
//!
//! `GetImage` never contains the cursor (it is drawn by the server/hardware as an overlay),
//! so when asked, the cursor image is fetched with XFixes `GetCursorImage`: ARGB pixels,
//! **premultiplied** alpha, plus the hotspot and the pointer position in root coordinates.
//! The blend is therefore `dst = src + dst * (1 - a)`, not the straight-alpha formula.

use ssx_types::{Frame, PixelFormat};
use x11rb::protocol::xfixes::ConnectionExt as _;

use crate::{
    error::{X11Error, X11Result},
    session::Session,
};

/// A cursor image ready to be blended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorImage {
    /// Top-left corner of the image in root-window coordinates (position minus hotspot).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// `0xAARRGGBB`, premultiplied.
    pub pixels: Vec<u32>,
}

impl Session {
    /// Current cursor, or `None` if XFixes is unavailable.
    pub(crate) fn cursor_image(&self) -> X11Result<Option<CursorImage>> {
        if !self.ext.xfixes {
            return Ok(None);
        }
        let r = self
            .conn
            .xfixes_get_cursor_image()?
            .reply()
            .map_err(|e| X11Error::from_reply("XFixesGetCursorImage", e))?;
        let (w, h) = (u32::from(r.width), u32::from(r.height));
        if r.cursor_image.len() < (w as usize) * (h as usize) {
            return Err(X11Error::Malformed("cursor image shorter than its size"));
        }
        Ok(Some(CursorImage {
            x: i32::from(r.x) - i32::from(r.xhot),
            y: i32::from(r.y) - i32::from(r.yhot),
            width: w,
            height: h,
            pixels: r.cursor_image,
        }))
    }
}

/// Blends `cursor` into an opaque `Bgra8` frame. The cursor position is in root
/// coordinates and the frame's `origin` says where it sits on the root; parts of the
/// cursor outside the frame are clipped. Does nothing for other pixel formats.
pub(crate) fn blend_cursor(frame: &mut Frame, cursor: &CursorImage) {
    if frame.format() != PixelFormat::Bgra8 {
        return;
    }
    let (ox, oy) = (frame.origin.x, frame.origin.y);
    let (fw, fh) = (i64::from(frame.width()), i64::from(frame.height()));
    for cy in 0..cursor.height {
        let dy = i64::from(cursor.y) + i64::from(cy) - i64::from(oy);
        if dy < 0 || dy >= fh {
            continue;
        }
        let row = frame.row_mut(dy as u32);
        for cx in 0..cursor.width {
            let dx = i64::from(cursor.x) + i64::from(cx) - i64::from(ox);
            if dx < 0 || dx >= fw {
                continue;
            }
            let Some(&argb) = cursor.pixels.get((cy * cursor.width + cx) as usize) else {
                continue;
            };
            let a = argb >> 24;
            if a == 0 && argb == 0 {
                continue;
            }
            let src = [(argb & 0xff), ((argb >> 8) & 0xff), ((argb >> 16) & 0xff)];
            let Some(px) = row.get_mut(dx as usize * 4..dx as usize * 4 + 4) else { continue };
            for (d, s) in px.iter_mut().zip(src) {
                // Premultiplied source over destination, rounded to nearest.
                *d = (s + (u32::from(*d) * (255 - a) + 127) / 255).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssx_types::{ColorSpace, Point, Size};

    fn frame(w: u32, h: u32, bgra: [u8; 4]) -> Frame {
        let data = bgra.iter().copied().cycle().take((w * h * 4) as usize).collect();
        Frame::from_raw(
            Size::new(w, h),
            (w * 4) as usize,
            PixelFormat::Bgra8,
            ColorSpace::Srgb,
            data,
        )
        .unwrap()
    }

    fn cur(x: i32, y: i32, w: u32, h: u32, pixels: Vec<u32>) -> CursorImage {
        CursorImage { x, y, width: w, height: h, pixels }
    }

    #[test]
    fn opaque_pixel_replaces_and_transparent_keeps() {
        let mut f = frame(4, 4, [10, 20, 30, 255]);
        // 2x1 cursor at (1,1): opaque red, fully transparent.
        blend_cursor(&mut f, &cur(1, 1, 2, 1, vec![0xffff_0000, 0x0000_0000]));
        assert_eq!(&f.row(1)[4..8], &[0, 0, 255, 255], "opaque red as BGRA");
        assert_eq!(&f.row(1)[8..12], &[10, 20, 30, 255]);
        assert_eq!(&f.row(0)[4..8], &[10, 20, 30, 255]);
    }

    #[test]
    fn premultiplied_half_alpha_math() {
        let mut f = frame(1, 1, [200, 200, 200, 255]);
        // 50% white, premultiplied: rgb = 128 * 1.0 ~ 0x80, a = 0x80.
        blend_cursor(&mut f, &cur(0, 0, 1, 1, vec![0x8080_8080]));
        // 128 + 200 * 127 / 255 = 128 + 99.6 = 227.6 -> 228
        assert_eq!(&f.row(0)[0..4], &[228, 228, 228, 255]);
        // Black at 50%: premultiplied rgb = 0.
        let mut f = frame(1, 1, [200, 100, 50, 255]);
        blend_cursor(&mut f, &cur(0, 0, 1, 1, vec![0x8000_0000]));
        assert_eq!(&f.row(0)[0..4], &[100, 50, 25, 255]);
    }

    #[test]
    fn cursor_is_clipped_at_every_edge() {
        let mut f = frame(3, 3, [0, 0, 0, 255]);
        let white = vec![0xffff_ffff; 4];
        // Overlapping the top-left corner and the bottom-right corner, and fully outside.
        blend_cursor(&mut f, &cur(-1, -1, 2, 2, white.clone()));
        blend_cursor(&mut f, &cur(2, 2, 2, 2, white.clone()));
        blend_cursor(&mut f, &cur(50, 50, 2, 2, white.clone()));
        blend_cursor(&mut f, &cur(-50, 0, 2, 2, white));
        let lit: Vec<(usize, usize)> = (0..3)
            .flat_map(|y| (0..3).map(move |x| (x, y)))
            .filter(|&(x, y)| f.row(y as u32)[x * 4] == 255)
            .collect();
        assert_eq!(lit, vec![(0, 0), (2, 2)]);
    }

    #[test]
    fn frame_origin_is_honoured() {
        let mut f = frame(2, 2, [0, 0, 0, 255]);
        f.origin = Point::new(100, 200);
        blend_cursor(&mut f, &cur(101, 200, 1, 1, vec![0xffff_ffff]));
        assert_eq!(&f.row(0)[4..8], &[255, 255, 255, 255]);
        assert_eq!(&f.row(0)[0..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn short_pixel_buffer_and_wrong_format_are_harmless() {
        let mut f = frame(2, 2, [1, 2, 3, 255]);
        blend_cursor(&mut f, &cur(0, 0, 2, 2, vec![0xffff_ffff]));
        assert_eq!(&f.row(0)[0..4], &[255, 255, 255, 255]);
        assert_eq!(&f.row(1)[0..4], &[1, 2, 3, 255]);
        let mut rgba = Frame::from_rgba8(1, 1, vec![1, 2, 3, 255]).unwrap();
        blend_cursor(&mut rgba, &cur(0, 0, 1, 1, vec![0xffff_ffff]));
        assert_eq!(rgba.data(), &[1, 2, 3, 255]);
    }
}
