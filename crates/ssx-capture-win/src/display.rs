//! Monitor enumeration: geometry and DPI from GDI/HiDpi, friendly name, refresh rate and
//! HDR state from `DisplayConfig`, peak luminance from DXGI.
//!
//! `Monitor::id` is the GDI device name (`\\.\DISPLAY1`). It is what `EnumDisplayMonitors`
//! and `EnumDisplaySettings` key on, survives across calls within a session, and lets
//! `DisplayConfig` data be joined via `DISPLAYCONFIG_SOURCE_DEVICE_NAME` (the join logic is in
//! `topology.rs`). The `HMONITOR` is *not* the id: handles can be recycled after a hot-plug,
//! so every capture re-resolves the id to a fresh handle.
//!
//! Each `DisplayConfig` query is best effort. If one fails the monitor is still listed, just
//! with less detail (`hdr: None`, no refresh rate), because an incomplete list is more
//! useful to a screenshot tool than an error.

use std::mem::size_of;

use ssx_capture::CaptureError;
use ssx_types::Monitor;
use windows::{
    Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
            DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
            QueryDisplayConfig,
        },
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LPARAM, RECT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
        },
        UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_PER_MONITOR_AWARE,
            GetAwarenessFromDpiAwarenessContext, GetDpiForMonitor, GetThreadDpiAwarenessContext,
            MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
        },
        UI::WindowsAndMessaging::MONITORINFOF_PRIMARY,
    },
    core::BOOL,
};

use crate::{
    chain::MonitorTarget,
    d3d,
    error::{BACKEND_NAME, WinError},
    filter::utf16_until_nul,
    geometry::{rect_from_edges, refresh_hz, scale_factor_from_dpi},
    hdr::{AdvancedColor, build_hdr_info},
    sys::hmonitor,
    topology::{DisplayPath, display_name, find_path},
};

/// Makes the process per-monitor-DPI-aware (v2).
///
/// Without this Windows reports DPI-virtualised monitor rectangles and window bounds, which
/// no longer match the physical pixels the capture APIs return. Declaring the awareness in
/// the application manifest is preferable and makes this a no-op success; calling it late
/// is a fallback. It is idempotent and safe to call repeatedly.
///
/// # Errors
/// If the awareness was already fixed to something other than per-monitor.
pub fn ensure_per_monitor_dpi_aware() -> Result<(), CaptureError> {
    // SAFETY: the context constant is valid; the call only changes process state.
    match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        Ok(()) => Ok(()),
        Err(e) => {
            // Typically E_ACCESSDENIED: the awareness was already set (manifest or an
            // earlier call). That is fine as long as it is per-monitor.
            // SAFETY: both calls are pure queries.
            let awareness =
                unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
            if awareness == DPI_AWARENESS_PER_MONITOR_AWARE {
                Ok(())
            } else {
                Err(CaptureError::backend(
                    BACKEND_NAME,
                    format!(
                        "the process is not per-monitor DPI aware ({e}); capture coordinates \
                         would be wrong. Declare <dpiAwareness>PerMonitorV2</dpiAwareness> in \
                         the application manifest"
                    ),
                ))
            }
        }
    }
}

/// Lists the attached monitors, primary first is *not* guaranteed: order follows
/// `EnumDisplayMonitors`.
pub(crate) fn enumerate_targets() -> Result<Vec<MonitorTarget>, WinError> {
    let handles = enum_hmonitors()?;
    if handles.is_empty() {
        return Err(WinError::Other(
            "no active displays (is this a service/session-0 process or a locked session?)".into(),
        ));
    }
    let paths = query_paths().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "DisplayConfig unavailable; monitors will lack names/HDR state");
        Vec::new()
    });
    Ok(handles.into_iter().filter_map(|h| build_target(h, &paths)).collect())
}

/// Resolves a [`Monitor::id`] to a fresh target.
pub(crate) fn target_by_id(id: &str) -> Result<MonitorTarget, WinError> {
    enumerate_targets()?
        .into_iter()
        .find(|t| t.monitor.id.eq_ignore_ascii_case(id))
        .ok_or_else(|| WinError::NotFound(id.to_owned()))
}

