//! The [`HotkeyManager`] trait implemented by the in-process backends.
//!
//! In-process backends (the `global-hotkey` crate, the XDG GlobalShortcuts portal) *grab*
//! keys and deliver [`HotkeyEvent`]s. Desktops that offer no such thing (sway, Hyprland, a
//! GNOME without the portal) are served by the binding generators in [`crate::bindings`]
//! instead, which write compositor configuration that runs a command.

use std::{fmt, sync::mpsc};

use crate::{chord::Chord, id::HotkeyId};

/// Press or release of a registered hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyState {
    /// The chord went down.
    Pressed,
    /// The chord was released. **May never arrive**: several GlobalShortcuts portal
    /// implementations do not emit `Deactivated`, so push-to-hold features must not depend
    /// on it there ([`HotkeyManager::reports_release`]).
    Released,
}

/// A registered hotkey fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyEvent {
    /// The id passed to [`HotkeyManager::register`].
    pub id: HotkeyId,
    /// Pressed or released.
    pub state: HotkeyState,
}

/// Which mechanism a manager uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// The `global-hotkey` crate: `RegisterHotKey` on Windows, Carbon on macOS, `XGrabKey`
    /// on X11.
    GlobalHotkey,
    /// `org.freedesktop.portal.GlobalShortcuts`.
    Portal,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackendKind::GlobalHotkey => "global-hotkey",
            BackendKind::Portal => "xdg-portal",
        })
    }
}

/// Errors from hotkey managers.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// The id is already registered.
    #[error("hotkey id {0} is already registered")]
    DuplicateId(HotkeyId),
    /// Another id already uses this chord in this manager.
    #[error("{chord} is already registered as {existing}")]
    DuplicateChord {
        /// The contested chord.
        chord: Chord,
        /// The id that owns it.
        existing: HotkeyId,
    },
    /// Nothing is registered under this id.
    #[error("no hotkey registered as {0}")]
    UnknownId(HotkeyId),
    /// Another application (or the desktop) already owns the key combination.
    #[error("{0} is already grabbed by another application or by the desktop")]
    InUse(Chord),
    /// The backend cannot run in this session.
    #[error("{backend} hotkeys are unavailable: {reason}")]
    Unavailable {
        /// The backend that failed to start.
        backend: BackendKind,
        /// What is missing, phrased as an action.
        reason: String,
    },
    /// The backend rejected the operation.
    #[error("{backend}: {message}")]
    Backend {
        /// The backend.
        backend: BackendKind,
        /// The backend's own explanation.
        message: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, HotkeyError>;

/// Grabs global hotkeys and reports them over a channel.
///
/// Managers are **not** required to be `Send`: on Windows the manager must live on the
/// thread that pumps window messages, and on macOS on the main thread. The receiver from
/// [`HotkeyManager::events`] can be polled from anywhere on that thread's event loop, or
/// drained by a dedicated thread (`recv`/`recv_timeout`) if the manager was created on a
/// thread that already runs one.
pub trait HotkeyManager: fmt::Debug {
    /// The mechanism in use.
    fn backend(&self) -> BackendKind;

    /// Registers `chord` under `id`. Fails without side effects if the id or chord is
    /// taken, or the desktop refuses.
    fn register(&mut self, id: HotkeyId, chord: Chord) -> Result<()>;

    /// Removes a registration.
    fn unregister(&mut self, id: &HotkeyId) -> Result<()>;

    /// The registrations currently active.
    fn registered(&self) -> Vec<(HotkeyId, Chord)>;

    /// Events for registered hotkeys. There is one receiver per manager.
    fn events(&self) -> &mpsc::Receiver<HotkeyEvent>;

    /// Whether [`HotkeyState::Released`] events are delivered on this backend.
    fn reports_release(&self) -> bool;
}
