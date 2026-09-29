//! Progress events streamed to the UI while a run executes.

use std::{sync::Mutex, sync::PoisonError, time::Duration};

use super::{Outcome, StepKind, StepStatus};

/// Something that happened during a run.
///
/// Ordering guarantees (per item): `RunStarted` first, `RunFinished` last; every
/// `StepStarted` is followed later by the `StepFinished` of the same step; steps that are
/// skipped without running produce only a `StepFinished`. Events of different items
/// interleave when a multi-file post runs in parallel.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The run began.
    RunStarted {
        /// Workflow id.
        workflow_id: String,
        /// Workflow display name.
        workflow_name: String,
    },
    /// A step is about to run.
    StepStarted {
        /// Item index; `None` for run-level steps.
        item: Option<usize>,
        /// The step.
        step: StepKind,
    },
    /// Byte progress of a running step (uploads).
    StepProgress {
        /// Item index.
        item: Option<usize>,
        /// The step.
        step: StepKind,
        /// Bytes done.
        done: u64,
        /// Total bytes if known.
        total: Option<u64>,
    },
    /// A step ended (or was skipped).
    StepFinished {
        /// Item index.
        item: Option<usize>,
        /// The step.
        step: StepKind,
        /// Result.
        status: StepStatus,
        /// Duration.
        duration: Duration,
    },
    /// The run ended.
    RunFinished {
        /// Overall outcome.
        outcome: Outcome,
    },
}

/// Receives [`Event`]s. Called from the engine's threads, possibly concurrently, so it must
/// be quick and must not block (forward to a channel and return).
pub trait EventSink: Send + Sync {
    /// Handles one event.
    fn event(&self, event: Event);
}

/// Discards all events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn event(&self, _: Event) {}
}

/// Collects events in memory (tests, logging).
#[derive(Debug, Default)]
pub struct CollectingSink(Mutex<Vec<Event>>);

impl CollectingSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of everything received so far.
    pub fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl EventSink for CollectingSink {
    fn event(&self, event: Event) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(event);
    }
}

impl<F: Fn(Event) + Send + Sync> EventSink for F {
    fn event(&self, event: Event) {
        self(event);
    }
}
