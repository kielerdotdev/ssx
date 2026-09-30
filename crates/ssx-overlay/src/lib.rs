//! Region-selection overlay.
#![deny(unsafe_code)]

pub mod app;
pub mod backend;
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
