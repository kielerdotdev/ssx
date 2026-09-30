//! Windows: a streaming Windows.Graphics.Capture (WGC) source.
//!
//! `ssx-capture-win` captures *one frame* per call (a fresh frame pool and session each
//! time), which is right for screenshots and far too slow for video. This source keeps one
//! capture session alive:
//!
//! * a dedicated worker thread (MTA) owns the D3D11 device, a **free-threaded** frame pool
//!   and the session; `FrameArrived` fires on a WGC thread and only wakes the worker, which
//!   drains the pool to the newest frame, copies it into a reused staging texture and reads
//!   it back;
//! * frames go through a one-slot [`Mailbox`] (newest wins), so a slow consumer never builds
//!   a backlog; [`FrameSource::next_frame`] waits on it. WGC only delivers a frame when the
//!   picture changed, so an idle desktop yields [`SourceEvent::Timeout`] and the session's
//!   pacer repeats the last frame (damage-driven variable frame rate -> constant fps);
//! * frames are stamped with WGC's own `SystemRelativeTime`, mapped onto the session clock
//!   ([`TimestampMapper`]), not with the time the event handler happened to run;
//! * the frame pool is re-created when a captured window is resized; the pipeline rescales
//!   frames of changing size;
//! * an HDR monitor is captured as `R16G16B16A16_FLOAT` scRGB (labelled from the texture
//!   that arrives), which the session tone-maps like a screenshot;
//! * the cursor is included by WGC when requested; the yellow capture border is turned off
//!   where the OS allows it (Windows 11), and updates are limited to twice the target rate
//!   (Windows 11 22H2) so a 144 Hz display does not cost three times the read-backs.
//!
//! Everything decidable without the OS lives in `wgc_logic` (unit-tested on all platforms).
//! This file is the glue and is **only compile- and lint-checked** here (Linux host,
//! `cargo clippy --target x86_64-pc-windows-msvc`); it has never run on Windows.
//!
//! Limits: one item per source, so `Desktop` is the primary monitor and a region uses the
//! monitor it overlaps most (see `wgc_logic::resolve_target`); there is no picker.
#![allow(unsafe_code)] // Win32/WinRT calls; every block carries its SAFETY argument.

