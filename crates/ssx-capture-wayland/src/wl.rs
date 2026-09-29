//! One Wayland connection, its registry snapshot and the event plumbing.
//!
//! **Why a connection per operation.** Every public capture call opens its own
//! [`Session`]: a private socket, event queue and registry snapshot. That makes
//! concurrent `WaylandCapture` users trivially independent (no shared read guard, no
//! events stolen by another thread's queue) and guarantees that monitor geometry is
//! read fresh, so output hotplug between two calls surfaces as `NotFound` instead of a
//! stale capture. Connecting to a compositor is sub-millisecond, negligible next to
//! copying a frame.
//!
//! **Why hand-rolled dispatch.** `EventQueue::blocking_dispatch` has no timeout. Here
//! every wait goes through [`Session::run_until`], which polls the socket with a
//! deadline, so a wedged compositor yields a timeout error rather than a hung thread.

use std::{
    fs::File,
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, ftruncate, memfd_create},
    time::Timespec,
};
use ssx_capture::{CaptureError, Result};
use ssx_types::{Rect, Size};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::WaylandError,
    delegate_noop, event_created_child,
    protocol::{wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::{
    ext::{
        foreign_toplevel_list::v1::client::{
            ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
            ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
        },
        image_capture_source::v1::client::{
            ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
            ext_image_capture_source_v1::ExtImageCaptureSourceV1,
            ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        },
        image_copy_capture::v1::client::{
            ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
            ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
            ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{coords::OutputGeom, format::ShmFormat, transform::Transform};

/// Wayland sizes are `i32` on the wire; saturate instead of wrapping.
pub(crate) fn wl_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Backend label used in errors that occur before a protocol has been chosen.
pub(crate) const BACKEND: &str = "wayland";

/// Refuse to allocate shm buffers larger than this (a hostile or buggy compositor could
/// advertise absurd dimensions).
const MAX_SHM_BYTES: usize = 1 << 30;

/// Where to connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Target {
    /// `$WAYLAND_DISPLAY` / `$WAYLAND_SOCKET`.
    #[default]
    Env,
    /// An explicit socket path (used by tests to reach a private compositor).
    Path(PathBuf),
}

/// An advertised global.
#[derive(Debug, Clone)]
pub(crate) struct Global {
    pub name: u32,
    pub interface: String,
}

/// Everything learned about one `wl_output`.
#[derive(Debug)]
pub(crate) struct OutputRec {
    pub global: u32,
    pub wl: wl_output::WlOutput,
    pub xdg: Option<ZxdgOutputV1>,
    pub geo_x: i32,
    pub geo_y: i32,
    pub phys_mm: (i32, i32),
    pub make: String,
    pub model: String,
    pub transform: Transform,
    /// Current mode in scan-out orientation, and refresh in mHz.
    pub mode: Option<(u32, u32, i32)>,
    pub int_scale: i32,
    pub name: Option<String>,
    pub description: Option<String>,
    pub logical_pos: Option<(i32, i32)>,
    pub logical_size: Option<(i32, i32)>,
}

impl OutputRec {
    /// Connector name, falling back to something unique-per-global.
    pub fn id(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("wl_output-{}", self.global))
    }

    /// The output as the coordinate model wants it; `None` for disabled outputs.
    pub fn geom(&self) -> Option<OutputGeom> {
        let (mw, mh, _) = self.mode?;
        if mw == 0 || mh == 0 {
            return None;
        }
        let scale = self.int_scale.max(1);
        let logical = match (self.logical_pos, self.logical_size) {
            (Some((x, y)), Some((w, h))) if w > 0 && h > 0 => Rect::new(x, y, w as u32, h as u32),
            _ => {
                // No xdg-output: derive the logical size from the mode and integer scale.
                let (dw, dh) = self.transform.upright_size(mw, mh);
                Rect::new(
                    self.geo_x,
                    self.geo_y,
                    (dw / scale as u32).max(1),
                    (dh / scale as u32).max(1),
                )
            }
        };
        Some(OutputGeom {
            id: self.id(),
            logical,
            mode: Size::new(mw, mh),
            transform: self.transform,
            int_scale: scale,
        })
    }
}

/// wlr-screencopy frame progress.
#[derive(Debug, Default)]
pub(crate) struct WlrFrame {
    /// `(wl_shm format, width, height, stride)` for each `buffer` event.
    pub buffers: Vec<(u32, u32, u32, u32)>,
    pub buffer_done: bool,
    pub y_invert: bool,
    pub ready: bool,
    pub failed: bool,
}

/// ext-image-copy-capture session + frame progress.
#[derive(Debug, Default)]
pub(crate) struct ExtProgress {
    pub size: (u32, u32),
    pub formats: Vec<u32>,
    /// Incremented by each `done` (a new batch of constraints).
    pub constraint_batches: u32,
    /// The next constraint event starts a new batch and clears the old one.
    fresh: bool,
    pub stopped: bool,
    pub ready: bool,
    /// `failure_reason` of a failed frame.
    pub failed: Option<u32>,
    pub transform: Transform,
}

impl ExtProgress {
    /// First constraint event after a `done` discards the previous batch's formats.
    fn begin_batch(&mut self) {
        if self.fresh {
            self.formats.clear();
            self.fresh = false;
        }
    }
}

/// A toplevel from `ext-foreign-toplevel-list-v1`.
#[derive(Debug)]
pub(crate) struct ToplevelRec {
    pub handle: ExtForeignToplevelHandleV1,
    pub title: String,
    pub app_id: String,
    pub identifier: String,
    pub done: bool,
    pub closed: bool,
}

/// Dispatch state for one connection.
pub(crate) struct State {
    registry: wl_registry::WlRegistry,
    pub globals: Vec<Global>,
    pub outputs: Vec<OutputRec>,
    pub shm: Option<wl_shm::WlShm>,
    pub shm_formats: Vec<u32>,
    pub xdg_manager: Option<ZxdgOutputManagerV1>,
    pub wlr_manager: Option<ZwlrScreencopyManagerV1>,
    pub ext_copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    pub ext_output_source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub ext_toplevel_source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    pub toplevel_list: Option<ExtForeignToplevelListV1>,
    pub toplevels: Vec<ToplevelRec>,
    syncs_done: u32,
    pub wlr: WlrFrame,
    pub ext: ExtProgress,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").field("globals", &self.globals.len()).finish()
    }
}

impl State {
    pub fn has_global(&self, interface: &str) -> bool {
        self.globals.iter().any(|g| g.interface == interface)
    }

    fn output_mut(&mut self, global: u32) -> Option<&mut OutputRec> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }
}

