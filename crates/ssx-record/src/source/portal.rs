//! GNOME / KDE (and any desktop with a portal): xdg-desktop-portal **`ScreenCast`** delivered
//! through a **`PipeWire`** stream.
//!
//! Flow: `CreateSession` -> `SelectSources` (with the saved *restore token*) -> `Start` (the
//! compositor's picker appears **once**; with a valid token it is skipped) ->
//! `OpenPipeWireRemote` (a socket to the `PipeWire` instance the compositor exports the
//! stream on) -> a `PipeWire` capture stream on the node the portal returned.
//!
//! * **Restore token.** After a successful `Start` the returned token is stored in the
//!   application's data directory (`portal-restore-token`), so the next recording of the
//!   same source starts without a prompt (`PersistMode::ExplicitlyRevoked`). A stale token is
//!   ignored by the portal, which then shows the picker again.
//! * **Buffers.** The format offered to `PipeWire` carries no DRM modifier, which tells the
//!   compositor to fall back to plain shared memory (memfd / `MemPtr`), mapped by `PipeWire`
//!   (`MAP_BUFFERS`) and copied out with safe slices. If a compositor still delivers a
//!   DMA-BUF the stream ends with [`SourceError::UnsupportedBuffer`] naming the problem.
//!   Zero-copy DMA-BUF import into the GPU path is documentation only (see the README).
//! * **Variable frame rate.** GNOME and KDE send a buffer only when the screen changed;
//!   the newest one waits in a one-slot [`Mailbox`], so a slow consumer never builds up
//!   a backlog, and the session's pacer repeats the last frame during idle time.
//! * **Threads.** The `PipeWire` main loop runs on its own thread; the portal handshake
//!   runs on the calling thread (`async-io`, no tokio).

use std::{
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread::JoinHandle,
    time::Duration,
};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode, Session,
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    },
};
use pipewire as pw;
use pw::{properties::properties, spa};
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

use super::{
    CaptureTarget, FrameSource, Mailbox, SourceConfig, SourceEvent, SourceInfo, Taken, VideoFrame,
};
use crate::{error::SourceError, time::Clock};

const BACKEND: &str = "pipewire";
const TOKEN_FILE: &str = "portal-restore-token";

/// What the stream negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Negotiated {
    format: spa::param::video::VideoFormat,
    size: Size,
}

type Shared = Arc<(Mutex<Option<Negotiated>>, Condvar)>;

/// Records a portal `ScreenCast` stream. See the module docs.
pub struct PortalSource {
    cfg: SourceConfig,
    token_path: Option<PathBuf>,
    bus_address: Option<String>,
    session: Option<Session<Screencast>>,
    mailbox: Arc<Mailbox<VideoFrame>>,
    quit: Option<pw::channel::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    info: SourceInfo,
    stopped: bool,
    /// Frames replaced before they were read (diagnostics).
    node_id: u32,
    used_restore_token: bool,
}

impl std::fmt::Debug for PortalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalSource").field("node_id", &self.node_id).finish_non_exhaustive()
    }
}

impl PortalSource {
    /// A source for `cfg`; the restore token lives in the user's data directory.
    pub fn new(cfg: SourceConfig) -> Self {
        Self {
            cfg,
            token_path: default_token_path(),
            bus_address: None,
            session: None,
            mailbox: Arc::new(Mailbox::default()),
            quit: None,
            worker: None,
            info: SourceInfo {
                name: "pipewire",
                size: Size::default(),
                format: PixelFormat::Bgra8,
                color_space: ColorSpace::Srgb,
                hdr: false,
                damage_driven: true,
                realtime: true,
            },
            stopped: false,
            node_id: 0,
            used_restore_token: false,
        }
    }

    /// Where the restore token is stored (`None` disables persistence).
    pub fn with_token_path(mut self, path: Option<PathBuf>) -> Self {
        self.token_path = path;
        self
    }

    /// Talks to the session bus at `address` instead of `$DBUS_SESSION_BUS_ADDRESS`
    /// (tests use a private bus with a mock portal).
    pub fn with_bus_address(mut self, address: impl Into<String>) -> Self {
        self.bus_address = Some(address.into());
        self
    }

    /// `true` if the previous recording's restore token was offered to the portal.
    pub fn used_restore_token(&self) -> bool {
        self.used_restore_token
    }

