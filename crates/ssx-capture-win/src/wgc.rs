//! Windows.Graphics.Capture (WGC): the primary capture path.
//!
//! WGC is the only Windows API that both captures individual windows and can deliver
//! `R16G16B16A16_FLOAT` scRGB frames from an HDR display. Each capture is self-contained:
//! a fresh frame pool and session are created, one frame is taken, and everything is torn
//! down again. Only the (expensive) D3D11 device is shared between captures, lazily created
//! and kept behind a `Mutex`; a lost device is dropped and re-created by the retry in
//! `chain::retry_once_on_device_lost`.
//!
//! Choices worth knowing about:
//!
//! * The pool uses the *free-threaded* variant so `FrameArrived` fires on a WGC thread and
//!   the caller thread needs no dispatcher queue or message pump. The event only wakes the
//!   waiting caller through a channel (no polling, no busy loop).
//! * The pool format is requested from the monitor's HDR state, but the resulting `Frame` is
//!   labelled from the texture that actually arrives.
//! * `IsBorderRequired(false)` (the yellow capture border) needs Windows 11; failure is
//!   ignored. `IsCursorCaptureEnabled` is always set explicitly because WGC defaults to
//!   *including* the cursor.
//! * WGC hands out no frame for a window that is occluded-and-cloaked or minimised, so a
//!   bounded wait turns "nothing ever arrives" into [`WinError::Timeout`] instead of a hang.

use std::{
    sync::{
        Mutex, PoisonError,
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use ssx_capture::CaptureOptions;
use ssx_types::{Frame, Size};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Graphics::{Direct3D11::ID3D11Texture2D, Dxgi::IDXGIDevice, Gdi::HMONITOR},
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    core::{IInspectable, Interface},
};

use crate::{
    chain::{MonitorCapturer, MonitorTarget, retry_once_on_device_lost},
    d3d::{self, D3dDevice},
    error::WinError,
    geometry::clamp_content_size,
    hdr::CaptureFormat,
    pixels::{Placement, frame_from_packed},
    sys::{api_err, ensure_com, hmonitor, hwnd},
    windows::WindowTarget,
};

/// How long to wait for the first frame. A static desktop still delivers one immediately;
/// only unsuitable targets (minimised, cloaked, protected) run into this.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// Frames kept in the pool; WGC recommends at least 2.
const POOL_BUFFERS: i32 = 2;

/// A shared D3D device plus its `WinRT` wrapper.
struct SharedDevice {
    d3d: D3dDevice,
    winrt: IDirect3DDevice,
}

// SAFETY: see `D3dDevice`; `IDirect3DDevice` is a WinRT object marked agile by the runtime.
// The struct lives inside a `Mutex`, so access is serialised anyway.
unsafe impl Send for SharedDevice {}

impl SharedDevice {
    fn create() -> Result<Self, WinError> {
        let d3d = D3dDevice::create(None)?;
        let dxgi: IDXGIDevice =
            d3d.device.cast().map_err(api_err("QueryInterface(IDXGIDevice)"))?;
        // SAFETY: `dxgi` is a valid DXGI device.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(api_err("CreateDirect3D11DeviceFromDXGIDevice"))?;
        let winrt = inspectable.cast().map_err(api_err("QueryInterface(IDirect3DDevice)"))?;
        Ok(Self { d3d, winrt })
    }
}

/// What a single capture should produce.
struct CaptureSpec {
    /// Format requested for the frame pool.
    format: CaptureFormat,
    placement: Placement,
}

/// The WGC capturer. Cheap to construct; the D3D device is created on first use.
pub(crate) struct Wgc {
    device: Mutex<Option<SharedDevice>>,
}

impl std::fmt::Debug for Wgc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wgc")
    }
}

/// Events delivered to the waiting caller thread.
enum Event {
    Frame,
    Closed,
}

