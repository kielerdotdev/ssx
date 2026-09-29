//! The two capture protocols, each reduced to "give me one upright BGRA image".
//!
//! Both flows are: learn buffer parameters from the compositor, allocate a matching shm
//! buffer, ask for a copy, wait for `ready`/`failed`, convert. The interesting part is
//! failure handling:
//!
//! * wlr-screencopy: `failed` is retried once (output mode changes between the parameter
//!   event and the copy are the usual cause).
//! * ext-image-copy-capture: `failed(buffer_constraints)` means the constraints changed
//!   under us (resolution switch, rotation). We wait for the fresh constraints, allocate
//!   again and retry; `stopped` is fatal for this source.

use ssx_capture::{CaptureError, Result};
use ssx_types::Rect;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_protocols::ext::{
    image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options,
};

use crate::{
    format::{ShmFormat, to_bgra},
    transform::Transform,
    wl::{ExtProgress, Session, WlrFrame, wl_i32},
};

/// One captured, upright, opaque, tightly packed BGRA image in native pixels.
#[derive(Debug)]
pub(crate) struct RawShot {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// `failure_reason` values of `ext_image_copy_capture_frame_v1`.
const FAIL_BUFFER_CONSTRAINTS: u32 = 1;
const FAIL_STOPPED: u32 = 2;
/// Attempts before giving up on a flapping compositor.
const MAX_ATTEMPTS: u32 = 4;

fn finish(
    buf: &crate::wl::ShmBuffer,
    fmt: ShmFormat,
    (w, h, stride): (u32, u32, u32),
    y_invert: bool,
    transform: Transform,
    proto: &'static str,
) -> Result<RawShot> {
    let data = buf
        .read()
        .map_err(|e| CaptureError::backend(proto, format!("reading the shm buffer failed: {e}")))?;
    let bgra = to_bgra(fmt, &data, w, h, stride as usize, y_invert)?;
    let (bgra, width, height) = transform.undo(bgra, w, h);
    Ok(RawShot { bgra, width, height })
}

/// Captures a whole output (or a logical sub-region of it) with `wlr-screencopy-v1`.
///
/// `region` is in output-local **logical** pixels, exactly as the protocol defines it.
/// `transform` is the output's `wl_output` transform (wlr buffers are in scan-out
/// orientation).
pub(crate) fn wlr_capture(
    s: &mut Session,
    output: &WlOutput,
    transform: Transform,
    cursor: bool,
    region: Option<Rect>,
) -> Result<RawShot> {
    const NAME: &str = "wayland-wlr-screencopy";
    let mgr = s
        .state
        .wlr_manager
        .clone()
        .ok_or_else(|| CaptureError::backend(NAME, "zwlr_screencopy_manager_v1 disappeared"))?;
    let mut last_err = None;
    for _attempt in 0..2 {
        s.state.wlr = WlrFrame::default();
        let overlay = i32::from(cursor);
        let frame = match region {
            Some(r) => mgr.capture_output_region(
                overlay,
                output,
                r.x,
                r.y,
                wl_i32(r.width),
                wl_i32(r.height),
                &s.qh,
                (),
            ),
            None => mgr.capture_output(overlay, output, &s.qh, ()),
        };
        // Protocol v3 announces the end of the parameter list with `buffer_done`; older
        // compositors only ever send the wl_shm `buffer` event.
        let v3 = wayland_client::Proxy::version(&frame) >= 3;
        s.run_until("waiting for wlr-screencopy buffer parameters", |st| {
            st.wlr.failed || if v3 { st.wlr.buffer_done } else { !st.wlr.buffers.is_empty() }
        })?;
        if s.state.wlr.failed {
            frame.destroy();
            last_err = Some(CaptureError::backend(
                NAME,
                "the compositor refused the capture (output disabled or region invalid)",
            ));
            continue;
        }
        let offered: Vec<u32> = s.state.wlr.buffers.iter().map(|b| b.0).collect();
        let Some(fmt) = ShmFormat::choose(&offered, &[]) else {
            frame.destroy();
            return Err(CaptureError::backend(
                NAME,
                format!(
                    "the compositor only offers unsupported buffer formats {offered:x?} \
                     (dma-buf-only capture is not implemented)"
                ),
            ));
        };
        let Some(&(_, w, h, stride)) = s.state.wlr.buffers.iter().find(|b| b.0 == fmt.to_wl())
        else {
            frame.destroy();
            return Err(CaptureError::backend(NAME, "buffer parameters vanished"));
        };
        let buf = s.alloc_shm(w, h, stride, fmt)?;
        frame.copy(&buf.buffer);
        s.run_until("waiting for wlr-screencopy to copy the frame", |st| {
            st.wlr.ready || st.wlr.failed
        })?;
        if s.state.wlr.failed {
            frame.destroy();
            buf.destroy();
            last_err = Some(CaptureError::backend(NAME, "the compositor failed the frame copy"));
            continue;
        }
        let shot = finish(&buf, fmt, (w, h, stride), s.state.wlr.y_invert, transform, NAME);
        frame.destroy();
        buf.destroy();
        s.flush();
        return shot;
    }
    Err(last_err.unwrap_or_else(|| CaptureError::backend(NAME, "capture failed")))
}

/// Captures an image-capture source (output or toplevel) with `ext-image-copy-capture-v1`.
///
/// `fallback_transform` is used if the compositor never sends a `transform` event.
pub(crate) fn ext_capture(
    s: &mut Session,
    source: &ExtImageCaptureSourceV1,
    cursor: bool,
    fallback_transform: Transform,
) -> Result<RawShot> {
    const NAME: &str = "wayland-ext-image-copy-capture";
    let mgr = s
        .state
        .ext_copy_manager
        .clone()
        .ok_or_else(|| CaptureError::backend(NAME, "ext_image_copy_capture_manager_v1 gone"))?;
    s.state.ext = ExtProgress::default();
    let options = if cursor { Options::PaintCursors } else { Options::empty() };
    let session = mgr.create_session(source, options, &s.qh, ());
    let result = ext_session_capture(s, &session, fallback_transform, NAME);
    session.destroy();
    s.flush();
    result
}

fn ext_session_capture(
    s: &mut Session,
    session: &wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
    fallback_transform: Transform,
    name: &'static str,
) -> Result<RawShot> {
    s.run_until("waiting for ext-image-copy-capture buffer constraints", |st| {
        st.ext.constraint_batches >= 1 || st.ext.stopped
    })?;
    for _attempt in 0..MAX_ATTEMPTS {
        if s.state.ext.stopped {
            return Err(CaptureError::backend(
                name,
                "the capture session was stopped by the compositor (source gone?)",
            ));
        }
        let (w, h) = s.state.ext.size;
        let shm_formats = s.state.shm_formats.clone();
        let Some(fmt) = ShmFormat::choose(&s.state.ext.formats, &shm_formats) else {
            return Err(CaptureError::backend(
                name,
                format!(
                    "the compositor only offers unsupported buffer formats {:x?} \
                     (dma-buf-only capture is not implemented)",
                    s.state.ext.formats
                ),
            ));
        };
        let stride = (w as usize).saturating_mul(fmt.bytes_per_pixel());
        let stride = u32::try_from(stride)
            .map_err(|_| CaptureError::backend(name, "buffer stride overflows"))?;
        let buf = s.alloc_shm(w, h, stride, fmt)?;
        let batches_used = s.state.ext.constraint_batches;
        s.state.ext.ready = false;
        s.state.ext.failed = None;
        s.state.ext.transform = fallback_transform;

        let frame = session.create_frame(&s.qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, wl_i32(w), wl_i32(h));
        frame.capture();
        s.run_until("waiting for the compositor to copy the frame", |st| {
            st.ext.ready || st.ext.failed.is_some() || st.ext.stopped
        })?;
        frame.destroy();

        if s.state.ext.ready {
            let shot = finish(&buf, fmt, (w, h, stride), false, s.state.ext.transform, name);
            buf.destroy();
            return shot;
        }
        buf.destroy();
        match s.state.ext.failed {
            Some(FAIL_STOPPED) => {
                return Err(CaptureError::backend(name, "the capture session was stopped"));
            }
            Some(FAIL_BUFFER_CONSTRAINTS) => {
                // Renegotiate: wait until constraints newer than the ones we used arrive.
                s.run_until("waiting for updated buffer constraints", |st| {
                    st.ext.constraint_batches > batches_used || st.ext.stopped
                })?;
            }
            // Unknown runtime error: the protocol says the client may retry.
            _ => {}
        }
    }
    Err(CaptureError::backend(
        name,
        format!("the compositor kept failing the capture ({MAX_ATTEMPTS} attempts)"),
    ))
}