fn raw<T: Into<u32>>(v: WEnum<T>) -> u32 {
    match v {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                match interface.as_str() {
                    "wl_output" => {
                        let wl = registry.bind::<wl_output::WlOutput, _, _>(
                            name,
                            version.min(4),
                            qh,
                            name,
                        );
                        state.outputs.push(OutputRec {
                            global: name,
                            wl,
                            xdg: None,
                            geo_x: 0,
                            geo_y: 0,
                            phys_mm: (0, 0),
                            make: String::new(),
                            model: String::new(),
                            transform: Transform::Normal,
                            mode: None,
                            int_scale: 1,
                            name: None,
                            description: None,
                            logical_pos: None,
                            logical_size: None,
                        });
                    }
                    "wl_shm" => {
                        state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "zxdg_output_manager_v1" => {
                        state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                    }
                    "zwlr_screencopy_manager_v1" => {
                        state.wlr_manager = Some(registry.bind(name, version.min(3), qh, ()));
                    }
                    "ext_image_copy_capture_manager_v1" => {
                        state.ext_copy_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "ext_output_image_capture_source_manager_v1" => {
                        state.ext_output_source_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                        state.ext_toplevel_source_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    _ => {}
                }
                state.globals.push(Global { name, interface });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.retain(|g| g.name != name);
                if let Some(pos) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(pos);
                    if let Some(x) = &o.xdg {
                        x.destroy();
                    }
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else { return };
        match event {
            wl_output::Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                make,
                model,
                transform,
                ..
            } => {
                o.geo_x = x;
                o.geo_y = y;
                o.phys_mm = (physical_width, physical_height);
                o.make = make;
                o.model = model;
                o.transform = Transform::from_wl(raw(transform));
            }
            wl_output::Event::Mode { flags, width, height, refresh } => {
                let current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    WEnum::Unknown(u) => u & 1 != 0,
                };
                // Only the current mode matters. If a compositor sends just one mode
                // without the flag, accept it.
                if current || o.mode.is_none() {
                    o.mode = Some((width.max(0) as u32, height.max(0) as u32, refresh));
                }
            }
            wl_output::Event::Scale { factor } => o.int_scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Description { description } => o.description = Some(description),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else { return };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height));
            }
            // wl_output v4 also sends the name; prefer whichever arrives first.
            zxdg_output_v1::Event::Name { name } => {
                o.name.get_or_insert(name);
            }
            zxdg_output_v1::Event::Description { description } => {
                o.description.get_or_insert(description);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event {
            state.shm_formats.push(raw(format));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.syncs_done += 1;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                state.wlr.buffers.push((raw(format), width, height, stride));
            }
            Event::BufferDone => state.wlr.buffer_done = true,
            Event::Flags { flags } => {
                state.wlr.y_invert = match flags {
                    WEnum::Value(f) => f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
                    WEnum::Unknown(u) => u & 1 != 0,
                };
            }
            Event::Ready { .. } => state.wlr.ready = true,
            Event::Failed => state.wlr.failed = true,
            // Damage and linux_dmabuf are irrelevant for a single shm copy.
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let e = &mut state.ext;
        match event {
            Event::BufferSize { width, height } => {
                e.begin_batch();
                e.size = (width, height);
            }
            Event::ShmFormat { format } => {
                e.begin_batch();
                e.formats.push(raw(format));
            }
            Event::Done => {
                e.constraint_batches += 1;
                e.fresh = true;
            }
            Event::Stopped => e.stopped = true,
            // dma-buf constraints are not used.
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform { transform } => {
                state.ext.transform = Transform::from_wl(raw(transform));
            }
            Event::Ready => state.ext.ready = true,
            Event::Failed { reason } => state.ext.failed = Some(raw(reason)),
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(ToplevelRec {
                handle: toplevel,
                title: String::new(),
                app_id: String::new(),
                identifier: String::new(),
                done: false,
                closed: false,
            });
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(t) = state.toplevels.iter_mut().find(|t| t.handle.id() == handle.id()) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                t.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Done => t.done = true,
            ext_foreign_toplevel_handle_v1::Event::Closed => t.closed = true,
            _ => {}
        }
    }
}