    /// The `PipeWire` node id the portal granted.
    pub fn node_id(&self) -> u32 {
        self.node_id
    }
}

fn default_token_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "ssx", "ssx").map(|d| d.data_dir().join(TOKEN_FILE))
}

fn read_token(path: Option<&PathBuf>) -> Option<String> {
    let t = std::fs::read_to_string(path?).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn write_token(path: Option<&PathBuf>, token: &str) {
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, token) {
        tracing::warn!(path = %path.display(), error = %e, "could not store the portal restore token");
    }
}

fn portal_err(e: &ashpd::Error) -> SourceError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            SourceError::PermissionDenied("the screen sharing dialog was cancelled".into())
        }
        ashpd::Error::PortalNotFound(_) => SourceError::Unavailable(
            "no xdg-desktop-portal with the ScreenCast interface is running (install xdg-desktop-portal and your desktop's backend)"
                .into(),
        ),
        other => {
            let text = other.to_string();
            if text.contains("was not provided by any .service")
                || text.contains("ServiceUnknown")
                || text.contains("NameHasNoOwner")
            {
                SourceError::Unavailable(format!(
                    "no xdg-desktop-portal is running on the session bus ({text})"
                ))
            } else {
                SourceError::backend("xdg-desktop-portal", text)
            }
        }
    }
}

/// Everything the portal handshake yields.
struct Granted {
    session: Session<Screencast>,
    fd: OwnedFd,
    node_id: u32,
    used_token: bool,
}

async fn negotiate(
    cfg: &SourceConfig,
    token_path: Option<&PathBuf>,
    bus_address: Option<&str>,
) -> Result<Granted, SourceError> {
    let proxy = match bus_address {
        Some(addr) => {
            let conn = ashpd::zbus::connection::Builder::address(addr)
                .map_err(|e| SourceError::Unavailable(format!("bad bus address: {e}")))?
                .build()
                .await
                .map_err(|e| SourceError::Unavailable(format!("cannot connect to the bus: {e}")))?;
            Screencast::with_connection(conn).await
        }
        None => Screencast::new().await,
    }
    .map_err(|e| portal_err(&e))?;
    let session =
        proxy.create_session(CreateSessionOptions::default()).await.map_err(|e| portal_err(&e))?;
    let token = read_token(token_path);
    let sources = match &cfg.target {
        CaptureTarget::Window(_) => SourceType::Window.into(),
        CaptureTarget::Monitor(_) | CaptureTarget::Desktop => SourceType::Monitor.into(),
        CaptureTarget::Pick | CaptureTarget::Region(_) => SourceType::Monitor | SourceType::Window,
    };
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(if cfg.cursor { CursorMode::Embedded } else { CursorMode::Hidden })
                .set_sources(sources)
                .set_multiple(false)
                .set_restore_token(token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(|e| portal_err(&e))?
        .response()
        .map_err(|e| portal_err(&e))?;
    let started = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| portal_err(&e))?
        .response()
        .map_err(|e| portal_err(&e))?;
    if let Some(t) = started.restore_token() {
        write_token(token_path, t);
    }
    let stream = started
        .streams()
        .first()
        .ok_or_else(|| SourceError::PermissionDenied("the portal granted no streams".into()))?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|e| portal_err(&e))?;
    Ok(Granted { session, fd, node_id, used_token: token.is_some() })
}

// ---- PipeWire side ------------------------------------------------------------------------

struct StreamData {
    format: spa::param::video::VideoInfoRaw,
    shared: Shared,
    mailbox: Arc<Mailbox<VideoFrame>>,
    clock: Clock,
}

fn format_object(fps: crate::time::Fps) -> spa::pod::Object {
    use spa::param::{
        format::{FormatProperties, MediaSubtype, MediaType},
        video::VideoFormat,
    };
    spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::BGR,
            VideoFormat::RGB
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps.num(), denom: fps.den() },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    )
}

