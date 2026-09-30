//! Screen recording on wlroots compositors (sway, Hyprland, ...): a streaming capture loop
//! over `ext-image-copy-capture-v1` and `wlr-screencopy-unstable-v1`.
//!
//! # Why this is not a loop over `ssx_capture_wayland::WaylandCapture`
//!
//! The still-image backend opens a **fresh connection per call** (registry snapshot, output
//! geometry, `wl_shm` pool, one frame, everything torn down), which keeps concurrent users
//! independent and is right for screenshots. Measured on headless sway (see
//! `tests/wayland.rs`, `cargo test -p ssx-record --test wayland -- --nocapture`), that
//! costs a few milliseconds per frame of pure setup and re-negotiation, and it cannot use
//! the protocols' damage tracking at all. So this module keeps **one connection, one
//! registry, and one shm buffer per output alive for the whole recording**; the public
//! crate is used only once, at start, to resolve monitors and window geometry (the
//! coordinate rules stay in one place).
//!
//! # Protocol handling
//!
//! * **ext-image-copy-capture-v1** (preferred when advertised): one capture *session* per
//!   output for the whole recording. The first frame declares full damage; afterwards the
//!   buffer is declared up to date, so the compositor holds the next frame until the
//!   output actually changes: a damage-driven stream with zero CPU on a static screen.
//! * **wlr-screencopy-unstable-v1**: `capture_output` per frame; with protocol version 2
//!   and a single output `copy_with_damage` is used, which gives the same "wait for a
//!   change" behaviour. Multi-output selections use plain `copy` on all outputs at once
//!   (waiting for damage on *all* outputs would stall on a static one).
//! * Requests are issued no earlier than the frame grid allows (`1/fps`), so a busy screen
//!   never produces more captures than frames wanted.
//!
//! Buffers are converted to tightly packed BGRA, the output transform is undone, and
//! several outputs are stitched with [`ssx_capture::blit`]. Outputs whose buffer size does
//! not equal their desktop-layout rectangle (fractional or mixed scaling) are **not**
//! resampled per frame (that would cost more than the capture); such a selection falls
//! back to the first output with a warning. DMA-BUF-only compositors are reported as
//! [`SourceError::UnsupportedBuffer`].

use std::{
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt, unix::net::UnixStream},
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, ftruncate, memfd_create},
    time::Timespec,
};
use ssx_capture::CaptureBackend;
use ssx_capture_wayland::{Config as WlConfig, OutputInfo, Target, Transform, WaylandCapture};
use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Rect, Size};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::WaylandError,
    delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::{CaptureTarget, FrameSource, SourceConfig, SourceEvent, SourceInfo, VideoFrame};
use crate::{error::SourceError, time::Clock};

const BACKEND: &str = "wayland";
const MAX_SHM_BYTES: usize = 1 << 30;
/// Give up on a frame request after this many consecutive failures.
const MAX_FAILURES: u32 = 5;

/// Which compositor to talk to. `Env` is the normal case; `Path` is for tests.
pub use ssx_capture_wayland::Target as WaylandTarget;

/// The protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `ext-image-copy-capture-v1`.
    Ext,
    /// `wlr-screencopy-unstable-v1`.
    Wlr,
}

fn raw<T: Into<u32>>(v: WEnum<T>) -> u32 {
    match v {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

// ---- pixel formats -----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
    Xrgb2101010,
    Argb2101010,
    Xbgr2101010,
    Abgr2101010,
    Bgr888,
    Rgb888,
}

const fn fourcc(s: [u8; 4]) -> u32 {
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

impl Fmt {
    /// Cheapest first.
    const PREFERENCE: [Fmt; 10] = [
        Fmt::Xrgb8888,
        Fmt::Argb8888,
        Fmt::Xbgr8888,
        Fmt::Abgr8888,
        Fmt::Xrgb2101010,
        Fmt::Xbgr2101010,
        Fmt::Argb2101010,
        Fmt::Abgr2101010,
        Fmt::Bgr888,
        Fmt::Rgb888,
    ];

    const fn from_wl(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Argb8888,
            1 => Self::Xrgb8888,
            c if c == fourcc(*b"AB24") => Self::Abgr8888,
            c if c == fourcc(*b"XB24") => Self::Xbgr8888,
            c if c == fourcc(*b"XR30") => Self::Xrgb2101010,
            c if c == fourcc(*b"AR30") => Self::Argb2101010,
            c if c == fourcc(*b"XB30") => Self::Xbgr2101010,
            c if c == fourcc(*b"AB30") => Self::Abgr2101010,
            c if c == fourcc(*b"BG24") => Self::Bgr888,
            c if c == fourcc(*b"RG24") => Self::Rgb888,
            _ => return None,
        })
    }

    const fn to_wl(self) -> u32 {
        match self {
            Self::Argb8888 => 0,
            Self::Xrgb8888 => 1,
            Self::Abgr8888 => fourcc(*b"AB24"),
            Self::Xbgr8888 => fourcc(*b"XB24"),
            Self::Xrgb2101010 => fourcc(*b"XR30"),
            Self::Argb2101010 => fourcc(*b"AR30"),
            Self::Xbgr2101010 => fourcc(*b"XB30"),
            Self::Abgr2101010 => fourcc(*b"AB30"),
            Self::Bgr888 => fourcc(*b"BG24"),
            Self::Rgb888 => fourcc(*b"RG24"),
        }
    }

    const fn bpp(self) -> usize {
        match self {
            Self::Bgr888 | Self::Rgb888 => 3,
            _ => 4,
        }
    }

    fn choose(offered: &[u32], supported: &[u32]) -> Option<Self> {
        offered
            .iter()
            .filter(|c| supported.is_empty() || supported.contains(c))
            .filter_map(|c| Self::from_wl(*c))
            .min_by_key(|f| Self::PREFERENCE.iter().position(|p| p == f))
    }
}

