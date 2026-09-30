//! Runtime backend choice on Linux, as pure functions so it can be tested with fixtures.
//!
//! The choice is made from what the session *offers*, not from the desktop's name:
//! `wlr-layer-shell` is used wherever the compositor advertises it (wlroots compositors,
//! `KWin`), the fullscreen-toplevel technique wherever only `xdg_wm_base` exists (Mutter), and
//! X11 only when there is no usable Wayland session or as a last resort. Preferring native
//! Wayland over `XWayland` matters: an X11 override-redirect window under `XWayland` covers
//! only other X clients, not native Wayland windows.

use crate::{error::OverlayError, types::BackendPreference};

/// The environment variables that decide the session type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` (or `None`).
    pub wayland_display: Option<String>,
    /// `DISPLAY` (or `None`).
    pub x11_display: Option<String>,
    /// `XDG_SESSION_TYPE`, informational.
    pub session_type: Option<String>,
}

impl SessionEnv {
    /// Reads the current process environment (empty values count as unset).
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            wayland_display: get("WAYLAND_DISPLAY"),
            x11_display: get("DISPLAY"),
            session_type: get("XDG_SESSION_TYPE"),
        }
    }
}

/// A concrete backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// X11 override-redirect window.
    X11,
    /// Wayland `zwlr_layer_shell_v1`.
    WaylandLayerShell,
    /// Wayland fullscreen `xdg_toplevel` per output.
    WaylandFullscreen,
}

impl Choice {
    /// Stable name for logs and errors.
    pub const fn name(self) -> &'static str {
        match self {
            Choice::X11 => "x11",
            Choice::WaylandLayerShell => "wayland-layer-shell",
            Choice::WaylandFullscreen => "wayland-fullscreen",
        }
    }
}

/// What the compositor advertises, reduced to what the overlay cares about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaylandCaps {
    /// `zwlr_layer_shell_v1` present.
    pub layer_shell: bool,
    /// `xdg_wm_base` present.
    pub xdg_shell: bool,
    /// `wl_shm` present (required by both Wayland paths).
    pub shm: bool,
    /// `wl_compositor` present.
    pub compositor: bool,
    /// `wl_seat` present.
    pub seat: bool,
    /// `wp_viewporter` present (exact scaling of desktop-resolution buffers).
    pub viewporter: bool,
    /// `wp_cursor_shape_manager_v1` present.
    pub cursor_shape: bool,
}

impl WaylandCaps {
    /// Builds the summary from the interface names in the registry.
    pub fn from_globals<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut c = Self::default();
        for n in names {
            match n {
                "zwlr_layer_shell_v1" => c.layer_shell = true,
                "xdg_wm_base" => c.xdg_shell = true,
                "wl_shm" => c.shm = true,
                "wl_compositor" => c.compositor = true,
                "wl_seat" => c.seat = true,
                "wp_viewporter" => c.viewporter = true,
                "wp_cursor_shape_manager_v1" => c.cursor_shape = true,
                _ => {}
            }
        }
        c
    }

    fn usable(self) -> bool {
        self.shm && self.compositor && self.seat
    }
}

