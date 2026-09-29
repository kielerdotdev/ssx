//! Top-level window enumeration and capture-target resolution.
//!
//! The list is meant for a "capture a window" picker, so it is filtered like Alt-Tab (see
//! `filter.rs` for the rules) and reports **DWM extended frame bounds**
//! (`DWMWA_EXTENDED_FRAME_BOUNDS`): `GetWindowRect` includes the invisible resize border
//! and shadow (about 7 px on Windows 10/11), which would make crops and previews look
//! offset. `EnumWindows` already walks in z-order, front to back, and that order is kept.
//!
//! Enumeration only *collects handles* inside the `EnumWindows` callback; all per-window
//! queries happen afterwards in safe-looking straight-line code, so the callback stays tiny
//! and cannot panic across the FFI boundary.

use std::{collections::HashMap, ffi::c_void, mem::size_of};

use ssx_types::{HdrInfo, Rect, WindowInfo};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
        },
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{
                EnumWindows, GWL_EXSTYLE, GetClassNameW, GetDesktopWindow, GetForegroundWindow,
                GetShellWindow, GetWindowDisplayAffinity, GetWindowLongW, GetWindowPlacement,
                GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
                IsIconic, IsWindow, IsWindowVisible, WINDOWPLACEMENT,
            },
        },
    },
    core::{BOOL, PWSTR},
};

use crate::{
    display,
    error::WinError,
    filter::{
        WindowFacts, exe_name_from_path, format_window_id, is_alt_tab_candidate, parse_window_id,
        utf16_until_nul,
    },
    geometry::{rect_from_edges, scale_factor_from_dpi},
    sys::hwnd,
};

/// A window resolved and validated for capture.
#[derive(Debug, Clone)]
pub(crate) struct WindowTarget {
    pub(crate) hwnd: isize,
    /// Extended frame bounds in physical desktop pixels.
    pub(crate) bounds: Rect,
    pub(crate) scale_factor: f64,
    /// HDR state of the monitor the window is mostly on (`None` if unknown).
    pub(crate) hdr: Option<HdrInfo>,
}

