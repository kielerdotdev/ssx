//! Global hotkeys for ssx that work on every target desktop, with honest fallbacks.
//!
//! No single mechanism works everywhere, so the crate offers three, plus the logic to pick:
//!
//! 1. **In-process grabbing** ([`GlobalHotkeys`], via the `global-hotkey` crate): Windows,
//!    macOS and X11. Events arrive on a channel ([`HotkeyManager::events`]).
//! 2. **The XDG GlobalShortcuts portal** ([`PortalHotkeys`], Linux): Wayland desktops that
//!    implement it (KDE Plasma, GNOME 48+). The compositor owns the shortcut; the chord
//!    you ask for is only a preference and key-release events may never arrive.
//! 3. **Binding generators** ([`bindings`]): for desktops that let applications do neither
//!    (sway, Hyprland, older GNOME, KDE without the portal), generate the configuration that
//!    runs a command such as `ssx capture region`, write it to a file this crate owns, and
//!    print the one line the user must add to their own config. Editing the user's config is
//!    always an explicit opt-in and reversible.
//!
//! [`detect::detect`] inspects the session (`XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE`,
//! `SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`, ...) and returns an ordered list of
//! [`Strategy`] candidates; [`open_best_manager`] walks the in-process ones.
//!
//! ```
//! use ssx_hotkeys::{Chord, Command, bindings::sway};
//!
//! let chord: Chord = "ctrl + shift + s".parse()?;
//! assert_eq!(chord.to_string(), "Ctrl+Shift+S");
//! let line = sway::bindsym_line(&chord, &Command::new("ssx").args(["capture", "region"]));
//! assert_eq!(line, "bindsym Ctrl+Shift+s exec ssx capture region");
//! # Ok::<(), ssx_hotkeys::ChordError>(())
//! ```
//!
//! # Running the in-process backends
//!
//! * **X11**: nothing extra; `global-hotkey` runs its own thread and X connection.
//! * **Windows**: create the manager *on the thread that runs your Win32 message loop*
//!   (winit/tao/egui event loops qualify); events only flow while it pumps messages.
//! * **macOS**: create it on the main thread while the main run loop is running.
//! * **Portal**: a private worker thread talks D-Bus; nothing to run.
//!
//! In every case read events with `manager.events().recv()` / `try_recv()` on a thread of
//! your choosing (the receiver stays on the manager's thread if that thread runs the UI
//! loop: poll it from there each iteration).

#![forbid(unsafe_code)]

pub mod bindings;
mod chord;
mod command;
pub mod detect;
mod id;
mod key;
mod manager;

#[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
mod backend_global;
#[cfg(target_os = "linux")]
mod backend_portal;

pub use chord::{Chord, ChordError, Modifiers};
pub use command::{Command, CommandError};
pub use detect::{Desktop, Detection, Environment, Platform, SessionType, Strategy, detect};
pub use id::{HotkeyId, InvalidHotkeyId};
pub use key::{Key, Named};
pub use manager::{BackendKind, HotkeyError, HotkeyEvent, HotkeyManager, HotkeyState};

#[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
pub use backend_global::GlobalHotkeys;
#[cfg(target_os = "linux")]
pub use backend_portal::PortalHotkeys;

/// Why [`open_best_manager`] could not produce an in-process manager.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Every in-process candidate failed, or the desktop has none. `detection` lists what
    /// to do instead (generators), `failures` says why each in-process candidate failed.
    #[error(
        "no in-process hotkey backend works here ({}); use generated bindings ({:?}) instead",
        failures.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "),
        detection.candidates
    )]
    NoBackend {
        /// The detection result.
        detection: Detection,
        /// One error per in-process candidate that was tried.
        failures: Vec<HotkeyError>,
    },
}

/// Opens the best in-process manager for this session: detects the desktop, then tries the
/// in-process candidates in order ([`GlobalHotkeys`], [`PortalHotkeys`]).
///
/// On failure the error carries the [`Detection`], whose remaining candidates are the
/// generator strategies to offer the user.
pub fn open_best_manager() -> Result<Box<dyn HotkeyManager>, OpenError> {
    open_manager_for(detect(&Environment::from_env(), Platform::current()))
}

/// [`open_best_manager`] with an explicit detection result (for tests and overrides).
pub fn open_manager_for(detection: Detection) -> Result<Box<dyn HotkeyManager>, OpenError> {
    let mut failures = Vec::new();
    for strategy in detection.candidates.iter().copied().filter(|s| s.is_in_process()) {
        let attempt: Result<Box<dyn HotkeyManager>, HotkeyError> = match strategy {
            #[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
            Strategy::GlobalHotkey => {
                backend_global::GlobalHotkeys::new().map(|m| Box::new(m) as _)
            }
            #[cfg(target_os = "linux")]
            Strategy::Portal => backend_portal::PortalHotkeys::connect().map(|m| Box::new(m) as _),
            _ => continue,
        };
        match attempt {
            Ok(m) => return Ok(m),
            Err(e) => {
                tracing::debug!(?strategy, error = %e, "hotkey backend unavailable");
                failures.push(e);
            }
        }
    }
    Err(OpenError::NoBackend { detection, failures })
}
