//! Input events fed to the [`SelectionModel`](super::SelectionModel).
//!
//! Backends translate native events into these. Positions are **virtual-desktop physical
//! pixels**; the backend does the surface-to-desktop mapping (`crate::mapping`).

use ssx_types::Point;

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// Primary button.
    Left,
    /// Secondary button (cancel / clear).
    Right,
    /// Middle button (ignored).
    Middle,
}

/// A pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvent {
    /// The pointer moved (or entered) to `pos`.
    Move {
        /// Desktop position.
        pos: Point,
    },
    /// A button went down. `time_ms` is a monotonic timestamp used for double-click
    /// detection (any epoch; only differences matter).
    Down {
        /// Desktop position.
        pos: Point,
        /// Which button.
        button: PointerButton,
        /// Monotonic milliseconds.
        time_ms: u64,
    },
    /// A button was released.
    Up {
        /// Desktop position.
        pos: Point,
        /// Which button.
        button: PointerButton,
    },
    /// Scroll wheel; positive scrolls up (zoom in).
    Wheel {
        /// Notches, positive = up.
        delta: i32,
    },
    /// The pointer left the overlay.
    Leave,
}

/// A logical key. Letters are matched by physical position on the backend where the
/// platform gives no layout-aware symbol (see the Wayland backend notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Cancel.
    Escape,
    /// Confirm.
    Enter,
    /// Cycle monitors.
    Tab,
    /// Move-while-dragging.
    Space,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Shift (either).
    Shift,
    /// Control (either).
    Control,
    /// Alt (either).
    Alt,
    /// A lower-case letter or digit.
    Char(char),
    /// Anything else.
    Other,
}

/// A key press or release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// The key.
    pub key: Key,
    /// `true` on press (including auto-repeat), `false` on release.
    pub pressed: bool,
}

/// Modifier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift held.
    pub shift: bool,
    /// Ctrl held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
}

/// Any input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Pointer event.
    Pointer(PointerEvent),
    /// Key event.
    Key(KeyEvent),
    /// Authoritative modifier state (e.g. from an X11 state mask).
    Modifiers(Modifiers),
}