/// Tears a capture down (in the right order) however the function exits.
struct Teardown<'a> {
    pool: &'a Direct3D11CaptureFramePool,
    session: &'a GraphicsCaptureSession,
    item: &'a GraphicsCaptureItem,
    frame_token: Option<i64>,
    closed_token: Option<i64>,
}

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        // All failures below are ignored: the objects are being discarded anyway and the
        // only sensible reaction to "already closed" is to carry on.
        if let Some(t) = self.frame_token {
            let _ = self.pool.RemoveFrameArrived(t);
        }
        if let Some(t) = self.closed_token {
            let _ = self.item.RemoveClosed(t);
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Wgc {
    pub(crate) fn new() -> Self {
        Self { device: Mutex::new(None) }
    }

    /// Whether this Windows build supports WGC at all (Windows 10 1803+ for the API,
    /// 1903+ for what we need in practice).
    pub(crate) fn is_supported() -> bool {
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
    }

    /// Captures a whole monitor.
    pub(crate) fn capture_monitor(
        &self,
        target: &MonitorTarget,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        let monitor = hmonitor(target.handle);
        let spec = CaptureSpec {
            format: CaptureFormat::for_hdr(target.monitor.hdr),
            placement: Placement {
                origin: target.monitor.rect.origin(),
                scale_factor: target.monitor.scale_factor,
                hdr: target.monitor.hdr,
            },
        };
        retry_once_on_device_lost("wgc monitor capture", || {
            self.capture(&|| item_for_monitor(monitor), &spec, opts)
        })
    }

    /// Captures a single window.
    pub(crate) fn capture_window(
        &self,
        target: &WindowTarget,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        let window = hwnd(target.hwnd);
        let spec = CaptureSpec {
            format: CaptureFormat::for_hdr(target.hdr),
            placement: Placement {
                origin: target.bounds.origin(),
                scale_factor: target.scale_factor,
                hdr: target.hdr,
            },
        };
        retry_once_on_device_lost("wgc window capture", || {
            self.capture(&|| item_for_window(window), &spec, opts)
        })
    }

    /// One attempt: locks the shared device (creating it if needed), captures, and drops the
    /// device if the GPU reports it lost so the next attempt starts fresh.
    fn capture(
        &self,
        make_item: &dyn Fn() -> Result<GraphicsCaptureItem, WinError>,
        spec: &CaptureSpec,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        ensure_com();
        // A poisoned lock only means another capture panicked; the device is still usable.
        let mut slot = self.device.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(SharedDevice::create()?);
        }
        let Some(device) = slot.as_ref() else {
            return Err(WinError::Other("no D3D device".into()));
        };
        match capture_on(device, make_item()?, spec, opts) {
            Ok(frame) => Ok(frame),
            Err(e) => {
                // Ask the device itself whether it died: the failing call may have
                // reported something less specific than DXGI_ERROR_DEVICE_REMOVED.
                let lost =
                    matches!(e, WinError::DeviceLost(_)) || device.d3d.check_alive().is_err();
                if !lost {
                    return Err(e);
                }
                *slot = None;
                Err(match e {
                    lost @ WinError::DeviceLost(_) => lost,
                    other => WinError::DeviceLost(other.to_string()),
                })
            }
        }
    }
}

fn item_for_monitor(monitor: HMONITOR) -> Result<GraphicsCaptureItem, WinError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(api_err("IGraphicsCaptureItemInterop"))?;
    // SAFETY: `monitor` is a handle obtained from a fresh enumeration; the call validates it.
    unsafe { interop.CreateForMonitor(monitor) }.map_err(api_err("CreateForMonitor"))
}

fn item_for_window(
    window: windows::Win32::Foundation::HWND,
) -> Result<GraphicsCaptureItem, WinError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(api_err("IGraphicsCaptureItemInterop"))?;
    // SAFETY: the call validates the window handle and fails cleanly for a dead one.
    unsafe { interop.CreateForWindow(window) }.map_err(api_err("CreateForWindow"))
}

fn pixel_format(format: CaptureFormat) -> DirectXPixelFormat {
    match format {
        CaptureFormat::Rgba16F => DirectXPixelFormat::R16G16B16A16Float,
        CaptureFormat::Bgra8 | CaptureFormat::Rgba8 => DirectXPixelFormat::B8G8R8A8UIntNormalized,
    }
}