/// Converts a shm buffer (any supported format, any stride) to tightly packed opaque BGRA.
fn to_bgra(
    fmt: Fmt,
    src: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    y_invert: bool,
) -> Result<Vec<u8>, SourceError> {
    let (wu, hu) = (w as usize, h as usize);
    let row_bytes = wu.checked_mul(fmt.bpp()).filter(|b| *b <= stride);
    let need = row_bytes.and_then(|r| stride.checked_mul(hu.saturating_sub(1)).map(|s| s + r));
    let (Some(row_bytes), Some(need)) = (row_bytes, need) else {
        return Err(SourceError::InvalidFrame(format!("stride {stride} too small for {w}x{h}")));
    };
    if src.len() < need {
        return Err(SourceError::InvalidFrame(format!(
            "buffer holds {} bytes but {need} are needed",
            src.len()
        )));
    }
    let mut out = vec![0u8; wu * hu * 4];
    let to8 = |v: u32| ((v * 255 + 511) / 1023) as u8;
    for y in 0..hu {
        let sy = if y_invert { hu - 1 - y } else { y };
        let s = &src[sy * stride..sy * stride + row_bytes];
        let d = &mut out[y * wu * 4..(y + 1) * wu * 4];
        match fmt {
            Fmt::Xrgb8888 | Fmt::Argb8888 => {
                d.copy_from_slice(s);
                for px in d.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
            Fmt::Xbgr8888 | Fmt::Abgr8888 => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    px.copy_from_slice(&[q[2], q[1], q[0], 255]);
                }
            }
            Fmt::Xrgb2101010 | Fmt::Argb2101010 => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    let v = u32::from_le_bytes([q[0], q[1], q[2], q[3]]);
                    px.copy_from_slice(&[
                        to8(v & 0x3ff),
                        to8((v >> 10) & 0x3ff),
                        to8((v >> 20) & 0x3ff),
                        255,
                    ]);
                }
            }
            Fmt::Xbgr2101010 | Fmt::Abgr2101010 => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    let v = u32::from_le_bytes([q[0], q[1], q[2], q[3]]);
                    px.copy_from_slice(&[
                        to8((v >> 20) & 0x3ff),
                        to8((v >> 10) & 0x3ff),
                        to8(v & 0x3ff),
                        255,
                    ]);
                }
            }
            // DRM naming is by significance: BGR888 is stored R, G, B in memory.
            Fmt::Bgr888 => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(3)) {
                    px.copy_from_slice(&[q[2], q[1], q[0], 255]);
                }
            }
            Fmt::Rgb888 => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(3)) {
                    px.copy_from_slice(&[q[0], q[1], q[2], 255]);
                }
            }
        }
    }
    Ok(out)
}

// ---- dispatch state ------------------------------------------------------------------------

#[derive(Debug)]
struct OutRec {
    global: u32,
    wl: wl_output::WlOutput,
    name: Option<String>,
    transform: Transform,
}

#[derive(Debug, Default)]
struct WlrProgress {
    buffers: Vec<(u32, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Debug, Default)]
struct ExtProgress {
    size: (u32, u32),
    formats: Vec<u32>,
    batches: u32,
    fresh: bool,
    stopped: bool,
    ready: bool,
    failed: Option<u32>,
    transform: Transform,
}

/// Per-output capture progress, indexed by slot; the `gen` in user data discards events of
/// frame objects we already replaced.
#[derive(Debug, Default)]
struct SlotEvents {
    wlr: WlrProgress,
    ext: ExtProgress,
    generation: u64,
    /// ext: constraints were invalidated; wait for a batch newer than this one.
    await_batch: Option<u32>,
    /// ext: the constraint batch the outstanding frame was requested with.
    batch_at_request: u32,
}

