//! Reading pixels from a drawable: MIT-SHM when possible, banded plain `GetImage` otherwise.
//!
//! Images are fetched in horizontal bands so that neither a huge screenshot (an 8K
//! multi-monitor desktop is over 100 MB) nor a small server request limit can make a
//! request fail; a band is at least one row, however wide.

use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Size};
use x11rb::{
    connection::RequestConnection,
    protocol::xproto::{ConnectionExt as _, ImageFormat},
};

use crate::{
    error::{X11Error, X11Result},
    pixels::PixelLayout,
    session::Session,
};

/// Fallback plain-reply budget when the server's request limit is larger.
const DEFAULT_PLAIN_CHUNK: usize = 4 * 1024 * 1024;

/// A rectangle inside a drawable, in the drawable's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Area {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Session {
    /// Layout of images of the given drawable `depth`, decoded with `visual`'s masks.
    pub(crate) fn layout(&self, depth: u8, visual: u32) -> X11Result<PixelLayout> {
        let format = self.format_for_depth(depth)?;
        let visual = self.visuals.get(&visual).ok_or_else(|| {
            X11Error::UnsupportedVisual(format!("visual {visual:#x} is not advertised"))
        })?;
        PixelLayout::new(*format, self.msb_first, visual)
    }

    fn plain_chunk_budget(&self) -> usize {
        let limit = self.conn.maximum_request_bytes();
        self.config.max_chunk_bytes.unwrap_or_else(|| limit.min(DEFAULT_PLAIN_CHUNK))
    }

    /// Captures `area` of `drawable` (which has `depth` and, for windows, `visual`) as a
    /// BGRA frame whose origin is `origin`.
    pub(crate) fn grab(
        &self,
        drawable: u32,
        depth: u8,
        visual: u32,
        area: Area,
        origin: Point,
    ) -> X11Result<Frame> {
        let layout = self.layout(depth, visual)?;
        let (w, h) = (area.width as usize, area.height as usize);
        let stride = layout.row_stride(w);
        let mut out = vec![0u8; w * h * 4];

        let x = i16::try_from(area.x).map_err(|_| X11Error::Malformed("x out of range"))?;
        let width =
            u16::try_from(area.width).map_err(|_| X11Error::Malformed("width too large"))?;

        let mut y = 0usize;
        while y < h {
            let want_bytes =
                if self.shm_available() { self.segment_cap() } else { self.plain_chunk_budget() };
            let rows = (want_bytes / stride.max(1)).clamp(1, h - y);
            let band_y = area
                .y
                .checked_add(i32::try_from(y).map_err(|_| X11Error::Malformed("y out of range"))?)
                .and_then(|v| i16::try_from(v).ok())
                .ok_or(X11Error::Malformed("y out of range"))?;
            let rows16 = u16::try_from(rows).unwrap_or(u16::MAX);
            let rows = usize::from(rows16);
            let data = self.fetch_band(drawable, x, band_y, width, rows16, stride * rows)?;
            layout.decode_rows(&data, w, rows, &mut out[y * w * 4..(y + rows) * w * 4])?;
            y += rows;
        }

        let mut frame = Frame::from_raw(
            Size::new(area.width, area.height),
            w * 4,
            PixelFormat::Bgra8,
            ColorSpace::Srgb,
            out,
        )
        .map_err(|_| X11Error::Malformed("frame geometry"))?;
        frame.origin = origin;
        Ok(frame)
    }

    fn fetch_band(
        &self,
        drawable: u32,
        x: i16,
        y: i16,
        width: u16,
        rows: u16,
        bytes: usize,
    ) -> X11Result<Vec<u8>> {
        if self.shm_available() {
            match self.shm_get_image(drawable, x, y, width, rows, bytes) {
                Ok(data) => return Ok(data),
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => {
                    tracing::debug!(error = %e, "MIT-SHM GetImage failed; retrying without it");
                    let plain = self.plain_get_image(drawable, x, y, width, rows)?;
                    // The plain path works, so SHM itself is the problem (remote
                    // display, no fd passing, sandbox): stop trying.
                    tracing::warn!("disabling MIT-SHM for this session");
                    self.disable_shm();
                    return Ok(plain);
                }
            }
        }
        self.plain_get_image(drawable, x, y, width, rows)
    }

    fn plain_get_image(
        &self,
        drawable: u32,
        x: i16,
        y: i16,
        width: u16,
        rows: u16,
    ) -> X11Result<Vec<u8>> {
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, x, y, width, rows, !0)?
            .reply()
            .map_err(|e| X11Error::from_reply("GetImage", e))?;
        Ok(reply.data)
    }
}
