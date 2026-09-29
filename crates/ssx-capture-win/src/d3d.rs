//! Direct3D 11 / DXGI plumbing shared by the WGC and Desktop Duplication paths: device
//! creation, output lookup and GPU-to-CPU read-back.
//!
//! Both capture APIs hand out a GPU texture. Turning it into a [`ssx_types::Frame`] means
//! copying it to a `D3D11_USAGE_STAGING` texture, mapping that, and de-padding the rows
//! (the padding maths lives in `pixels.rs` where it is unit-tested). Doing the copy with
//! `CopySubresourceRegion` also crops to a smaller "content size" in the same GPU step.

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN},
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, IDXGIAdapter, IDXGIAdapter1,
                IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
            },
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

use crate::{
    error::WinError,
    hdr::{CaptureFormat, DxgiOutputHdr},
    pixels,
    sys::api_err,
};

/// A D3D11 device and its immediate context.
///
/// The device is free-threaded (created without `D3D11_CREATE_DEVICE_SINGLETHREADED`), but
/// the *immediate context* is not: callers serialise use of it (the WGC backend keeps
/// the device behind a `Mutex`; the DDA backend creates a private device per capture).
pub(crate) struct D3dDevice {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
}

// SAFETY: `ID3D11Device` is free-threaded. `ID3D11DeviceContext` (immediate) must not be used
// concurrently, but every `D3dDevice` is either owned by a single capture call or stored
// behind a `Mutex` that is held for the whole capture, so no two threads touch the context
// at once; moving the value between threads is fine.
unsafe impl Send for D3dDevice {}

impl D3dDevice {
    /// Creates a hardware device with BGRA support (required by WGC), on `adapter` if given
    /// (Desktop Duplication needs the device on the output's own adapter).
    pub(crate) fn create(adapter: Option<&IDXGIAdapter>) -> Result<Self, WinError> {
        let driver: D3D_DRIVER_TYPE =
            if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let mut device = None;
        let mut context = None;
        // SAFETY: the out-pointers reference live local `Option`s; `adapter` (if any) is a
        // valid COM pointer for the duration of the call.
        unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }
        .map_err(api_err("D3D11CreateDevice"))?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(WinError::Other("D3D11CreateDevice returned no device".into()));
        };
        // WGC's free-threaded frame pool touches the device from its own threads while we use
        // the immediate context, so make the context's multithread protection explicit
        // rather than relying on the default. Best effort: it is the default anyway.
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            // SAFETY: plain COM call on a valid interface.
            let _ = unsafe { multithread.SetMultithreadProtected(true) };
        }
        Ok(Self { device, context })
    }

    /// `Err(DeviceLost)` if the GPU device has been removed or reset since creation.
    pub(crate) fn check_alive(&self) -> Result<(), WinError> {
        // SAFETY: plain COM call on a valid device.
        unsafe { self.device.GetDeviceRemovedReason() }
            .map_err(|e| WinError::DeviceLost(format!("device removed: {}", e.message())))
    }
}

/// CPU-side pixels read back from a GPU texture, tightly packed.
pub(crate) struct CpuImage {
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Format of the texture the pixels came from (not what was requested).
    pub(crate) format: CaptureFormat,
}

/// Unmaps a staging resource on drop so every early return releases the mapping.
struct MappedGuard<'a> {
    context: &'a ID3D11DeviceContext,
    resource: &'a ID3D11Texture2D,
}

impl Drop for MappedGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful `Map` of subresource 0 of `resource`.
        unsafe { self.context.Unmap(self.resource, 0) };
    }
}

