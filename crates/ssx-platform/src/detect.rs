//! Choosing a capture backend for the current session.

use std::fmt;

use ssx_capture::{CaptureBackend, CaptureError};

/// The capture backends ssx knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Windows Graphics Capture / DXGI / GDI.
    Windows,
    /// Direct Wayland screencopy protocols (sway, Hyprland, other wlroots compositors).
    Wayland,
    /// `xdg-desktop-portal` and KWin `ScreenShot2` (GNOME, KDE Plasma).
    Portal,
    /// X11 (`GetImage` + MIT-SHM).
    X11,
}

impl BackendKind {
    /// Stable lowercase name, as accepted by `SSX_BACKEND`.
    pub const fn name(self) -> &'static str {
        match self {
            BackendKind::Windows => "windows",
            BackendKind::Wayland => "wayland",
            BackendKind::Portal => "portal",
            BackendKind::X11 => "x11",
        }
    }

    /// Parses an `SSX_BACKEND` value.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "windows" | "win" => Some(BackendKind::Windows),
            "wayland" | "wlr" => Some(BackendKind::Wayland),
            "portal" | "gnome" | "kde" => Some(BackendKind::Portal),
            "x11" | "xorg" => Some(BackendKind::X11),
            _ => None,
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One backend that was tried during detection.
#[derive(Debug)]
pub struct Attempt {
    pub kind: BackendKind,
    /// `Ok` if it initialised, otherwise why not.
    pub result: Result<(), String>,
}

/// The outcome of [`detect_backend`].
#[derive(Debug)]
pub struct Detected {
    pub kind: BackendKind,
    pub backend: Box<dyn CaptureBackend>,
    /// Every candidate tried, in order, including the one that won.
    pub attempts: Vec<Attempt>,
}

/// Session facts that decide the candidate order. Split out so it can be tested without
/// touching the real environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionEnv {
    pub ssx_backend: Option<String>,
    pub xdg_session_type: Option<String>,
    pub wayland_display: bool,
    pub display: bool,
}

impl SessionEnv {
    pub(crate) fn from_process() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            ssx_backend: var("SSX_BACKEND"),
            xdg_session_type: var("XDG_SESSION_TYPE"),
            wayland_display: var("WAYLAND_DISPLAY").is_some(),
            display: var("DISPLAY").is_some(),
        }
    }
}

/// Candidate order for a session. A Wayland session never falls back to X11: XWayland only
/// shows X clients, so an X11 "screenshot" there would silently miss native windows.
pub(crate) fn candidates(env: &SessionEnv) -> Result<Vec<BackendKind>, CaptureError> {
    if let Some(forced) = &env.ssx_backend {
        return BackendKind::from_name(forced).map(|k| vec![k]).ok_or_else(|| {
            CaptureError::NoBackend(format!(
                "SSX_BACKEND={forced:?} is not one of: windows, wayland, portal, x11"
            ))
        });
    }
    if cfg!(windows) {
        return Ok(vec![BackendKind::Windows]);
    }
    let wayland = env.wayland_display
        || env.xdg_session_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("wayland"));
    if wayland {
        // wlroots compositors first (silent, no prompt); otherwise GNOME/KDE via portal.
        return Ok(vec![BackendKind::Wayland, BackendKind::Portal]);
    }
    if env.display {
        return Ok(vec![BackendKind::X11]);
    }
    Err(CaptureError::NoBackend(
        "no graphical session found: neither WAYLAND_DISPLAY nor DISPLAY is set".into(),
    ))
}

