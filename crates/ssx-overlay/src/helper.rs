//! Client side of the helper process: [`select_via_helper`].
//!
//! The helper is a separate executable (`ssx-overlay`) so that the overlay never shares an
//! event loop with the tray daemon, a crash or compositor hiccup cannot take the daemon
//! down, and every X11/Wayland quirk (grabs, layer-shell exclusivity, keyboard focus) stays
//! in a process that lives for a second or two. The cost is one process spawn, which the
//! README's measurements show is negligible next to converting a 4K frame.

use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use ssx_types::PixelFormat;

use crate::{
    error::{HelperError, OverlayError},
    protocol::{FrameDesc, PROTOCOL_VERSION, Request, Response},
    shm::SharedFrame,
    types::{OverlayInput, OverlayOutcome},
};

/// How long the helper may run when `OverlayOptions::timeout_ms` is unset. Generous: the user
/// may leave the overlay up while reading something; a wedged helper is still reaped.
pub const DEFAULT_HELPER_TIMEOUT: Duration = Duration::from_secs(600);

/// Extra time the client waits beyond `OverlayOptions::timeout_ms` before it kills a helper
/// that has not answered (the helper normally times itself out and reports `Cancelled`).
pub const HELPER_KILL_GRACE: Duration = Duration::from_secs(3);

/// Kills and reaps the child when dropped, so no code path (panic, early return, timeout)
/// can leave an overlay covering the screen.
#[derive(Debug)]
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child present until finish()")
    }

    /// Waits up to `grace` for a clean exit, then kills.
    fn finish(mut self, grace: Duration) -> Option<std::process::ExitStatus> {
        let mut c = self.0.take()?;
        let deadline = Instant::now() + grace;
        loop {
            match c.try_wait() {
                Ok(Some(s)) => return Some(s),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = c.kill();
                    return c.wait().ok();
                }
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Runs the overlay in a helper process and returns its outcome.
///
/// The frame is handed over through a sealed memfd (Linux) or a private temporary file, and
/// the rest of the request as JSON on stdin. The call blocks until the user finishes or
/// `input.options.timeout_ms` (default [`DEFAULT_HELPER_TIMEOUT`]) plus [`HELPER_KILL_GRACE`]
/// elapses, in which case the helper is killed and [`HelperError::Timeout`] returned. Dropping the call (panic, early
/// return) also kills the helper.
pub fn select_via_helper(
    input: &OverlayInput,
    helper: &Path,
) -> Result<OverlayOutcome, OverlayError> {
    let (outcome, _) = select_via_helper_timed(input, helper)?;
    Ok(outcome)
}

/// Like [`select_via_helper`] but also returns the helper's start-up timings.
pub fn select_via_helper_timed(
    input: &OverlayInput,
    helper: &Path,
) -> Result<(OverlayOutcome, crate::protocol::Timing), OverlayError> {
    let f = &input.desktop;
    if f.width() == 0 || f.height() == 0 {
        return Err(OverlayError::InvalidInput("the desktop frame is empty".into()));
    }
    if !matches!(f.format(), PixelFormat::Rgba8 | PixelFormat::Bgra8)
        || f.color_space() != ssx_types::ColorSpace::Srgb
    {
        return Err(OverlayError::InvalidInput(format!(
            "the overlay needs an 8-bit sRGB frame, got {:?}/{:?}; tone-map it first",
            f.format(),
            f.color_space()
        )));
    }
    // The helper enforces `timeout_ms` itself and answers `Cancelled`; the client only waits
    // a little longer as a backstop against a wedged helper.
    let timeout = input
        .options
        .timeout_ms
        .map_or(DEFAULT_HELPER_TIMEOUT, |ms| Duration::from_millis(ms) + HELPER_KILL_GRACE);
    let shared = SharedFrame::create(f)?;
    let request = Request {
        version: PROTOCOL_VERSION,
        frame: FrameDesc {
            width: f.width(),
            height: f.height(),
            stride: f.stride(),
            format: f.format(),
            origin: f.origin,
            scale_factor: f.scale_factor,
            source: shared.source(),
        },
        monitors: input.monitors.clone(),
        windows: input.windows.clone(),
        options: input.options.clone(),
    };
    let json =
        serde_json::to_vec(&request).map_err(|e| OverlayError::InvalidInput(e.to_string()))?;

    let child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| HelperError::Spawn { path: helper.to_path_buf(), source })?;
    let mut guard = ChildGuard(Some(child));

    // Feed the request; a helper that dies early makes this fail, which we report through
    // the exit status below rather than as a bare broken pipe.
    if let Some(mut stdin) = guard.child().stdin.take() {
        let _ = stdin.write_all(&json);
    }
    let mut stdout = guard.child().stdout.take().expect("piped");
    let mut stderr = guard.child().stderr.take().expect("piped");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let (etx, erx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let tail = &buf[buf.len().saturating_sub(2048)..];
        let _ = etx.send(String::from_utf8_lossy(tail).trim().to_owned());
    });

    let out = match rx.recv_timeout(timeout) {
        Ok(out) => out,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            drop(guard); // kills and reaps
            return Err(HelperError::Timeout(timeout).into());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Vec::new(),
    };
    let status = guard.finish(Duration::from_secs(5));
    let stderr_tail = erx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    drop(shared);

    let line = String::from_utf8_lossy(&out);
    let line = line.lines().rev().find(|l| l.trim_start().starts_with('{'));
    match line {
        Some(l) => match serde_json::from_str::<Response>(l) {
            Ok(Response::Outcome { outcome, timing }) => Ok((outcome, timing)),
            Ok(Response::Error { message }) => Err(HelperError::Reported(message).into()),
            Err(e) => Err(HelperError::BadAnswer(format!("{e}: {l}")).into()),
        },
        None => Err(HelperError::Crashed {
            status: status.map_or_else(|| "unknown status".into(), |s| s.to_string()),
            stderr: stderr_tail,
        }
        .into()),
    }
}