/// Converts one mapped `PipeWire` buffer to a tightly packed BGRA frame.
fn to_frame(
    fmt: spa::param::video::VideoFormat,
    data: &[u8],
    (w, h): (u32, u32),
    stride: usize,
) -> Result<Frame, SourceError> {
    use spa::param::video::VideoFormat as V;
    let bpp = match fmt {
        V::BGRx | V::BGRA | V::RGBx | V::RGBA => 4,
        V::BGR | V::RGB => 3,
        other => {
            return Err(SourceError::UnsupportedBuffer(format!("pixel format {other:?}")));
        }
    };
    let (wu, hu) = (w as usize, h as usize);
    let stride = if stride == 0 { wu * bpp } else { stride };
    if stride < wu * bpp || data.len() < stride * hu.saturating_sub(1) + wu * bpp {
        return Err(SourceError::InvalidFrame(format!(
            "buffer of {} bytes is too small for {w}x{h} stride {stride}",
            data.len()
        )));
    }
    let mut out = vec![0u8; wu * hu * 4];
    for y in 0..hu {
        let s = &data[y * stride..y * stride + wu * bpp];
        let d = &mut out[y * wu * 4..(y + 1) * wu * 4];
        match fmt {
            V::BGRx | V::BGRA => {
                d.copy_from_slice(s);
                for px in d.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
            V::RGBx | V::RGBA => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    px.copy_from_slice(&[q[2], q[1], q[0], 255]);
                }
            }
            V::BGR => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(3)) {
                    px.copy_from_slice(&[q[0], q[1], q[2], 255]);
                }
            }
            _ => {
                for (px, q) in d.chunks_exact_mut(4).zip(s.chunks_exact(3)) {
                    px.copy_from_slice(&[q[2], q[1], q[0], 255]);
                }
            }
        }
    }
    Frame::from_raw(Size::new(w, h), wu * 4, PixelFormat::Bgra8, ColorSpace::Srgb, out)
        .map_err(|e| SourceError::InvalidFrame(e.to_string()))
}

/// What the `PipeWire` thread needs from its owner.
struct PwJob {
    fd: OwnedFd,
    node_id: u32,
    fps: crate::time::Fps,
    clock: Clock,
    shared: Shared,
    mailbox: Arc<Mailbox<VideoFrame>>,
}

#[allow(clippy::too_many_lines)] // one linear PipeWire setup + main loop
fn run_pipewire(
    job: PwJob,
    quit_rx: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), SourceError>>,
) {
    let PwJob { fd, node_id, fps, clock, shared, mailbox } = job;
    let (shared, mailbox) = (&shared, &mailbox);
    pw::init();
    let setup = (|| -> Result<_, pw::Error> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_fd_rc(fd, None)?;
        Ok((mainloop, context, core))
    })();
    let (mainloop, _context, core) = match setup {
        Ok(x) => x,
        Err(e) => {
            let _ = ready.send(Err(SourceError::backend(
                BACKEND,
                format!("cannot connect to PipeWire: {e}"),
            )));
            return;
        }
    };
    let stream = match pw::stream::StreamBox::new(
        &core,
        "ssx-record",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready
                .send(Err(SourceError::backend(BACKEND, format!("cannot create the stream: {e}"))));
            return;
        }
    };
    let data = StreamData {
        format: spa::param::video::VideoInfoRaw::default(),
        shared: Arc::clone(shared),
        mailbox: Arc::clone(mailbox),
        clock,
    };
    let mb_state = Arc::clone(mailbox);
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            tracing::debug!(?old, ?new, "pipewire stream state");
            if let pw::stream::StreamState::Error(msg) = new {
                mb_state.end(Err(format!("PipeWire stream error: {msg}")));
            }
        })
        .param_changed(|_, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((mt, st)) = spa::param::format_utils::parse_format(param) else { return };
            if mt != spa::param::format::MediaType::Video || st != spa::param::format::MediaSubtype::Raw {
                return;
            }
            if ud.format.parse(param).is_err() {
                return;
            }
            let n = Negotiated {
                format: ud.format.format(),
                size: Size::new(ud.format.size().width, ud.format.size().height),
            };
            tracing::info!(?n, "pipewire format negotiated");
            let (lock, cv) = &*ud.shared;
            *lock.lock().unwrap_or_else(PoisonError::into_inner) = Some(n);
            cv.notify_all();
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let dtype = d.type_();
            if dtype == spa::buffer::DataType::DmaBuf {
                ud.mailbox.end(Err(
                    "the compositor sent DMA-BUF buffers only; shared-memory capture is not offered by this desktop".into(),
                ));
                return;
            }
            let chunk_size = d.chunk().size() as usize;
            let stride = usize::try_from(d.chunk().stride()).unwrap_or(0);
            let offset = d.chunk().offset() as usize;
            if chunk_size == 0 {
                // A metadata-only buffer (cursor moved, nothing to draw).
                return;
            }
            let Some(bytes) = d.data() else { return };
            let end = (offset + chunk_size).min(bytes.len());
            let (w, h) = (ud.format.size().width, ud.format.size().height);
            match to_frame(ud.format.format(), &bytes[offset.min(end)..end], (w, h), stride) {
                Ok(mut frame) => {
                    let ts = ud.clock.now();
                    frame.timestamp = Some(ts);
                    ud.mailbox.put(VideoFrame { frame, timestamp: ts });
                }
                Err(e) => ud.mailbox.end(Err(e.to_string())),
            }
        })
        .register();
    let _listener = match listener {
        Ok(l) => l,
        Err(e) => {
            let _ = ready.send(Err(SourceError::backend(
                BACKEND,
                format!("cannot register the listener: {e}"),
            )));
            return;
        }
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format_object(fps)),
    )
    .map(|(c, _)| c.into_inner());
    let Ok(bytes) = bytes else {
        let _ = ready.send(Err(SourceError::backend(BACKEND, "cannot build the format pod")));
        return;
    };
    let Some(pod) = spa::pod::Pod::from_bytes(&bytes) else {
        let _ = ready.send(Err(SourceError::backend(BACKEND, "invalid format pod")));
        return;
    };
    let mut params = [pod];
    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        let _ = ready.send(Err(SourceError::backend(
            BACKEND,
            format!("cannot connect the stream to node {node_id}: {e}"),
        )));
        return;
    }
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |()| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    mailbox.end(Ok(()));
}

