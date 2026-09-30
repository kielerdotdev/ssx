//! The Windows and macOS tray: `tray-icon` on a `tao` event loop, on the main thread.
//!
//! Both platforms need the tray icon *and* the global-hotkey manager to live on the thread
//! that pumps native messages, so this module owns that thread's loop for the whole life of the
//! daemon:
//!
//! * the tray icon (and its menu) is built from the same [`TrayView`] model as on Linux and
//!   rebuilt whenever the app pushes a new view (through an event-loop proxy, from any thread);
//! * menu clicks come back as [`Action`]s and are handed to the app (which never blocks);
//! * the [`HotkeyRunner`] is created here, receives `apply` commands from the app as loop events
//!   and is polled for key presses on every wake-up (the loop wakes ten times a second: the
//!   price of `global-hotkey` delivering events on a channel instead of a message).
//!
//! **Not verified on real Windows or macOS** (this crate is developed on Linux): it is
//! type-checked for `x86_64-pc-windows-msvc`. The manual checklist is in the README.

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy},
    platform::run_return::EventLoopExtRunReturn,
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu as NativeMenu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::{
    app::App,
    daemon::Origin,
    hotkeys_glue::{HotkeyControl, HotkeyRunner, HotkeyStatus, Plan},
    icons::render,
    menu::{Action, Item, Menu},
    tray::{TrayHandle, TrayView},
};

enum UserEvent {
    /// A menu entry was clicked (its action id).
    Menu(String),
    /// Show this view.
    Refresh(Box<TrayView>),
    /// Register hotkeys and answer with the status.
    Hotkeys { plan: Box<Plan>, enabled: bool, reply: mpsc::Sender<HotkeyStatus> },
    /// Leave the loop.
    Quit,
}

/// Pushes views into the loop from any thread.
struct NativeTray {
    proxy: EventLoopProxy<UserEvent>,
}

impl std::fmt::Debug for NativeTray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTray").finish_non_exhaustive()
    }
}

impl TrayHandle for NativeTray {
    fn refresh(&self, view: &TrayView) {
        let _ = self.proxy.send_event(UserEvent::Refresh(Box::new(view.clone())));
    }

    fn shutdown(&self) {
        let _ = self.proxy.send_event(UserEvent::Quit);
    }
}

/// The hotkey manager lives on the loop thread; the app talks to it through events.
struct NativeHotkeys {
    proxy: EventLoopProxy<UserEvent>,
    initial: HotkeyStatus,
}

impl HotkeyControl for NativeHotkeys {
    fn initial_status(&self) -> HotkeyStatus {
        self.initial.clone()
    }

    fn apply(&self, plan: Plan, enabled: bool) -> HotkeyStatus {
        let (reply, rx) = mpsc::channel();
        let sent = self
            .proxy
            .send_event(UserEvent::Hotkeys { plan: Box::new(plan), enabled, reply })
            .is_ok();
        if !sent {
            return self.initial.clone();
        }
        rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| self.initial.clone())
    }
}

fn native_menu(model: &Menu) -> NativeMenu {
    let menu = NativeMenu::new();
    for item in &model.items {
        let appended = match item {
            Item::Separator => menu.append(&PredefinedMenuItem::separator()),
            Item::Header(text) => menu.append(&MenuItem::new(text, false, None)),
            Item::Entry(e) => {
                // A tab separates the label from the right-aligned accelerator text in Win32
                // menus; the hotkey is registered by the app, not by the menu.
                let text = match &e.shortcut {
                    Some(s) => format!("{}\t{s}", e.label),
                    None => e.label.clone(),
                };
                match e.checked {
                    Some(checked) => menu.append(&CheckMenuItem::with_id(
                        e.action.id(),
                        text,
                        e.enabled,
                        checked,
                        None,
                    )),
                    None => menu.append(&MenuItem::with_id(e.action.id(), text, e.enabled, None)),
                }
            }
        };
        if let Err(e) = appended {
            tracing::warn!("cannot add a tray menu item: {e}");
        }
    }
    menu
}

fn native_icon(view: &TrayView) -> Option<Icon> {
    let img = render(view.icon, 32);
    Icon::from_rgba(img.rgba, img.width, img.height).ok()
}

fn build_tray(view: &TrayView) -> Option<TrayIcon> {
    let mut b = TrayIconBuilder::new()
        .with_menu(Box::new(native_menu(&view.menu)))
        .with_tooltip(&view.tooltip);
    if let Some(icon) = native_icon(view) {
        b = b.with_icon(icon);
    }
    match b.build() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("cannot create the tray icon: {e}");
            None
        }
    }
}

fn update_tray(tray: &TrayIcon, view: &TrayView) {
    tray.set_menu(Some(Box::new(native_menu(&view.menu))));
    if let Err(e) = tray.set_tooltip(Some(&view.tooltip)) {
        tracing::debug!("cannot update the tray tooltip: {e}");
    }
    if let Err(e) = tray.set_icon(native_icon(view)) {
        tracing::debug!("cannot update the tray icon: {e}");
    }
}

/// Runs the event loop until the app quits. Returns when the loop has ended so the caller
/// can shut the rest down.
pub fn run(app: Arc<App>) {
    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = p.send_event(UserEvent::Menu(e.id.0));
    }));

    let mut runner = if app.no_hotkeys() {
        HotkeyRunner::unavailable("disabled with --no-hotkeys")
    } else {
        HotkeyRunner::open()
    };
    let initial = HotkeyStatus {
        backend: runner.backend().map(|b| b.to_string()),
        unavailable: runner.unavailable_reason().cloned(),
        ..HotkeyStatus::default()
    };

    // The app registers hotkeys and pushes views through these; both go through the loop.
    {
        let app = Arc::clone(&app);
        let hk = NativeHotkeys { proxy: proxy.clone(), initial };
        let _ = std::thread::Builder::new().name("ssx-attach-hotkeys".into()).spawn(move || {
            app.attach_hotkeys(Box::new(hk));
        });
    }
    app.set_tray(Some(Arc::new(NativeTray { proxy: proxy.clone() })));
    {
        // A quit requested elsewhere (IPC, signal) ends the loop.
        let app = Arc::clone(&app);
        let p = proxy.clone();
        let _ = std::thread::Builder::new().name("ssx-quit-watch".into()).spawn(move || {
            app.wait_quit();
            let _ = p.send_event(UserEvent::Quit);
        });
    }

    let show_tray = !app.no_tray();
    let mut tray: Option<TrayIcon> = None;
    let _ = event_loop.run_return(move |event, _target, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        match event {
            Event::NewEvents(StartCause::Init) if show_tray => {
                tray = build_tray(&app.tray_view());
            }
            Event::UserEvent(UserEvent::Refresh(view)) => {
                if let Some(t) = &tray {
                    update_tray(t, &view);
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if let Some(a) = Action::from_id(&id) {
                    app.handle_action(a, Origin::Tray);
                } else {
                    tracing::debug!("unknown tray menu id {id:?}");
                }
            }
            Event::UserEvent(UserEvent::Hotkeys { plan, enabled, reply }) => {
                let _ = reply.send(runner.apply(&plan, enabled));
            }
            Event::UserEvent(UserEvent::Quit) => {
                tray = None;
                app.request_quit();
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
        for target in runner.try_poll() {
            app.handle_hotkey(target);
        }
    });
}
