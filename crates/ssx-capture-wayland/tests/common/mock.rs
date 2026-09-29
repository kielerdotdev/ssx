//! A scriptable mock Wayland compositor (wayland-server) for the protocols headless sway
//! 1.9 cannot exercise: `ext-image-copy-capture-v1` (sway only gained it in 1.10), unusual
//! `wl_shm` formats, `y_invert`, stride padding, renegotiation, hangs and disconnects.
//!
//! It paints a deterministic pattern per output so the client's output can be compared
//! pixel-exactly, and logs the requests it received so tests can assert on them.
#![allow(dead_code)] // each test binary uses a different subset

use std::{
    fs::File,
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use ssx_capture_wayland::Transform;
use wayland_protocols::{
    ext::{
        foreign_toplevel_list::v1::server::{
            ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
            ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
        },
        image_capture_source::v1::server::{
            ext_foreign_toplevel_image_capture_source_manager_v1::{
                self, ExtForeignToplevelImageCaptureSourceManagerV1,
            },
            ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
            ext_output_image_capture_source_manager_v1::{
                self, ExtOutputImageCaptureSourceManagerV1,
            },
        },
        image_copy_capture::v1::server::{
            ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
            ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1, Options},
            ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
        },
    },
    xdg::xdg_output::zv1::server::{
        zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource, WEnum,
    backend::{ClientData, ClientId, DisconnectReason},
    protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool},
};

use super::pattern;

// ---- wl_shm format codes used by the mock ---------------------------------------------
pub const ARGB8888: u32 = 0;
pub const XRGB8888: u32 = 1;
pub const XBGR8888: u32 = 0x3432_4258; // XB24
pub const XBGR2101010: u32 = 0x3033_4258; // XB30
pub const ARGB2101010: u32 = 0x3033_5241; // AR30
pub const BGR888: u32 = 0x3432_4742; // BG24
pub const ALL_FORMATS: [u32; 6] = [ARGB8888, XRGB8888, XBGR8888, XBGR2101010, ARGB2101010, BGR888];

fn bytes_per_pixel(fmt: u32) -> usize {
    if fmt == BGR888 { 3 } else { 4 }
}

