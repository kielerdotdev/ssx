//! Frame sources: where video frames come from.
//!
//! A [`FrameSource`] is *pull based* with a timeout, because that shape fits every
//! backend without forcing threads on the caller:
//!
//! * polling backends (X11, wlroots screencopy) sleep to their frame grid inside
//!   [`FrameSource::next_frame`] and capture one frame per call;
//! * event-driven backends (PipeWire, Windows Graphics Capture) run their own thread or
//!   callback and hand the newest frame over through a one-slot mailbox; `next_frame`
//!   waits on it and returns [`SourceEvent::Timeout`] when the screen did not change
//!   (damage-driven variable frame rate). The session's pacer then repeats the last
//!   frame to keep the output at a constant rate.
//!
//! Sources report their *native* pixel format ([`SourceInfo`]): HDR desktops deliver
//! `Rgba16F` scRGB, which the session tone-maps (GPU or CPU) before encoding, exactly as
//! for screenshots. Timestamps are on the shared [`Clock`] and strictly non-decreasing.

use std::time::Duration;

use ssx_types::{ColorSpace, Frame, PixelFormat, Rect, Size};

use crate::{
    error::SourceError,
    time::{Clock, Fps},
};

pub mod synthetic;

#[cfg(all(unix, not(target_vendor = "apple")))]
pub mod x11;

#[cfg(target_os = "linux")]
pub mod wlroots;

#[cfg(all(target_os = "linux", feature = "portal"))]
pub mod portal;

#[cfg(windows)]
pub mod windows;

pub mod auto;

/// A captured frame with its capture time.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// The pixels in the source's native format.
    pub frame: Frame,
    /// Capture time on the session [`Clock`]. Non-decreasing within one source.
    pub timestamp: Duration,
}

/// What a source produced on one poll.
#[derive(Debug)]
pub enum SourceEvent {
    /// A new frame.
    Frame(VideoFrame),
    /// Nothing new within the timeout (an idle, damage-driven screen).
    Timeout,
    /// The source ended by itself (window closed, stream stopped, synthetic limit).
    Ended,
}

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The whole virtual desktop (or the compositor's choice on portals).
    Desktop,
    /// One monitor by `Monitor::id`.
    Monitor(String),
    /// One window by `WindowInfo::id`.
    Window(String),
    /// A rectangle of the virtual desktop in physical pixels.
    Region(Rect),
    /// Let the user choose (the xdg-desktop-portal picker on GNOME/KDE).
    Pick,
}

/// Options common to all sources.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    /// What to capture.
    pub target: CaptureTarget,
    /// Target frame rate (polling sources pace themselves to it; others use it as a hint).
    pub fps: Fps,
    /// Draw the mouse cursor into the frames.
    pub cursor: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self { target: CaptureTarget::Desktop, fps: Fps::FPS_30, cursor: true }
    }
}

/// What a running source produces. Valid after [`FrameSource::start`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceInfo {
    /// Backend name (`x11-shm`, `wlr-screencopy`, `pipewire`, `wgc`, `synthetic`).
    pub name: &'static str,
    /// Size of the first frame (later frames may differ; the pipeline rescales).
    pub size: Size,
    /// Native pixel format.
    pub format: PixelFormat,
    /// Native colour space.
    pub color_space: ColorSpace,
    /// `true` when frames are HDR (`Rgba16F` scRGB) and need tone mapping.
    pub hdr: bool,
    /// The source only delivers frames when the picture changes.
    pub damage_driven: bool,
}

/// A screen (or synthetic) video source. See the module docs for the contract.
pub trait FrameSource: Send {
    /// Backend name.
    fn name(&self) -> &'static str;

    /// Opens the capture and starts the clock-relative timestamps. Blocks until the first
    /// frame's format is known so [`FrameSource::info`] is valid afterwards. Interactive
    /// sources (the portal picker) may show UI here.
    fn start(&mut self, clock: Clock) -> Result<(), SourceError>;

    /// Format information; only meaningful after a successful [`FrameSource::start`].
    fn info(&self) -> SourceInfo;

    /// Waits up to `timeout` for the next frame.
    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError>;

    /// Stops capturing and releases OS resources. Idempotent.
    fn stop(&mut self);
}