use std::{
    ffi::c_void,
    sync::{
        Arc,
        mpsc::{self, RecvTimeoutError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use ssx_capture::CaptureBackend;
use ssx_capture_win::WindowsCapture;
use ssx_types::{HdrInfo, PixelFormat, Point, Rect, Size};
use windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{HMODULE, HWND, POINT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::{Common::DXGI_SAMPLE_DESC, IDXGIDevice},
            Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint},
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::{IInspectable, Interface},
};

use super::{
    CaptureTarget, FrameSource, Mailbox, SourceConfig, SourceEvent, SourceInfo, Taken, VideoFrame,
    crop_to_region,
    wgc_logic::{
        MonitorDesc, PoolAction, ResolvedTarget, TimestampMapper, clamp_content, depad_rows,
        frame_from_readback, native_format, pool_action, resolve_target,
    },
};
use crate::{
    error::SourceError,
    time::{Clock, Fps},
};

/// How long to wait for the first frame. A desktop delivers one immediately; only a
/// minimised, cloaked or protected window runs into this.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Buffers in the frame pool (WGC recommends at least 2; 3 absorbs a slow read-back).
const POOL_BUFFERS: i32 = 3;

fn api(what: &'static str) -> impl FnOnce(windows::core::Error) -> SourceError {
    move |e| SourceError::backend("wgc", format!("{what}: {} ({:#x})", e.message(), e.code().0))
}

/// What the worker captures.
#[derive(Debug, Clone)]
enum ItemSpec {
    /// The monitor containing this desktop point.
    Monitor(Point),
    /// A window by raw handle.
    Window(isize),
}

/// Everything the worker needs, decided on the caller's thread.
#[derive(Debug, Clone)]
struct Setup {
    item: ItemSpec,
    /// Region to keep (desktop coordinates), for `CaptureTarget::Region`.
    crop: Option<Rect>,
    /// Where the captured item sits on the desktop.
    origin: Point,
    scale_factor: f64,
    /// HDR state of the monitor (windows are always captured as 8-bit).
    hdr: Option<HdrInfo>,
    fps: Fps,
    cursor: bool,
}

/// Messages to the worker.
enum Msg {
    Frame,
    Closed,
    Stop,
}

/// Records a monitor or window through Windows.Graphics.Capture.
#[derive(Debug)]
pub struct WgcSource {
    cfg: SourceConfig,
    mailbox: Arc<Mailbox<VideoFrame>>,
    control: Option<mpsc::Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
    info: Option<SourceInfo>,
    clock: Option<Clock>,
    ended: bool,
}

impl WgcSource {
    /// A source for `cfg`. Nothing is opened until [`FrameSource::start`].
    pub fn new(cfg: SourceConfig) -> Self {
        Self {
            cfg,
            mailbox: Arc::new(Mailbox::default()),
            control: None,
            worker: None,
            info: None,
            clock: None,
            ended: false,
        }
    }

    /// Whether this Windows build supports WGC (Windows 10 1903+ in practice).
    pub fn is_available() -> bool {
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
    }

    fn setup(&self) -> Result<Setup, SourceError> {
        let backend = WindowsCapture::new()?;
        let monitors = backend.monitors()?;
        let descs: Vec<MonitorDesc> = monitors
            .iter()
            .map(|m| MonitorDesc {
                id: m.id.clone(),
                rect: m.rect,
                primary: m.primary,
                hdr_active: m.hdr.is_some_and(|h| h.active),
            })
            .collect();
        let (item, crop, origin, scale_factor, hdr) =
            match resolve_target(&self.cfg.target, &descs)? {
                ResolvedTarget::Monitor { index, crop } => {
                    let m = &monitors[index];
                    let centre = Point::new(
                        m.rect.x + (m.rect.width / 2) as i32,
                        m.rect.y + (m.rect.height / 2) as i32,
                    );
                    (ItemSpec::Monitor(centre), crop, m.rect.origin(), m.scale_factor, m.hdr)
                }
                ResolvedTarget::Window(hwnd) => {
                    let id = hwnd.to_string();
                    let scale = backend
                        .windows()?
                        .into_iter()
                        .find(|w| w.id == id)
                        .ok_or_else(|| SourceError::TargetNotFound(format!("window {id}")))?;
                    (ItemSpec::Window(hwnd), None, scale.rect.origin(), 1.0, None)
                }
            };
        Ok(Setup {
            item,
            crop,
            origin,
            scale_factor,
            hdr,
            fps: self.cfg.fps,
            cursor: self.cfg.cursor,
        })
    }
}

impl FrameSource for WgcSource {
    fn name(&self) -> &'static str {
        "wgc"
    }

    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        if !Self::is_available() {
            return Err(SourceError::Unavailable(
                "Windows.Graphics.Capture needs Windows 10 version 1903 or newer".into(),
            ));
        }
        if matches!(self.cfg.target, CaptureTarget::Pick) {
            return Err(SourceError::Unsupported(
                "the Windows source has no picker; choose a monitor or window".into(),
            ));
        }
        let setup = self.setup()?;
        self.mailbox = Arc::new(Mailbox::default());
        let (tx, rx) = mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<SourceInfo, SourceError>>();
        let mailbox = Arc::clone(&self.mailbox);
        let events = tx.clone();
        let worker = std::thread::Builder::new()
            .name("ssx-wgc".into())
            .spawn(move || run(&setup, clock, &mailbox, &rx, &events, &ready_tx))
            .map_err(|e| SourceError::backend("wgc", format!("cannot start the worker: {e}")))?;
        match ready_rx.recv_timeout(FIRST_FRAME_TIMEOUT + Duration::from_secs(5)) {
            Ok(Ok(info)) => {
                self.info = Some(info);
                self.control = Some(tx);
                self.worker = Some(worker);
                self.clock = Some(clock);
                self.ended = false;
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                let _ = tx.send(Msg::Stop);
                Err(SourceError::backend("wgc", "the capture worker did not report back"))
            }
        }
    }

    fn info(&self) -> SourceInfo {
        self.info.unwrap_or(SourceInfo {
            name: "wgc",
            size: Size::default(),
            format: PixelFormat::Bgra8,
            color_space: ssx_types::ColorSpace::Srgb,
            hdr: false,
            damage_driven: true,
            realtime: true,
        })
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        if self.clock.is_none() {
            return Err(SourceError::backend("wgc", "next_frame before start"));
        }
        if self.ended {
            return Ok(SourceEvent::Ended);
        }
        match self.mailbox.take(timeout) {
            Taken::Value(f) => Ok(SourceEvent::Frame(f)),
            Taken::Timeout => Ok(SourceEvent::Timeout),
            Taken::Ended(Ok(())) => {
                self.ended = true;
                Ok(SourceEvent::Ended)
            }
            Taken::Ended(Err(m)) => {
                self.ended = true;
                Err(SourceError::backend("wgc", m))
            }
        }
    }

    fn stop(&mut self) {
        self.ended = true;
        if let Some(tx) = self.control.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---- worker ---------------------------------------------------------------------------------

/// Keeps the worker thread's `WinRT` apartment initialised.
struct ComGuard(bool);

impl ComGuard {
    fn init() -> Self {
        // SAFETY: no preconditions; balanced by `RoUninitialize` in `Drop` on this thread.
        Self(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful `RoInitialize` of `init` on this thread.
            unsafe { RoUninitialize() };
        }
    }
}

fn run(
    setup: &Setup,
    clock: Clock,
    mailbox: &Mailbox<VideoFrame>,
    rx: &mpsc::Receiver<Msg>,
    events: &mpsc::Sender<Msg>,
    ready: &mpsc::Sender<Result<SourceInfo, SourceError>>,
) {
    let _com = ComGuard::init();
    let mut mapper = TimestampMapper::new();
    let mut capture = match Capture::open(setup, events) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // The first frame fixes the format the source reports.
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let first = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Frame) => match capture.process(setup, clock, &mut mapper) {
                Ok(Some(f)) => break f,
                Ok(None) => {}
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            },
            Ok(Msg::Closed) => {
                let _ = ready.send(Err(SourceError::TargetNotFound(
                    "the capture target closed before the first frame".into(),
                )));
                return;
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                let _ = ready.send(Err(SourceError::backend(
                    "wgc",
                    "no frame within 5 s (window minimised, cloaked or protected?)",
                )));
                return;
            }
        }
    };
    let info = SourceInfo {
        name: "wgc",
        size: first.frame.size(),
        format: first.frame.format(),
        color_space: first.frame.color_space(),
        hdr: first.frame.format() == PixelFormat::Rgba16F,
        damage_driven: true,
        realtime: true,
    };
    mailbox.put(first);
    if ready.send(Ok(info)).is_err() {
        return;
    }
    loop {
        match rx.recv() {
            Ok(Msg::Frame) => match capture.process(setup, clock, &mut mapper) {
                Ok(Some(f)) => mailbox.put(f),
                Ok(None) => {}
                Err(e) => {
                    mailbox.end(Err(e.to_string()));
                    return;
                }
            },
            Ok(Msg::Closed) => {
                mailbox.end(Ok(()));
                return;
            }
            Ok(Msg::Stop) | Err(_) => return,
        }
    }
}