/// Encodes an upright BGRA image into `fmt` with `stride`, padding rows with 0xAA and
/// giving alpha/X channels deliberately wrong values (0) to prove they are ignored.
pub fn encode(fmt: u32, bgra: &[u8], w: u32, h: u32, stride: usize) -> Vec<u8> {
    let mut out = vec![0xAAu8; stride * h as usize];
    let ten = |v: u8| (u32::from(v) << 2) | (u32::from(v) >> 6);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = (y * w as usize + x) * 4;
            let (b, g, r) = (bgra[s], bgra[s + 1], bgra[s + 2]);
            let d = y * stride + x * bytes_per_pixel(fmt);
            match fmt {
                ARGB8888 => out[d..d + 4].copy_from_slice(&[b, g, r, 0]),
                XRGB8888 => out[d..d + 4].copy_from_slice(&[b, g, r, 0x11]),
                XBGR8888 => out[d..d + 4].copy_from_slice(&[r, g, b, 0x11]),
                XBGR2101010 => {
                    let v = (ten(b) << 20) | (ten(g) << 10) | ten(r);
                    out[d..d + 4].copy_from_slice(&v.to_le_bytes());
                }
                ARGB2101010 => {
                    let v = (ten(r) << 20) | (ten(g) << 10) | ten(b);
                    out[d..d + 4].copy_from_slice(&v.to_le_bytes());
                }
                BGR888 => out[d..d + 3].copy_from_slice(&[r, g, b]),
                _ => panic!("mock cannot encode {fmt:#x}"),
            }
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct MockOutput {
    pub name: &'static str,
    pub x: i32,
    pub y: i32,
    /// Mode in scan-out orientation.
    pub mode: (u32, u32),
    pub transform: Transform,
    pub scale: i32,
}

impl MockOutput {
    pub fn new(name: &'static str, x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { name, x, y, mode: (w, h), transform: Transform::Normal, scale: 1 }
    }
    pub fn scale(mut self, s: i32) -> Self {
        self.scale = s;
        self
    }
    pub fn transform(mut self, t: Transform) -> Self {
        self.transform = t;
        self
    }
    /// Upright (displayed) pixel size.
    pub fn displayed(&self) -> (u32, u32) {
        self.transform.upright_size(self.mode.0, self.mode.1)
    }
    pub fn logical(&self) -> (i32, i32) {
        let (w, h) = self.displayed();
        (w as i32 / self.scale, h as i32 / self.scale)
    }
}

#[derive(Clone, Debug)]
pub struct MockToplevel {
    pub title: &'static str,
    pub app_id: &'static str,
    pub w: u32,
    pub h: u32,
    pub seed: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behavior {
    Normal,
    /// First capture: send new constraints, then `failed(buffer_constraints)`.
    RenegotiateConstraintsFirst,
    /// First capture: send `failed(buffer_constraints)`, then new constraints.
    RenegotiateFailedFirst,
    /// First capture fails with `unknown`; the retry succeeds.
    UnknownFailOnce,
    /// Capture fails with `stopped` and the session is stopped.
    Stop,
    /// Never answer a capture.
    Hang,
    /// Drop every connection when a capture is requested.
    Disconnect,
    /// wlr: the first frame `failed`, the retry succeeds.
    WlrFailOnce,
}

#[derive(Clone, Debug)]
pub struct MockCfg {
    pub outputs: Vec<MockOutput>,
    pub formats: Vec<u32>,
    pub ext: bool,
    pub wlr: bool,
    pub wlr_version: u32,
    pub xdg_output: bool,
    pub y_invert: bool,
    pub stride_pad: u32,
    pub behavior: Behavior,
    /// Format and size delta offered after a renegotiation.
    pub renegotiate: (u32, (u32, u32)),
    pub toplevels: Vec<MockToplevel>,
}

impl MockCfg {
    pub fn new(outputs: Vec<MockOutput>) -> Self {
        Self {
            outputs,
            formats: vec![XRGB8888],
            ext: true,
            wlr: false,
            wlr_version: 3,
            xdg_output: true,
            y_invert: false,
            stride_pad: 0,
            behavior: Behavior::Normal,
            renegotiate: (XBGR8888, (16, 8)),
            toplevels: Vec::new(),
        }
    }
}

/// Upright BGRA pixels of output `idx` (seed = idx + 1).
pub fn output_image(o: &MockOutput, idx: usize) -> Vec<u8> {
    let (w, h) = o.displayed();
    image_of(idx as u8 + 1, w, h)
}

pub fn image_of(seed: u8, w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = pattern(seed, x, y);
            v.extend_from_slice(&[b, g, r, 255]);
        }
    }
    v
}

// ---- server plumbing ------------------------------------------------------------------

struct Pool {
    file: File,
}

struct BufData {
    pool: Arc<Pool>,
    offset: usize,
    width: u32,
    height: u32,
    stride: usize,
}

#[derive(Clone)]
enum Src {
    Output(usize),
    Toplevel(usize),
}

struct SessData {
    src: Src,
    paint_cursors: bool,
    /// Current buffer constraints.
    constraints: Mutex<(u32, u32, u32)>,
    captures: Mutex<u32>,
}

struct FrameData {
    session: ExtImageCopyCaptureSessionV1,
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
}

struct WlrFrameData {
    output: usize,
    region: Option<(i32, i32, i32, i32)>,
    size: (u32, u32),
    stride: u32,
    format: u32,
    captures: Mutex<u32>,
}

pub struct Mock {
    cfg: MockCfg,
    log: Arc<Mutex<Vec<String>>>,
    wlr_captures: Arc<Mutex<u32>>,
    disconnect: bool,
}