/// Copies the top-left `size` (width, height) of `src` to the CPU.
///
/// The read-back format is taken from `src` itself, so the result is labelled correctly
/// even if the API delivered a different format than was requested.
pub(crate) fn read_back(
    d3d: &D3dDevice,
    src: &ID3D11Texture2D,
    size: (u32, u32),
) -> Result<CpuImage, WinError> {
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `src_desc` is a valid out-pointer.
    unsafe { src.GetDesc(&raw mut src_desc) };
    let format = CaptureFormat::from_dxgi(src_desc.Format.0).ok_or_else(|| {
        WinError::Other(format!(
            "unsupported capture texture format (DXGI_FORMAT {})",
            src_desc.Format.0
        ))
    })?;
    let (width, height) = (size.0.min(src_desc.Width), size.1.min(src_desc.Height));
    if width == 0 || height == 0 {
        return Err(WinError::Other("capture texture is empty".into()));
    }

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: src_desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: `staging_desc` and the out-pointer are valid for the call.
    unsafe { d3d.device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging)) }
        .map_err(api_err("CreateTexture2D(staging)"))?;
    let staging =
        staging.ok_or_else(|| WinError::Other("CreateTexture2D returned no texture".into()))?;

    let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
    // SAFETY: both textures belong to `d3d.device` (or a device sharing the adapter, which
    // WGC/DDA guarantee), `region` lies within `src` (clamped above) and has the same
    // format as `staging`.
    unsafe {
        d3d.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, src, 0, Some(&raw const region));
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` is a STAGING texture with CPU read access; `mapped` is a valid
    // out-pointer. Map waits for the GPU copy above to finish.
    unsafe { d3d.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }
        .map_err(api_err("ID3D11DeviceContext::Map"))?;
    let _unmap = MappedGuard { context: &d3d.context, resource: &staging };

    let pitch = mapped.RowPitch as usize;
    let row_bytes = width as usize * format.bytes_per_pixel();
    let len = pixels::mapped_len(pitch, row_bytes, height)
        .ok_or_else(|| WinError::Other("mapped surface size overflows".into()))?;
    if mapped.pData.is_null() {
        return Err(WinError::Other("Map returned a null pointer".into()));
    }
    // SAFETY: a successful Map of a `width` x `height` texture exposes at least
    // `RowPitch * (height - 1) + row_bytes` readable bytes at `pData`, which is exactly
    // `len`; the mapping stays alive until `_unmap` drops at the end of this function, and
    // the slice is consumed (copied) before that.
    let mapped_bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
    let data = pixels::depad_rows(mapped_bytes, pitch, width, height, format.bytes_per_pixel())?;
    Ok(CpuImage { data, width, height, format })
}

/// A DXGI output together with the adapter that owns it.
pub(crate) struct OutputRef {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) output: IDXGIOutput,
}

/// Finds the DXGI output that drives `monitor`.
pub(crate) fn find_output(monitor: HMONITOR) -> Result<OutputRef, WinError> {
    // SAFETY: plain factory creation; the result is an owned COM pointer.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(api_err("CreateDXGIFactory1"))?;
    for adapter_index in 0.. {
        // SAFETY: COM call on a valid factory.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(api_err("IDXGIFactory1::EnumAdapters1")(e)),
        };
        for output_index in 0.. {
            // SAFETY: COM call on a valid adapter.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(api_err("IDXGIAdapter::EnumOutputs")(e)),
            };
            // SAFETY: COM call on a valid output.
            let desc = unsafe { output.GetDesc() }.map_err(api_err("IDXGIOutput::GetDesc"))?;
            if desc.Monitor == monitor {
                return Ok(OutputRef { adapter, output });
            }
        }
    }
    Err(WinError::Other("no DXGI output corresponds to this monitor".into()))
}

/// HDR details DXGI knows about `monitor` (colour space and peak luminance). `None` when
/// the output or `IDXGIOutput6` (Windows 10 1703+) is unavailable.
pub(crate) fn output_hdr_details(monitor: HMONITOR) -> Option<DxgiOutputHdr> {
    let found = find_output(monitor).ok()?;
    let output6: IDXGIOutput6 = found.output.cast().ok()?;
    // SAFETY: COM call on a valid output.
    let desc = unsafe { output6.GetDesc1() }.ok()?;
    Some(DxgiOutputHdr {
        hdr10: desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        max_luminance_nits: desc.MaxLuminance,
    })
}
