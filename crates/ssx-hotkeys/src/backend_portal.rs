//! Backend for the XDG **GlobalShortcuts** portal (`org.freedesktop.portal.GlobalShortcuts`).
//!
//! On Wayland an application cannot grab keys; it *asks the compositor* through this
//! portal, and the compositor owns the shortcut (the user can rebind it in the desktop's
//! settings). What is known about support:
//!
//! * **KDE Plasma** (`xdg-desktop-portal-kde`): implemented since Plasma 5.27/6.
//! * **GNOME**: implemented from GNOME 48 (`xdg-desktop-portal-gnome`); earlier releases
//!   have no such interface, so [`PortalHotkeys::connect`] fails and the `gsettings`
//!   generator is the fallback.
//! * **Hyprland** (`xdg-desktop-portal-hyprland`): implemented, but shortcuts only fire
//!   when the user also writes a `global` bind line in `hyprland.conf`.
//! * **sway/wlroots, XFCE, Cinnamon, MATE, LXQt** (`-wlr`/`-gtk` portals): not implemented.
//!
//! Behaviour that shapes the API:
//!
//! * The chord is only a *preference* (`preferred_trigger`); the compositor may show a
//!   dialog, ignore it, or let the user pick another key. [`PortalHotkeys::triggers`]
//!   reports what was actually bound.
//! * `BindShortcuts` may be called once per session on some implementations, so every
//!   change of the registered set closes the session and binds the whole set in a new one.
//!   Registering N hotkeys one by one therefore costs N binds (and possibly N dialogs);
//!   use [`PortalHotkeys::register_all`] to bind them together.
//! * Key **release** (`Deactivated`) is part of the spec but several implementations never
//!   send it, so [`HotkeyManager::reports_release`] is `false`; do not build push-to-hold
//!   features on it.
//!
//! The portal is spoken to on a private thread with its own D-Bus connection (blocking,
//! runtime-agnostic `async-io`), so no async runtime is needed by the caller.

use std::{
    collections::HashMap,
    fmt,
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_lite::{StreamExt, future};

use crate::{
    chord::Chord,
    id::HotkeyId,
    manager::{BackendKind, HotkeyError, HotkeyEvent, HotkeyManager, HotkeyState, Result},
};

/// How long a bind may take: the compositor may show a confirmation dialog.
const BIND_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for the portal to answer while connecting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the worker checks for new commands while idle.
const TICK: Duration = Duration::from_millis(40);

type BindResult = std::result::Result<HashMap<String, String>, String>;

enum Command {
    /// Replace the bound set. Answers with `id -> trigger description`.
    Bind(Vec<(HotkeyId, Chord)>, mpsc::Sender<BindResult>),
    Shutdown,
}

/// A [`HotkeyManager`] using the GlobalShortcuts portal.
pub struct PortalHotkeys {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<HotkeyEvent>,
    entries: Vec<(HotkeyId, Chord)>,
    triggers: HashMap<String, String>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for PortalHotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalHotkeys")
            .field("registered", &self.entries.iter().map(|(i, c)| format!("{i}={c}")).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn unavailable(reason: impl Into<String>) -> HotkeyError {
    HotkeyError::Unavailable { backend: BackendKind::Portal, reason: reason.into() }
}

fn backend_error(message: impl Into<String>) -> HotkeyError {
    HotkeyError::Backend { backend: BackendKind::Portal, message: message.into() }
}

async fn connect_bus(address: Option<&str>) -> std::result::Result<zbus::Connection, String> {
    let builder = match address {
        Some(a) => zbus::connection::Builder::address(a),
        None => zbus::connection::Builder::session(),
    };
    builder
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| format!("cannot connect to the D-Bus session bus ({e}); is a graphical session running?"))
}

impl PortalHotkeys {
    /// Connects to the portal on the session bus. Fails with
    /// [`HotkeyError::Unavailable`] when there is no bus, no portal, or the portal does not
    /// implement GlobalShortcuts (GNOME before 48, wlroots, ...).
    pub fn connect() -> Result<Self> {
        Self::connect_to(None)
    }

    /// Like [`connect`](Self::connect) but on an explicit bus address (tests, containers).
    pub fn connect_to(bus_address: Option<&str>) -> Result<Self> {
        let address = bus_address.map(str::to_owned);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let worker = std::thread::Builder::new()
            .name("ssx-portal-hotkeys".into())
            .spawn(move || {
                async_io::block_on(worker(address, cmd_rx, event_tx, ready_tx));
            })
            .map_err(|e| unavailable(format!("cannot start the portal thread: {e}")))?;
        match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(Self {
                commands: cmd_tx,
                events,
                entries: Vec::new(),
                triggers: HashMap::new(),
                worker: Some(worker),
            }),
            Ok(Err(reason)) => {
                let _ = worker.join();
                Err(unavailable(reason))
            }
            Err(_) => Err(unavailable("the portal did not answer in time")),
        }
    }

    /// Registers several hotkeys with a single `BindShortcuts` call.
    pub fn register_all(&mut self, hotkeys: Vec<(HotkeyId, Chord)>) -> Result<()> {
        let mut wanted = self.entries.clone();
        for (id, chord) in hotkeys {
            if wanted.iter().any(|(i, _)| *i == id) {
                return Err(HotkeyError::DuplicateId(id));
            }
            if let Some((existing, _)) = wanted.iter().find(|(_, c)| *c == chord) {
                return Err(HotkeyError::DuplicateChord { chord, existing: existing.clone() });
            }
            wanted.push((id, chord));
        }
        self.rebind(wanted)
    }

    /// What the compositor reports as the actual trigger for each registered id (e.g.
    /// `"Ctrl+Shift+S"` or a localized description). May differ from the requested chord.
    pub fn triggers(&self) -> &HashMap<String, String> {
        &self.triggers
    }

    fn rebind(&mut self, wanted: Vec<(HotkeyId, Chord)>) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(Command::Bind(wanted.clone(), tx))
            .map_err(|_| unavailable("the portal thread has stopped"))?;
        match rx.recv_timeout(BIND_TIMEOUT) {
            Ok(Ok(triggers)) => {
                self.entries = wanted;
                self.triggers = triggers;
                Ok(())
            }
            Ok(Err(message)) => Err(backend_error(message)),
            Err(_) => Err(backend_error("timed out waiting for the compositor to bind the shortcuts")),
        }
    }
}

