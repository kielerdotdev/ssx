//! The Linux tray: a StatusNotifierItem served by `ksni` (pure Rust over D-Bus, no GTK).
//!
//! `ksni` runs its own thread and asks [`SsxTray`] for the icon, the tooltip and the menu
//! whenever we tell it something changed ([`TrayHandle::refresh`]). Menu clicks call closures
//! that send the [`Action`] to the app's [`ActionSink`] and return at once.
//!
//! **No watcher, no failure.** Without `org.kde.StatusNotifierWatcher` (GNOME without the
//! AppIndicator extension, a bare compositor) the first attempt fails with
//! `Error::Watcher`. [`start`] then reports the reason and advice to the caller (which shows a
//! one-time notification) and starts the tray again in "assume it will appear" mode, so the icon
//! shows up as soon as the user enables the extension or starts their bar, without restarting
//! ssx. If the session has no D-Bus at all the tray is simply unavailable; the daemon runs on.

use std::sync::Arc;

use ksni::{
    Category, Icon, Status, ToolTip,
    blocking::{Handle, TrayMethods},
    menu::{CheckmarkItem, StandardItem},
};

use crate::{
    icons::{IconImage, render_set},
    menu::{Action, IconKind, Item},
    tray::{ActionSink, TrayHandle, TrayProblem, TrayView, missing_watcher_advice},
};

/// dbusmenu shortcut of an accelerator such as `Ctrl+PrintScreen`: `[["Control", "Print"]]`.
pub fn dbusmenu_shortcut(accel: &str) -> Vec<Vec<String>> {
    let parts: Vec<String> = accel
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Control".to_owned(),
            "alt" | "option" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            "super" | "win" | "windows" | "meta" | "cmd" | "command" => "Super".to_owned(),
            "printscreen" | "prtsc" | "print" => "Print".to_owned(),
            "escape" | "esc" => "Escape".to_owned(),
            _ => p.to_owned(),
        })
        .collect();
    if parts.len() < 2 && parts.first().is_none_or(|p| p.is_empty()) {
        return Vec::new();
    }
    vec![parts]
}

fn icons_for(kind: IconKind) -> Vec<Icon> {
    render_set(kind).iter().map(to_ksni_icon).collect()
}

fn to_ksni_icon(img: &IconImage) -> Icon {
    Icon {
        width: i32::try_from(img.width).unwrap_or(16),
        height: i32::try_from(img.height).unwrap_or(16),
        data: img.to_argb32(),
    }
}

/// The tray model ksni queries.
struct SsxTray {
    view: TrayView,
    icons: Vec<Icon>,
    sink: ActionSink,
}

impl std::fmt::Debug for SsxTray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsxTray").field("tooltip", &self.view.tooltip).finish_non_exhaustive()
    }
}

impl ksni::Tray for SsxTray {
    fn id(&self) -> String {
        "ssx".to_owned()
    }

    fn title(&self) -> String {
        "ssx".to_owned()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        match self.view.icon {
            IconKind::Error => Status::NeedsAttention,
            IconKind::Idle | IconKind::Recording => Status::Active,
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: self.view.tooltip.clone(),
            description: String::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // A left click while recording stops the recording (the red icon is a button); the
        // menu is on the right click, so an idle left click does nothing surprising.
        if self.view.icon == IconKind::Recording {
            (self.sink)(Action::StopRecording);
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.view
            .menu
            .items
            .iter()
            .map(|item| match item {
                Item::Separator => ksni::MenuItem::Separator,
                Item::Header(text) => StandardItem {
                    label: text.clone(),
                    enabled: false,
                    ..StandardItem::default()
                }
                .into(),
                Item::Entry(e) => {
                    let action = e.action.clone();
                    let shortcut = e.shortcut.as_deref().map(dbusmenu_shortcut).unwrap_or_default();
                    match e.checked {
                        Some(checked) => CheckmarkItem {
                            label: e.label.clone(),
                            enabled: e.enabled,
                            checked,
                            shortcut,
                            activate: Box::new(move |t: &mut Self| (t.sink)(action.clone())),
                            ..CheckmarkItem::default()
                        }
                        .into(),
                        None => StandardItem {
                            label: e.label.clone(),
                            enabled: e.enabled,
                            shortcut,
                            activate: Box::new(move |t: &mut Self| (t.sink)(action.clone())),
                            ..StandardItem::default()
                        }
                        .into(),
                    }
                }
            })
            .collect()
    }
}

/// A running ksni tray.
pub struct KsniHandle {
    handle: Handle<SsxTray>,
}

impl std::fmt::Debug for KsniHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KsniHandle").finish_non_exhaustive()
    }
}

impl TrayHandle for KsniHandle {
    fn refresh(&self, view: &TrayView) {
        let view = view.clone();
        let icons = icons_for(view.icon);
        // `None` means the tray service already ended (the host went away for good).
        let _ = self.handle.update(move |t: &mut SsxTray| {
            t.icons = icons;
            t.view = view;
        });
    }

    fn shutdown(&self) {
        self.handle.shutdown().wait();
    }
}

/// How starting the tray went.
#[derive(Debug)]
pub enum Started {
    /// The tray is up and registered.
    Running(Arc<dyn TrayHandle>),
    /// There is no StatusNotifierWatcher yet: the tray will appear when one does. The
    /// problem should be shown to the user once.
    Waiting(Arc<dyn TrayHandle>, TrayProblem),
    /// No tray is possible in this session (no D-Bus).
    Unavailable(String),
}