/// Runs one capture of `item` on `device`.
fn capture_on(
    device: &SharedDevice,
    item: GraphicsCaptureItem,
    spec: &CaptureSpec,
    opts: CaptureOptions,
) -> Result<Frame, WinError> {
    let item_size = item.Size().map_err(api_err("GraphicsCaptureItem::Size"))?;
    if item_size.Width <= 0 || item_size.Height <= 0 {
        return Err(WinError::Other(format!(
            "capture target reports an empty size ({}x{})",
            item_size.Width, item_size.Height
        )));
    }

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device.winrt,
        pixel_format(spec.format),
        POOL_BUFFERS,
        item_size,
    )
    .map_err(api_err("Direct3D11CaptureFramePool::CreateFreeThreaded"))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(api_err("Direct3D11CaptureFramePool::CreateCaptureSession"))?;
    let mut teardown = Teardown {
        pool: &pool,
        session: &session,
        item: &item,
        frame_token: None,
        closed_token: None,
    };

    // WGC includes the cursor unless told otherwise, so always set it explicitly.
    if let Err(e) = session.SetIsCursorCaptureEnabled(opts.include_cursor) {
        tracing::warn!(error = %e, want_cursor = opts.include_cursor, "could not set cursor capture");
    }
    // Windows 11 only; older builds return an error which is fine.
    let _ = session.SetIsBorderRequired(false);

    let (tx, rx) = mpsc::channel::<Event>();
    let frame_tx = tx.clone();
    teardown.frame_token = Some(
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |_, _| {
                // The receiver being gone just means the capture already finished.
                let _ = frame_tx.send(Event::Frame);
                Ok(())
            },
        ))
        .map_err(api_err("FrameArrived"))?,
    );
    teardown.closed_token = Some(
        item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            let _ = tx.send(Event::Closed);
            Ok(())
        }))
        .map_err(api_err("GraphicsCaptureItem::Closed"))?,
    );

    session.StartCapture().map_err(api_err("GraphicsCaptureSession::StartCapture"))?;

    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let captured = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Frame) => {
                // `TryGetNextFrame` can legitimately return nothing (an earlier wake-up
                // already consumed the frame); keep waiting until the deadline.
                if let Ok(frame) = pool.TryGetNextFrame() {
                    break frame;
                }
            }
            Ok(Event::Closed) => return Err(WinError::SourceClosed),
            Err(RecvTimeoutError::Timeout) => return Err(WinError::Timeout(FIRST_FRAME_TIMEOUT)),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(WinError::Other("capture event channel closed unexpectedly".into()));
            }
        }
    };

    let result = read_frame(device, &captured, spec);
    // Return the buffer to the pool promptly; failure is irrelevant at this point.
    let _ = captured.Close();
    result
}

/// Copies a captured WGC frame to the CPU and wraps it in a [`Frame`].
fn read_frame(
    device: &SharedDevice,
    captured: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    spec: &CaptureSpec,
) -> Result<Frame, WinError> {
    let content = captured.ContentSize().map_err(api_err("Direct3D11CaptureFrame::ContentSize"))?;
    let surface = captured.Surface().map_err(api_err("Direct3D11CaptureFrame::Surface"))?;
    let access: IDirect3DDxgiInterfaceAccess =
        surface.cast().map_err(api_err("QueryInterface(IDirect3DDxgiInterfaceAccess)"))?;
    // SAFETY: the surface is backed by a D3D11 texture; `GetInterface` performs a checked
    // QueryInterface for the requested type.
    let texture: ID3D11Texture2D =
        unsafe { access.GetInterface() }.map_err(api_err("GetInterface(ID3D11Texture2D)"))?;

    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid out-pointer.
    unsafe { texture.GetDesc(&raw mut desc) };
    let (width, height) =
        clamp_content_size((content.Width, content.Height), (desc.Width, desc.Height))
            .ok_or_else(|| WinError::Other("captured frame has no content".into()))?;

    let image = d3d::read_back(&device.d3d, &texture, (width, height))?;
    if image.format != spec.format {
        tracing::debug!(
            requested = ?spec.format,
            received = ?image.format,
            "WGC delivered a different pixel format than requested; labelling from the texture"
        );
    }
    frame_from_packed(
        image.data,
        Size::new(image.width, image.height),
        image.format,
        &spec.placement,
    )
}

/// Chain stage wrapper so WGC can be listed next to the other capturers.
pub(crate) struct WgcStage(pub(crate) std::sync::Arc<Wgc>);

impl MonitorCapturer for WgcStage {
    fn name(&self) -> &'static str {
        "wgc"
    }

    fn capture(&self, target: &MonitorTarget, opts: CaptureOptions) -> Result<Frame, WinError> {
        self.0.capture_monitor(target, opts)
    }
}