impl FrameSource for PortalSource {
    fn name(&self) -> &'static str {
        "pipewire"
    }

    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        // Portal handshake (may show the picker).
        let granted = async_io::block_on(negotiate(
            &self.cfg,
            self.token_path.as_ref(),
            self.bus_address.as_deref(),
        ))?;
        self.used_restore_token = granted.used_token;
        self.node_id = granted.node_id;
        self.session = Some(granted.session);

        let shared: Shared = Arc::new((Mutex::new(None), Condvar::new()));
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (fd, node_id, fps) = (granted.fd, granted.node_id, self.cfg.fps);
        let (sh2, mb2) = (Arc::clone(&shared), Arc::clone(&self.mailbox));
        self.worker = Some(
            std::thread::Builder::new()
                .name("ssx-pipewire".into())
                .spawn(move || {
                    let job = PwJob { fd, node_id, fps, clock, shared: sh2, mailbox: mb2 };
                    run_pipewire(job, quit_rx, &ready_tx);
                })
                .map_err(|e| {
                    SourceError::backend(BACKEND, format!("cannot start the PipeWire thread: {e}"))
                })?,
        );
        self.quit = Some(quit_tx);
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| SourceError::backend(BACKEND, "PipeWire did not start in time"))??;

        // Wait for the negotiated format.
        let (lock, cv) = &*shared;
        let mut g = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while g.is_none() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                drop(g);
                self.stop();
                return Err(SourceError::backend(
                    BACKEND,
                    "the stream never negotiated a format (is the node producing video?)",
                ));
            };
            g = cv.wait_timeout(g, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        if let Some(n) = *g {
            self.info.size = n.size;
        }
        Ok(())
    }

    fn info(&self) -> SourceInfo {
        self.info
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        if self.stopped {
            return Ok(SourceEvent::Ended);
        }
        match self.mailbox.take(timeout) {
            Taken::Value(f) => Ok(SourceEvent::Frame(f)),
            Taken::Timeout => Ok(SourceEvent::Timeout),
            Taken::Ended(Ok(())) => Ok(SourceEvent::Ended),
            Taken::Ended(Err(m)) if m.contains("DMA-BUF") => Err(SourceError::UnsupportedBuffer(m)),
            Taken::Ended(Err(m)) => Err(SourceError::backend(BACKEND, m)),
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(s) = self.session.take() {
            let _ = async_io::block_on(s.close());
        }
    }
}

impl Drop for PortalSource {
    fn drop(&mut self) {
        self.stop();
    }
}