struct State {
    shm: Option<wl_shm::WlShm>,
    shm_formats: Vec<u32>,
    outs: Vec<OutRec>,
    wlr: Option<ZwlrScreencopyManagerV1>,
    ext: Option<ExtImageCopyCaptureManagerV1>,
    ext_src: Option<ExtOutputImageCaptureSourceManagerV1>,
    slots: Vec<SlotEvents>,
    syncs: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_output" => {
                    let wl =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                    st.outs.push(OutRec {
                        global: name,
                        wl,
                        name: None,
                        transform: Transform::Normal,
                    });
                }
                "wl_shm" => st.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => {
                    st.wlr = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    st.ext = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    st.ext_src = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outs.iter_mut().find(|o| o.global == *global) else { return };
        match event {
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Geometry { transform, .. } => {
                o.transform = Transform::from_wl(raw(transform));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event {
            st.shm_formats.push(raw(format));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            st.syncs += 1;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, (usize, u64)> for State {
    fn event(
        st: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        (slot, generation): &(usize, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(s) = st.slots.get_mut(*slot) else { return };
        if s.generation != *generation {
            return;
        }
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                s.wlr.buffers.push((raw(format), width, height, stride));
            }
            Event::BufferDone => s.wlr.buffer_done = true,
            Event::Flags { flags } => {
                s.wlr.y_invert = match flags {
                    WEnum::Value(f) => f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
                    WEnum::Unknown(u) => u & 1 != 0,
                };
            }
            Event::Ready { .. } => s.wlr.ready = true,
            Event::Failed => s.wlr.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, usize> for State {
    fn event(
        st: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(s) = st.slots.get_mut(*slot) else { return };
        use ext_image_copy_capture_session_v1::Event;
        let e = &mut s.ext;
        let begin = |e: &mut ExtProgress| {
            if e.fresh {
                e.formats.clear();
                e.fresh = false;
            }
        };
        match event {
            Event::BufferSize { width, height } => {
                begin(e);
                e.size = (width, height);
            }
            Event::ShmFormat { format } => {
                begin(e);
                e.formats.push(raw(format));
            }
            Event::Done => {
                e.batches += 1;
                e.fresh = true;
            }
            Event::Stopped => e.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, (usize, u64)> for State {
    fn event(
        st: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        (slot, generation): &(usize, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(s) = st.slots.get_mut(*slot) else { return };
        if s.generation != *generation {
            return;
        }
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform { transform } => s.ext.transform = Transform::from_wl(raw(transform)),
            Event::Ready => s.ext.ready = true,
            Event::Failed { reason } => s.ext.failed = Some(raw(reason)),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);

// ---- the source ------------------------------------------------------------------------------

struct ShmBuf {
    buffer: wl_buffer::WlBuffer,
    file: File,
    len: usize,
    fmt: Fmt,
    size: (u32, u32),
    stride: u32,
}

struct Slot {
    id: String,
    wl: wl_output::WlOutput,
    transform: Transform,
    /// Desktop rectangle of this output.
    rect: Rect,
    buf: Option<ShmBuf>,
    wlr_frame: Option<ZwlrScreencopyFrameV1>,
    ext_source: Option<ExtImageCaptureSourceV1>,
    ext_session: Option<ExtImageCopyCaptureSessionV1>,
    ext_frame: Option<ExtImageCopyCaptureFrameV1>,
    /// A request is outstanding and its result has not been consumed.
    in_flight: bool,
    /// wlr: `copy` was issued for the current frame object.
    copy_issued: bool,
    /// The buffer holds the previous frame's pixels (no damage needed).
    primed: bool,
    failures: u32,
    result: Option<Result<Frame, SourceError>>,
}

/// One live connection with everything needed to stream frames.
struct Stream {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    st: State,
    protocol: Protocol,
    slots: Vec<Slot>,
    /// Use `copy_with_damage` / hold-until-change semantics (single output only).
    damage: bool,
    cursor: bool,
    canvas: Rect,
    crop: Option<Rect>,
}

/// Streams a wlroots compositor. See the module docs.
pub struct WlrootsSource {
    cfg: SourceConfig,
    target: Target,
    force: Option<Protocol>,
    allow_damage: bool,
    stream: Option<Stream>,
    clock: Option<Clock>,
    next_due: Duration,
    size: Size,
    stopped: bool,
    /// The frame captured by `start` to learn the size; delivered first.
    pending_first: Option<Frame>,
    /// Frames delivered, for the diagnostics.
    delivered: u64,
    warnings: Vec<String>,
}

impl std::fmt::Debug for WlrootsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrootsSource")
            .field("size", &self.size)
            .field("delivered", &self.delivered)
            .finish_non_exhaustive()
    }
}

impl WlrootsSource {
    /// A source for `cfg` on `$WAYLAND_DISPLAY`.
    pub fn new(cfg: SourceConfig) -> Self {
        Self {
            cfg,
            target: Target::Env,
            force: None,
            allow_damage: true,
            stream: None,
            clock: None,
            next_due: Duration::ZERO,
            size: Size::default(),
            stopped: false,
            pending_first: None,
            delivered: 0,
            warnings: Vec::new(),
        }
    }

    /// Connects to an explicit compositor socket (tests).
    pub fn on_target(mut self, target: Target) -> Self {
        self.target = target;
        self
    }

    /// Forces a protocol instead of picking the best advertised one.
    pub fn with_protocol(mut self, p: Protocol) -> Self {
        self.force = Some(p);
        self
    }

    /// Disables damage-driven capture: every request completes at the next output refresh
    /// even when nothing changed (used to measure the raw capture rate).
    pub fn without_damage(mut self) -> Self {
        self.allow_damage = false;
        self
    }

    /// Notes about degraded behaviour (mixed-DPI fallback, static window rectangle).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The protocol in use (after `start`).
    pub fn protocol(&self) -> Option<Protocol> {
        self.stream.as_ref().map(|s| s.protocol)
    }

    /// `true` if `$WAYLAND_DISPLAY` is set and the compositor offers a screen-copy protocol.
    pub fn is_available() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some() && WaylandCapture::detect().is_ok()
    }
}

fn backend_err(msg: impl Into<String>) -> SourceError {
    SourceError::backend(BACKEND, msg)
}

fn map_wl(e: &WaylandError, what: &str) -> SourceError {
    match e {
        WaylandError::Io(io) => {
            backend_err(format!("lost the connection to the compositor while {what}: {io}"))
        }
        WaylandError::Protocol(p) => backend_err(format!(
            "protocol error {} on {}: {} (while {what})",
            p.code, p.object_interface, p.message
        )),
    }
}

impl Stream {
    fn dispatch(&mut self, what: &str) -> Result<(), SourceError> {
        self.queue.dispatch_pending(&mut self.st).map_err(|e| match e {
            wayland_client::DispatchError::Backend(b) => map_wl(&b, what),
            wayland_client::DispatchError::BadMessage { interface, .. } => {
                backend_err(format!("malformed {interface} message"))
            }
        })?;
        Ok(())
    }

    /// Dispatches events until `done` holds or `deadline` passes. `Ok(true)` if `done`.
    fn run_until(
        &mut self,
        what: &str,
        deadline: Instant,
        done: impl Fn(&State) -> bool,
    ) -> Result<bool, SourceError> {
        loop {
            self.dispatch(what)?;
            if done(&self.st) {
                return Ok(true);
            }
            self.conn.flush().map_err(|e| map_wl(&e, what))?;
            let Some(guard) = self.queue.prepare_read() else { continue };
            let Some(remaining) =
                deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
            else {
                return Ok(false);
            };
            let ts = Timespec {
                tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(remaining.subsec_nanos()),
            };
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                match poll(&mut fds, Some(&ts)) {
                    Ok(n) => n > 0,
                    Err(rustix::io::Errno::INTR) => false,
                    Err(e) => return Err(backend_err(format!("poll failed: {e}"))),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(map_wl(&e, what)),
                }
            }
        }
    }

    fn roundtrip(&mut self, what: &str) -> Result<(), SourceError> {
        let target = self.st.syncs + 1;
        let _cb = self.conn.display().sync(&self.qh, ());
        let deadline = Instant::now() + Duration::from_secs(5);
        if self.run_until(what, deadline, |s| s.syncs >= target)? {
            Ok(())
        } else {
            Err(backend_err(format!("timed out while {what} (is the compositor responsive?)")))
        }
    }

    fn alloc(&mut self, fmt: Fmt, w: u32, h: u32, stride: u32) -> Result<ShmBuf, SourceError> {
        let shm = self.st.shm.clone().ok_or_else(|| backend_err("compositor has no wl_shm"))?;
        let len = (stride as usize).saturating_mul(h as usize);
        if len == 0 || len > MAX_SHM_BYTES || i32::try_from(len).is_err() {
            return Err(backend_err(format!("refusing a {w}x{h} (stride {stride}) shm buffer")));
        }
        let fd = memfd_create("ssx-record", MemfdFlags::CLOEXEC)
            .map_err(|e| backend_err(format!("memfd_create failed: {e}")))?;
        ftruncate(&fd, len as u64).map_err(|e| backend_err(format!("ftruncate failed: {e}")))?;
        let pool =
            shm.create_pool(fd.as_fd(), i32::try_from(len).unwrap_or(i32::MAX), &self.qh, ());
        let wl_fmt = wl_shm::Format::try_from(fmt.to_wl())
            .map_err(|()| backend_err(format!("{fmt:?} is not a known wl_shm format")))?;
        let buffer = pool.create_buffer(
            0,
            i32::try_from(w).unwrap_or(i32::MAX),
            i32::try_from(h).unwrap_or(i32::MAX),
            i32::try_from(stride).unwrap_or(i32::MAX),
            wl_fmt,
            &self.qh,
            (),
        );
        pool.destroy();
        Ok(ShmBuf { buffer, file: File::from(fd), len, fmt, size: (w, h), stride })
    }
}

impl Slot {
    /// Reads the buffer, converts to upright BGRA and wraps it in a frame at `rect`.
    fn read_frame(&self, y_invert: bool, transform: Transform) -> Result<Frame, SourceError> {
        let b = self.buf.as_ref().ok_or_else(|| backend_err("no buffer"))?;
        let mut data = vec![0u8; b.len];
        b.file
            .read_exact_at(&mut data, 0)
            .map_err(|e| backend_err(format!("reading the shm buffer: {e}")))?;
        let bgra = to_bgra(b.fmt, &data, b.size.0, b.size.1, b.stride as usize, y_invert)?;
        let (bgra, w, h) = transform.undo(bgra, b.size.0, b.size.1);
        let mut f = Frame::from_raw(
            Size::new(w, h),
            w as usize * 4,
            PixelFormat::Bgra8,
            ColorSpace::Srgb,
            bgra,
        )
        .map_err(|e| SourceError::InvalidFrame(e.to_string()))?;
        f.origin = self.rect.origin();
        Ok(f)
    }
}

fn select_outputs(
    cap: &WaylandCapture,
    target: &CaptureTarget,
    warnings: &mut Vec<String>,
) -> Result<(Vec<OutputInfo>, Option<Rect>), SourceError> {
    let all = cap.outputs()?;
    if all.is_empty() {
        return Err(SourceError::TargetNotFound("the compositor has no enabled outputs".into()));
    }
    let (mut chosen, crop): (Vec<OutputInfo>, Option<Rect>) = match target {
        CaptureTarget::Desktop => (all.clone(), None),
        CaptureTarget::Monitor(id) => {
            let o = all
                .iter()
                .find(|o| &o.id == id)
                .cloned()
                .ok_or_else(|| SourceError::TargetNotFound(format!("monitor {id:?}")))?;
            (vec![o], None)
        }
        CaptureTarget::Region(r) => (intersecting(&all, *r)?, Some(*r)),
        CaptureTarget::Window(id) => {
            let win = cap
                .windows()?
                .into_iter()
                .find(|w| &w.id == id)
                .ok_or_else(|| SourceError::TargetNotFound(format!("window {id:?}")))?;
            warnings.push(
                "window recording crops the window's rectangle at the moment recording starts; \
                 it does not follow the window when it moves"
                    .into(),
            );
            (intersecting(&all, win.rect)?, Some(win.rect))
        }
        CaptureTarget::Pick => {
            return Err(SourceError::Unsupported(
                "the wlroots capture path has no picker; use the xdg-desktop-portal source".into(),
            ));
        }
    };
    // Frames are stitched at native resolution, so an output must be 1:1 with its
    // rectangle in the desktop layout.
    if chosen.iter().any(|o| o.native_size != o.rect.size()) {
        if chosen.len() > 1 || crop.is_some() {
            warnings.push(
                "outputs with different or fractional scales are not resampled per frame; recording the first output only"
                    .into(),
            );
            chosen.truncate(1);
            return Ok((chosen, None));
        }
        // A single output scaled against the layout is fine: we record it natively.
    }
    Ok((chosen, crop))
}

fn intersecting(all: &[OutputInfo], r: Rect) -> Result<Vec<OutputInfo>, SourceError> {
    let v: Vec<OutputInfo> =
        all.iter().filter(|o| o.rect.intersect(r).is_some()).cloned().collect();
    if v.is_empty() {
        Err(SourceError::TargetNotFound(format!("region {r:?} lies outside every output")))
    } else {
        Ok(v)
    }
}

impl FrameSource for WlrootsSource {
    fn name(&self) -> &'static str {
        "wlroots"
    }

    #[allow(clippy::too_many_lines)] // one linear setup sequence: connect, bind, negotiate
    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        let wl_cfg = WlConfig { target: self.target.clone(), ..WlConfig::default() };
        let helper = WaylandCapture::with_config(wl_cfg)?;
        let (chosen, crop) = select_outputs(&helper, &self.cfg.target, &mut self.warnings)?;

        let conn = match &self.target {
            Target::Env => Connection::connect_to_env().map_err(|e| {
                SourceError::Unavailable(format!("cannot connect to a Wayland compositor: {e}"))
            })?,
            Target::Path(p) => {
                let s = UnixStream::connect(p).map_err(|e| {
                    SourceError::Unavailable(format!("cannot connect to {}: {e}", p.display()))
                })?;
                Connection::from_socket(s)
                    .map_err(|e| backend_err(format!("Wayland handshake failed: {e}")))?
            }
        };
        let queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let st = State {
            shm: None,
            shm_formats: Vec::new(),
            outs: Vec::new(),
            wlr: None,
            ext: None,
            ext_src: None,
            slots: Vec::new(),
            syncs: 0,
        };
        let mut s = Stream {
            conn,
            queue,
            qh,
            st,
            protocol: Protocol::Wlr,
            slots: Vec::new(),
            damage: false,
            cursor: self.cfg.cursor,
            canvas: Rect::default(),
            crop,
        };
        s.roundtrip("listing globals")?;
        s.roundtrip("reading outputs")?;

        let has_shm = s.st.shm.is_some();
        let ext_ok = has_shm && s.st.ext.is_some() && s.st.ext_src.is_some();
        let wlr_ok = has_shm && s.st.wlr.is_some();
        s.protocol = match (self.force, ext_ok, wlr_ok) {
            (Some(Protocol::Ext) | None, true, _) => Protocol::Ext,
            (Some(Protocol::Wlr) | None, _, true) => Protocol::Wlr,
            (Some(p), _, _) => {
                return Err(SourceError::Unavailable(format!(
                    "{p:?} was requested but the compositor does not advertise it"
                )));
            }
            (None, false, false) => {
                return Err(SourceError::Unavailable(
                    "the compositor advertises neither ext-image-copy-capture-v1 nor wlr-screencopy-v1; \
                     use the xdg-desktop-portal source on GNOME and KDE"
                        .into(),
                ));
            }
        };

        // Map the chosen outputs to wl_output objects by connector name.
        for o in &chosen {
            let rec =
                s.st.outs
                    .iter()
                    .find(|r| r.name.as_deref() == Some(o.id.as_str()))
                    .or_else(|| (s.st.outs.len() == 1).then(|| &s.st.outs[0]))
                    .ok_or_else(|| SourceError::TargetNotFound(format!("wl_output {}", o.id)))?;
            s.slots.push(Slot {
                id: o.id.clone(),
                wl: rec.wl.clone(),
                transform: rec.transform,
                rect: o.rect,
                buf: None,
                wlr_frame: None,
                ext_source: None,
                ext_session: None,
                ext_frame: None,
                in_flight: false,
                copy_issued: false,
                primed: false,
                failures: 0,
                result: None,
            });
            s.st.slots.push(SlotEvents::default());
        }
        s.damage = self.allow_damage
            && s.slots.len() == 1
            && match s.protocol {
                Protocol::Ext => true,
                Protocol::Wlr => s.st.wlr.as_ref().is_some_and(|m| m.version() >= 2),
            };

        if s.protocol == Protocol::Ext {
            let mgr = s.st.ext.clone().ok_or_else(|| backend_err("ext manager vanished"))?;
            let src_mgr =
                s.st.ext_src.clone().ok_or_else(|| backend_err("ext source manager vanished"))?;
            let options = if self.cfg.cursor { Options::PaintCursors } else { Options::empty() };
            for (i, slot) in s.slots.iter_mut().enumerate() {
                let source = src_mgr.create_source(&slot.wl, &s.qh, ());
                let session = mgr.create_session(&source, options, &s.qh, i);
                slot.ext_source = Some(source);
                slot.ext_session = Some(session);
            }
            let n = s.slots.len();
            let deadline = Instant::now() + Duration::from_secs(5);
            if !s.run_until("waiting for buffer constraints", deadline, |st| {
                (0..n).all(|i| st.slots[i].ext.batches >= 1 || st.slots[i].ext.stopped)
            })? {
                return Err(backend_err(
                    "timed out waiting for ext-image-copy-capture constraints",
                ));
            }
        }

        // The union of the outputs' rectangles is the canvas.
        s.canvas = s.slots.iter().map(|sl| sl.rect).reduce(Rect::union).unwrap_or_default();
        self.clock = Some(clock);
        self.stream = Some(s);
        self.next_due = clock.now();

        // Capture one frame now to learn the real output size (this also validates the
        // whole path before the session commits to an encoder).
        let first = self.capture_blocking(Duration::from_secs(5))?;
        self.size = first.size();
        self.pending_first = Some(first);
        Ok(())
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            name: "wlroots",
            size: self.size,
            format: PixelFormat::Bgra8,
            color_space: ColorSpace::Srgb,
            hdr: false,
            damage_driven: self.stream.as_ref().is_some_and(|s| s.damage),
            realtime: true,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        let Some(clock) = self.clock else {
            return Err(backend_err("next_frame before start"));
        };
        if self.stopped {
            return Ok(SourceEvent::Ended);
        }
        if let Some(mut f) = self.pending_first.take() {
            let ts = clock.now();
            f.timestamp = Some(ts);
            self.next_due = ts + self.cfg.fps.frame_duration();
            self.delivered += 1;
            return Ok(SourceEvent::Frame(VideoFrame { frame: f, timestamp: ts }));
        }
        let deadline = Instant::now() + timeout;
        let step = self.cfg.fps.frame_duration();
        let mut stream = self.stream.take().ok_or_else(|| backend_err("not started"))?;
        let result = (|| -> Result<Option<Frame>, SourceError> {
            // Do not ask for a frame before the grid allows it.
            let wait = self.next_due.saturating_sub(clock.now());
            if wait > timeout && !stream.slots.iter().any(|s| s.in_flight) {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            if !stream.slots.iter().any(|s| s.in_flight) {
                clock.sleep_until(self.next_due.saturating_sub(Duration::from_micros(500)));
                stream.request_all()?;
            }
            match stream.poll_frame(deadline)? {
                Some(f) => Ok(Some(f)),
                None => Ok(None),
            }
        })();
        self.stream = Some(stream);
        match result? {
            None => Ok(SourceEvent::Timeout),
            Some(mut frame) => {
                let ts = clock.now();
                frame.timestamp = Some(ts);
                self.next_due =
                    if ts > self.next_due + step { ts + step } else { self.next_due + step };
                self.delivered += 1;
                Ok(SourceEvent::Frame(VideoFrame { frame, timestamp: ts }))
            }
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        if let Some(mut s) = self.stream.take() {
            s.teardown();
        }
    }
}

impl WlrootsSource {
    /// Requests every output and waits for the assembled frame (startup only).
    fn capture_blocking(&mut self, timeout: Duration) -> Result<Frame, SourceError> {
        let s = self.stream.as_mut().ok_or_else(|| backend_err("not started"))?;
        s.request_all()?;
        let deadline = Instant::now() + timeout;
        s.poll_frame(deadline)?.ok_or_else(|| {
            backend_err("the compositor did not deliver a first frame (is an output enabled?)")
        })
    }
}

impl Stream {
    /// Issues one capture request per output.
    fn request_all(&mut self) -> Result<(), SourceError> {
        for i in 0..self.slots.len() {
            self.request(i)?;
        }
        self.conn.flush().map_err(|e| map_wl(&e, "requesting a frame"))?;
        Ok(())
    }

    fn request(&mut self, i: usize) -> Result<(), SourceError> {
        if self.slots[i].in_flight {
            return Ok(());
        }
        self.st.slots[i].generation += 1;
        self.slots[i].copy_issued = false;
        let generation = self.st.slots[i].generation;
        match self.protocol {
            Protocol::Wlr => {
                self.st.slots[i].wlr = WlrProgress::default();
                let mgr = self.st.wlr.clone().ok_or_else(|| backend_err("wlr manager vanished"))?;
                let frame = mgr.capture_output(
                    i32::from(self.cursor),
                    &self.slots[i].wl,
                    &self.qh,
                    (i, generation),
                );
                self.slots[i].wlr_frame = Some(frame);
            }
            Protocol::Ext => {
                self.st.slots[i].ext.ready = false;
                self.st.slots[i].ext.failed = None;
                match self.st.slots[i].await_batch {
                    // Waiting for renegotiated constraints; `advance_ext` re-requests.
                    Some(b) if self.st.slots[i].ext.batches <= b => {}
                    _ => {
                        self.st.slots[i].await_batch = None;
                        self.issue_ext(i, generation)?;
                    }
                }
            }
        }
        self.slots[i].in_flight = true;
        Ok(())
    }

    /// Attaches the buffer and starts an ext capture (allocating it on first use).
    fn issue_ext(&mut self, i: usize, generation: u64) -> Result<(), SourceError> {
        let (w, h) = self.st.slots[i].ext.size;
        let fmt = Fmt::choose(&self.st.slots[i].ext.formats, &self.st.shm_formats).ok_or_else(|| {
            SourceError::UnsupportedBuffer(format!(
                "the compositor only offers buffer formats {:x?} (DMA-BUF-only capture is not implemented)",
                self.st.slots[i].ext.formats
            ))
        })?;
        let stride =
            u32::try_from(w as usize * fmt.bpp()).map_err(|_| backend_err("stride overflow"))?;
        let need_new = self.slots[i]
            .buf
            .as_ref()
            .is_none_or(|b| b.fmt != fmt || b.size != (w, h) || b.stride != stride);
        if need_new {
            if let Some(old) = self.slots[i].buf.take() {
                old.buffer.destroy();
            }
            let b = self.alloc(fmt, w, h, stride)?;
            self.slots[i].buf = Some(b);
            self.slots[i].primed = false;
        }
        let session =
            self.slots[i].ext_session.clone().ok_or_else(|| backend_err("no ext session"))?;
        let frame = session.create_frame(&self.qh, (i, generation));
        if let Some(b) = &self.slots[i].buf {
            frame.attach_buffer(&b.buffer);
        }
        if !self.slots[i].primed || !self.damage {
            // A fresh (or stale) buffer: everything is out of date, capture immediately.
            frame.damage_buffer(
                0,
                0,
                i32::try_from(w).unwrap_or(i32::MAX),
                i32::try_from(h).unwrap_or(i32::MAX),
            );
        }
        frame.capture();
        self.slots[i].ext_frame = Some(frame);
        Ok(())
    }

    /// Waits until every requested output has a result, then assembles them. `Ok(None)`
    /// on timeout (the requests stay in flight).
    fn poll_frame(&mut self, deadline: Instant) -> Result<Option<Frame>, SourceError> {
        loop {
            // Progress every in-flight slot as far as events allow.
            for i in 0..self.slots.len() {
                if self.slots[i].in_flight {
                    self.advance(i)?;
                }
            }
            if self.slots.iter().all(|s| !s.in_flight) {
                return self.assemble().map(Some);
            }
            // Wake up only for events that let some slot make progress.
            let watch: Vec<(bool, bool)> =
                self.slots.iter().map(|s| (s.in_flight, s.copy_issued)).collect();
            let progressed = self.run_until("waiting for a frame", deadline, |st| {
                watch.iter().enumerate().any(|(i, (in_flight, copy_issued))| {
                    let s = &st.slots[i];
                    *in_flight
                        && (s.wlr.ready
                            || s.wlr.failed
                            || (!*copy_issued && (s.wlr.buffer_done || !s.wlr.buffers.is_empty()))
                            || s.ext.ready
                            || s.ext.failed.is_some()
                            || s.ext.stopped
                            || s.await_batch.is_some_and(|b| s.ext.batches > b))
                })
            })?;
            if !progressed {
                return Ok(None);
            }
        }
    }

    /// Consumes what the compositor has reported so far for slot `i`.
    fn advance(&mut self, i: usize) -> Result<(), SourceError> {
        match self.protocol {
            Protocol::Wlr => self.advance_wlr(i),
            Protocol::Ext => self.advance_ext(i),
        }
    }

    fn advance_wlr(&mut self, i: usize) -> Result<(), SourceError> {
        if self.st.slots[i].wlr.failed {
            return self.wlr_failed(i);
        }
        if self.st.slots[i].wlr.ready {
            let y_invert = self.st.slots[i].wlr.y_invert;
            let frame = self.slots[i].read_frame(y_invert, self.slots[i].transform);
            if let Some(f) = self.slots[i].wlr_frame.take() {
                f.destroy();
            }
            self.slots[i].in_flight = false;
            self.slots[i].copy_issued = false;
            self.slots[i].failures = 0;
            self.slots[i].result = Some(frame);
            return Ok(());
        }
        if self.slots[i].copy_issued {
            return Ok(());
        }
        // Buffer parameters known? (protocol v3 ends the list with `buffer_done`.)
        let ev = &self.st.slots[i].wlr;
        let v3 = self.slots[i].wlr_frame.as_ref().is_some_and(|f| f.version() >= 3);
        if !(if v3 { ev.buffer_done } else { !ev.buffers.is_empty() }) {
            return Ok(());
        }
        let offered: Vec<u32> = ev.buffers.iter().map(|b| b.0).collect();
        let fmt = Fmt::choose(&offered, &[]).ok_or_else(|| {
            SourceError::UnsupportedBuffer(format!(
                "the compositor only offers buffer formats {offered:x?} (DMA-BUF-only capture is not implemented)"
            ))
        })?;
        let &(_, w, h, stride) = ev
            .buffers
            .iter()
            .find(|b| b.0 == fmt.to_wl())
            .ok_or_else(|| backend_err("buffer parameters vanished"))?;
        let need_new = self.slots[i]
            .buf
            .as_ref()
            .is_none_or(|b| b.fmt != fmt || b.size != (w, h) || b.stride != stride);
        if need_new {
            if let Some(old) = self.slots[i].buf.take() {
                old.buffer.destroy();
            }
            let b = self.alloc(fmt, w, h, stride)?;
            self.slots[i].buf = Some(b);
        }
        if let (Some(frame), Some(b)) = (&self.slots[i].wlr_frame, &self.slots[i].buf) {
            if self.damage {
                frame.copy_with_damage(&b.buffer);
            } else {
                frame.copy(&b.buffer);
            }
        }
        self.slots[i].copy_issued = true;
        self.conn.flush().map_err(|e| map_wl(&e, "copying a frame"))?;
        Ok(())
    }

    fn wlr_failed(&mut self, i: usize) -> Result<(), SourceError> {
        if let Some(f) = self.slots[i].wlr_frame.take() {
            f.destroy();
        }
        self.slots[i].in_flight = false;
        self.slots[i].copy_issued = false;
        self.slots[i].failures += 1;
        if self.slots[i].failures >= MAX_FAILURES {
            return Err(backend_err(format!(
                "the compositor keeps failing the capture of {} (output disabled?)",
                self.slots[i].id
            )));
        }
        // Retry: the output mode probably changed between the parameters and the copy.
        self.request(i)
    }

    fn advance_ext(&mut self, i: usize) -> Result<(), SourceError> {
        let e = &self.st.slots[i].ext;
        if e.stopped {
            return Err(backend_err(format!(
                "the capture session of {} was stopped by the compositor",
                self.slots[i].id
            )));
        }
        if e.ready {
            let transform = e.transform;
            let frame = self.slots[i].read_frame(false, transform);
            if let Some(f) = self.slots[i].ext_frame.take() {
                f.destroy();
            }
            self.slots[i].in_flight = false;
            self.slots[i].failures = 0;
            // The buffer now holds the latest picture: later captures may wait for damage.
            self.slots[i].primed = true;
            self.slots[i].result = Some(frame);
            return Ok(());
        }
        if let Some(reason) = e.failed {
            if let Some(f) = self.slots[i].ext_frame.take() {
                f.destroy();
            }
            self.slots[i].in_flight = false;
            self.slots[i].primed = false;
            self.slots[i].failures += 1;
            if self.slots[i].failures >= MAX_FAILURES || reason == 2 {
                return Err(backend_err(format!(
                    "the compositor failed the capture of {} (reason {reason})",
                    self.slots[i].id
                )));
            }
            // Constraints changed (resolution switch): wait for the new ones, then retry.
            // New constraints may already have arrived (before the failure): then the
            // check below re-requests right away.
            self.st.slots[i].await_batch = Some(self.st.slots[i].batch_at_request);
            self.st.slots[i].ext.failed = None;
            self.slots[i].in_flight = true;
            return Ok(());
        }
        if let Some(b) = self.st.slots[i].await_batch
            && self.st.slots[i].ext.batches > b
        {
            self.slots[i].in_flight = false;
            self.st.slots[i].await_batch = None;
            return self.request(i);
        }
        Ok(())
    }

    /// Stitches the per-output results into one frame and crops to the region.
    fn assemble(&mut self) -> Result<Frame, SourceError> {
        let mut frames = Vec::with_capacity(self.slots.len());
        for s in &mut self.slots {
            match s.result.take() {
                Some(Ok(f)) => frames.push(f),
                Some(Err(e)) => return Err(e),
                None => return Err(backend_err("an output produced no frame")),
            }
        }
        let mut canvas = if frames.len() == 1 {
            frames.remove(0)
        } else {
            let mut c = Frame::new(self.canvas.size(), PixelFormat::Bgra8, ColorSpace::Srgb);
            c.origin = self.canvas.origin();
            for f in &frames {
                ssx_capture::blit(&mut c, f)
                    .map_err(|e| SourceError::InvalidFrame(format!("stitching outputs: {e}")))?;
            }
            c
        };
        if let Some(region) = self.crop
            && let Some(r) = region.intersect(canvas.rect())
        {
            canvas = super::crop_to_region(canvas, Some(r))?;
        }
        canvas.origin = Point::default();
        Ok(canvas)
    }

    fn teardown(&mut self) {
        for s in &mut self.slots {
            if let Some(f) = s.wlr_frame.take() {
                f.destroy();
            }
            if let Some(f) = s.ext_frame.take() {
                f.destroy();
            }
            if let Some(x) = s.ext_session.take() {
                x.destroy();
            }
            if let Some(x) = s.ext_source.take() {
                x.destroy();
            }
            if let Some(b) = s.buf.take() {
                b.buffer.destroy();
            }
        }
        // Errors mean the connection is gone; nothing left to release.
        let _ = self.conn.flush();
    }
}
