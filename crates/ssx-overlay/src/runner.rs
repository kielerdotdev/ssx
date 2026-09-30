//! Helper-process entry point: read a request, run the overlay, write the response.
//!
//! Split from `src/bin/ssx-overlay.rs` so tests can drive it with in-memory pipes.

use std::{
    io::{Read, Write},
    time::Instant,
};

use crate::{
    app::OverlayApp,
    backend,
    error::OverlayError,
    protocol::{PROTOCOL_VERSION, Request, Response, Timing},
    render::PixelView,
    shm::MappedFrame,
};

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

/// Runs one helper session. `t0` is the moment the process started (for timings). Returns
/// the process exit code: 0 whenever a response was written.
pub fn run_helper(t0: Instant, mut stdin: impl Read, mut stdout: impl Write) -> i32 {
    let response = match serve(t0, &mut stdin) {
        Ok((outcome, timing)) => Response::Outcome { outcome, timing },
        Err(e) => Response::Error { message: e.to_string() },
    };
    let ok = matches!(response, Response::Outcome { .. });
    // Ignore write errors: the client is gone, and there is nobody left to tell.
    let _ = serde_json::to_writer(&mut stdout, &response);
    let _ = writeln!(stdout);
    let _ = stdout.flush();
    if ok { 0 } else { 2 }
}

fn serve(
    t0: Instant,
    stdin: &mut impl Read,
) -> Result<(crate::types::OverlayOutcome, Timing), OverlayError> {
    let mut buf = Vec::new();
    stdin.read_to_end(&mut buf)?;
    let req: Request = serde_json::from_slice(&buf)
        .map_err(|e| OverlayError::InvalidInput(format!("unreadable request: {e}")))?;
    if req.version != PROTOCOL_VERSION {
        return Err(OverlayError::InvalidInput(format!(
            "protocol version {} is not supported (this helper speaks {PROTOCOL_VERSION})",
            req.version
        )));
    }
    let mapped = MappedFrame::open(&req.frame.source)?;
    let f = &req.frame;
    let view = PixelView {
        size: ssx_types::Size::new(f.width, f.height),
        stride: f.stride,
        format: f.format,
        color_space: ssx_types::ColorSpace::Srgb,
        data: mapped.as_slice(),
        origin: f.origin,
        scale_factor: f.scale_factor,
    };
    let mut app = OverlayApp::from_view(view, req.monitors, req.windows, req.options)?;
    let ready_ms = ms(t0);
    tracing::debug!(ready_ms, "overlay ready");
    // The mapping stays alive until here; the renderer already holds its own converted copy.
    drop(mapped);
    backend::run(&mut app)?;
    let timing = Timing {
        ready_ms,
        first_frame_ms: app.first_frame_after().map(|d| ready_ms + d.as_secs_f64() * 1000.0),
    };
    let outcome = app.outcome().unwrap_or(crate::types::OverlayOutcome::Cancelled);
    Ok((outcome, timing))
}