struct Cd;
impl ClientData for Cd {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// A running mock compositor.
pub struct MockServer {
    pub socket: PathBuf,
    pub log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl MockServer {
    pub fn start(cfg: MockCfg) -> MockServer {
        let dir = tempfile::Builder::new().prefix("ssx-mock").tempdir().expect("tempdir");
        let socket = dir.path().join("mock-wayland");
        let listener = ListeningSocket::bind_absolute(socket.clone()).expect("bind mock socket");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log2, stop2) = (log.clone(), stop.clone());
        let thread = std::thread::spawn(move || serve(cfg, listener, log2, stop2));
        MockServer { socket, log, stop, thread: Some(thread), _dir: dir }
    }

    pub fn config(&self) -> ssx_capture_wayland::Config {
        ssx_capture_wayland::Config {
            target: ssx_capture_wayland::Target::Path(self.socket.clone()),
            ipc: ssx_capture_wayland::Ipc::Disabled,
            timeout: Duration::from_secs(3),
            ..ssx_capture_wayland::Config::default()
        }
    }

    pub fn logged(&self, needle: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|l| l.contains(needle)).count()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(
    cfg: MockCfg,
    listener: ListeningSocket,
    log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
) {
    let mut display: Display<Mock> = Display::new().expect("display");
    let dh = display.handle();
    for i in 0..cfg.outputs.len() {
        dh.create_global::<Mock, wl_output::WlOutput, _>(4, i);
    }
    dh.create_global::<Mock, wl_shm::WlShm, _>(1, ());
    if cfg.xdg_output {
        dh.create_global::<Mock, ZxdgOutputManagerV1, _>(3, ());
    }
    if cfg.ext {
        dh.create_global::<Mock, ExtImageCopyCaptureManagerV1, _>(1, ());
        dh.create_global::<Mock, ExtOutputImageCaptureSourceManagerV1, _>(1, ());
        if !cfg.toplevels.is_empty() {
            dh.create_global::<Mock, ExtForeignToplevelListV1, _>(1, ());
            dh.create_global::<Mock, ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ());
        }
    }
    if cfg.wlr {
        dh.create_global::<Mock, ZwlrScreencopyManagerV1, _>(cfg.wlr_version, ());
    }
    let mut state = Mock { cfg, log, wlr_captures: Arc::new(Mutex::new(0)), disconnect: false };
    while !stop.load(Ordering::SeqCst) && !state.disconnect {
        while let Ok(Some(stream)) = listener.accept() {
            let _ = display.handle().insert_client(stream, Arc::new(Cd));
        }
        let _ = display.dispatch_clients(&mut state);
        let _ = display.flush_clients();
        std::thread::sleep(Duration::from_millis(1));
    }
    // Dropping `display` here closes every client connection.
}

impl Mock {
    fn note(&self, s: impl Into<String>) {
        self.log.lock().unwrap().push(s.into());
    }
}

// ---- wl_output / xdg-output / wl_shm ----------------------------------------------------

impl GlobalDispatch<wl_output::WlOutput, usize> for Mock {
    fn bind(
        st: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<wl_output::WlOutput>,
        idx: &usize,
        init: &mut DataInit<'_, Self>,
    ) {
        let out = init.init(res, *idx);
        let o = &st.cfg.outputs[*idx];
        out.geometry(
            o.x,
            o.y,
            300,
            200,
            wl_output::Subpixel::Unknown,
            "MockMake".into(),
            "MockModel".into(),
            wl_output::Transform::try_from(transform_code(o.transform)).unwrap(),
        );
        out.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            o.mode.0 as i32,
            o.mode.1 as i32,
            60_000,
        );
        out.scale(o.scale);
        out.name(o.name.into());
        out.description(format!("Mock {}", o.name));
        out.done();
    }
}

