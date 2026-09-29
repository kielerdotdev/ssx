//! GDI `BitBlt` capture: the last resort, SDR only.
//!
//! GDI works everywhere (including sessions where WGC and Desktop Duplication are
//! unavailable) but knows nothing about HDR: on an HDR display Windows hands it an already
//! clipped 8-bit image, so highlights are lost. A warning is logged in that case rather than
//! failing, because a clipped screenshot beats no screenshot as the final fallback.
//!
//! `CAPTUREBLT` is required to include layered (semi-transparent / per-pixel-alpha) windows;
//! without it they are simply missing from the result. The cursor is not part of the screen
//! DC, so it is drawn by hand with `DrawIconEx` when requested.

use std::{ffi::c_void, mem::size_of};

use ssx_capture::CaptureOptions;
use ssx_types::{Frame, Point, Size};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC,
        HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
        ICONINFO,
    },
};

use crate::{
    chain::{MonitorCapturer, MonitorTarget},
    error::WinError,
    geometry::cursor_draw_position,
    hdr::CaptureFormat,
    pixels::{Placement, frame_from_packed},
};

/// The screen device context, released on drop.
struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `GetDC(None)` and is released exactly once.
        unsafe { ReleaseDC(Some(HWND::default()), self.0) };
    }
}

/// A memory DC with a compatible bitmap selected into it; restores the DC and frees both
/// GDI objects on drop (a bitmap that is still selected cannot be deleted).
struct MemoryBitmap {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    selected: bool,
}

impl MemoryBitmap {
    fn new(screen: HDC, width: i32, height: i32) -> Result<Self, WinError> {
        // SAFETY: `screen` is a valid DC.
        let dc = unsafe { CreateCompatibleDC(Some(screen)) };
        if dc.is_invalid() {
            return Err(WinError::Other("CreateCompatibleDC failed".into()));
        }
        // SAFETY: `screen` is a valid DC; sizes are positive.
        let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
        if bitmap.is_invalid() {
            // SAFETY: `dc` was created above and is not used again.
            let _ = unsafe { DeleteDC(dc) };
            return Err(WinError::Other(format!(
                "CreateCompatibleBitmap({width}x{height}) failed (out of GDI memory?)"
            )));
        }
        // SAFETY: both handles are valid and the bitmap is not selected elsewhere.
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Self { dc, bitmap, previous, selected: true })
    }

    /// `GetDIBits` requires the bitmap not to be selected into a DC.
    fn deselect(&mut self) {
        if self.selected {
            // SAFETY: restores the DC's original object; both handles are valid.
            unsafe { SelectObject(self.dc, self.previous) };
            self.selected = false;
        }
    }
}

impl Drop for MemoryBitmap {
    fn drop(&mut self) {
        self.deselect();
        // SAFETY: each handle was created in `new` and is freed exactly once, bitmap first
        // (it is no longer selected).
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The GDI capturer.
#[derive(Debug, Default)]
pub(crate) struct Gdi;

impl Gdi {
    /// Captures a whole monitor with `BitBlt`.
    pub(crate) fn capture_monitor(
        target: &MonitorTarget,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        let rect = target.monitor.rect;
        let (Ok(width), Ok(height)) = (i32::try_from(rect.width), i32::try_from(rect.height))
        else {
            return Err(WinError::InvalidRegion(rect));
        };
        if rect.is_empty() {
            return Err(WinError::InvalidRegion(rect));
        }
        if target.monitor.hdr.is_some_and(|h| h.active) {
            tracing::warn!("capturing an HDR display with GDI: highlights will be clipped");
        }

        // SAFETY: `GetDC(None)` returns the screen DC or a null handle.
        let screen = ScreenDc(unsafe { GetDC(None) });
        if screen.0.is_invalid() {
            return Err(WinError::Other("GetDC(NULL) failed".into()));
        }
        let mut memory = MemoryBitmap::new(screen.0, width, height)?;

        // SAFETY: both DCs are valid and the destination bitmap is selected into `memory.dc`.
        unsafe {
            BitBlt(
                memory.dc,
                0,
                0,
                width,
                height,
                Some(screen.0),
                rect.x,
                rect.y,
                SRCCOPY | CAPTUREBLT,
            )
        }
        .map_err(crate::sys::api_err("BitBlt"))?;

        if opts.include_cursor {
            draw_cursor(memory.dc, rect.origin());
        }
        memory.deselect();

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height requests a top-down DIB, i.e. rows in natural order.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let mut data = vec![0u8; rect.width as usize * rect.height as usize * 4];
        // SAFETY: `data` holds `width * height * 4` bytes, exactly what a 32-bpp top-down DIB
        // of `height` scan lines needs; the bitmap is deselected as required.
        let lines = unsafe {
            GetDIBits(
                memory.dc,
                memory.bitmap,
                0,
                rect.height,
                Some(data.as_mut_ptr().cast::<c_void>()),
                &raw mut info,
                DIB_RGB_COLORS,
            )
        };
        if lines <= 0 {
            return Err(WinError::Other("GetDIBits returned no scan lines".into()));
        }

        // GDI leaves the alpha byte at zero; `frame_from_packed` makes it opaque.
        frame_from_packed(
            data,
            Size::new(rect.width, rect.height),
            CaptureFormat::Bgra8,
            &Placement {
                origin: rect.origin(),
                scale_factor: target.monitor.scale_factor,
                hdr: None,
            },
        )
    }
}

/// Draws the current cursor onto `dc`, whose (0, 0) is the desktop point `frame_origin`.
/// Best effort: any failure just leaves the cursor out.
fn draw_cursor(dc: HDC, frame_origin: Point) {
    let mut cursor = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..CURSORINFO::default() };
    // SAFETY: `cbSize` is set as required; valid out-pointer.
    if unsafe { GetCursorInfo(&raw mut cursor) }.is_err() || cursor.flags.0 & CURSOR_SHOWING.0 == 0
    {
        return;
    }
    let icon = HICON(cursor.hCursor.0);
    let mut icon_info = ICONINFO::default();
    // SAFETY: valid handle and out-pointer.
    if unsafe { GetIconInfo(icon, &raw mut icon_info) }.is_err() {
        return;
    }
    let hotspot = Point::new(icon_info.xHotspot.cast_signed(), icon_info.yHotspot.cast_signed());
    // `GetIconInfo` hands over ownership of the two bitmaps.
    // SAFETY: each non-null bitmap is deleted once.
    unsafe {
        if !icon_info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        }
        if !icon_info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        }
    }
    let at = cursor_draw_position(
        Point::new(cursor.ptScreenPos.x, cursor.ptScreenPos.y),
        hotspot,
        frame_origin,
    );
    // SAFETY: `dc` is a valid memory DC and `icon` a valid cursor handle.
    let _ = unsafe { DrawIconEx(dc, at.x, at.y, icon, 0, 0, 0, None, DI_NORMAL) };
}

/// Chain stage wrapper.
pub(crate) struct GdiStage;

impl MonitorCapturer for GdiStage {
    fn name(&self) -> &'static str {
        "gdi"
    }

    fn capture(&self, target: &MonitorTarget, opts: CaptureOptions) -> Result<Frame, WinError> {
        Gdi::capture_monitor(target, opts)
    }
}