// Objects that never send events we care about.
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);

/// Maps a low-level Wayland error to the crate's error type.
fn map_wl_error(e: &WaylandError, what: &str) -> CaptureError {
    match e {
        WaylandError::Io(io) => CaptureError::backend(
            BACKEND,
            format!("lost the connection to the compositor while {what}: {io}"),
        ),
        WaylandError::Protocol(p) => CaptureError::backend(
            BACKEND,
            format!(
                "protocol error {} on {}: {} (while {what})",
                p.code, p.object_interface, p.message
            ),
        ),
    }
}

/// A shared-memory buffer the compositor writes into.
///
/// Pixels are read back with `pread` on the memfd rather than through an `mmap`: the
/// compositor's writes are visible through the shared page cache, and it keeps this crate
/// free of `unsafe`. The extra copy is dwarfed by the pixel conversion that follows.
#[derive(Debug)]
pub(crate) struct ShmBuffer {
    pub buffer: wl_buffer::WlBuffer,
    file: File,
    len: usize,
}

impl ShmBuffer {
    /// Reads the whole buffer.
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut v = vec![0u8; self.len];
        self.file.read_exact_at(&mut v, 0)?;
        Ok(v)
    }

    pub fn destroy(self) {
        self.buffer.destroy();
    }
}

