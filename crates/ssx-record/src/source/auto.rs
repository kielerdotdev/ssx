//! Picking the right capture path for the running desktop.
//!
//! | Session | Source | Why |
//! |---|---|---|
//! | Windows | Windows Graphics Capture | the only API with window capture and float HDR frames |
//! | Wayland, wlroots (sway, Hyprland, ...) | [`WlrootsSource`](super::wlroots::WlrootsSource) | prompt-free, damage driven |
//! | Wayland, GNOME / KDE | xdg-desktop-portal ScreenCast + PipeWire | the only sanctioned way there; shows the compositor's picker once, then remembers the choice |
//! | X11 (and XWayland as a last resort) | MIT-SHM `GetImage` loop | prompt-free |
//!
//! `Auto` probes in that order and returns the first that can connect; every failure is
//! kept so the final error says why each path was rejected.

use super::{FrameSource, SourceConfig};
use crate::error::SourceError;

/// Which capture path to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// Choose for the running session (see the module docs).
    #[default]
    Auto,
    /// X11 / XWayland `GetImage` loop.
    X11,
    /// Direct wlroots protocols.
    Wayland,
    /// xdg-desktop-portal ScreenCast (GNOME, KDE, anything with a portal).
    Portal,
    /// Windows Graphics Capture.
    Windows,
    /// The synthetic test source (1280x720 colour bars).
    Synthetic,
}

impl SourceKind {
    /// Parses a command-line name (`x11`, `wayland`, `portal`, `windows`, `synthetic`, `auto`).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "x11" => Self::X11,
            "wayland" | "wlroots" => Self::Wayland,
            "portal" | "pipewire" => Self::Portal,
            "windows" | "wgc" => Self::Windows,
            "synthetic" | "test" => Self::Synthetic,
            _ => return None,
        })
    }
}

/// Opens a source of `kind` for `cfg` (not yet started).
pub fn open(kind: SourceKind, cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    match kind {
        SourceKind::Synthetic => {
            let s = super::synthetic::SyntheticConfig::new(1280, 720, cfg.fps);
            Ok(Box::new(super::synthetic::SyntheticSource::new(s)))
        }
        SourceKind::X11 => open_x11(cfg),
        SourceKind::Wayland => open_wayland(cfg),
        SourceKind::Portal => open_portal(cfg),
        SourceKind::Windows => open_windows(cfg),
        SourceKind::Auto => auto(cfg),
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn open_x11(cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    if !super::x11::X11Source::is_available() {
        return Err(SourceError::Unavailable("$DISPLAY is not set".into()));
    }
    Ok(Box::new(super::x11::X11Source::new(cfg)))
}

#[cfg(not(all(unix, not(target_vendor = "apple"))))]
fn open_x11(_: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Err(SourceError::Unsupported("X11 capture exists only on Linux and BSD".into()))
}

#[cfg(target_os = "linux")]
fn open_wayland(cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    if !super::wlroots::WlrootsSource::is_available() {
        return Err(SourceError::Unavailable(
            "no Wayland compositor with ext-image-copy-capture or wlr-screencopy (GNOME/KDE need the portal)"
                .into(),
        ));
    }
    Ok(Box::new(super::wlroots::WlrootsSource::new(cfg)))
}

#[cfg(not(target_os = "linux"))]
fn open_wayland(_: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Err(SourceError::Unsupported("Wayland capture exists only on Linux".into()))
}

#[cfg(all(target_os = "linux", feature = "portal"))]
fn open_portal(cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Ok(Box::new(super::portal::PortalSource::new(cfg)))
}

#[cfg(not(all(target_os = "linux", feature = "portal")))]
fn open_portal(_: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Err(SourceError::Unsupported("the portal source needs Linux and the `portal` feature".into()))
}

#[cfg(windows)]
fn open_windows(cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Ok(Box::new(super::windows::WgcSource::new(cfg)))
}

#[cfg(not(windows))]
fn open_windows(_: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    Err(SourceError::Unsupported("Windows Graphics Capture exists only on Windows".into()))
}

fn auto(cfg: SourceConfig) -> Result<Box<dyn FrameSource>, SourceError> {
    if cfg!(windows) {
        return open_windows(cfg);
    }
    let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut why = Vec::new();
    if wayland {
        match open_wayland(cfg.clone()) {
            Ok(s) => return Ok(s),
            Err(e) => why.push(format!("wayland: {e}")),
        }
        match open_portal(cfg.clone()) {
            Ok(s) => return Ok(s),
            Err(e) => why.push(format!("portal: {e}")),
        }
    }
    match open_x11(cfg) {
        Ok(s) => Ok(s),
        Err(e) => {
            why.push(format!("x11: {e}"));
            Err(SourceError::Unavailable(format!(
                "no screen capture path works in this session ({})",
                why.join("; ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse() {
        assert_eq!(SourceKind::parse("X11"), Some(SourceKind::X11));
        assert_eq!(SourceKind::parse("wlroots"), Some(SourceKind::Wayland));
        assert_eq!(SourceKind::parse("portal"), Some(SourceKind::Portal));
        assert_eq!(SourceKind::parse("synthetic"), Some(SourceKind::Synthetic));
        assert_eq!(SourceKind::parse("nope"), None);
    }

    #[test]
    fn synthetic_source_opens_everywhere() {
        let s = open(SourceKind::Synthetic, SourceConfig::default()).unwrap();
        assert_eq!(s.name(), "synthetic");
    }
}
