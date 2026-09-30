//! Backend built on the [`global-hotkey`](https://docs.rs/global-hotkey) crate.
//!
//! * **X11**: the crate runs its own thread with its own X connection (`XGrabKey` on the
//!   root window), so no event loop is needed from us. It connects using `$DISPLAY` inside
//!   that thread and only *logs* connection failures, so [`GlobalHotkeys::new`] checks
//!   `$DISPLAY` up front. Grabs made through XWayland only fire while an XWayland window has
//!   focus, so this backend must not be picked on Wayland sessions
//!   ([`crate::detect`] never does).
//! * **Windows**: `RegisterHotKey`. The manager must be created on a thread that runs a
//!   Win32 message loop, and events are only produced while that loop pumps messages.
//! * **macOS**: Carbon hotkeys, created on the main thread while the main run loop runs.
//!
//! The crate reports events on a process-wide channel. This wrapper drains it on a small
//! forwarding thread, translates crate ids to [`HotkeyId`]s and delivers
//! [`HotkeyEvent`]s on a per-manager channel. Because the source channel is global, create
//! **at most one** `GlobalHotkeys` per process.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};

use crate::{
    chord::Chord,
    id::HotkeyId,
    manager::{BackendKind, HotkeyError, HotkeyEvent, HotkeyManager, HotkeyState, Result},
};

/// A [`HotkeyManager`] using the `global-hotkey` crate.
pub struct GlobalHotkeys {
    inner: GlobalHotKeyManager,
    entries: Vec<Entry>,
    by_crate_id: Arc<Mutex<HashMap<u32, HotkeyId>>>,
    events: mpsc::Receiver<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    forwarder: Option<JoinHandle<()>>,
}

struct Entry {
    id: HotkeyId,
    chord: Chord,
}

impl fmt::Debug for GlobalHotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalHotkeys")
            .field("registered", &self.entries.iter().map(|e| e.chord.to_string()).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn unavailable(reason: impl Into<String>) -> HotkeyError {
    HotkeyError::Unavailable { backend: BackendKind::GlobalHotkey, reason: reason.into() }
}

impl GlobalHotkeys {
    /// Creates the manager. See the module docs for per-OS threading requirements.
    pub fn new() -> Result<Self> {
        #[cfg(all(unix, not(target_vendor = "apple")))]
        if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
            return Err(unavailable(
                "DISPLAY is not set, so there is no X server to grab keys on (Wayland sessions \
                 need the GlobalShortcuts portal or compositor bindings instead)",
            ));
        }
        let inner = GlobalHotKeyManager::new().map_err(|e| unavailable(e.to_string()))?;
        let by_crate_id: Arc<Mutex<HashMap<u32, HotkeyId>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();
        let forwarder = {
            let (map, stop) = (Arc::clone(&by_crate_id), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("ssx-hotkey-events".into())
                .spawn(move || forward_events(&map, &stop, &tx))
                .map_err(|e| unavailable(format!("cannot start the event thread: {e}")))?
        };
        Ok(Self { inner, entries: Vec::new(), by_crate_id, events, stop, forwarder: Some(forwarder) })
    }

    fn lock_map(&self) -> std::sync::MutexGuard<'_, HashMap<u32, HotkeyId>> {
        self.by_crate_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn forward_events(
    map: &Mutex<HashMap<u32, HotkeyId>>,
    stop: &AtomicBool,
    tx: &mpsc::Sender<HotkeyEvent>,
) {
    let source = GlobalHotKeyEvent::receiver();
    while !stop.load(Ordering::Relaxed) {
        match source.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => {
                let id = map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&ev.id)
                    .cloned();
                let Some(id) = id else { continue };
                let state = match ev.state {
                    global_hotkey::HotKeyState::Pressed => HotkeyState::Pressed,
                    global_hotkey::HotKeyState::Released => HotkeyState::Released,
                };
                if tx.send(HotkeyEvent { id, state }).is_err() {
                    break; // manager dropped
                }
            }
            Err(e) if e.is_timeout() => {}
            Err(_) => break, // channel closed
        }
    }
}

impl HotkeyManager for GlobalHotkeys {
    fn backend(&self) -> BackendKind {
        BackendKind::GlobalHotkey
    }

    fn register(&mut self, id: HotkeyId, chord: Chord) -> Result<()> {
        if self.entries.iter().any(|e| e.id == id) {
            return Err(HotkeyError::DuplicateId(id));
        }
        if let Some(e) = self.entries.iter().find(|e| e.chord == chord) {
            return Err(HotkeyError::DuplicateChord { chord, existing: e.id.clone() });
        }
        let hotkey = chord.to_hotkey();
        // Map first: on Windows an event can arrive before `register` returns.
        self.lock_map().insert(hotkey.id(), id.clone());
        if let Err(e) = self.inner.register(hotkey) {
            self.lock_map().remove(&hotkey.id());
            return Err(match e {
                global_hotkey::Error::AlreadyRegistered(_) => HotkeyError::InUse(chord),
                other => HotkeyError::Backend {
                    backend: BackendKind::GlobalHotkey,
                    message: other.to_string(),
                },
            });
        }
        self.entries.push(Entry { id, chord });
        Ok(())
    }

    fn unregister(&mut self, id: &HotkeyId) -> Result<()> {
        let pos = self
            .entries
            .iter()
            .position(|e| &e.id == id)
            .ok_or_else(|| HotkeyError::UnknownId(id.clone()))?;
        let hotkey = self.entries[pos].chord.to_hotkey();
        self.inner.unregister(hotkey).map_err(|e| HotkeyError::Backend {
            backend: BackendKind::GlobalHotkey,
            message: e.to_string(),
        })?;
        self.lock_map().remove(&hotkey.id());
        self.entries.remove(pos);
        Ok(())
    }

    fn registered(&self) -> Vec<(HotkeyId, Chord)> {
        self.entries.iter().map(|e| (e.id.clone(), e.chord)).collect()
    }

    fn events(&self) -> &mpsc::Receiver<HotkeyEvent> {
        &self.events
    }

    fn reports_release(&self) -> bool {
        true
    }
}

impl Drop for GlobalHotkeys {
    fn drop(&mut self) {
        for e in &self.entries {
            let _ = self.inner.unregister(e.chord.to_hotkey());
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.forwarder.take() {
            let _ = t.join();
        }
    }
}