/// Starts the tray with an initial `view`. `desktop` is `XDG_CURRENT_DESKTOP`.
pub fn start(view: &TrayView, sink: &ActionSink, desktop: &str) -> Started {
    let make = || SsxTray {
        view: view.clone(),
        icons: icons_for(view.icon),
        sink: Arc::clone(sink),
    };
    match make().spawn() {
        Ok(handle) => Started::Running(Arc::new(KsniHandle { handle })),
        Err(ksni::Error::Dbus(e)) => {
            Started::Unavailable(format!("no D-Bus session bus to show a tray icon on: {e}"))
        }
        Err(e @ (ksni::Error::Watcher(_) | ksni::Error::WontShow)) => {
            tracing::info!("no tray yet: {e}");
            match make().assume_sni_available(true).spawn() {
                Ok(handle) => {
                    Started::Waiting(Arc::new(KsniHandle { handle }), missing_watcher_advice(desktop))
                }
                Err(e2) => Started::Unavailable(format!("cannot show a tray icon: {e2}")),
            }
        }
        Err(e) => Started::Unavailable(format!("cannot show a tray icon: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ksni::Tray;

    use super::*;
    use crate::menu::{UiState, build_menu, icon_kind, tooltip};

    fn tray_for(ui: &UiState) -> SsxTray {
        let view = TrayView {
            menu: build_menu(&ssx_core::settings::Settings::default(), ui),
            tooltip: tooltip(ui),
            icon: icon_kind(ui),
        };
        SsxTray {
            icons: icons_for(view.icon),
            view,
            sink: Arc::new(|_| {}),
        }
    }

    #[test]
    fn shortcuts_use_dbusmenu_names() {
        assert_eq!(dbusmenu_shortcut("Ctrl+PrintScreen"), [["Control", "Print"]]);
        assert_eq!(dbusmenu_shortcut("Ctrl+Shift+Alt+PrintScreen"), [["Control", "Shift", "Alt", "Print"]]);
        assert_eq!(dbusmenu_shortcut("Super+E"), [["Super", "E"]]);
        assert_eq!(dbusmenu_shortcut("F9"), [["F9"]]);
        assert!(dbusmenu_shortcut("").is_empty());
        assert!(dbusmenu_shortcut(" + ").is_empty());
    }

    #[test]
    fn the_ksni_menu_mirrors_the_model_row_for_row() {
        let ui = UiState::default();
        let tray = tray_for(&ui);
        let model = &tray.view.menu.items;
        let items = tray.menu();
        assert_eq!(items.len(), model.len());
        for (m, k) in model.iter().zip(&items) {
            match (m, k) {
                (Item::Separator, ksni::MenuItem::Separator) => {}
                (Item::Header(t), ksni::MenuItem::Standard(s)) => {
                    assert_eq!(&s.label, t);
                    assert!(!s.enabled);
                }
                (Item::Entry(e), ksni::MenuItem::Standard(s)) => {
                    assert_eq!((&s.label, s.enabled), (&e.label, e.enabled));
                    assert_eq!(s.shortcut.is_empty(), e.shortcut.is_none(), "{}", e.label);
                }
                (Item::Entry(e), ksni::MenuItem::Checkmark(c)) => {
                    assert_eq!(Some(c.checked), e.checked);
                    assert_eq!(c.label, e.label);
                }
                _ => panic!("the ksni item does not match the model row {m:?}"),
            }
        }
    }

    #[test]
    fn clicking_an_item_sends_its_action() {
        let sent: Arc<Mutex<Vec<Action>>> = Arc::default();
        let sent2 = Arc::clone(&sent);
        let ui = UiState::default();
        let mut tray = tray_for(&ui);
        tray.sink = Arc::new(move |a| sent2.lock().unwrap().push(a));
        let items = tray.menu();
        let quit_pos = tray
            .view
            .menu
            .items
            .iter()
            .position(|i| matches!(i, Item::Entry(e) if e.action == Action::Quit))
            .unwrap();
        let ksni::MenuItem::Standard(quit) = &items[quit_pos] else { panic!() };
        (quit.activate)(&mut tray);
        assert_eq!(*sent.lock().unwrap(), [Action::Quit]);
    }

    #[test]
    fn the_tooltip_status_and_icons_follow_the_view() {
        let mut ui = UiState::default();
        let t = tray_for(&ui);
        assert_eq!(t.tool_tip().title, "ssx: Ready");
        assert_eq!(t.status(), Status::Active);
        assert_eq!(t.icon_pixmap().len(), crate::icons::SNI_SIZES.len());
        assert!(t.icon_pixmap().iter().all(|i| i.data.len() == (i.width * i.height * 4) as usize));
        ui.last_error = Some("boom".into());
        assert_eq!(tray_for(&ui).status(), Status::NeedsAttention);
        ui.recording = crate::events::RecordingView::Recording { elapsed: std::time::Duration::from_secs(3) };
        let rec = tray_for(&ui);
        assert_eq!(rec.status(), Status::Active);
        assert!(rec.tool_tip().title.contains("Recording 0:03"));
    }

    #[test]
    fn a_left_click_stops_a_recording_and_does_nothing_otherwise() {
        let sent: Arc<Mutex<Vec<Action>>> = Arc::default();
        let sent2 = Arc::clone(&sent);
        let mut idle = tray_for(&UiState::default());
        idle.sink = Arc::new(move |a| sent2.lock().unwrap().push(a));
        idle.activate(0, 0);
        assert!(sent.lock().unwrap().is_empty());
        let mut ui = UiState::default();
        ui.recording = crate::events::RecordingView::Recording { elapsed: std::time::Duration::ZERO };
        let mut rec = tray_for(&ui);
        let sent3 = Arc::clone(&sent);
        rec.sink = Arc::new(move |a| sent3.lock().unwrap().push(a));
        rec.activate(0, 0);
        assert_eq!(*sent.lock().unwrap(), [Action::StopRecording]);
    }
}
