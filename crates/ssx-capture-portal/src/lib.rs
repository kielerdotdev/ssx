//! ssx capture backend for **GNOME (Mutter) and KDE Plasma (KWin) on Wayland**.
//!
//! These compositors do not offer the direct screencopy protocols that wlroots-based
//! ones do (`wlr-screencopy`, `ext-image-copy-capture`), so screenshots go through
//! desktop-specific D-Bus services instead. [`PortalCapture`] picks one at runtime:
//!
//! 1. **KDE `org.kde.KWin.ScreenShot2`**: silent, per-monitor, can include the cursor.
//!    KWin serves it only to applications that declare it in their `.desktop` file; see
//!    [`kwin_desktop_entry`]. Without that, calls fail with
//!    [`CaptureError::PermissionDenied`](ssx_capture::CaptureError) (or transparently fall
//!    back to the portal, see [`PortalConfig::kwin_fallback_to_portal`]).
//! 2. **`org.freedesktop.portal.Screenshot`** (via `ashpd`): the only supported route on
//!    GNOME, where `org.gnome.Shell.Screenshot` is restricted to allow-listed applications.
//!    GNOME may show a permission dialog on first use. The portal returns one image of the
//!    whole desktop; per-monitor capture crops it using monitor rectangles read from
//!    Mutter's `DisplayConfig` or Wayland `xdg-output` (both need no permission).
//!
//! Not offered, deliberately: window enumeration (Wayland forbids it; use
//! [`PortalCapture::capture_interactive`], whose desktop-native picker can select a
//! window) and cursor control on the portal path (the portal has no such option).
//!
//! The crate is empty on non-Linux targets.
//!
//! ```no_run
//! # #[cfg(target_os = "linux")]
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use ssx_capture::{CaptureBackend, CaptureOptions};
//! use ssx_capture_portal::PortalCapture;
//!
//! let cap = PortalCapture::detect()?;
//! println!("using {}", cap.name());
//! let frame = cap.capture_desktop(&CaptureOptions::default())?;
//! frame.save("shot.png")?;
//! # Ok(())
//! # }
//! # fn main() {}
//! ```

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod capture;
mod desktop_entry;
#[cfg(target_os = "linux")]
mod kwin;
#[cfg(target_os = "linux")]
mod layout;
#[cfg(target_os = "linux")]
mod mutter;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod qimage;
#[cfg(target_os = "linux")]
mod uri;
#[cfg(target_os = "linux")]
mod wl_output;

#[cfg(target_os = "linux")]
pub use capture::{DESKTOP_MONITOR_ID, PortalCapture, PortalConfig, Strategy};
pub use desktop_entry::{
    DesktopEntryError, KWIN_RESTRICTED_INTERFACES_LINE, KWIN_SCREENSHOT_INTERFACE,
    kwin_desktop_entry,
};
#[cfg(target_os = "linux")]
pub use kwin::InteractiveKind;
