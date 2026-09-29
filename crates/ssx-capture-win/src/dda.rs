//! DXGI Desktop Duplication: fallback when Windows.Graphics.Capture is unavailable.
//!
//! Duplication is per *output* and tied to the GPU adapter that drives it, so each capture
//! creates a private D3D11 device on that adapter (no state is shared between calls).
//! `IDXGIOutput5::DuplicateOutput1` is asked for `[R16G16B16A16_FLOAT, B8G8R8A8_UNORM]` so an
//! HDR display yields linear scRGB; older systems fall back to `IDXGIOutput1::DuplicateOutput`
//! (8-bit). As with WGC, the resulting `Frame` is labelled from the texture that arrived.
//!
//! # Known limitations
//!
//! * **The cursor is not composited.** Duplication delivers the pointer as a separate shape
//!   plus position; blending it into float scRGB frames correctly is out of scope here, so
//!   `include_cursor` is ignored on this path (a warning is logged). WGC and GDI honour it.
//! * **Rotated displays** are handled by rotating the texture to the display orientation
//!   (see [`crate::pixels::rotate`]); the direction has not been verified on hardware, and a
//!   size mismatch against the monitor rectangle is logged.
//! * It cannot capture the secure desktop (UAC prompt, lock screen) and fails with
//!   `E_ACCESSDENIED` there; the error message says so.

use ssx_capture::CaptureOptions;
use ssx_types::{Frame, Size};
use windows::{
    Win32::Graphics::{
        Direct3D11::ID3D11Texture2D,
        Dxgi::{
            Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
            DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIOutput, IDXGIOutput1, IDXGIOutput5,
            IDXGIOutputDuplication, IDXGIResource,
        },
    },
    core::Interface,
};

use crate::{
    acquire::{ACQUIRE_TIMEOUT_MS, Step, classify, next_step},
    chain::{MonitorCapturer, MonitorTarget},
    d3d::{self, D3dDevice},
    error::WinError,
    hdr::CaptureFormat,
    pixels::{Placement, Rotation, frame_from_packed, rotate},
    sys::{api_err, hmonitor},
};

/// Releases the acquired duplication frame on drop (a frame must be released before the
/// next `AcquireNextFrame`, and before the duplication is dropped).
struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful `AcquireNextFrame`. A failure here
        // (e.g. access lost meanwhile) leaves nothing to clean up.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

/// The Desktop Duplication capturer.
#[derive(Debug, Default)]
pub(crate) struct Dda;

impl Dda {
    /// Captures a whole monitor via Desktop Duplication.
    pub(crate) fn capture_monitor(
        target: &MonitorTarget,
        opts: CaptureOptions,
    ) -> Result<Frame, WinError> {
        crate::sys::ensure_com();
        if opts.include_cursor {
            tracing::warn!(
                "desktop duplication does not composite the cursor; capturing without it"
            );
        }
        let found = d3d::find_output(hmonitor(target.handle))?;
        let adapter: IDXGIAdapter =
            found.adapter.cast().map_err(api_err("QueryInterface(IDXGIAdapter)"))?;
        let device = D3dDevice::create(Some(&adapter))?;
        let mut duplication = create_duplication(&found.output, &device)?;

        let (mut attempts, mut recreates) = (0u32, 0u32);
        let (resource, rotation) = loop {
            attempts += 1;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: valid out-pointers; the duplication is live.
            let acquired = unsafe {
                duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &raw mut info, &raw mut resource)
            };
            let outcome = classify(match &acquired {
                // A frame without a resource cannot be used even if it claims a present.
                Ok(()) if resource.is_some() => Ok(info.LastPresentTime),
                Ok(()) => Ok(0),
                Err(e) => Err(e.code().0),
            });
            match next_step(outcome, attempts, recreates) {
                Step::Done => {
                    // SAFETY: plain query on a live duplication.
                    let rotation = unsafe { duplication.GetDesc() }.Rotation.0;
                    let Some(resource) = resource else {
                        return Err(WinError::Other("duplication returned no resource".into()));
                    };
                    break (resource, Rotation::from_dxgi(rotation));
                }
                Step::Retry => {
                    if acquired.is_ok() {
                        // A frame without an image still has to be released.
                        // SAFETY: the acquire above succeeded.
                        let _ = unsafe { duplication.ReleaseFrame() };
                    }
                }
                Step::Recreate => {
                    tracing::debug!("desktop duplication access lost; re-creating it");
                    duplication = create_duplication(&found.output, &device)?;
                    recreates += 1;
                }
                Step::Fail => {
                    return Err(match acquired {
                        Err(e) => api_err("IDXGIOutputDuplication::AcquireNextFrame")(e),
                        Ok(()) => WinError::Timeout(std::time::Duration::from_millis(
                            u64::from(ACQUIRE_TIMEOUT_MS) * u64::from(attempts),
                        )),
                    });
                }
            }
        };

        // The frame is now acquired; the guard releases it when this scope ends.
        let _frame = FrameGuard(&duplication);
        let texture: ID3D11Texture2D =
            resource.cast().map_err(api_err("QueryInterface(ID3D11Texture2D)"))?;
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out-pointer.
        unsafe { texture.GetDesc(&raw mut desc) };
        let image = d3d::read_back(&device, &texture, (desc.Width, desc.Height))?;

        let (data, width, height) = rotate(
            &image.data,
            image.width,
            image.height,
            image.format.bytes_per_pixel(),
            rotation,
        )?;
        let expected = target.monitor.rect.size();
        if Size::new(width, height) != expected {
            tracing::warn!(
                got = ?(width, height),
                expected = ?(expected.width, expected.height),
                ?rotation,
                "desktop duplication size differs from the monitor rectangle"
            );
        }
        if target.monitor.hdr.is_some_and(|h| h.active) && image.format != CaptureFormat::Rgba16F {
            tracing::warn!(
                "HDR display but desktop duplication returned 8-bit pixels; highlights are clipped"
            );
        }
        frame_from_packed(
            data,
            Size::new(width, height),
            image.format,
            &Placement {
                origin: target.monitor.rect.origin(),
                scale_factor: target.monitor.scale_factor,
                hdr: target.monitor.hdr,
            },
        )
    }
}

/// Creates the duplication, preferring the float-capable `DuplicateOutput1`.
fn create_duplication(
    output: &IDXGIOutput,
    device: &D3dDevice,
) -> Result<IDXGIOutputDuplication, WinError> {
    if let Ok(output5) = output.cast::<IDXGIOutput5>() {
        // SAFETY: `device.device` is a valid D3D11 device on the output's adapter.
        match unsafe {
            output5.DuplicateOutput1(
                &device.device,
                0,
                &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM],
            )
        } {
            Ok(d) => return Ok(d),
            Err(e) => {
                tracing::debug!(error = %e, "DuplicateOutput1 failed; trying DuplicateOutput");
            }
        }
    }
    let output1: IDXGIOutput1 = output.cast().map_err(api_err("QueryInterface(IDXGIOutput1)"))?;
    // SAFETY: as above.
    unsafe { output1.DuplicateOutput(&device.device) }.map_err(api_err("DuplicateOutput"))
}

/// Chain stage wrapper.
pub(crate) struct DdaStage;

impl MonitorCapturer for DdaStage {
    fn name(&self) -> &'static str {
        "dda"
    }

    fn capture(&self, target: &MonitorTarget, opts: CaptureOptions) -> Result<Frame, WinError> {
        Dda::capture_monitor(target, opts)
    }
}
