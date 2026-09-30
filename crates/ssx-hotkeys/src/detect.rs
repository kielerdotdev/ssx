//! Working out which hotkey mechanism suits the running desktop.
//!
//! Detection is a pure function of an [`Environment`] snapshot (so it is testable with
//! fixtures) and answers with an *ordered list* of [`Strategy`] candidates, best first.
//! It cannot know things only a running system can tell (does this GNOME's portal
//! implement GlobalShortcuts? is `gsettings` installed?), so the in-process candidates are
//! probed at runtime by [`crate::open_best_manager`], and the generator candidates are
//! offered to the user as explicit actions.
//!
//! The choices, and why:
//!
//! * **Windows, macOS, X11**: `global-hotkey` grabs keys directly. Works on GNOME/KDE/etc.
//!   *on X11* too.
//! * **sway**: no in-app grabbing and `xdg-desktop-portal-wlr` has no GlobalShortcuts, so
//!   only a `bindsym` include file works.
//! * **Hyprland**: `xdg-desktop-portal-hyprland` implements GlobalShortcuts, but only as a
//!   target for `bind = ..., global, appid:name` lines the user must write anyway, so a
//!   generated `bind = ..., exec, ...` file is the more direct answer.
//! * **GNOME (Wayland)**: GNOME 48+ implements the portal; older versions do not, and then
//!   the `gsettings` custom-keybindings route works on every version.
//! * **KDE Plasma (Wayland)**: Plasma implements the portal; the fallback is a KDE
//!   "command shortcut" (`.desktop` file + `kglobalshortcutsrc`).
//! * **Other Wayland desktops**: the portal if it exists, otherwise the user binds
//!   `ssx capture ...` commands in their own compositor.

use std::collections::HashMap;

/// The operating system family, separate from `cfg!` so detection can be tested for any OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Microsoft Windows.
    Windows,
    /// macOS.
    MacOs,
    /// Linux and other Unixes.
    Linux,
}

impl Platform {
    /// The platform this binary was compiled for.
    pub const fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

/// The environment variables detection reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// `XDG_CURRENT_DESKTOP` (colon-separated, e.g. `ubuntu:GNOME`).
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_TYPE` (`x11`, `wayland`, `tty`).
    pub xdg_session_type: Option<String>,
    /// `WAYLAND_DISPLAY`.
    pub wayland_display: Option<String>,
    /// `DISPLAY`.
    pub display: Option<String>,
    /// `SWAYSOCK`.
    pub swaysock: Option<String>,
    /// `HYPRLAND_INSTANCE_SIGNATURE`.
    pub hyprland_instance_signature: Option<String>,
}

impl Environment {
    /// Reads the real process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Builds an environment from `(name, value)` pairs (test fixtures).
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let map: HashMap<&str, &str> = pairs.into_iter().collect();
        Self::from_lookup(|k| map.get(k).map(|v| (*v).to_owned()))
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |k: &str| get(k).filter(|v| !v.is_empty());
        Self {
            xdg_current_desktop: non_empty("XDG_CURRENT_DESKTOP"),
            xdg_session_type: non_empty("XDG_SESSION_TYPE"),
            wayland_display: non_empty("WAYLAND_DISPLAY"),
            display: non_empty("DISPLAY"),
            swaysock: non_empty("SWAYSOCK"),
            hyprland_instance_signature: non_empty("HYPRLAND_INSTANCE_SIGNATURE"),
        }
    }
}

/// A desktop environment or compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Desktop {
    /// GNOME Shell (Mutter).
    Gnome,
    /// KDE Plasma (KWin).
    Kde,
    /// sway.
    Sway,
    /// Hyprland.
    Hyprland,
    /// niri.
    Niri,
    /// COSMIC.
    Cosmic,
    /// Xfce.
    Xfce,
    /// Cinnamon, MATE, LXQt, Budgie or anything else.
    Other,
    /// Not a desktop session (Windows, macOS, or nothing set).
    Unknown,
}

/// X11 or Wayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionType {
    /// An X11 session.
    X11,
    /// A Wayland session.
    Wayland,
    /// Neither could be determined.
    Unknown,
}

/// One way of getting global hotkeys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// Grab keys in-process with the `global-hotkey` crate.
    GlobalHotkey,
    /// The XDG GlobalShortcuts portal (in-process; needs a portal that implements it).
    Portal,
    /// Generate a sway `bindsym` include file.
    SwayConfig,
    /// Generate a Hyprland `bind = ` source file.
    HyprlandConfig,
    /// Register GNOME custom keybindings through `gsettings`.
    GnomeGsettings,
    /// Register KDE command shortcuts (`.desktop` + `kglobalshortcutsrc`).
    KdeShortcuts,
    /// Nothing can be automated; tell the user to bind `ssx` CLI commands themselves.
    CliOnly,
}

impl Strategy {
    /// `true` for strategies that grab keys inside the process, `false` for the ones that
    /// produce desktop configuration.
    pub const fn is_in_process(self) -> bool {
        matches!(self, Strategy::GlobalHotkey | Strategy::Portal)
    }
}

/// The result of [`detect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The desktop found.
    pub desktop: Desktop,
    /// The session type found.
    pub session: SessionType,
    /// Strategies in order of preference.
    pub candidates: Vec<Strategy>,
}

