//! What the daemon tells its user interface (tray, notifications, tooltip).
//!
//! The supervisor and the recording controller do not know what a tray or a notification is:
//! they emit [`UiEvent`]s into a [`UiSink`] and the application layer decides how to show
//! them. That is what makes them testable with a recording sink.

use std::{sync::Mutex, sync::PoisonError, time::Duration};

use ssx_core::{ipc::RunSummary, workflow::NotificationLevel};

/// The recording as the UI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingView {
    /// Nothing is being recorded.
    #[default]
    Idle,
    /// The region overlay (or the compositor's picker) is up; no frames are captured yet.
    Selecting,
    /// Capturing for this long.
    Recording {
        /// Time since the first frame was requested.
        elapsed: Duration,
    },
    /// The user asked to stop; the file is being finalised.
    Stopping,
}

impl RecordingView {
    /// A recording exists in any phase.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Something the UI may want to show.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// A run began executing.
    RunStarted {
        /// Run id.
        run_id: u64,
        /// Workflow display name.
        name: String,
        /// The run needs the user (region overlay, editor): it blocks other interactive runs.
        interactive: bool,
    },
    /// The engine started a step; `text` is a short progress phrase ("Uploading...").
    Step {
        /// Run id.
        run_id: u64,
        /// Progress text.
        text: String,
    },
    /// A run ended.
    RunFinished {
        /// Run id.
        run_id: u64,
        /// Workflow display name.
        name: String,
        /// The result.
        summary: RunSummary,
        /// The workflow itself showed a notification (so the daemon must not repeat it).
        notified: bool,
    },
    /// A run is waiting for a free slot.
    RunQueued {
        /// Run id.
        run_id: u64,
        /// Workflow display name.
        name: String,
    },
    /// The recording changed phase (or, while recording, a second passed).
    Recording(RecordingView),
    /// A message for the user that no workflow produced (busy, rejected, started recording).
    Notice {
        /// Prominence.
        level: NotificationLevel,
        /// Headline.
        title: String,
        /// Details.
        body: String,
    },
}

/// Receives [`UiEvent`]s. Called from any thread; must not block.
pub trait UiSink: Send + Sync {
    /// Handles one event.
    fn emit(&self, event: UiEvent);
}

/// Discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullUi;

impl UiSink for NullUi {
    fn emit(&self, _: UiEvent) {}
}

/// Keeps every event (tests).
#[derive(Debug, Default)]
pub struct CollectingUi(Mutex<Vec<UiEvent>>);

impl CollectingUi {
    /// An empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of everything received so far.
    pub fn events(&self) -> Vec<UiEvent> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Removes and returns everything received so far.
    pub fn take(&self) -> Vec<UiEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl UiSink for CollectingUi {
    fn emit(&self, event: UiEvent) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(event);
    }
}

impl<T: UiSink + ?Sized> UiSink for std::sync::Arc<T> {
    fn emit(&self, event: UiEvent) {
        (**self).emit(event);
    }
}