/// One live capture session. Dropping it removes the handlers and closes session and pool.
struct Capture {
    winrt: IDirect3DDevice,
    reader: Reader,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    pool_size: (u32, u32),
    pool_format: DirectXPixelFormat,
    frame_token: i64,
    closed_token: i64,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Failures are ignored: the objects are being discarded and "already closed" needs
        // no reaction.
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Capture {
    fn open(setup: &Setup, events: &mpsc::Sender<Msg>) -> Result<Self, SourceError> {
        let (device, context) = create_device()?;
        let dxgi: IDXGIDevice = device.cast().map_err(api("QueryInterface(IDXGIDevice)"))?;
        // SAFETY: `dxgi` is a valid DXGI device.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(api("CreateDirect3D11DeviceFromDXGIDevice"))?;
        let winrt: IDirect3DDevice =
            inspectable.cast().map_err(api("QueryInterface(IDirect3DDevice)"))?;

        let item = create_item(&setup.item)?;
        let size = item.Size().map_err(api("GraphicsCaptureItem::Size"))?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err(SourceError::backend(
                "wgc",
                format!(
                    "the capture target reports an empty size ({}x{})",
                    size.Width, size.Height
                ),
            ));
        }
        let hdr = setup.hdr.is_some_and(|h| h.active) && matches!(setup.item, ItemSpec::Monitor(_));
        let pool_format = if hdr {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };
        let pool =
            Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt, pool_format, POOL_BUFFERS, size)
                .map_err(api("Direct3D11CaptureFramePool::CreateFreeThreaded"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(api("Direct3D11CaptureFramePool::CreateCaptureSession"))?;
        // WGC includes the cursor unless told otherwise: always set it explicitly.
        if let Err(e) = session.SetIsCursorCaptureEnabled(setup.cursor) {
            tracing::warn!(error = %e, "could not set cursor capture");
        }
        // Windows 11 / 22H2 only; older builds return an error, which is fine.
        let _ = session.SetIsBorderRequired(false);
        let interval = setup.fps.frame_duration() / 2;
        let _ =
            session.SetMinUpdateInterval(TimeSpan { Duration: (interval.as_nanos() / 100) as i64 });

        let frame_tx = events.clone();
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |_, _| {
                    // A gone receiver only means the capture already ended.
                    let _ = frame_tx.send(Msg::Frame);
                    Ok(())
                },
            ))
            .map_err(api("FrameArrived"))?;
        let closed_tx = events.clone();
        let closed_token = item
            .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                let _ = closed_tx.send(Msg::Closed);
                Ok(())
            }))
            .map_err(api("GraphicsCaptureItem::Closed"))?;
        // From here on `Drop` cleans up, whichever way we leave.
        let capture = Self {
            winrt,
            reader: Reader { device, context, staging: None },
            pool,
            session,
            item,
            pool_size: (size.Width as u32, size.Height as u32),
            pool_format,
            frame_token,
            closed_token,
        };
        capture.session.StartCapture().map_err(api("GraphicsCaptureSession::StartCapture"))?;
        Ok(capture)
    }

    /// Takes the newest frame out of the pool (older ones are dropped: a recorder wants the
    /// current screen) and turns it into a [`VideoFrame`]. `None` when the wake-up was for a
    /// frame an earlier call already consumed.
    fn process(
        &mut self,
        setup: &Setup,
        clock: Clock,
        mapper: &mut TimestampMapper,
    ) -> Result<Option<VideoFrame>, SourceError> {
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = newest.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(captured) = newest else { return Ok(None) };
        let arrival = clock.now();
        let result = self.read(&captured, setup, arrival, mapper);
        // Return the buffer to the pool promptly; failure is irrelevant here.
        let _ = captured.Close();
        // A resized window keeps arriving in the old buffer size until the pool is re-created.
        let content = captured.ContentSize().map_err(api("Direct3D11CaptureFrame::ContentSize"))?;
        if let PoolAction::Recreate { width, height } =
            pool_action(self.pool_size, (content.Width, content.Height))
        {
            self.pool
                .Recreate(
                    &self.winrt,
                    self.pool_format,
                    POOL_BUFFERS,
                    SizeInt32 { Width: width as i32, Height: height as i32 },
                )
                .map_err(api("Direct3D11CaptureFramePool::Recreate"))?;
            self.pool_size = (width, height);
        }
        result.map(Some)
    }

    fn read(
        &mut self,
        captured: &Direct3D11CaptureFrame,
        setup: &Setup,
        arrival: Duration,
        mapper: &mut TimestampMapper,
    ) -> Result<VideoFrame, SourceError> {
        let content = captured.ContentSize().map_err(api("Direct3D11CaptureFrame::ContentSize"))?;
        let system = captured
            .SystemRelativeTime()
            .map_err(api("Direct3D11CaptureFrame::SystemRelativeTime"))?
            .Duration;
        let surface = captured.Surface().map_err(api("Direct3D11CaptureFrame::Surface"))?;
        let access: IDirect3DDxgiInterfaceAccess =
            surface.cast().map_err(api("QueryInterface(IDirect3DDxgiInterfaceAccess)"))?;
        // SAFETY: the surface is backed by a D3D11 texture; `GetInterface` performs a checked
        // QueryInterface for the requested type.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(api("GetInterface(ID3D11Texture2D)"))?;
        let image = self.reader.read(&texture, (content.Width, content.Height))?;
        let timestamp = mapper.map(system, arrival);
        let sdr_white = setup.hdr.map(|h| h.sdr_white_nits);
        let mut frame = frame_from_readback(
            image.data,
            Size::new(image.width, image.height),
            image.dxgi,
            setup.origin,
            setup.scale_factor,
            sdr_white,
        )?;
        frame.timestamp = Some(timestamp);
        let frame = crop_to_region(frame, setup.crop)?;
        Ok(VideoFrame { frame, timestamp })
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), SourceError> {
    let mut device = None;
    let mut context = None;
    // SAFETY: the out-pointers reference live local `Option`s.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )
    }
    .map_err(api("D3D11CreateDevice"))?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err(SourceError::backend("wgc", "D3D11CreateDevice returned no device"));
    };
    // WGC's free-threaded pool touches the device from its own threads while the worker uses
    // the immediate context: make the multithread protection explicit (it is the default).
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        // SAFETY: plain COM call on a valid interface.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