impl Detection {
    /// The best strategy.
    pub fn primary(&self) -> Strategy {
        self.candidates.first().copied().unwrap_or(Strategy::CliOnly)
    }
}

/// Identifies the desktop from the environment.
pub fn detect_desktop(env: &Environment) -> Desktop {
    let tokens: Vec<String> = env
        .xdg_current_desktop
        .as_deref()
        .unwrap_or("")
        .split(':')
        .map(str::to_ascii_lowercase)
        .collect();
    let has = |name: &str| tokens.iter().any(|t| t == name);
    if env.swaysock.is_some() || has("sway") {
        Desktop::Sway
    } else if env.hyprland_instance_signature.is_some() || has("hyprland") {
        Desktop::Hyprland
    } else if has("kde") || has("plasma") {
        Desktop::Kde
    } else if has("gnome") || has("gnome-classic") || has("ubuntu") || has("unity") {
        Desktop::Gnome
    } else if has("niri") {
        Desktop::Niri
    } else if has("cosmic") {
        Desktop::Cosmic
    } else if has("xfce") {
        Desktop::Xfce
    } else if tokens.iter().all(String::is_empty) {
        Desktop::Unknown
    } else {
        Desktop::Other
    }
}

/// Identifies the session type from the environment.
pub fn detect_session(env: &Environment) -> SessionType {
    match env.xdg_session_type.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("x11") => SessionType::X11,
        Some("wayland") => SessionType::Wayland,
        _ if env.wayland_display.is_some() => SessionType::Wayland,
        _ if env.display.is_some() => SessionType::X11,
        _ => SessionType::Unknown,
    }
}