unsafe extern "system" fn collect_window(window: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<isize>` passed by `top_level_handles`, valid for the
    // whole synchronous `EnumWindows` call.
    let out = unsafe { &mut *(data.0 as *mut Vec<isize>) };
    out.push(window.0 as isize);
    BOOL(1)
}

/// All top-level window handles, front to back.
fn top_level_handles() -> Result<Vec<isize>, WinError> {
    let mut handles: Vec<isize> = Vec::new();
    // SAFETY: the callback only pushes to the Vec behind `data`, valid for the call.
    unsafe { EnumWindows(Some(collect_window), LPARAM((&raw mut handles) as isize)) }
        .map_err(crate::sys::api_err("EnumWindows"))?;
    Ok(handles)
}

fn window_title(window: HWND) -> String {
    // SAFETY: plain queries on a window handle; a stale handle just yields 0.
    let len = unsafe { GetWindowTextLengthW(window) };
    let Ok(len) = usize::try_from(len) else { return String::new() };
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    // SAFETY: `buf` has room for `len + 1` UTF-16 units including the terminator.
    let copied = unsafe { GetWindowTextW(window, &mut buf) };
    usize::try_from(copied)
        .map_or_else(|_| String::new(), |n| utf16_until_nul(&buf[..n.min(buf.len())]))
}

fn window_class(window: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable buffer; the API truncates to its length.
    let n = unsafe { GetClassNameW(window, &mut buf) };
    usize::try_from(n).map_or_else(|_| String::new(), |n| utf16_until_nul(&buf[..n.min(buf.len())]))
}

fn is_cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: `cloaked` is a valid 4-byte out buffer matching `cbAttribute`.
    let r = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    };
    r.is_ok() && cloaked != 0
}

/// Visible bounds of a window in physical desktop pixels.
///
/// Minimised windows report the restored position instead (DWM reports a parked position
/// far off-screen for them). Falls back to `GetWindowRect` when DWM composition is not
/// available.
fn window_bounds(window: HWND, minimized: bool) -> Rect {
    if minimized {
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..WINDOWPLACEMENT::default()
        };
        // SAFETY: `placement.length` is set as required; valid out-pointer.
        if unsafe { GetWindowPlacement(window, &raw mut placement) }.is_ok() {
            let r = placement.rcNormalPosition;
            return rect_from_edges(r.left, r.top, r.right, r.bottom);
        }
    }
    let mut r = RECT::default();
    // SAFETY: `r` is a valid `RECT` and its size is passed.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    if dwm.is_err() {
        // SAFETY: valid out-pointer.
        if unsafe { GetWindowRect(window, &raw mut r) }.is_err() {
            return Rect::default();
        }
    }
    rect_from_edges(r.left, r.top, r.right, r.bottom)
}

/// Executable file name (`notepad.exe`) of the process owning `window`, cached per pid.
fn process_name(window: HWND, cache: &mut HashMap<u32, Option<String>>) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: valid out-pointer.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    if pid == 0 {
        return None;
    }
    cache.entry(pid).or_insert_with(|| query_process_name(pid)).clone()
}

/// Closes a process handle on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `OpenProcess` and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn query_process_name(pid: u32) -> Option<String> {
    // SAFETY: plain call; failure (e.g. protected or elevated process) yields `None`.
    let handle =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units; the API updates `len` to the written count.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
    }
    .ok()?;
    exe_name_from_path(&utf16_until_nul(&buf[..(len as usize).min(buf.len())]))
}

/// Lists windows suitable for capture, front to back.
pub(crate) fn enumerate() -> Result<Vec<WindowInfo>, WinError> {
    let handles = top_level_handles()?;
    // SAFETY: plain queries.
    let (shell, desktop, foreground) =
        unsafe { (GetShellWindow(), GetDesktopWindow(), GetForegroundWindow()) };
    let mut names = HashMap::new();
    let mut out = Vec::new();
    for raw in handles {
        let window = hwnd(raw);
        // SAFETY: plain queries on a window handle; stale handles yield defaults.
        let (visible, minimized, ex_style) = unsafe {
            (
                IsWindowVisible(window).as_bool(),
                IsIconic(window).as_bool(),
                GetWindowLongW(window, GWL_EXSTYLE) as u32,
            )
        };
        if !visible {
            // Cheap early-out: the vast majority of top-level windows are invisible.
            continue;
        }
        let title = window_title(window);
        let class_name = window_class(window);
        let rect = window_bounds(window, minimized);
        let facts = WindowFacts {
            visible,
            cloaked: is_cloaked(window),
            title: &title,
            ex_style,
            class_name: &class_name,
            is_shell_or_desktop: window == shell || window == desktop,
            minimized,
            rect,
        };
        if !is_alt_tab_candidate(&facts) {
            continue;
        }
        out.push(WindowInfo {
            id: format_window_id(raw),
            app_name: process_name(window, &mut names),
            title,
            rect,
            minimized,
            focused: window == foreground,
        });
    }
    Ok(out)
}

/// Validates a window id for capture and gathers what the capturers need.
///
/// # Errors
/// [`WinError::NotFound`] for a bad or dead id, [`WinError::Minimized`] for an iconic
/// window, [`WinError::Protected`] when the window opted out of capture
/// (`SetWindowDisplayAffinity`).
pub(crate) fn resolve_target(id: &str) -> Result<WindowTarget, WinError> {
    let raw = parse_window_id(id).ok_or_else(|| WinError::NotFound(id.to_owned()))?;
    let window = hwnd(raw);
    // SAFETY: plain queries on a window handle.
    if !unsafe { IsWindow(Some(window)) }.as_bool() {
        return Err(WinError::NotFound(id.to_owned()));
    }
    // SAFETY: plain query.
    if unsafe { IsIconic(window) }.as_bool() {
        return Err(WinError::Minimized);
    }
    let mut affinity = 0u32;
    // SAFETY: valid out-pointer. Failure leaves 0 (no restriction), which is the safe default.
    let _ = unsafe { GetWindowDisplayAffinity(window, &raw mut affinity) };
    if crate::filter::affinity_blocks_capture(affinity) {
        return Err(WinError::Protected);
    }
    let bounds = window_bounds(window, false);
    if bounds.is_empty() {
        return Err(WinError::Other(format!("window {id} has an empty area")));
    }
    // SAFETY: plain queries.
    let (dpi, monitor) =
        unsafe { (GetDpiForWindow(window), MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST)) };
    // HDR state decides float vs 8-bit; a failed lookup means SDR, which still works.
    let hdr = display::target_by_handle(monitor).ok().and_then(|t| t.monitor.hdr);
    Ok(WindowTarget { hwnd: raw, bounds, scale_factor: scale_factor_from_dpi(dpi), hdr })
}