/// Resolves an `HMONITOR` (e.g. from `MonitorFromWindow`) to a target.
pub(crate) fn target_by_handle(handle: HMONITOR) -> Result<MonitorTarget, WinError> {
    let raw = handle.0 as isize;
    enumerate_targets()?
        .into_iter()
        .find(|t| t.handle == raw)
        .ok_or_else(|| WinError::NotFound(format!("HMONITOR {raw:#x}")))
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<isize>` passed by `enum_hmonitors`, which outlives the
    // synchronous `EnumDisplayMonitors` call, and the callback never runs concurrently.
    let out = unsafe { &mut *(data.0 as *mut Vec<isize>) };
    out.push(monitor.0 as isize);
    BOOL(1)
}

fn enum_hmonitors() -> Result<Vec<HMONITOR>, WinError> {
    let mut raw: Vec<isize> = Vec::new();
    // SAFETY: the callback only touches the Vec behind `data`, which is valid for the whole
    // (synchronous) call.
    let ok = unsafe {
        EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM((&raw mut raw) as isize))
    };
    if !ok.as_bool() {
        return Err(WinError::Other("EnumDisplayMonitors failed".into()));
    }
    Ok(raw.into_iter().map(hmonitor).collect())
}

fn build_target(handle: HMONITOR, paths: &[DisplayPath]) -> Option<MonitorTarget> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `MONITORINFOEXW` starts with a `MONITORINFO`, `cbSize` says the buffer is the
    // larger struct, so the API may write the extra `szDevice` field.
    let ok = unsafe { GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        tracing::debug!(handle = ?handle.0, "GetMonitorInfoW failed; skipping monitor");
        return None;
    }
    let device = utf16_until_nul(&info.szDevice);
    let r = info.monitorInfo.rcMonitor;
    let rect = rect_from_edges(r.left, r.top, r.right, r.bottom);

    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    // SAFETY: valid out-pointers. On failure the DPI stays 0 and maps to scale 1.0.
    let dpi =
        unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y) }
            .map_or(0, |()| dpi_x);

    let path = find_path(paths, &device);
    let dxgi = d3d::output_hdr_details(handle);
    let advanced_color = path.and_then(|p| p.advanced_color);
    let hdr = (advanced_color.is_some() || dxgi.is_some())
        .then(|| build_hdr_info(advanced_color, path.and_then(|p| p.raw_sdr_white), dxgi));

    let monitor = Monitor {
        name: display_name(path.map_or("", |p| p.friendly_name.as_str()), &device),
        id: device,
        rect,
        scale_factor: scale_factor_from_dpi(dpi),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        refresh_hz: path.and_then(|p| p.refresh_hz),
        hdr,
    };
    Some(MonitorTarget { monitor, handle: handle.0 as isize })
}

/// Issues a `DisplayConfigGetDeviceInfo` request.
///
/// # Safety
/// `T` must be a `DISPLAYCONFIG_*` request struct whose first field is a
/// `DISPLAYCONFIG_DEVICE_INFO_HEADER` with `type` and `size` already set to match `T`.
unsafe fn get_device_info<T>(request: &mut T) -> bool {
    // The pointer is derived from the whole struct, so the API may write all of it.
    let header = std::ptr::from_mut(request).cast::<DISPLAYCONFIG_DEVICE_INFO_HEADER>();
    // SAFETY: as above.
    let status = unsafe { DisplayConfigGetDeviceInfo(header) };
    status == 0
}

fn query_paths() -> Result<Vec<DisplayPath>, WinError> {
    let raw_paths = query_raw_paths()?;
    let mut out = Vec::with_capacity(raw_paths.len());
    for path in &raw_paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: `source` is a SOURCE_NAME request with a matching header.
        if !unsafe { get_device_info(&mut source) } {
            continue;
        }

        let target_header = |ty, size| DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: ty,
            size: size as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        };

        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: target_header(
                DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(),
            ),
            ..Default::default()
        };
        // SAFETY: TARGET_NAME request with a matching header.
        let friendly_name = if unsafe { get_device_info(&mut target) } {
            utf16_until_nul(&target.monitorFriendlyDeviceName)
        } else {
            String::new()
        };

        let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
            header: target_header(
                DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(),
            ),
            ..Default::default()
        };
        // SAFETY: ADVANCED_COLOR_INFO request with a matching header; reading the `value`
        // arm of the bit-field union is valid for any bit pattern.
        let advanced_color = unsafe {
            get_device_info(&mut color).then(|| AdvancedColor::from_bits(color.Anonymous.value))
        };

        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: target_header(
                DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
            ),
            ..Default::default()
        };
        // SAFETY: SDR_WHITE_LEVEL request with a matching header.
        let raw_sdr_white = unsafe { get_device_info(&mut white) }.then_some(white.SDRWhiteLevel);

        let rate = path.targetInfo.refreshRate;
        out.push(DisplayPath {
            gdi_device_name: utf16_until_nul(&source.viewGdiDeviceName),
            friendly_name,
            refresh_hz: refresh_hz(rate.Numerator, rate.Denominator),
            advanced_color,
            raw_sdr_white,
        });
    }
    Ok(out)
}

/// `QueryDisplayConfig` for active paths, retrying if the topology changes between the
/// size query and the data query (`ERROR_INSUFFICIENT_BUFFER`).
fn query_raw_paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>, WinError> {
    for _ in 0..4 {
        let (mut n_paths, mut n_modes) = (0u32, 0u32);
        // SAFETY: valid out-pointers.
        let rc = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &raw mut n_paths, &raw mut n_modes)
        };
        if rc != ERROR_SUCCESS {
            return Err(WinError::api("GetDisplayConfigBufferSizes", rc.0.cast_signed(), "failed"));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        // SAFETY: the buffers hold `n_paths` / `n_modes` elements, which is what the counts
        // passed in say; the API updates the counts to the number actually written.
        let rc = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut n_paths,
                paths.as_mut_ptr(),
                &raw mut n_modes,
                modes.as_mut_ptr(),
                None,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if rc != ERROR_SUCCESS {
            return Err(WinError::api("QueryDisplayConfig", rc.0.cast_signed(), "failed"));
        }
        paths.truncate(n_paths as usize);
        return Ok(paths);
    }
    Err(WinError::Other("display topology kept changing during QueryDisplayConfig".into()))
}