fn create_item(spec: &ItemSpec) -> Result<GraphicsCaptureItem, SourceError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(api("IGraphicsCaptureItemInterop"))?;
    match spec {
        ItemSpec::Monitor(p) => {
            // SAFETY: plain query; `MONITOR_DEFAULTTONULL` yields a null handle for a point
            // outside every monitor (hot-unplugged since enumeration).
            let monitor =
                unsafe { MonitorFromPoint(POINT { x: p.x, y: p.y }, MONITOR_DEFAULTTONULL) };
            if monitor.is_invalid() {
                return Err(SourceError::TargetNotFound("the monitor is gone".into()));
            }
            // SAFETY: `monitor` was just obtained; the call validates it.
            unsafe { interop.CreateForMonitor(monitor) }.map_err(api("CreateForMonitor"))
        }
        ItemSpec::Window(raw) => {
            let window = HWND(*raw as *mut c_void);
            // SAFETY: the call validates the handle and fails cleanly for a dead window.
            unsafe { interop.CreateForWindow(window) }.map_err(api("CreateForWindow"))
        }
    }
}

// ---- GPU -> CPU read-back -------------------------------------------------------------------

/// A staging texture that is reused while the frame size and format stay the same.
struct Staging {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: i32,
}

struct Reader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<Staging>,
}

