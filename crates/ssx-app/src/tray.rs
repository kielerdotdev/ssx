//! What every tray backend shares: the view it renders, the handle the app updates it
//! through, and the advice shown when the desktop has no tray.
//!
//! Backends: `tray_ksni` (Linux, StatusNotifierItem over D-Bus, no GTK) and `tray_native`
//! (Windows and macOS, `tray-icon` + `tao`). Both are thin: they translate a [`TrayView`]
//! (the [`Menu`] model, a tooltip, an [`IconKind`]) into their toolkit's items and hand clicks
//! back as [`Action`]s. Everything else is decided in `menu`.

use std::sync::Arc;

use crate::menu::{Action, IconKind, Menu};

/// Everything a backend renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    /// The menu.
    pub menu: Menu,
    /// The one-line tooltip.
    pub tooltip: String,
    /// The icon.
    pub icon: IconKind,
}

/// Receives the action of a clicked menu entry. Called on the backend's thread: must not
/// block (hand the work to another thread).
pub type ActionSink = Arc<dyn Fn(Action) + Send + Sync>;

/// A running tray icon.
pub trait TrayHandle: Send + Sync + std::fmt::Debug {
    /// Shows a new state.
    fn refresh(&self, view: &TrayView);
    /// Removes the icon.
    fn shutdown(&self);
}

/// Why there is no tray icon (yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayProblem {
    /// What is missing, in one sentence.
    pub reason: String,
    /// What the user can do, in one or two sentences.
    pub advice: String,
    /// The one-time-notice key (so the advice is shown once per installation).
    pub notice_key: &'static str,
}

/// The advice for a desktop that has no StatusNotifierWatcher. `desktop` is
/// `XDG_CURRENT_DESKTOP` (may be empty).
pub fn missing_watcher_advice(desktop: &str) -> TrayProblem {
    let d = desktop.to_ascii_lowercase();
    if d.contains("gnome") || d.contains("unity") || d.contains("pop") || d.contains("ubuntu") {
        TrayProblem {
            reason: "GNOME shows no tray icons without an extension".to_owned(),
            advice: "Install and enable the \"AppIndicator and KStatusNotifierItem Support\" \
                     extension (package gnome-shell-extension-appindicator), then restart ssx. \
                     ssx keeps working without the tray: use the hotkeys or the `ssx` command."
                .to_owned(),
            notice_key: "tray-gnome-appindicator",
        }
    } else {
        TrayProblem {
            reason: "no StatusNotifier tray is running on this desktop".to_owned(),
            advice: "Start a system tray that supports StatusNotifierItem (KDE Plasma, waybar's \
                     tray module, snixembed for older bars), then restart ssx. ssx keeps \
                     working without the tray: use the hotkeys or the `ssx` command."
                .to_owned(),
            notice_key: "tray-no-watcher",
        }
    }
}

/// A tray that shows nothing (`--no-tray`, tests, a desktop without one).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTray;

impl TrayHandle for NoTray {
    fn refresh(&self, _: &TrayView) {}
    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_and_derivatives_get_the_extension_advice() {
        for d in ["GNOME", "ubuntu:GNOME", "Unity", "pop:GNOME", "X-Cinnamon:GNOME"] {
            let p = missing_watcher_advice(d);
            assert!(p.advice.contains("AppIndicator"), "{d}: {}", p.advice);
            assert_eq!(p.notice_key, "tray-gnome-appindicator");
        }
    }

    #[test]
    fn other_desktops_get_generic_advice_and_a_different_key() {
        for d in ["", "sway", "Hyprland", "XFCE"] {
            let p = missing_watcher_advice(d);
            assert!(!p.advice.contains("AppIndicator"), "{d}");
            assert_eq!(p.notice_key, "tray-no-watcher");
            assert!(p.advice.contains("hotkeys") && p.advice.contains("ssx"));
        }
    }

    #[test]
    fn no_tray_accepts_everything() {
        let t = NoTray;
        t.refresh(&TrayView { menu: Menu::default(), tooltip: String::new(), icon: IconKind::Idle });
        t.shutdown();
    }
}