/// Orders the backends to try. `probe` connects to Wayland and reports the capabilities; it
/// is only called when the environment says Wayland could work.
pub fn plan(
    pref: BackendPreference,
    env: &SessionEnv,
    probe: impl FnOnce() -> Result<WaylandCaps, String>,
) -> Result<Vec<Choice>, OverlayError> {
    match pref {
        BackendPreference::X11 => return Ok(vec![Choice::X11]),
        BackendPreference::WaylandLayerShell => return Ok(vec![Choice::WaylandLayerShell]),
        BackendPreference::WaylandFullscreen => return Ok(vec![Choice::WaylandFullscreen]),
        BackendPreference::Windows => {
            return Err(OverlayError::Unsupported("the Windows backend was requested on Linux"));
        }
        BackendPreference::Auto => {}
    }
    let mut out = Vec::new();
    let mut notes = Vec::new();
    if env.wayland_display.is_some() {
        match probe() {
            Ok(c) if c.usable() => {
                if c.layer_shell {
                    out.push(Choice::WaylandLayerShell);
                }
                if c.xdg_shell {
                    out.push(Choice::WaylandFullscreen);
                }
                if out.is_empty() {
                    notes.push(
                        "wayland: neither zwlr_layer_shell_v1 nor xdg_wm_base advertised".into(),
                    );
                }
            }
            Ok(_) => notes.push("wayland: wl_shm, wl_compositor or wl_seat missing".into()),
            Err(e) => notes.push(format!("wayland: {e}")),
        }
    } else {
        notes.push("wayland: WAYLAND_DISPLAY is not set".into());
    }
    if env.x11_display.is_some() {
        out.push(Choice::X11);
    } else {
        notes.push("x11: DISPLAY is not set".into());
    }
    if out.is_empty() {
        return Err(OverlayError::NoBackend { tried: notes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interfaces advertised by typical compositors (trimmed to relevant + noise).
    const SWAY: &[&str] = &[
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "wl_output",
        "xdg_wm_base",
        "zwlr_layer_shell_v1",
        "wp_viewporter",
        "wp_cursor_shape_manager_v1",
        "zxdg_output_manager_v1",
        "zwlr_virtual_pointer_manager_v1",
    ];
    const HYPRLAND: &[&str] = &[
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "xdg_wm_base",
        "zwlr_layer_shell_v1",
        "wp_viewporter",
        "wp_fractional_scale_manager_v1",
        "wp_cursor_shape_manager_v1",
    ];
    const KWIN: &[&str] = &[
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "xdg_wm_base",
        "zwlr_layer_shell_v1",
        "wp_viewporter",
        "org_kde_plasma_shell",
        "wp_cursor_shape_manager_v1",
    ];
    const MUTTER: &[&str] = &[
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "xdg_wm_base",
        "wp_viewporter",
        "wp_fractional_scale_manager_v1",
        "zxdg_output_manager_v1",
        "gtk_shell1",
        "wp_cursor_shape_manager_v1",
    ];

    fn env(wayland: bool, x11: bool) -> SessionEnv {
        SessionEnv {
            wayland_display: wayland.then(|| "wayland-1".into()),
            x11_display: x11.then(|| ":0".into()),
            session_type: None,
        }
    }

    fn auto(env: &SessionEnv, globals: &[&str]) -> Result<Vec<Choice>, OverlayError> {
        plan(BackendPreference::Auto, env, || {
            Ok(WaylandCaps::from_globals(globals.iter().copied()))
        })
    }

    #[test]
    fn wlroots_and_kde_use_layer_shell_then_fullscreen_then_x11() {
        for g in [SWAY, HYPRLAND, KWIN] {
            let p = auto(&env(true, true), g).unwrap();
            assert_eq!(p, vec![Choice::WaylandLayerShell, Choice::WaylandFullscreen, Choice::X11]);
        }
        assert_eq!(
            auto(&env(true, false), SWAY).unwrap(),
            vec![Choice::WaylandLayerShell, Choice::WaylandFullscreen]
        );
    }

    #[test]
    fn gnome_uses_fullscreen_toplevels_and_prefers_native_over_xwayland() {
        let p = auto(&env(true, true), MUTTER).unwrap();
        assert_eq!(p, vec![Choice::WaylandFullscreen, Choice::X11]);
    }

    #[test]
    fn plain_x11_session() {
        let p =
            plan(BackendPreference::Auto, &env(false, true), || panic!("must not probe")).unwrap();
        assert_eq!(p, vec![Choice::X11]);
    }

    #[test]
    fn wayland_probe_failure_falls_back_to_x11_and_reports_when_nothing_is_left() {
        let e = env(true, true);
        let p = plan(BackendPreference::Auto, &e, || Err("connection refused".into())).unwrap();
        assert_eq!(p, vec![Choice::X11]);
        let err =
            plan(BackendPreference::Auto, &env(true, false), || Err("connection refused".into()))
                .unwrap_err()
                .to_string();
        assert!(err.contains("connection refused") && err.contains("DISPLAY"), "{err}");
    }

    #[test]
    fn no_display_at_all_is_an_actionable_error() {
        let err = plan(BackendPreference::Auto, &env(false, false), || unreachable!()).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("WAYLAND_DISPLAY") && s.contains("DISPLAY"), "{s}");
    }

    #[test]
    fn compositor_without_shm_or_seat_is_rejected() {
        let p = auto(&env(true, false), &["wl_compositor", "xdg_wm_base", "zwlr_layer_shell_v1"]);
        assert!(p.is_err());
    }

    #[test]
    fn explicit_preference_skips_probing() {
        let never = || -> Result<WaylandCaps, String> { panic!("must not probe") };
        assert_eq!(
            plan(BackendPreference::X11, &env(true, true), never).unwrap(),
            vec![Choice::X11]
        );
        assert_eq!(
            plan(BackendPreference::WaylandFullscreen, &env(false, false), never).unwrap(),
            vec![Choice::WaylandFullscreen]
        );
        assert!(plan(BackendPreference::Windows, &env(true, true), never).is_err());
    }

    #[test]
    fn caps_from_globals() {
        let c = WaylandCaps::from_globals(SWAY.iter().copied());
        assert!(c.layer_shell && c.xdg_shell && c.viewporter && c.cursor_shape && c.usable());
        let c = WaylandCaps::from_globals(MUTTER.iter().copied());
        assert!(!c.layer_shell && c.xdg_shell && c.usable());
    }
}