pub fn transform_code(t: Transform) -> u32 {
    match t {
        Transform::Normal => 0,
        Transform::Rot90 => 1,
        Transform::Rot180 => 2,
        Transform::Rot270 => 3,
        Transform::Flipped => 4,
        Transform::Flipped90 => 5,
        Transform::Flipped180 => 6,
        Transform::Flipped270 => 7,
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &usize,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ZxdgOutputManagerV1, ()> for Mock {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<ZxdgOutputManagerV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for Mock {
    fn request(
        st: &mut Self,
        _: &Client,
        _: &ZxdgOutputManagerV1,
        req: zxdg_output_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_output_manager_v1::Request::GetXdgOutput { id, output } = req {
            let idx = *output.data::<usize>().expect("output data");
            let xo = init.init(id, idx);
            let o = &st.cfg.outputs[idx];
            let (lw, lh) = o.logical();
            xo.logical_position(o.x, o.y);
            xo.logical_size(lw, lh);
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZxdgOutputV1,
        _: zxdg_output_v1::Request,
        _: &usize,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for Mock {
    fn bind(
        st: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<wl_shm::WlShm>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(res, ());
        for f in ALL_FORMATS {
            shm.format(wl_shm::Format::try_from(f).expect("known format"));
        }
        let _ = st;
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        req: wl_shm::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, .. } = req {
            init.init(id, Arc::new(Pool { file: File::from(fd) }));
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, Arc<Pool>> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        req: wl_shm_pool::Request,
        pool: &Arc<Pool>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, offset, width, height, stride, .. } = req {
            init.init(
                id,
                BufData {
                    pool: pool.clone(),
                    offset: offset as usize,
                    width: width as u32,
                    height: height as u32,
                    stride: stride as usize,
                },
            );
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufData> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &BufData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// ---- ext-image-copy-capture -------------------------------------------------------------

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Mock {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<ExtOutputImageCaptureSourceManagerV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        req: ext_output_image_capture_source_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_output_image_capture_source_manager_v1::Request::CreateSource {
            source,
            output,
        } = req
        {
            let idx = *output.data::<usize>().expect("output data");
            init.init(source, Src::Output(idx));
        }
    }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Mock {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        req: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
            source,
            toplevel_handle,
        } = req
        {
            let idx = *toplevel_handle.data::<usize>().expect("toplevel data");
            init.init(source, Src::Toplevel(idx));
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, Src> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Request,
        _: &Src,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtForeignToplevelListV1, ()> for Mock {
    fn bind(
        st: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        res: New<ExtForeignToplevelListV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let list = init.init(res, ());
        for (i, t) in st.cfg.toplevels.iter().enumerate() {
            let h = client
                .create_resource::<ExtForeignToplevelHandleV1, usize, Mock>(dh, 1, i)
                .expect("create toplevel handle");
            list.toplevel(&h);
            h.title(t.title.into());
            h.app_id(t.app_id.into());
            h.identifier(format!("mock-{i}"));
            h.done();
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelListV1,
        _: ext_foreign_toplevel_list_v1::Request,
        (): &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, usize> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelHandleV1,
        _: ext_foreign_toplevel_handle_v1::Request,
        _: &usize,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for Mock {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<ExtImageCopyCaptureManagerV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(res, ());
    }
}

impl Mock {
    /// Source size in buffer (scan-out) pixels.
    fn source_size(&self, src: &Src) -> (u32, u32) {
        match src {
            Src::Output(i) => self.cfg.outputs[*i].mode,
            Src::Toplevel(i) => (self.cfg.toplevels[*i].w, self.cfg.toplevels[*i].h),
        }
    }

    fn send_constraints(
        session: &ExtImageCopyCaptureSessionV1,
        (fmt_formats, w, h): (&[u32], u32, u32),
    ) {
        session.buffer_size(w, h);
        for f in fmt_formats {
            session.shm_format(wl_shm::Format::try_from(*f).expect("known format"));
        }
        session.done();
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Mock {
    fn request(
        st: &mut Self,
        _: &Client,
        _: &ExtImageCopyCaptureManagerV1,
        req: ext_image_copy_capture_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_manager_v1::Request::CreateSession {
            session,
            source,
            options,
        } = req
        {
            let src = source.data::<Src>().expect("source data").clone();
            let paint = matches!(options, WEnum::Value(o) if o.contains(Options::PaintCursors));
            st.note(format!("create_session paint_cursors={paint}"));
            let (w, h) = st.source_size(&src);
            let data = SessData {
                src,
                paint_cursors: paint,
                constraints: Mutex::new((
                    st.cfg.formats.first().copied().unwrap_or(XRGB8888),
                    w,
                    h,
                )),
                captures: Mutex::new(0),
            };
            let s = init.init(session, data);
            Mock::send_constraints(&s, (&st.cfg.formats, w, h));
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, SessData> for Mock {
    fn request(
        _: &mut Self,
        _: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        req: ext_image_copy_capture_session_v1::Request,
        _: &SessData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = req {
            init.init(frame, FrameData { session: session.clone(), buffer: Mutex::new(None) });
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameData> for Mock {
    fn request(
        st: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        req: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match req {
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
                *data.buffer.lock().unwrap() = Some(buffer);
            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                let sess = data.session.data::<SessData>().expect("session data");
                let n = {
                    let mut c = sess.captures.lock().unwrap();
                    *c += 1;
                    *c
                };
                st.note(format!("capture #{n} cursors={}", sess.paint_cursors));
                st.ext_capture(frame, &data.session, sess, data, n);
            }
            _ => {}
        }
    }
}

impl Mock {
    fn ext_capture(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        session: &ExtImageCopyCaptureSessionV1,
        sess: &SessData,
        data: &FrameData,
        n: u32,
    ) {
        use Behavior::*;
        match self.cfg.behavior {
            Hang => return,
            Disconnect => {
                self.disconnect = true;
                return;
            }
            Stop => {
                frame.failed(FailureReason::Stopped);
                session.stopped();
                return;
            }
            UnknownFailOnce if n == 1 => {
                frame.failed(FailureReason::Unknown);
                return;
            }
            RenegotiateConstraintsFirst | RenegotiateFailedFirst if n == 1 => {
                let (fmt, (dw, dh)) = self.cfg.renegotiate;
                let (_, w, h) = *sess.constraints.lock().unwrap();
                let new = (fmt, w - dw, h - dh);
                *sess.constraints.lock().unwrap() = new;
                if self.cfg.behavior == RenegotiateConstraintsFirst {
                    Mock::send_constraints(session, (&[fmt], new.1, new.2));
                    frame.failed(FailureReason::BufferConstraints);
                } else {
                    frame.failed(FailureReason::BufferConstraints);
                    Mock::send_constraints(session, (&[fmt], new.1, new.2));
                }
                return;
            }
            _ => {}
        }
        let (fmt, w, h) = *sess.constraints.lock().unwrap();
        let buf = data.buffer.lock().unwrap().clone().expect("client attached a buffer");
        let bd = buf.data::<BufData>().expect("buffer data");
        if (bd.width, bd.height) != (w, h) {
            frame.failed(FailureReason::BufferConstraints);
            return;
        }
        // Content (in buffer orientation) and the transform the compositor reports.
        let (upright, transform) = match &sess.src {
            Src::Output(i) => {
                let o = &self.cfg.outputs[*i];
                let (uw, uh) = o.displayed();
                if (w, h) == o.mode {
                    (o.transform.apply(&output_image(o, *i), uw, uh), o.transform)
                } else {
                    // Renegotiated (smaller) buffer: untransformed pattern of the new size.
                    (image_of(*i as u8 + 1, w, h), Transform::Normal)
                }
            }
            Src::Toplevel(i) => {
                let t = &self.cfg.toplevels[*i];
                (image_of(t.seed, t.w, t.h), Transform::Normal)
            }
        };
        let pixels = encode(fmt, &upright, w, h, bd.stride);
        bd.pool.file.write_all_at(&pixels, bd.offset as u64).expect("write shm");
        frame.transform(wl_output::Transform::try_from(transform_code(transform)).unwrap());
        frame.presentation_time(0, 1, 2);
        frame.ready();
    }
}

// ---- wlr-screencopy -----------------------------------------------------------------------

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Mock {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        res: New<ZwlrScreencopyManagerV1>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Mock {
    fn request(
        st: &mut Self,
        _: &Client,
        _: &ZwlrScreencopyManagerV1,
        req: zwlr_screencopy_manager_v1::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let (frame, overlay, output, region) = match req {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor, output, None),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (frame, overlay_cursor, output, Some((x, y, width, height))),
            _ => return,
        };
        let idx = *output.data::<usize>().expect("output data");
        let o = &st.cfg.outputs[idx];
        st.note(format!("wlr capture overlay={overlay} region={region:?} output={}", o.name));
        let fmt = st.cfg.formats.first().copied().unwrap_or(XRGB8888);
        let size = match region {
            // wlroots semantics: the region is logical, the buffer is region * scale.
            Some((_, _, w, h)) => (w as u32 * o.scale as u32, h as u32 * o.scale as u32),
            None => o.mode,
        };
        let stride = size.0 * bytes_per_pixel(fmt) as u32 + st.cfg.stride_pad;
        let f = init.init(
            frame,
            WlrFrameData {
                output: idx,
                region,
                size,
                stride,
                format: fmt,
                captures: Mutex::new(0),
            },
        );
        f.buffer(wl_shm::Format::try_from(fmt).expect("format"), size.0, size.1, stride);
        if f.version() >= 3 {
            f.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, WlrFrameData> for Mock {
    fn request(
        st: &mut Self,
        _: &Client,
        frame: &ZwlrScreencopyFrameV1,
        req: zwlr_screencopy_frame_v1::Request,
        data: &WlrFrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let (zwlr_screencopy_frame_v1::Request::Copy { buffer }
        | zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer }) = req
        else {
            return;
        };
        *data.captures.lock().unwrap() += 1;
        let total = {
            let mut c = st.wlr_captures.lock().unwrap();
            *c += 1;
            *c
        };
        if st.cfg.behavior == Behavior::WlrFailOnce && total == 1 {
            frame.failed();
            return;
        }
        let bd = buffer.data::<BufData>().expect("buffer data");
        let o = &st.cfg.outputs[data.output];
        let (uw, uh) = o.displayed();
        let upright = output_image(o, data.output);
        // Region crop in *logical* units times scale (Normal transform only).
        let (bgra, w, h) = match data.region {
            Some((x, y, rw, rh)) => {
                let s = o.scale as u32;
                let (x0, y0, cw, ch) = (x as u32 * s, y as u32 * s, rw as u32 * s, rh as u32 * s);
                let mut v = Vec::new();
                for row in y0..y0 + ch {
                    let start = ((row * uw + x0) * 4) as usize;
                    v.extend_from_slice(&upright[start..start + (cw * 4) as usize]);
                }
                (v, cw, ch)
            }
            None => (o.transform.apply(&upright, uw, uh), o.mode.0, o.mode.1),
        };
        assert_eq!((w, h), data.size);
        let mut pixels = encode(data.format, &bgra, w, h, data.stride as usize);
        if st.cfg.y_invert {
            // Store bottom row first and tell the client.
            let stride = data.stride as usize;
            let rows: Vec<Vec<u8>> = pixels.chunks(stride).map(<[u8]>::to_vec).collect();
            pixels = rows.into_iter().rev().flatten().collect();
            frame.flags(zwlr_screencopy_frame_v1::Flags::YInvert);
        } else {
            frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
        }
        bd.pool.file.write_all_at(&pixels, bd.offset as u64).expect("write shm");
        frame.ready(0, 1, 2);
    }
}
