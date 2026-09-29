//! Direct-protocol Wayland capture for wlroots-family compositors (sway, Hyprland, …).
//!
//! [`WaylandCapture`] implements [`ssx_capture::CaptureBackend`] on top of the two
//! screen-copy protocols compositors expose to ordinary clients:
//!
//! 1. `ext-image-copy-capture-v1` (+ `ext-image-capture-source-v1`), the standard
//!    protocol, preferred when advertised;
//! 2. `wlr-screencopy-unstable-v1`, supported by every wlroots compositor for years.
//!
//! GNOME and KDE expose neither to ordinary clients; [`WaylandCapture::detect`] then fails
//! with [`ssx_capture::CaptureError::NoBackend`] pointing at the portal backend
//! (`ssx-capture-portal`), which this crate deliberately does not reimplement.
//!
//! Window enumeration has no Wayland protocol with geometry, so it comes from the
//! compositor's own IPC: sway (i3-ipc over `$SWAYSOCK`) and Hyprland (`.socket.sock`).
//! See `docs/wayland-coordinates.md` for the coordinate model (logical layout vs physical
//! pixels) and its consequences for mixed-DPI setups.
//!
//! The crate is empty on non-Linux targets so that the workspace builds everywhere.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod coords;
#[cfg(target_os = "linux")]
mod format;
#[cfg(target_os = "linux")]
mod hyprland;
#[cfg(target_os = "linux")]
mod ipc;
#[cfg(target_os = "linux")]
mod resample;
#[cfg(target_os = "linux")]
mod sway;
#[cfg(target_os = "linux")]
mod transform;
#[cfg(target_os = "linux")]
mod wl;

#[cfg(target_os = "linux")]
pub use backend::{Config, OutputInfo, Protocol, WaylandCapture};
#[cfg(target_os = "linux")]
pub use ipc::Ipc;
#[cfg(target_os = "linux")]
pub use transform::Transform;
#[cfg(target_os = "linux")]
pub use wl::Target;