impl HotkeyManager for PortalHotkeys {
    fn backend(&self) -> BackendKind {
        BackendKind::Portal
    }

    fn register(&mut self, id: HotkeyId, chord: Chord) -> Result<()> {
        self.register_all(vec![(id, chord)])
    }

    fn unregister(&mut self, id: &HotkeyId) -> Result<()> {
        if !self.entries.iter().any(|(i, _)| i == id) {
            return Err(HotkeyError::UnknownId(id.clone()));
        }
        let wanted = self.entries.iter().filter(|(i, _)| i != id).cloned().collect();
        self.rebind(wanted)
    }

    fn registered(&self) -> Vec<(HotkeyId, Chord)> {
        self.entries.clone()
    }

    fn events(&self) -> &mpsc::Receiver<HotkeyEvent> {
        &self.events
    }

    fn reports_release(&self) -> bool {
        false
    }
}

impl Drop for PortalHotkeys {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

enum Wake {
    Activated(Option<ashpd::desktop::global_shortcuts::Activated>),
    Deactivated(Option<ashpd::desktop::global_shortcuts::Deactivated>),
    Tick,
}

async fn worker(
    address: Option<String>,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<HotkeyEvent>,
    ready: mpsc::Sender<std::result::Result<(), String>>,
) {
    let setup = async {
        let conn = connect_bus(address.as_deref()).await?;
        let gs = GlobalShortcuts::with_connection(conn)
            .await
            .map_err(|e| format!("the GlobalShortcuts portal is not available ({e}); GNOME before 48, wlroots and XFCE-style portals do not implement it"))?;
        // Subscribe before anything is bound so no activation can be missed.
        let activated = gs.receive_activated().await.map_err(|e| e.to_string())?;
        let deactivated = gs.receive_deactivated().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((gs, Box::pin(activated), Box::pin(deactivated)))
    };
    let setup = future::or(setup, async {
        async_io::Timer::after(CONNECT_TIMEOUT).await;
        Err("timed out talking to the portal".to_owned())
    })
    .await;
    let (gs, mut activated, mut deactivated) = match setup {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let mut session: Option<ashpd::desktop::Session<GlobalShortcuts>> = None;
    let mut ids: HashMap<String, HotkeyId> = HashMap::new();
    loop {
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::Shutdown => {
                    if let Some(s) = session.take() {
                        let _ = s.close().await;
                    }
                    return;
                }
                Command::Bind(wanted, reply) => {
                    let result = bind(&gs, &mut session, &wanted).await;
                    if result.is_ok() {
                        ids = wanted.into_iter().map(|(id, _)| (id.to_string(), id)).collect();
                    }
                    let _ = reply.send(result);
                }
            }
        }
        let wake = future::or(
            future::or(
                async { Wake::Activated(activated.next().await) },
                async { Wake::Deactivated(deactivated.next().await) },
            ),
            async {
                async_io::Timer::after(TICK).await;
                Wake::Tick
            },
        )
        .await;
        // Signals carry the session handle, but `ashpd` does not expose ours; only one
        // session is ever live (the old one is closed before rebinding), so the shortcut
        // id alone identifies the hotkey.
        let (shortcut, state) = match &wake {
            Wake::Activated(Some(a)) => (a.shortcut_id().to_owned(), HotkeyState::Pressed),
            Wake::Deactivated(Some(d)) => (d.shortcut_id().to_owned(), HotkeyState::Released),
            Wake::Activated(None) | Wake::Deactivated(None) => {
                tracing::warn!("GlobalShortcuts signal stream ended; stopping the portal worker");
                return;
            }
            Wake::Tick => continue,
        };
        if let Some(id) = ids.get(&shortcut) {
            if events.send(HotkeyEvent { id: id.clone(), state }).is_err() {
                return; // manager dropped
            }
        }
    }
}

async fn bind(
    gs: &GlobalShortcuts,
    session: &mut Option<ashpd::desktop::Session<GlobalShortcuts>>,
    wanted: &[(HotkeyId, Chord)],
) -> BindResult {
    if let Some(old) = session.take() {
        let _ = old.close().await;
    }
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let new_session = gs
        .create_session(ashpd::desktop::CreateSessionOptions::default())
        .await
        .map_err(|e| format!("CreateSession failed: {e}"))?;
    let shortcuts: Vec<NewShortcut> = wanted
        .iter()
        .map(|(id, chord)| {
            NewShortcut::new(id.to_string(), id.to_string())
                .preferred_trigger(chord.to_portal_trigger().as_str())
        })
        .collect();
    let request = gs
        .bind_shortcuts(&new_session, &shortcuts, None, ashpd::desktop::global_shortcuts::BindShortcutsOptions::default())
        .await
        .map_err(|e| format!("BindShortcuts failed: {e}"))?;
    let bound = request.response().map_err(|e| format!("the shortcuts were not bound: {e}"))?;
    let triggers = bound
        .shortcuts()
        .iter()
        .map(|s| (s.id().to_owned(), s.trigger_description().to_owned()))
        .collect();
    *session = Some(new_session);
    Ok(triggers)
}
