//! X11 screen recording: a paced MIT-SHM `GetImage` loop.
//!
//! [`ssx_capture_x11::X11Capture`] already keeps one connection and one shared-memory
//! segment alive between calls (`Session` reuse, `memfd` attached with `ShmAttachFd`), so
//! a capture *loop* is simply repeated `capture_*` calls; no per-frame connection setup
//! happens. What this module adds is pacing:
//!
//! * a **frame grid**: the next capture is due at `previous due + 1/fps`. When the machine
//!   falls behind, missed slots are skipped (a screen recorder wants the *current* screen,
//!   not a burst of catch-up captures);
//! * `next_frame(timeout)` sleeps to the deadline with [`Clock::sleep_until`] and returns
//!   [`SourceEvent::Timeout`] instead if the next slot is beyond `timeout`.
//!
//! Cursor: XFixes `GetCursorImage` blended into the frame by the capture crate on every
//! call when [`SourceConfig::cursor`] is set. XDamage is *not* used: `GetImage` of the
//! root window costs about the same as a damage round trip on the local server and the
//! pacer already turns static content into cheap duplicates downstream.
//!
//! Monitor and window selections are re-resolved by id on every call by the capture
//! crate, so a window that disappears surfaces as an error (the session ends gracefully).

use std::time::Duration;

use ssx_capture::{CaptureBackend, CaptureOptions};
use ssx_capture_x11::{X11Capture, X11Config};
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

use super::{CaptureTarget, FrameSource, SourceConfig, SourceEvent, SourceInfo, VideoFrame};
use crate::{error::SourceError, time::Clock};

/// Records an X11 (or XWayland) display.
#[derive(Debug)]
pub struct X11Source {
    cfg: SourceConfig,
    display: Option<String>,
    cap: Option<X11Capture>,
    clock: Option<Clock>,
    next_due: Duration,
    /// The frame captured by `start` to learn the size; delivered first.
    first: Option<Frame>,
    size: Size,
    stopped: bool,
}

impl X11Source {
    /// A source for `cfg` on `$DISPLAY`.
    pub fn new(cfg: SourceConfig) -> Self {
        Self {
            cfg,
            display: None,
            cap: None,
            clock: None,
            next_due: Duration::ZERO,
            first: None,
            size: Size::default(),
            stopped: false,
        }
    }

    /// Uses an explicit display name (`":1"`) instead of `$DISPLAY`.
    pub fn on_display(mut self, display: impl Into<String>) -> Self {
        self.display = Some(display.into());
        self
    }

    /// `true` if an X server is reachable (`$DISPLAY` set), without connecting.
    pub fn is_available() -> bool {
        std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
    }

    fn grab(&self) -> Result<Frame, SourceError> {
        let cap = self.cap.as_ref().ok_or_else(|| SourceError::backend("x11", "not started"))?;
        let opts = CaptureOptions { include_cursor: self.cfg.cursor };
        let frame = match &self.cfg.target {
            CaptureTarget::Desktop | CaptureTarget::Pick => cap.capture_desktop(&opts),
            CaptureTarget::Monitor(id) => cap.capture_monitor(id, &opts),
            CaptureTarget::Window(id) => cap.capture_window(id, &opts),
            CaptureTarget::Region(r) => cap.capture_region(*r, &opts),
        }?;
        Ok(frame)
    }
}

impl FrameSource for X11Source {
    fn name(&self) -> &'static str {
        "x11-shm"
    }

    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        let cfg = match &self.display {
            Some(d) => X11Config::for_display(d.clone()),
            None => X11Config::default(),
        };
        self.cap = Some(X11Capture::with_config(cfg)?);
        self.clock = Some(clock);
        let first = self.grab()?;
        self.size = first.size();
        self.first = Some(first);
        self.next_due = clock.now();
        self.stopped = false;
        Ok(())
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            name: "x11-shm",
            size: self.size,
            format: PixelFormat::Bgra8,
            color_space: ColorSpace::Srgb,
            hdr: false,
            damage_driven: false,
            realtime: true,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        let Some(clock) = self.clock else {
            return Err(SourceError::backend("x11", "next_frame before start"));
        };
        if self.stopped {
            return Ok(SourceEvent::Ended);
        }
        if let Some(mut f) = self.first.take() {
            let ts = clock.now();
            f.timestamp = Some(ts);
            self.next_due = ts + self.cfg.fps.frame_duration();
            return Ok(SourceEvent::Frame(VideoFrame { frame: f, timestamp: ts }));
        }
        let deadline = clock.now() + timeout;
        if self.next_due > deadline {
            clock.sleep_until(deadline);
            return Ok(SourceEvent::Timeout);
        }
        clock.sleep_until(self.next_due);
        let mut frame = self.grab()?;
        let ts = clock.now();
        frame.timestamp = Some(ts);
        let step = self.cfg.fps.frame_duration();
        // On the grid, unless we are more than one frame late: then skip the missed slots.
        self.next_due = if ts > self.next_due + step { ts + step } else { self.next_due + step };
        Ok(SourceEvent::Frame(VideoFrame { frame, timestamp: ts }))
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.cap = None;
        self.first = None;
    }
}
