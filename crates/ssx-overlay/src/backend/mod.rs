//! Windowing backends and the runtime choice between them.
//!
//! A backend owns the native window(s), translates native input into
//! [`InputEvent`](crate::model::InputEvent)s and presents the buffers the renderer fills.
//! Everything else (state machine, rendering, damage) is shared through
//! [`OverlayApp`](crate::app::OverlayApp).
//!
//! | platform | backend |
//! |---|---|
//! | Linux X11 | [`x11`]: one override-redirect window over the virtual desktop |
//! | Linux Wayland with `wlr-layer-shell` (sway, Hyprland, KDE) | [`wayland`], layer `overlay`, one surface per output |
//! | Linux Wayland without it (GNOME/Mutter) | [`wayland`], one fullscreen `xdg_toplevel` per output |
//! | Windows | `windows`: one topmost popup over the virtual desktop |
//! | macOS | unsupported stub |

use crate::{app::OverlayApp, error::OverlayError};

#[cfg(target_os = "linux")]
pub mod plan;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use plan::{Choice, SessionEnv, WaylandCaps, plan};

/// How a backend failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Could not even get a window up (no display, missing protocol). The next candidate
    /// backend may be tried.
    Setup(String),
    /// Failed after the overlay was visible. Never retried: the user would see two overlays.
    Runtime(OverlayError),
}

impl From<OverlayError> for Failure {
    fn from(e: OverlayError) -> Self {
        Failure::Runtime(e)
    }
}

/// Runs the overlay on the best backend for this session until the app has an outcome.
pub fn run(app: &mut OverlayApp) -> Result<(), OverlayError> {
    #[cfg(target_os = "linux")]
    {
        use crate::types::BackendPreference;
        let env = SessionEnv::from_process();
        let candidates = plan(app.options().backend, &env, wayland::probe)?;
        let mut tried = Vec::new();
        for c in candidates {
            let r = match c {
                Choice::X11 => x11::run(app),
                Choice::WaylandLayerShell => wayland::run(app, wayland::Flavour::LayerShell),
                Choice::WaylandFullscreen => wayland::run(app, wayland::Flavour::Fullscreen),
            };
            match r {
                Ok(()) => return Ok(()),
                Err(Failure::Runtime(e)) => return Err(e),
                Err(Failure::Setup(m)) => {
                    tracing::warn!(backend = c.name(), "overlay backend unavailable: {m}");
                    tried.push(format!("{}: {m}", c.name()));
                }
            }
        }
        let _ = BackendPreference::Auto;
        Err(OverlayError::NoBackend { tried })
    }
    #[cfg(windows)]
    {
        windows::run(app).map_err(|f| match f {
            Failure::Setup(m) => OverlayError::NoBackend { tried: vec![format!("windows: {m}")] },
            Failure::Runtime(e) => e,
        })
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = app;
        Err(OverlayError::Unsupported(
            "macOS and other platforms: a borderless NSWindow overlay is not implemented yet",
        ))
    }
}