/// Picks and initialises the capture backend for this session.
///
/// `SSX_BACKEND=windows|wayland|portal|x11` forces one. Otherwise: Windows uses the Windows
/// backend; a Wayland session tries direct wlroots protocols and then the desktop portal
/// (GNOME/KDE); an X11 session uses X11. `preferred` overrides the environment variable.
pub fn detect_backend(preferred: Option<BackendKind>) -> Result<Detected, CaptureError> {
    let mut env = SessionEnv::from_process();
    if let Some(p) = preferred {
        env.ssx_backend = Some(p.name().to_owned());
    }
    let order = candidates(&env)?;
    let mut attempts = Vec::new();
    for kind in order {
        match init(kind) {
            Ok(backend) => {
                attempts.push(Attempt { kind, result: Ok(()) });
                tracing::info!(backend = backend.name(), "capture backend selected");
                return Ok(Detected { kind, backend, attempts });
            }
            Err(e) => {
                tracing::debug!(%kind, error = %e, "capture backend unavailable");
                attempts.push(Attempt { kind, result: Err(e.to_string()) });
            }
        }
    }
    let why = attempts
        .iter()
        .map(|a| format!("{}: {}", a.kind, a.result.as_ref().err().map_or("ok", String::as_str)))
        .collect::<Vec<_>>()
        .join("; ");
    Err(CaptureError::NoBackend(why))
}

fn init(kind: BackendKind) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    match kind {
        #[cfg(windows)]
        BackendKind::Windows => Ok(Box::new(ssx_capture_win::WindowsCapture::new()?)),
        #[cfg(target_os = "linux")]
        BackendKind::Wayland => Ok(Box::new(ssx_capture_wayland::WaylandCapture::detect()?)),
        #[cfg(target_os = "linux")]
        BackendKind::Portal => Ok(Box::new(ssx_capture_portal::PortalCapture::detect()?)),
        #[cfg(target_os = "linux")]
        BackendKind::X11 => Ok(Box::new(ssx_capture_x11::X11Capture::with_config(
            ssx_capture_x11::X11Config::default(),
        )?)),
        #[allow(unreachable_patterns)] // every arm above is cfg-gated
        other => Err(CaptureError::NoBackend(format!(
            "the {other} backend is not available on this operating system"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(f: impl FnOnce(&mut SessionEnv)) -> SessionEnv {
        let mut e = SessionEnv::default();
        f(&mut e);
        e
    }

    #[cfg(not(windows))]
    #[test]
    fn wayland_session_prefers_wlroots_then_portal_and_never_x11() {
        let e = env(|e| {
            e.wayland_display = true;
            e.display = true; // XWayland is always present alongside
        });
        assert_eq!(candidates(&e).unwrap(), [BackendKind::Wayland, BackendKind::Portal]);
        let e = env(|e| e.xdg_session_type = Some("Wayland".into()));
        assert_eq!(candidates(&e).unwrap(), [BackendKind::Wayland, BackendKind::Portal]);
    }

    #[cfg(not(windows))]
    #[test]
    fn x11_session_uses_x11() {
        let e = env(|e| e.display = true);
        assert_eq!(candidates(&e).unwrap(), [BackendKind::X11]);
    }

    #[cfg(not(windows))]
    #[test]
    fn no_session_is_a_helpful_error() {
        let err = candidates(&SessionEnv::default()).unwrap_err().to_string();
        assert!(err.contains("WAYLAND_DISPLAY") && err.contains("DISPLAY"), "{err}");
    }

    #[test]
    fn override_wins_and_is_validated() {
        let e = env(|e| {
            e.wayland_display = true;
            e.ssx_backend = Some("X11".into());
        });
        assert_eq!(candidates(&e).unwrap(), [BackendKind::X11]);
        let e = env(|e| e.ssx_backend = Some("nope".into()));
        assert!(candidates(&e).unwrap_err().to_string().contains("SSX_BACKEND"));
    }

    #[test]
    fn names_round_trip() {
        for k in [BackendKind::Windows, BackendKind::Wayland, BackendKind::Portal, BackendKind::X11]
        {
            assert_eq!(BackendKind::from_name(k.name()), Some(k));
        }
        assert_eq!(BackendKind::from_name("  KDE "), Some(BackendKind::Portal));
    }

    #[cfg(windows)]
    #[test]
    fn windows_always_uses_the_windows_backend() {
        assert_eq!(candidates(&SessionEnv::default()).unwrap(), [BackendKind::Windows]);
    }
}