/// Tightly packed pixels read back from a texture.
struct Image {
    data: Vec<u8>,
    width: u32,
    height: u32,
    /// `DXGI_FORMAT` of the source texture.
    dxgi: u32,
}

impl Reader {
    /// Copies the top-left `content` part of `src` to the CPU.
    fn read(&mut self, src: &ID3D11Texture2D, content: (i32, i32)) -> Result<Image, SourceError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out-pointer.
        unsafe { src.GetDesc(&raw mut desc) };
        let dxgi = desc.Format.0 as u32;
        let (format, _) = native_format(dxgi).ok_or_else(|| {
            SourceError::InvalidFrame(format!("unsupported capture texture format {dxgi}"))
        })?;
        let (width, height) = clamp_content(content, (desc.Width, desc.Height))
            .ok_or_else(|| SourceError::InvalidFrame("the captured frame has no content".into()))?;

        let reuse = self
            .staging
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height && s.format == desc.Format.0);
        if !reuse {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut texture = None;
            // SAFETY: descriptor and out-pointer are valid for the call.
            unsafe {
                self.device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut texture))
            }
            .map_err(api("CreateTexture2D(staging)"))?;
            let texture = texture.ok_or_else(|| {
                SourceError::backend("wgc", "CreateTexture2D returned no texture")
            })?;
            self.staging = Some(Staging { texture, width, height, format: desc.Format.0 });
        }
        let Some(staging) = self.staging.as_ref() else {
            return Err(SourceError::backend("wgc", "no staging texture"));
        };

        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        // SAFETY: both textures belong to this device, `region` lies within `src` (clamped
        // above) and both have the same format.
        unsafe {
            self.context.CopySubresourceRegion(
                &staging.texture,
                0,
                0,
                0,
                0,
                src,
                0,
                Some(&raw const region),
            );
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a STAGING texture with CPU read access; valid out-pointer. Map waits for the
        // GPU copy above.
        unsafe { self.context.Map(&staging.texture, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }
            .map_err(api("ID3D11DeviceContext::Map"))?;
        let pitch = mapped.RowPitch as usize;
        let bpp = format.bytes_per_pixel();
        let data = if mapped.pData.is_null() {
            Err(SourceError::InvalidFrame("Map returned a null pointer".into()))
        } else {
            let len = pitch * (height as usize - 1) + width as usize * bpp;
            // SAFETY: a successful Map of a `width` x `height` texture exposes at least
            // `RowPitch * (height - 1) + row_bytes` readable bytes at `pData`; the mapping
            // lives until the `Unmap` below and the slice is copied before that.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            depad_rows(bytes, pitch, width, height, bpp)
        };
        // SAFETY: balances the successful Map of subresource 0 above.
        unsafe { self.context.Unmap(&staging.texture, 0) };
        Ok(Image { data: data?, width, height, dxgi })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// Needs an interactive desktop session; run with `cargo test -- --ignored` on Windows.
    #[test]
    #[ignore = "needs an interactive Windows desktop"]
    fn records_the_primary_monitor_for_a_second() {
        let mut src = WgcSource::new(SourceConfig::default());
        let clock = Clock::start();
        src.start(clock).expect("start");
        let info = src.info();
        assert!(info.size.width > 0 && info.size.height > 0);
        let t0 = Instant::now();
        let (mut frames, mut last) = (0, Duration::ZERO);
        while t0.elapsed() < Duration::from_secs(1) {
            match src.next_frame(Duration::from_millis(100)).expect("next_frame") {
                SourceEvent::Frame(f) => {
                    assert!(f.timestamp >= last, "timestamps never go backwards");
                    last = f.timestamp;
                    frames += 1;
                }
                SourceEvent::Timeout => {}
                SourceEvent::Ended => break,
            }
        }
        src.stop();
        assert!(frames >= 1, "at least the first frame arrives");
    }
}
