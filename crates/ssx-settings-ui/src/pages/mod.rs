//! The pages. Each page is a pure `State` (everything that can be decided without a screen,
//! unit tested next to it) and a thin `ui()` function that draws it and feeds edits back into
//! the working copy of the settings.

pub mod about;
pub mod capture;
pub mod general;
pub mod hotkeys;
pub mod shared;
pub mod workflows;

use chrono::{DateTime, FixedOffset};
use std::sync::Arc;

use ssx_core::settings::Settings;

use crate::{
    host::Host,
    nav::Page,
    task::Waker,
    ui_kit::Toasts,
    uploader_registry::Registry,
    validation::Issues,
};

/// What a page gets to work with each frame.
pub struct Cx<'a> {
    /// The working copy being edited.
    pub settings: &'a mut Settings,
    /// The validator's findings for it (as of the start of the frame).
    pub issues: &'a Issues,
    /// The machine.
    pub host: &'a Host,
    /// The upload destinations that exist (as of the start of the frame).
    pub registry: Arc<Registry>,
    /// Wakes the UI from worker threads.
    pub wake: &'a Waker,
    /// Messages to show.
    pub toasts: &'a mut Toasts,
    /// "Now", in the local time zone.
    pub now: DateTime<FixedOffset>,
    /// Set to ask the window to switch page.
    pub go_to: &'a mut Option<Page>,
}

impl std::fmt::Debug for Cx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cx").field("now", &self.now).finish_non_exhaustive()
    }
}

impl Cx<'_> {
    /// Seconds on the UI clock, for toasts.
    pub fn time(&self, ctx: &egui::Context) -> f64 {
        ctx.input(|i| i.time)
    }
}
