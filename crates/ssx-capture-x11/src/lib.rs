//! ssx capture backend for **X11 and XWayland**, built on the pure-Rust `x11rb`.
//!
//! [`X11Capture`] implements [`ssx_capture::CaptureBackend`] with no permission prompts:
//!
//! * **Pixels**: `GetImage` on the root window, through **MIT-SHM** when the connection is
//!   local (a `memfd` handed to the server with `ShmAttachFd`, so no `unsafe` and no SysV
//!   IPC) and plain banded `GetImage` otherwise (remote displays, old servers, or
//!   [`X11Config::use_shm`]` = false`). Any visual layout the server advertises is decoded:
//!   depth 15/16/24/30/32, packed 24 bpp, RGB-ordered masks, big-endian servers.
//! * **Monitors**: RandR 1.5 `GetMonitors`, falling back to RandR 1.2 CRTCs and then to a
//!   single monitor covering the root. Refresh rate and rotation come from the CRTC.
//! * **Scale factor** (best effort, one value for every monitor because X has no
//!   per-monitor scale): XSETTINGS `Xft/DPI`, else `Xft.dpi` from `RESOURCE_MANAGER`, else
//!   `Gdk/WindowScalingFactor`, else 1.0. Captured pixels are always physical.
//! * **Cursor**: XFixes `GetCursorImage`, blended with premultiplied alpha.
//! * **Windows**: EWMH (`_NET_CLIENT_LIST_STACKING`, `_NET_ACTIVE_WINDOW`,
//!   `_NET_FRAME_EXTENTS`, `_NET_WM_STATE_HIDDEN`); see [`X11Capture`] for the two window
//!   capture modes and their occlusion caveat.
//!
//! Frames are opaque `Bgra8`/sRGB. The crate is empty on Windows and macOS.
//!
//! ```no_run
//! # #[cfg(all(unix, not(target_vendor = "apple")))]
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use ssx_capture::{CaptureBackend, CaptureOptions};
//! use ssx_capture_x11::X11Capture;
//!
//! let cap = X11Capture::connect()?;
//! let frame = cap.capture_desktop(&CaptureOptions { include_cursor: true })?;
//! frame.save("shot.png")?;
//! # Ok(())
//! # }
//! # fn main() {}
//! ```

#![forbid(unsafe_code)]
#![cfg(all(unix, not(target_vendor = "apple")))]

mod capture;
mod config;
mod cursor;
mod error;
mod grab;
mod monitors;
mod pixels;
mod scale;
mod session;
mod shm;
mod windows;

pub use capture::{Features, X11Capture};
pub use config::{WindowCaptureMode, X11Config};
pub use error::X11Error;
pub use monitors::{Rotation, X11Monitor};