impl<T: FrameSource + ?Sized> FrameSource for Box<T> {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        (**self).start(clock)
    }
    fn info(&self) -> SourceInfo {
        (**self).info()
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        (**self).next_frame(timeout)
    }
    fn stop(&mut self) {
        (**self).stop();
    }
}

/// Crops `region` (desktop coordinates) out of a frame whose `origin` places it on the
/// desktop. Returns the frame unchanged when `region` is `None`.
pub(crate) fn crop_to_region(frame: Frame, region: Option<Rect>) -> Result<Frame, SourceError> {
    let Some(region) = region else { return Ok(frame) };
    if frame.rect() == region {
        return Ok(frame);
    }
    frame
        .crop_desktop(region)
        .map_err(|e| SourceError::InvalidFrame(format!("cropping to {region:?}: {e}")))
}

/// Mailbox holding only the newest value: the hand-off used by event-driven backends.
/// A producer callback overwrites, the consumer takes; overwritten frames are counted.
#[derive(Debug)]
pub(crate) struct Mailbox<T> {
    slot: std::sync::Mutex<MailboxState<T>>,
    cv: std::sync::Condvar,
}

#[derive(Debug)]
struct MailboxState<T> {
    value: Option<T>,
    ended: Option<Result<(), String>>,
    overwritten: u64,
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self {
            slot: std::sync::Mutex::new(MailboxState { value: None, ended: None, overwritten: 0 }),
            cv: std::sync::Condvar::new(),
        }
    }
}

/// Result of [`Mailbox::take`].
#[derive(Debug)]
pub(crate) enum Taken<T> {
    Value(T),
    Timeout,
    Ended(Result<(), String>),
}

impl<T> Mailbox<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxState<T>> {
        self.slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stores `value`, replacing an unread one.
    #[allow(dead_code)] // used by the event-driven backends only
    pub(crate) fn put(&self, value: T) {
        let mut g = self.lock();
        if g.value.replace(value).is_some() {
            g.overwritten += 1;
        }
        drop(g);
        self.cv.notify_one();
    }

    /// Marks the stream ended (`Ok`) or failed (`Err(message)`).
    #[allow(dead_code)] // used by the event-driven backends only
    pub(crate) fn end(&self, result: Result<(), String>) {
        let mut g = self.lock();
        if g.ended.is_none() {
            g.ended = Some(result);
        }
        drop(g);
        self.cv.notify_all();
    }

    /// Frames that were replaced before being read.
    #[allow(dead_code)] // diagnostics of the event-driven backends
    pub(crate) fn overwritten(&self) -> u64 {
        self.lock().overwritten
    }

    /// Takes the newest value, waiting up to `timeout`. A pending value is delivered
    /// before an end/failure is reported.
    pub(crate) fn take(&self, timeout: Duration) -> Taken<T> {
        let deadline = std::time::Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            if let Some(v) = g.value.take() {
                return Taken::Value(v);
            }
            if let Some(e) = g.ended.clone() {
                return Taken::Ended(e);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Taken::Timeout;
            }
            g = self
                .cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_keeps_only_the_newest_value() {
        let m = Mailbox::default();
        m.put(1);
        m.put(2);
        m.put(3);
        assert_eq!(m.overwritten(), 2);
        assert!(matches!(m.take(Duration::ZERO), Taken::Value(3)));
        assert!(matches!(m.take(Duration::from_millis(5)), Taken::Timeout));
    }

    #[test]
    fn mailbox_delivers_pending_value_before_end() {
        let m = Mailbox::default();
        m.put(7);
        m.end(Err("boom".into()));
        assert!(matches!(m.take(Duration::ZERO), Taken::Value(7)));
        assert!(matches!(m.take(Duration::ZERO), Taken::Ended(Err(e)) if e == "boom"));
    }

    #[test]
    fn mailbox_wakes_a_waiting_consumer() {
        let m = std::sync::Arc::new(Mailbox::default());
        let m2 = std::sync::Arc::clone(&m);
        let h = std::thread::spawn(move || m2.take(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(30));
        m.put(9);
        assert!(matches!(h.join().unwrap(), Taken::Value(9)));
    }
}