/// Chooses hotkey strategies for `env` on `platform`.
pub fn detect(env: &Environment, platform: Platform) -> Detection {
    if platform != Platform::Linux {
        return Detection {
            desktop: Desktop::Unknown,
            session: SessionType::Unknown,
            candidates: vec![Strategy::GlobalHotkey],
        };
    }
    let desktop = detect_desktop(env);
    let session = detect_session(env);
    let mut candidates = match session {
        SessionType::X11 => vec![Strategy::GlobalHotkey],
        SessionType::Wayland => match desktop {
            Desktop::Sway => vec![Strategy::SwayConfig],
            Desktop::Hyprland => vec![Strategy::HyprlandConfig],
            Desktop::Gnome => vec![Strategy::Portal, Strategy::GnomeGsettings],
            Desktop::Kde => vec![Strategy::Portal, Strategy::KdeShortcuts],
            _ => vec![Strategy::Portal, Strategy::CliOnly],
        },
        SessionType::Unknown => match desktop {
            // A compositor's own variables are proof enough even without session vars.
            Desktop::Sway => vec![Strategy::SwayConfig],
            Desktop::Hyprland => vec![Strategy::HyprlandConfig],
            _ => vec![Strategy::CliOnly],
        },
    };
    // Config-based fallbacks are also useful on X11 GNOME/KDE, e.g. when the user prefers
    // the desktop's own shortcut settings page over an app grabbing keys.
    if session == SessionType::X11 {
        match desktop {
            Desktop::Gnome => candidates.push(Strategy::GnomeGsettings),
            Desktop::Kde => candidates.push(Strategy::KdeShortcuts),
            _ => {}
        }
    }
    Detection { desktop, session, candidates }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Strategy::*;

    fn case(pairs: &[(&str, &str)]) -> Detection {
        detect(&Environment::from_pairs(pairs.iter().copied()), Platform::Linux)
    }

    #[test]
    fn detection_matrix() {
        #[rustfmt::skip]
        type Case<'a> = (&'a [(&'a str, &'a str)], Desktop, SessionType, &'a [Strategy]);
        let matrix: &[Case<'_>] = &[
            // X11 everywhere: the crate can grab keys.
            (
                &[("XDG_CURRENT_DESKTOP", "XFCE"), ("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0")],
                Desktop::Xfce,
                SessionType::X11,
                &[GlobalHotkey],
            ),
            (
                &[
                    ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
                    ("XDG_SESSION_TYPE", "x11"),
                    ("DISPLAY", ":0"),
                ],
                Desktop::Gnome,
                SessionType::X11,
                &[GlobalHotkey, GnomeGsettings],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "KDE"), ("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0")],
                Desktop::Kde,
                SessionType::X11,
                &[GlobalHotkey, KdeShortcuts],
            ),
            // Wayland desktops.
            (
                &[
                    ("XDG_CURRENT_DESKTOP", "GNOME"),
                    ("XDG_SESSION_TYPE", "wayland"),
                    ("WAYLAND_DISPLAY", "wayland-0"),
                    ("DISPLAY", ":0"),
                ],
                Desktop::Gnome,
                SessionType::Wayland,
                &[Portal, GnomeGsettings],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"), ("XDG_SESSION_TYPE", "wayland")],
                Desktop::Gnome,
                SessionType::Wayland,
                &[Portal, GnomeGsettings],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "KDE"), ("XDG_SESSION_TYPE", "wayland")],
                Desktop::Kde,
                SessionType::Wayland,
                &[Portal, KdeShortcuts],
            ),
            (
                &[
                    ("XDG_CURRENT_DESKTOP", "sway"),
                    ("XDG_SESSION_TYPE", "wayland"),
                    ("SWAYSOCK", "/run/user/1000/sway-ipc.sock"),
                ],
                Desktop::Sway,
                SessionType::Wayland,
                &[SwayConfig],
            ),
            (
                &[("XDG_SESSION_TYPE", "wayland"), ("SWAYSOCK", "/run/x")],
                Desktop::Sway,
                SessionType::Wayland,
                &[SwayConfig],
            ),
            (
                &[
                    ("XDG_CURRENT_DESKTOP", "Hyprland"),
                    ("XDG_SESSION_TYPE", "wayland"),
                    ("HYPRLAND_INSTANCE_SIGNATURE", "abc_123"),
                ],
                Desktop::Hyprland,
                SessionType::Wayland,
                &[HyprlandConfig],
            ),
            (
                &[("XDG_SESSION_TYPE", "wayland"), ("HYPRLAND_INSTANCE_SIGNATURE", "abc")],
                Desktop::Hyprland,
                SessionType::Wayland,
                &[HyprlandConfig],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "niri"), ("XDG_SESSION_TYPE", "wayland")],
                Desktop::Niri,
                SessionType::Wayland,
                &[Portal, CliOnly],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "COSMIC"), ("XDG_SESSION_TYPE", "wayland")],
                Desktop::Cosmic,
                SessionType::Wayland,
                &[Portal, CliOnly],
            ),
            (
                &[("XDG_CURRENT_DESKTOP", "X-Cinnamon"), ("XDG_SESSION_TYPE", "wayland")],
                Desktop::Other,
                SessionType::Wayland,
                &[Portal, CliOnly],
            ),
            // Missing session type: fall back to display variables.
            (
                &[
                    ("XDG_CURRENT_DESKTOP", "GNOME"),
                    ("WAYLAND_DISPLAY", "wayland-0"),
                    ("DISPLAY", ":0"),
                ],
                Desktop::Gnome,
                SessionType::Wayland,
                &[Portal, GnomeGsettings],
            ),
            (&[("DISPLAY", ":1")], Desktop::Unknown, SessionType::X11, &[GlobalHotkey]),
            // Compositor variables without session info (e.g. launched from a service).
            (&[("SWAYSOCK", "/s")], Desktop::Sway, SessionType::Unknown, &[SwayConfig]),
            (
                &[("HYPRLAND_INSTANCE_SIGNATURE", "x")],
                Desktop::Hyprland,
                SessionType::Unknown,
                &[HyprlandConfig],
            ),
            // Nothing at all (ssh, tty, container).
            (&[], Desktop::Unknown, SessionType::Unknown, &[CliOnly]),
            (&[("XDG_SESSION_TYPE", "tty")], Desktop::Unknown, SessionType::Unknown, &[CliOnly]),
            // Empty values count as unset.
            (
                &[("XDG_CURRENT_DESKTOP", ""), ("DISPLAY", ""), ("SWAYSOCK", "")],
                Desktop::Unknown,
                SessionType::Unknown,
                &[CliOnly],
            ),
            // Session type wins over leftover compositor variables.
            (
                &[("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0"), ("SWAYSOCK", "/stale")],
                Desktop::Sway,
                SessionType::X11,
                &[GlobalHotkey],
            ),
            // Case-insensitive.
            (
                &[("XDG_CURRENT_DESKTOP", "kde"), ("XDG_SESSION_TYPE", "WAYLAND")],
                Desktop::Kde,
                SessionType::Wayland,
                &[Portal, KdeShortcuts],
            ),
        ];
        for (pairs, desktop, session, want) in matrix {
            let d = case(pairs);
            assert_eq!((d.desktop, d.session), (*desktop, *session), "{pairs:?}");
            assert_eq!(d.candidates, *want, "{pairs:?}");
            assert_eq!(d.primary(), want[0]);
        }
    }

    #[test]
    fn windows_and_macos_always_use_the_crate() {
        for p in [Platform::Windows, Platform::MacOs] {
            let d = detect(&Environment::default(), p);
            assert_eq!(d.candidates, vec![GlobalHotkey]);
        }
    }

    #[test]
    fn in_process_flag() {
        assert!(GlobalHotkey.is_in_process() && Portal.is_in_process());
        for s in [SwayConfig, HyprlandConfig, GnomeGsettings, KdeShortcuts, CliOnly] {
            assert!(!s.is_in_process());
        }
    }
}
