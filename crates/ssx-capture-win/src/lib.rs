//! Windows capture backend for ssx.
//!
//! [`WindowsCapture`] implements [`ssx_capture::CaptureBackend`] on top of three
//! Windows APIs, tried in this order for monitor capture:
//!
//! 1. **Windows.Graphics.Capture** (`wgc`): the only API that captures windows and that can
//!    deliver float `R16G16B16A16_FLOAT` scRGB frames for HDR displays without going
//!    through a lossy SDR conversion first.
//! 2. **DXGI Desktop Duplication** (`dda`): fallback for machines or sessions where WGC is
//!    unavailable (Windows before 1903, some RDP/virtual-GPU setups).
//! 3. **GDI** (`gdi`): last resort. SDR only.
//!
//! # Layout: logic vs. glue
//!
//! Only `display`, `wgc`, `dda`, `gdi`, `windows` and `d3d` touch the `windows` crate, and
//! they compile only on Windows. Everything that can be decided without calling the OS
//! lives in platform-independent modules (`hdr`, `geometry`, `pixels`, `filter`,
//! `topology`, `chain`, `acquire`, `error`) with unit tests that run on every platform.
//! The glue is kept thin so that what cannot be tested off Windows is small and reviewable.
//!
//! # Requirements
//!
//! The process must be **per-monitor-DPI-aware (v2)**, otherwise Windows returns
//! DPI-virtualised coordinates that do not match captured pixels. Prefer declaring it in the
//! application manifest; [`ensure_per_monitor_dpi_aware`] is the runtime fallback and is
//! also called (best effort) by [`WindowsCapture::new`].
//!
//! On non-Windows targets this crate contains only the platform-independent logic so the
//! workspace builds everywhere.

// The platform-independent modules are only *used* by the Windows glue, so on other
// targets they look dead; they are still compiled and unit-tested there.
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod acquire;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod chain;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod error;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod filter;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod geometry;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod hdr;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod pixels;
#[cfg_attr(not(windows), allow(dead_code, reason = "used only by the Windows glue"))]
mod topology;

#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod dda;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod gdi;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod wgc;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
mod backend;

#[cfg(windows)]
pub use backend::WindowsCapture;
#[cfg(windows)]
pub use display::ensure_per_monitor_dpi_aware;
