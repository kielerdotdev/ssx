//! Region-selection overlay: the ShareX-style frozen-screen crosshair you drag a rectangle on.
//!
//! * [`select`] shows the overlay in this process; [`select_via_helper`] runs the same code in
//!   the `ssx-overlay` helper process (recommended: isolates crashes and windowing quirks
//!   from the caller's event loop). Both take an [`OverlayInput`] and return an
//!   [`OverlayOutcome`] in virtual-desktop physical pixels.
//! * [`model`] is the pure state machine (no windowing), [`render`] the CPU renderer,
//!   [`backend`] the per-platform windows. See the crate README for the architecture, the
//!   interaction table, what was verified where, and manual test checklists.
#![deny(unsafe_code)]
// Pixel and coordinate maths converts between u32 sizes and i32/i64 positions all the time;
// every value is bounded by screen dimensions (< 2^17) long before a wrap could happen.
#![allow(clippy::cast_possible_wrap)]

pub mod app;
pub mod backend;
pub mod demo;
pub mod error;
pub mod helper;
pub mod mapping;
pub mod model;
pub mod protocol;
pub mod render;
pub mod runner;
pub mod shm;
pub mod types;

pub use error::{HelperError, OverlayError};
pub use helper::{select_via_helper, select_via_helper_timed};
pub use types::*;

/// Shows the overlay in this process and blocks until the user finishes.
///
/// This is the same code path the `ssx-overlay` helper runs.
pub fn select(input: OverlayInput) -> Result<OverlayOutcome, OverlayError> {
    let mut app = app::OverlayApp::new(input)?;
    backend::run(&mut app)?;
    Ok(app.outcome().unwrap_or(OverlayOutcome::Cancelled))
}