/// A connection with its own event queue and registry snapshot.
pub(crate) struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    pub qh: QueueHandle<State>,
    pub state: State,
    timeout: Duration,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    /// Connects, enumerates globals and outputs (two roundtrips).
    pub fn open(target: &Target, timeout: Duration) -> Result<Session> {
        let conn = match target {
            Target::Env => Connection::connect_to_env().map_err(|e| {
                CaptureError::NoBackend(format!(
                    "cannot connect to a Wayland compositor ({e}); is WAYLAND_DISPLAY set?"
                ))
            })?,
            Target::Path(p) => {
                let stream = UnixStream::connect(p).map_err(|e| {
                    CaptureError::NoBackend(format!(
                        "cannot connect to the Wayland socket {}: {e}",
                        p.display()
                    ))
                })?;
                Connection::from_socket(stream).map_err(|e| {
                    CaptureError::NoBackend(format!("Wayland handshake failed: {e}"))
                })?
            }
        };
        let queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let state = State {
            registry,
            globals: Vec::new(),
            outputs: Vec::new(),
            shm: None,
            shm_formats: Vec::new(),
            xdg_manager: None,
            wlr_manager: None,
            ext_copy_manager: None,
            ext_output_source_manager: None,
            ext_toplevel_source_manager: None,
            toplevel_list: None,
            toplevels: Vec::new(),
            syncs_done: 0,
            wlr: WlrFrame::default(),
            ext: ExtProgress::default(),
        };
        let mut s = Session { conn, queue, qh, state, timeout };
        s.roundtrip("listing globals")?;
        // Outputs and shm formats arrive after the bind; xdg-output needs a second trip.
        s.roundtrip("reading outputs")?;
        if let Some(mgr) = s.state.xdg_manager.clone() {
            let qh = s.qh.clone();
            for o in &mut s.state.outputs {
                o.xdg = Some(mgr.get_xdg_output(&o.wl, &qh, o.global));
            }
            s.roundtrip("reading logical output geometry")?;
        }
        Ok(s)
    }

    /// Geometry of every enabled output, in registry order.
    pub fn geoms(&self) -> Vec<OutputGeom> {
        self.state.outputs.iter().filter_map(OutputRec::geom).collect()
    }

    /// A `wl_display.sync` roundtrip with the session timeout.
    pub fn roundtrip(&mut self, what: &str) -> Result<()> {
        let target = self.state.syncs_done + 1;
        let _cb = self.conn.display().sync(&self.qh, ());
        self.run_until(what, |s| s.syncs_done >= target)
    }

    /// Dispatches events until `done(state)` holds, or errors on disconnect, protocol
    /// error, or after the session timeout.
    pub fn run_until(&mut self, what: &str, done: impl Fn(&State) -> bool) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(|e| match e {
                wayland_client::DispatchError::Backend(b) => map_wl_error(&b, what),
                wayland_client::DispatchError::BadMessage { interface, .. } => {
                    CaptureError::backend(BACKEND, format!("malformed {interface} message"))
                }
            })?;
            if done(&self.state) {
                return Ok(());
            }
            self.conn.flush().map_err(|e| map_wl_error(&e, what))?;
            let Some(guard) = self.queue.prepare_read() else { continue };
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now).filter(|d| !d.is_zero())
            else {
                return Err(CaptureError::backend(
                    BACKEND,
                    format!(
                        "timed out after {:?} while {what} (is the compositor responsive?)",
                        self.timeout
                    ),
                ));
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
                    Err(e) => {
                        return Err(CaptureError::backend(BACKEND, format!("poll failed: {e}")));
                    }
                }
            };
            if !readable {
                // Dropping the guard cancels the prepared read; loop re-checks the deadline.
                continue;
            }
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(map_wl_error(&e, what)),
            }
        }
    }

    /// Allocates a shm-backed `wl_buffer`.
    pub fn alloc_shm(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: ShmFormat,
    ) -> Result<ShmBuffer> {
        let shm = self
            .state
            .shm
            .clone()
            .ok_or_else(|| CaptureError::backend(BACKEND, "compositor has no wl_shm"))?;
        let len = (stride as usize).saturating_mul(height as usize);
        if len == 0 || len > MAX_SHM_BYTES || i32::try_from(len).is_err() {
            return Err(CaptureError::backend(
                BACKEND,
                format!("refusing a {width}x{height} (stride {stride}) shm buffer"),
            ));
        }
        let fd = memfd_create("ssx-capture", MemfdFlags::CLOEXEC)
            .map_err(|e| CaptureError::backend(BACKEND, format!("memfd_create failed: {e}")))?;
        ftruncate(&fd, len as u64)
            .map_err(|e| CaptureError::backend(BACKEND, format!("ftruncate failed: {e}")))?;
        // `len` was checked against i32::MAX above.
        let pool =
            shm.create_pool(fd.as_fd(), i32::try_from(len).unwrap_or(i32::MAX), &self.qh, ());
        let wl_format = wl_shm::Format::try_from(format.to_wl()).map_err(|()| {
            CaptureError::backend(BACKEND, format!("{format:?} is not a known wl_shm format"))
        })?;
        let buffer = pool.create_buffer(
            0,
            wl_i32(width),
            wl_i32(height),
            wl_i32(stride),
            wl_format,
            &self.qh,
            (),
        );
        pool.destroy();
        Ok(ShmBuffer { buffer, file: File::from(fd), len })
    }

    /// Binds `ext_foreign_toplevel_list_v1` (if advertised) and waits for the initial list.
    pub fn load_toplevels(&mut self) -> Result<()> {
        if self.state.toplevel_list.is_none() {
            let Some(g) = self
                .state
                .globals
                .iter()
                .find(|g| g.interface == "ext_foreign_toplevel_list_v1")
                .cloned()
            else {
                return Err(CaptureError::backend(
                    BACKEND,
                    "compositor does not advertise ext-foreign-toplevel-list-v1",
                ));
            };
            let list = self.state.registry.bind(g.name, 1, &self.qh, ());
            self.state.toplevel_list = Some(list);
        }
        // One roundtrip delivers the toplevels and their `done`s.
        self.roundtrip("listing toplevels")?;
        self.roundtrip("reading toplevel properties")
    }

    /// Last-chance flush so destroy requests reach the compositor before we drop.
    pub fn flush(&self) {
        // Errors here mean the connection is gone; nothing left to clean up.
        let _ = self.conn.flush();
    }
}
