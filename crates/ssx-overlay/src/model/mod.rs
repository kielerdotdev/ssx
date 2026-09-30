//! The pure, windowing-free heart of the overlay.
//!
//! * [`events`]: input events (desktop pixel coordinates).
//! * [`state`]: [`SelectionModel`], the interaction state machine.
//! * [`scene`]: [`Scene`], what a frame shows, plus layout helpers (labels, loupe).
//! * [`damage`]: dirty rectangles between two scenes.
//! * [`geometry`]: handles, snapping, rectangle subtraction, shape masks.
//!
//! Nothing in here touches a window, a clock or a pixel buffer, so all of it is unit and
//! property tested headlessly.

pub mod damage;
pub mod events;
pub mod geometry;
pub mod scene;
pub mod state;

pub use events::{InputEvent, Key, KeyEvent, Modifiers, PointerButton, PointerEvent};
pub use scene::{CursorHint, Scene};
pub use state::{Finish, FinishShape, ModelConfig, SelectionModel};

#[cfg(test)]
mod tests;
