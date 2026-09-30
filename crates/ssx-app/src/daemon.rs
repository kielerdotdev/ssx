//! The supervisor: admits, runs, cancels and finishes workflow runs.
//!
//! Everything that starts work (hotkeys, the tray, IPC, the `PostFiles` coalescer) calls
//! [`Supervisor::submit`]. The supervisor decides, with a pure function ([`decide`]) over a
//! snapshot of its state, whether the run may start now, must wait, must be refused, or (for a
//! recording workflow while a recording exists) means *stop the recording*:
//!
//! | class | rule |
//! |---|---|
//! | regular (fullscreen, files, clipboard, ...) | up to `max_concurrent` at once, the rest **queue** in order |
//! | interactive (region overlay, editor, save dialog) | **one at a time**: a second one is *refused* with a message, never queued behind a window that may stay open for minutes |
//! | recording | **one at a time**; the same hotkey *toggles* (stops it); needs the overlay, so it is refused while another interactive run is open |
//! | anything, while shutting down | refused |
//!
//! The supervisor never touches the screen, the network or the file system: work is done by an
//! injected [`JobRunner`] (the real one drives `ssx_core::workflow::Engine`), progress goes to an
//! injected [`UiSink`](crate::events::UiSink), time comes from an injected
//! [`Clock`](crate::clock::Clock). That is what lets the tests below run whole scenarios
//! (queueing, refusal, cancellation, shutdown with a recording in flight) deterministically.
//!
//! One thread per run (bounded by the admission rules), no polling: an idle daemon owns no
//! running worker.

use std::{
    collections::{BTreeMap, VecDeque},
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use ssx_core::{
    ipc::{
        ActiveRunInfo, ErrorCode, ItemSummary, RecordAudio, RecordTarget, RegionMode, RunSummary,
    },
    settings::{AfterCapture, InputKind, Workflow},
    workflow::{CancelToken, NotificationLevel, Outcome},
};

use crate::{
    clock::Clock,
    events::{UiEvent, UiSink},
    ids::RunIds,
    recording::{RecordingBusy, RecordingController, StopError},
};

/// Where a request came from. Requests from a person (hotkey, tray) get their refusals as
/// notifications; IPC callers get them as error responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A global hotkey.
    Hotkey,
    /// The tray menu.
    Tray,
    /// The IPC server (CLI, shell shims, `ssx-app` relaunch).
    Ipc,
}

/// What a recording captures and hears (see [`ssx_core::ipc::RecordSpec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordPlanSpec {
    /// The picture.
    pub target: RecordTarget,
    /// The sound; `None` keeps the configured default.
    pub audio: Option<RecordAudio>,
}

impl Default for RecordPlanSpec {
    fn default() -> Self {
        Self { target: RecordTarget::Interactive, audio: None }
    }
}

/// What a run does.
#[derive(Debug, Clone, PartialEq)]
pub enum JobSpec {
    /// A workflow started by name, hotkey or capture request.
    Workflow {
        /// The workflow (a copy of the settings entry).
        workflow: Workflow,
        /// Capture delay override.
        delay_ms: Option<u32>,
        /// How the region overlay selects.
        mode: Option<RegionMode>,
    },
    /// A workflow over files.
    Files {
        /// The workflow.
        workflow: Workflow,
        /// The paths, deduplicated, in order.
        paths: Vec<PathBuf>,
        /// Open images in the editor first.
        edit_first: bool,
    },
    /// A recording workflow.
    Record {
        /// The workflow.
        workflow: Workflow,
        /// What to record.
        plan: RecordPlanSpec,
        /// Stop the recording instead when one exists (hotkey and tray behaviour); when false
        /// a running recording refuses the request.
        toggle: bool,
    },
}

impl JobSpec {
    /// The workflow.
    pub fn workflow(&self) -> &Workflow {
        match self {
            Self::Workflow { workflow, .. }
            | Self::Files { workflow, .. }
            | Self::Record { workflow, .. } => workflow,
        }
    }

    /// The admission class.
    pub fn class(&self) -> Class {
        match self {
            Self::Record { workflow, toggle, .. } => {
                Class::Record { toggle: *toggle, workflow_id: workflow.id.clone() }
            }
            Self::Files { workflow, edit_first, .. } => {
                if *edit_first || workflow_is_interactive(workflow) {
                    Class::Interactive
                } else {
                    Class::Regular
                }
            }
            Self::Workflow { workflow, .. } => {
                if workflow_is_interactive(workflow) {
                    Class::Interactive
                } else {
                    Class::Regular
                }
            }
        }
    }
}

/// `true` when a run of `wf` may put a window in front of the user and wait for them.
pub fn workflow_is_interactive(wf: &Workflow) -> bool {
    wf.input == InputKind::CaptureRegion
        || wf.after_capture.contains(&AfterCapture::OpenEditor)
        || wf.after_capture.contains(&AfterCapture::SaveAsDialog)
}

/// Admission class of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// Runs alongside others, queues when the daemon is busy.
    Regular,
    /// Needs the user; one at a time.
    Interactive,
    /// A recording.
    Record {
        /// Same-workflow requests stop a running recording.
        toggle: bool,
        /// The workflow id.
        workflow_id: String,
    },
}

/// A request to run something.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRequest {
    /// Use this id instead of drawing one (the coalescer allocates a batch's id when the
    /// batch opens, so the first caller could already be told).
    pub run_id: Option<u64>,
    /// Who asks.
    pub origin: Origin,
    /// What to do.
    pub spec: JobSpec,
}

/// A run, as the runner receives it.
#[derive(Debug, Clone)]
pub struct Job {
    /// Run id.
    pub run_id: u64,
    /// Workflow display name.
    pub name: String,
    /// What to do.
    pub spec: JobSpec,
    /// Aborts the run (discarding an unfinished recording).
    pub cancel: CancelToken,
    /// Ends a recording gracefully, keeping the file.
    pub stop: CancelToken,
}

/// What a runner returns.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutput {
    /// The result.
    pub summary: RunSummary,
    /// The workflow showed its own notification.
    pub notified: bool,
}

/// Does the actual work of a run. Blocks until the run is done.
pub trait JobRunner: Send + Sync + 'static {
    /// Runs `job`; progress goes to `ui`. Must return promptly once `job.cancel` fires.
    fn run(&self, job: &Job, ui: &dyn UiSink) -> RunOutput;
}

/// Why a request was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// Something else is in the way (message says what and how to get past it).
    #[error("{0}")]
    Busy(String),
    /// The daemon is exiting.
    #[error("ssx is shutting down")]
    ShuttingDown,
    /// Nothing to act on.
    #[error("{0}")]
    NotRunning(String),
    /// The request itself is wrong.
    #[error("{0}")]
    Invalid(String),
    /// No workflow matches the reference.
    #[error("{0}")]
    UnknownWorkflow(String),
}

impl Rejection {
    /// The IPC error code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Busy(_) | Self::ShuttingDown => ErrorCode::Busy,
            Self::NotRunning(_) => ErrorCode::NotRunning,
            Self::Invalid(_) => ErrorCode::InvalidRequest,
            Self::UnknownWorkflow(_) => ErrorCode::UnknownWorkflow,
        }
    }
}

/// What happened to an accepted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// The run started.
    Started {
        /// Its id.
        run_id: u64,
    },
    /// The run waits for a free slot.
    Queued {
        /// Its id.
        run_id: u64,
    },
    /// The request was a toggle and a recording existed: it is stopping.
    StoppedRecording {
        /// The recording's run id.
        run_id: u64,
    },
}

impl Submitted {
    /// The run id the caller should follow.
    pub fn run_id(self) -> u64 {
        match self {
            Self::Started { run_id }
            | Self::Queued { run_id }
            | Self::StoppedRecording { run_id } => run_id,
        }
    }
}

/// The facts [`decide`] needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdmissionState {
    /// Regular runs executing.
    pub active_regular: usize,
    /// Interactive runs executing (a recording that is still choosing its region counts).
    pub active_interactive: usize,
    /// The recording, if any: `(run id, workflow id)`.
    pub recording: Option<(u64, String)>,
    /// The daemon is exiting.
    pub shutting_down: bool,
}

/// The admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Start now.
    Start,
    /// Wait for a slot.
    Queue,
    /// Stop the running recording.
    ToggleStop {
        /// The recording's run.
        run_id: u64,
    },
    /// Refuse.
    Reject(Rejection),
}

/// The pure admission rules. See the module docs.
pub fn decide(state: &AdmissionState, class: &Class, max_concurrent: usize) -> Decision {
    if state.shutting_down {
        return Decision::Reject(Rejection::ShuttingDown);
    }
    match class {
        Class::Regular => {
            if state.active_regular < max_concurrent {
                Decision::Start
            } else {
                Decision::Queue
            }
        }
        Class::Interactive => {
            if state.active_interactive > 0 {
                Decision::Reject(Rejection::Busy(
                    "another capture is still waiting for you; finish it (Enter or Esc on the \
                     overlay) or cancel it from the tray menu first"
                        .to_owned(),
                ))
            } else {
                Decision::Start
            }
        }
        Class::Record { toggle, workflow_id } => match &state.recording {
            Some((run_id, running)) if *toggle && running == workflow_id => {
                Decision::ToggleStop { run_id: *run_id }
            }
            Some((_, running)) => Decision::Reject(Rejection::Busy(format!(
                "a recording is already running ({running}); stop it first{}",
                if *toggle { " (use its own hotkey or the tray menu)" } else { "" }
            ))),
            None if state.active_interactive > 0 => Decision::Reject(Rejection::Busy(
                "finish or cancel the capture that is open before starting a recording".to_owned(),
            )),
            None => Decision::Start,
        },
    }
}

/// Limits of the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Regular runs executing at once.
    pub max_concurrent: usize,
    /// Finished runs remembered for `WaitRun`.
    pub remember_finished: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_concurrent: 4, remember_finished: 64 }
    }
}

/// How long the supervisor gives things at shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownGrace {
    /// A recording gets this long to finalise its file.
    pub recording: Duration,
    /// Running uploads and the like get this long before being cancelled.
    pub runs: Duration,
    /// After cancelling, wait this long for the threads to unwind.
    pub cancelled: Duration,
}

impl Default for ShutdownGrace {
    fn default() -> Self {
        Self {
            recording: Duration::from_secs(15),
            runs: Duration::from_secs(5),
            cancelled: Duration::from_secs(5),
        }
    }
}

/// The result of waiting for a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Waited {
    /// It finished.
    Finished(RunSummary),
    /// Still running after the timeout.
    TimedOut,
    /// No such run (never existed, or forgotten).
    Unknown,
}

/// Current load, for status displays.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadStatus {
    /// Runs executing.
    pub active: Vec<ActiveRunInfo>,
    /// Runs waiting.
    pub queued: usize,
    /// Something interactive is open.
    pub interactive: bool,
}

struct ActiveRun {
    name: String,
    class: Class,
    started: Duration,
    cancel: CancelToken,
}

impl ActiveRun {
    fn interactive(&self) -> bool {
        matches!(self.class, Class::Interactive)
    }
}

#[derive(Default)]
struct Inner {
    active: BTreeMap<u64, ActiveRun>,
    queue: VecDeque<Job>,
    finished: VecDeque<(u64, RunSummary)>,
    shutting_down: bool,
}

/// The supervisor. See the module docs.
pub struct Supervisor {
    inner: Mutex<Inner>,
    changed: Condvar,
    runner: Arc<dyn JobRunner>,
    ui: Arc<dyn UiSink>,
    clock: Arc<dyn Clock>,
    ids: RunIds,
    recording: Arc<RecordingController>,
    limits: Limits,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").field("limits", &self.limits).finish_non_exhaustive()
    }
}

/// A summary for a run that did not execute (cancelled while queued, panicked, ...).
pub fn plain_summary(run_id: u64, outcome: Outcome, message: impl Into<String>) -> RunSummary {
    RunSummary { run_id, outcome, message: message.into(), items: Vec::new() }
}

impl Supervisor {
    /// Builds a supervisor. `recording` must be the controller shared with the
    /// [`TrackedRecorder`](crate::recording::TrackedRecorder) the runner's services use.
    pub fn new(
        runner: Arc<dyn JobRunner>,
        ui: Arc<dyn UiSink>,
        clock: Arc<dyn Clock>,
        ids: RunIds,
        recording: Arc<RecordingController>,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            runner,
            ui,
            clock,
            ids,
            recording,
            limits,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The recording controller.
    pub fn recording(&self) -> &Arc<RecordingController> {
        &self.recording
    }

    fn admission(&self, inner: &Inner) -> AdmissionState {
        AdmissionState {
            active_regular: inner.active.values().filter(|a| a.class == Class::Regular).count(),
            active_interactive: inner.active.values().filter(|a| a.interactive()).count()
                + usize::from(matches!(
                    self.recording.view(),
                    crate::events::RecordingView::Selecting
                )),
            recording: self.recording.run_id().zip(self.recording.workflow_id()),
            shutting_down: inner.shutting_down,
        }
    }

    /// Submits a request. See the module docs for the rules.
    pub fn submit(self: &Arc<Self>, req: JobRequest) -> Result<Submitted, Rejection> {
        let class = req.spec.class();
        let name = req.spec.workflow().name.clone();
        let mut inner = self.lock();
        let decision = decide(&self.admission(&inner), &class, self.limits.max_concurrent);
        let outcome = match decision {
            Decision::Reject(r) => {
                drop(inner);
                if req.origin != Origin::Ipc {
                    self.ui.emit(UiEvent::Notice {
                        level: NotificationLevel::Warning,
                        title: "ssx is busy".to_owned(),
                        body: r.to_string(),
                    });
                }
                return Err(r);
            }
            Decision::ToggleStop { run_id } => {
                drop(inner);
                return match self.recording.stop() {
                    Ok(()) => Ok(Submitted::StoppedRecording { run_id }),
                    Err(StopError::NotRunning) => {
                        Err(Rejection::NotRunning("no recording is running".to_owned()))
                    }
                };
            }
            Decision::Start | Decision::Queue => decision,
        };
        let run_id = req.run_id.unwrap_or_else(|| self.ids.next_id());
        let job = Job {
            run_id,
            name: name.clone(),
            spec: req.spec,
            cancel: CancelToken::new(),
            stop: CancelToken::new(),
        };
        if outcome == Decision::Queue {
            inner.queue.push_back(job);
            drop(inner);
            self.ui.emit(UiEvent::RunQueued { run_id, name });
            return Ok(Submitted::Queued { run_id });
        }
        if let Class::Record { .. } = &class {
            let claimed = self.recording.begin(
                run_id,
                &job.spec.workflow().id,
                job.cancel.clone(),
                job.stop.clone(),
            );
            if let Err(RecordingBusy { workflow_id, .. }) = claimed {
                return Err(Rejection::Busy(format!(
                    "a recording is already running ({workflow_id}); stop it first"
                )));
            }
        }
        inner.active.insert(
            run_id,
            ActiveRun { name, class, started: self.clock.now(), cancel: job.cancel.clone() },
        );
        drop(inner);
        self.spawn(job);
        Ok(Submitted::Started { run_id })
    }

    fn spawn(self: &Arc<Self>, job: Job) {
        let me = Arc::clone(self);
        let run_id = job.run_id;
        let spawned = std::thread::Builder::new()
            .name(format!("ssx-run-{run_id}"))
            .spawn(move || me.execute(&job));
        if let Err(e) = spawned {
            tracing::error!(run_id, "cannot start a worker thread: {e}");
            self.finish(
                run_id,
                "?",
                RunOutput {
                    summary: plain_summary(
                        run_id,
                        Outcome::Failed,
                        format!("cannot start a worker thread: {e}"),
                    ),
                    notified: false,
                },
            );
        }
    }

    fn execute(self: &Arc<Self>, job: &Job) {
        let interactive = matches!(job.spec.class(), Class::Interactive | Class::Record { .. });
        self.ui.emit(UiEvent::RunStarted {
            run_id: job.run_id,
            name: job.name.clone(),
            interactive,
        });
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.runner.run(job, &*self.ui)));
        let output = result.unwrap_or_else(|payload| {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            tracing::error!(run_id = job.run_id, "workflow run panicked: {what}");
            RunOutput {
                summary: plain_summary(
                    job.run_id,
                    Outcome::Failed,
                    format!("ssx hit an internal error while running this workflow ({what}); the daemon is still running"),
                ),
                notified: false,
            }
        });
        self.finish(job.run_id, &job.name, output);
    }

    fn finish(self: &Arc<Self>, run_id: u64, name: &str, output: RunOutput) {
        self.recording.run_ended(run_id);
        {
            let mut inner = self.lock();
            inner.active.remove(&run_id);
            inner.finished.push_back((run_id, output.summary.clone()));
            while inner.finished.len() > self.limits.remember_finished {
                inner.finished.pop_front();
            }
        }
        self.changed.notify_all();
        self.ui.emit(UiEvent::RunFinished {
            run_id,
            name: name.to_owned(),
            summary: output.summary,
            notified: output.notified,
        });
        self.pump();
    }

    /// Starts queued runs while there is capacity.
    fn pump(self: &Arc<Self>) {
        loop {
            let job = {
                let mut inner = self.lock();
                if inner.shutting_down {
                    return;
                }
                let regular = inner.active.values().filter(|a| a.class == Class::Regular).count();
                if regular >= self.limits.max_concurrent {
                    return;
                }
                let Some(job) = inner.queue.pop_front() else { return };
                inner.active.insert(
                    job.run_id,
                    ActiveRun {
                        name: job.name.clone(),
                        class: job.spec.class(),
                        started: self.clock.now(),
                        cancel: job.cancel.clone(),
                    },
                );
                job
            };
            self.spawn(job);
        }
    }

    /// Cancels one run (aborts; an unfinished recording is discarded). Also removes a queued
    /// run. `true` if something was cancelled.
    pub fn cancel(self: &Arc<Self>, run_id: u64) -> bool {
        let queued = {
            let mut inner = self.lock();
            if let Some(a) = inner.active.get(&run_id) {
                a.cancel.cancel();
                return true;
            }
            inner.queue.iter().position(|j| j.run_id == run_id).and_then(|i| inner.queue.remove(i))
        };
        let Some(job) = queued else { return false };
        let output = RunOutput {
            summary: plain_summary(run_id, Outcome::Cancelled, "Cancelled"),
            notified: true,
        };
        {
            let mut inner = self.lock();
            inner.finished.push_back((run_id, output.summary.clone()));
        }
        self.changed.notify_all();
        self.ui.emit(UiEvent::RunFinished {
            run_id,
            name: job.name,
            summary: output.summary,
            notified: true,
        });
        true
    }

    /// The tray's Cancel: aborts every run the user is looking at (overlay, editor, region
    /// selection of a recording). Returns how many runs were cancelled. A recording that is
    /// already capturing is *not* touched (that is what Stop is for).
    pub fn cancel_interactive(&self) -> usize {
        let inner = self.lock();
        let mut n = 0;
        for (id, a) in &inner.active {
            let selecting_recording = matches!(a.class, Class::Record { .. })
                && self.recording.run_id() == Some(*id)
                && matches!(self.recording.view(), crate::events::RecordingView::Selecting);
            if a.interactive() || selecting_recording {
                a.cancel.cancel();
                n += 1;
            }
        }
        n
    }

    /// Cancels everything that runs or waits (used at shutdown).
    pub fn cancel_all(self: &Arc<Self>) {
        let ids: Vec<u64> = {
            let inner = self.lock();
            inner.active.keys().copied().chain(inner.queue.iter().map(|j| j.run_id)).collect()
        };
        for id in ids {
            self.cancel(id);
        }
    }

    /// Stops the running recording gracefully (the file is kept and the workflow continues).
    pub fn stop_recording(&self) -> Result<(), Rejection> {
        self.recording.stop().map_err(|StopError::NotRunning| {
            Rejection::NotRunning("no recording is running".to_owned())
        })
    }

    /// Waits for a run to finish. `None` waits without limit.
    pub fn wait(&self, run_id: u64, timeout: Option<Duration>) -> Waited {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut inner = self.lock();
        loop {
            if let Some((_, s)) = inner.finished.iter().find(|(id, _)| *id == run_id) {
                return Waited::Finished(s.clone());
            }
            let known = inner.active.contains_key(&run_id)
                || inner.queue.iter().any(|j| j.run_id == run_id);
            if !known {
                return Waited::Unknown;
            }
            inner = match deadline {
                None => self.changed.wait(inner).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let Some(left) = d.checked_duration_since(Instant::now()) else {
                        return Waited::TimedOut;
                    };
                    self.changed.wait_timeout(inner, left).unwrap_or_else(PoisonError::into_inner).0
                }
            };
        }
    }

    /// The current load.
    pub fn load(&self) -> LoadStatus {
        let inner = self.lock();
        let now = self.clock.now();
        LoadStatus {
            active: inner
                .active
                .iter()
                .map(|(id, a)| ActiveRunInfo {
                    run_id: *id,
                    name: a.name.clone(),
                    running_secs: now.saturating_sub(a.started).as_secs(),
                })
                .collect(),
            queued: inner.queue.len(),
            interactive: inner.active.values().any(ActiveRun::interactive),
        }
    }

    /// `true` when nothing runs or waits.
    pub fn is_idle(&self) -> bool {
        let inner = self.lock();
        inner.active.is_empty() && inner.queue.is_empty()
    }

    fn wait_no_active(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        while !inner.active.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            inner =
                self.changed.wait_timeout(inner, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }

    /// Winds everything down: refuses new work, drops the queue, stops a recording
    /// gracefully (so its file is finalised, never left corrupt), gives running runs a
    /// moment, then cancels what is left. Returns `true` if everything ended in time.
    pub fn shutdown(self: &Arc<Self>, grace: ShutdownGrace) -> bool {
        let queued: Vec<u64> = {
            let mut inner = self.lock();
            inner.shutting_down = true;
            inner.queue.iter().map(|j| j.run_id).collect()
        };
        for id in queued {
            self.cancel(id);
        }
        let mut clean = true;
        if self.recording.stop().is_ok() && !self.recording.wait_idle(grace.recording) {
            tracing::error!("the recording did not finish within {:?}", grace.recording);
            clean = false;
        }
        if !self.wait_no_active(grace.runs) {
            tracing::warn!("cancelling runs that are still active at shutdown");
            self.cancel_all();
            clean &= self.wait_no_active(grace.cancelled);
        }
        clean
    }
}

/// Builds the `ItemSummary` list of a finished report (shared by the real runner and tests).
pub fn items_of(
    paths_urls_errors: impl IntoIterator<Item = (Option<PathBuf>, Option<String>, Option<String>)>,
) -> Vec<ItemSummary> {
    paths_urls_errors
        .into_iter()
        .map(|(path, url, error)| ItemSummary { path, url, error })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, Sender};

    use super::*;
    use crate::{
        clock::FakeClock,
        events::{CollectingUi, RecordingView},
    };

    fn wf(id: &str, input: InputKind, after: &[AfterCapture]) -> Workflow {
        Workflow {
            id: id.into(),
            name: format!("Workflow {id}"),
            input,
            after_capture: after.to_vec(),
            ..Workflow::default()
        }
    }

    fn shot(id: &str) -> Workflow {
        wf(id, InputKind::CaptureFullscreen, &[AfterCapture::SaveToFile])
    }
    fn region(id: &str) -> Workflow {
        wf(id, InputKind::CaptureRegion, &[AfterCapture::SaveToFile])
    }
    fn record(id: &str) -> Workflow {
        wf(id, InputKind::RecordScreen, &[AfterCapture::Upload])
    }

    fn req(spec: JobSpec) -> JobRequest {
        JobRequest { run_id: None, origin: Origin::Ipc, spec }
    }
    fn run(w: Workflow) -> JobRequest {
        req(JobSpec::Workflow { workflow: w, delay_ms: None, mode: None })
    }
    fn files(w: Workflow, paths: &[&str]) -> JobRequest {
        req(JobSpec::Files {
            workflow: w,
            paths: paths.iter().map(PathBuf::from).collect(),
            edit_first: false,
        })
    }
    fn rec(w: Workflow, toggle: bool) -> JobRequest {
        req(JobSpec::Record { workflow: w, plan: RecordPlanSpec::default(), toggle })
    }

    // ---- the pure admission rules ---------------------------------------------------------

    fn st() -> AdmissionState {
        AdmissionState::default()
    }

    #[test]
    fn regular_runs_start_until_the_limit_then_queue() {
        let mut s = st();
        for n in 0..4 {
            s.active_regular = n;
            assert_eq!(decide(&s, &Class::Regular, 4), Decision::Start);
        }
        s.active_regular = 4;
        assert_eq!(decide(&s, &Class::Regular, 4), Decision::Queue);
    }

    #[test]
    fn interactive_runs_are_exclusive_and_refused_not_queued() {
        let mut s = st();
        assert_eq!(decide(&s, &Class::Interactive, 4), Decision::Start);
        s.active_interactive = 1;
        let Decision::Reject(Rejection::Busy(m)) = decide(&s, &Class::Interactive, 4) else {
            panic!("expected a refusal")
        };
        assert!(m.contains("tray"), "the refusal says how to get out: {m}");
        assert_eq!(decide(&s, &Class::Regular, 4), Decision::Start, "regular work is unaffected");
    }

    #[test]
    fn recording_rules() {
        let rc = |toggle| Class::Record { toggle, workflow_id: "rec".into() };
        let mut s = st();
        assert_eq!(decide(&s, &rc(true), 4), Decision::Start);
        s.active_interactive = 1;
        assert!(matches!(decide(&s, &rc(true), 4), Decision::Reject(Rejection::Busy(_))));
        s.active_interactive = 0;
        s.recording = Some((9, "rec".into()));
        assert_eq!(decide(&s, &rc(true), 4), Decision::ToggleStop { run_id: 9 });
        let Decision::Reject(Rejection::Busy(m)) = decide(&s, &rc(false), 4) else { panic!() };
        assert!(m.contains("already running"), "{m}");
        let other = Class::Record { toggle: true, workflow_id: "gif".into() };
        assert!(
            matches!(decide(&s, &other, 4), Decision::Reject(Rejection::Busy(_))),
            "a different recording workflow does not toggle the running one"
        );
    }

    #[test]
    fn nothing_is_accepted_while_shutting_down() {
        let s = AdmissionState { shutting_down: true, ..st() };
        for c in [
            Class::Regular,
            Class::Interactive,
            Class::Record { toggle: true, workflow_id: "r".into() },
        ] {
            assert_eq!(decide(&s, &c, 4), Decision::Reject(Rejection::ShuttingDown));
        }
        assert_eq!(Rejection::ShuttingDown.code(), ErrorCode::Busy);
        assert_eq!(Rejection::NotRunning(String::new()).code(), ErrorCode::NotRunning);
        assert_eq!(Rejection::Invalid(String::new()).code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn workflows_are_classified_by_what_they_show() {
        assert!(workflow_is_interactive(&region("r")));
        assert!(workflow_is_interactive(&wf(
            "e",
            InputKind::CaptureFullscreen,
            &[AfterCapture::OpenEditor]
        )));
        assert!(workflow_is_interactive(&wf(
            "d",
            InputKind::CaptureWindow,
            &[AfterCapture::SaveAsDialog]
        )));
        assert!(!workflow_is_interactive(&shot("s")));
        assert!(!workflow_is_interactive(&wf("f", InputKind::Files, &[AfterCapture::Upload])));
        let edit = JobSpec::Files { workflow: shot("s"), paths: vec![], edit_first: true };
        assert_eq!(edit.class(), Class::Interactive);
    }

    // ---- the supervisor with a controllable runner ------------------------------------------

    /// A runner whose runs finish when the test says so (or when cancelled / stopped).
    struct Gated {
        started: Mutex<Sender<u64>>,
        gates: Mutex<BTreeMap<u64, Sender<Outcome>>>,
        panic_on: Mutex<Option<u64>>,
        /// Recordings stay in the "choosing a region" phase (never mark started).
        selecting: std::sync::atomic::AtomicBool,
        recording: Arc<RecordingController>,
    }

    struct Rig {
        sup: Arc<Supervisor>,
        runner: Arc<Gated>,
        started: Receiver<u64>,
        ui: Arc<CollectingUi>,
        clock: Arc<FakeClock>,
    }

    impl JobRunner for Gated {
        fn run(&self, job: &Job, _ui: &dyn UiSink) -> RunOutput {
            let (tx, rx) = mpsc::channel();
            self.gates.lock().unwrap().insert(job.run_id, tx);
            if *self.panic_on.lock().unwrap() == Some(job.run_id) {
                panic!("boom");
            }
            let recording = matches!(job.spec, JobSpec::Record { .. });
            if recording && !self.selecting.load(std::sync::atomic::Ordering::SeqCst) {
                self.recording.mark_started();
            }
            self.started.lock().unwrap().send(job.run_id).unwrap();
            // Wait for: the test's release, the run's cancel, or (recordings) the stop token.
            let outcome = loop {
                if let Ok(o) = rx.recv_timeout(Duration::from_millis(5)) {
                    break o;
                }
                if job.cancel.is_cancelled() {
                    break Outcome::Cancelled;
                }
                if recording && job.stop.is_cancelled() {
                    // A graceful stop finalises the file, then the run carries on and succeeds.
                    self.recording.mark_finished();
                    break Outcome::Success;
                }
            };
            RunOutput {
                summary: plain_summary(job.run_id, outcome, format!("{outcome:?}")),
                notified: false,
            }
        }
    }

    fn rig(max_concurrent: usize) -> Rig {
        rig_remembering(max_concurrent, 64)
    }

    fn rig_remembering(max_concurrent: usize, remember_finished: usize) -> Rig {
        let clock = Arc::new(FakeClock::new());
        let ui = Arc::new(CollectingUi::new());
        let recording = Arc::new(RecordingController::new(clock.clone(), ui.clone()));
        let (tx, started) = mpsc::channel();
        let runner = Arc::new(Gated {
            started: Mutex::new(tx),
            gates: Mutex::new(BTreeMap::new()),
            panic_on: Mutex::new(None),
            selecting: std::sync::atomic::AtomicBool::new(false),
            recording: recording.clone(),
        });
        let sup = Supervisor::new(
            runner.clone(),
            ui.clone(),
            clock.clone(),
            RunIds::new(),
            recording,
            Limits { max_concurrent, remember_finished },
        );
        Rig { sup, runner, started, ui, clock }
    }

    impl Rig {
        fn started(&self) -> u64 {
            self.started.recv_timeout(Duration::from_secs(5)).expect("a run started")
        }
        fn nothing_started(&self) {
            assert!(
                self.started.recv_timeout(Duration::from_millis(80)).is_err(),
                "no run may start"
            );
        }
        fn release(&self, run_id: u64, outcome: Outcome) {
            let gate = self.runner.gates.lock().unwrap().get(&run_id).cloned();
            gate.expect("the run reached the runner").send(outcome).unwrap();
        }
        fn finish(&self, run_id: u64) -> RunSummary {
            match self.sup.wait(run_id, Some(Duration::from_secs(5))) {
                Waited::Finished(s) => s,
                other => panic!("run {run_id}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_run_starts_finishes_and_is_reported() {
        let r = rig(4);
        let s = r.sup.submit(run(shot("a"))).unwrap();
        assert_eq!(s, Submitted::Started { run_id: 1 });
        assert_eq!(r.started(), 1);
        assert_eq!(r.sup.load().active.len(), 1);
        r.release(1, Outcome::Success);
        assert_eq!(r.finish(1).outcome, Outcome::Success);
        assert!(r.sup.is_idle());
        let ev = r.ui.events();
        assert!(matches!(&ev[0], UiEvent::RunStarted { run_id: 1, interactive: false, .. }));
        assert!(matches!(ev.last(), Some(UiEvent::RunFinished { run_id: 1, .. })));
    }

    #[test]
    fn extra_regular_runs_queue_in_order_and_start_as_slots_free_up() {
        let r = rig(2);
        let ids: Vec<Submitted> =
            (0..4).map(|i| r.sup.submit(files(shot("f"), &[&format!("/{i}")])).unwrap()).collect();
        assert_eq!(ids[2], Submitted::Queued { run_id: 3 });
        assert_eq!(ids[3], Submitted::Queued { run_id: 4 });
        r.started();
        r.started();
        r.nothing_started();
        assert_eq!(r.sup.load().queued, 2);
        r.release(1, Outcome::Success);
        assert_eq!(r.started(), 3, "the oldest queued run goes first");
        r.release(2, Outcome::Success);
        assert_eq!(r.started(), 4);
        for id in [3, 4] {
            r.release(id, Outcome::Success);
        }
        for id in 1..=4 {
            r.finish(id);
        }
        assert!(r.ui.events().iter().any(|e| matches!(e, UiEvent::RunQueued { run_id: 3, .. })));
    }

    #[test]
    fn a_second_interactive_capture_is_refused_and_a_person_is_told() {
        let r = rig(4);
        r.sup.submit(run(region("r1"))).unwrap();
        r.started();
        let mut again = run(region("r2"));
        again.origin = Origin::Hotkey;
        let e = r.sup.submit(again).unwrap_err();
        assert!(matches!(e, Rejection::Busy(_)));
        assert!(
            r.ui.events()
                .iter()
                .any(|e| matches!(e, UiEvent::Notice { level: NotificationLevel::Warning, .. })),
            "a hotkey press that is refused says so"
        );
        // An IPC caller gets the error response instead of a notification.
        r.ui.take();
        let _ = r.sup.submit(run(region("r3"))).unwrap_err();
        assert!(r.ui.events().iter().all(|e| !matches!(e, UiEvent::Notice { .. })));
        // Non-interactive work is fine meanwhile.
        r.sup.submit(run(shot("s"))).unwrap();
        r.started();
        r.release(1, Outcome::Cancelled);
        r.release(2, Outcome::Success);
        r.finish(1);
        // The overlay is closed: the next one is accepted.
        r.sup.submit(run(region("r4"))).unwrap();
    }

    #[test]
    fn cancel_aborts_a_running_run_and_removes_a_queued_one() {
        let r = rig(1);
        r.sup.submit(run(shot("a"))).unwrap();
        r.started();
        let queued = r.sup.submit(run(shot("b"))).unwrap();
        assert!(matches!(queued, Submitted::Queued { .. }));
        assert!(r.sup.cancel(queued.run_id()));
        assert_eq!(r.finish(queued.run_id()).outcome, Outcome::Cancelled);
        r.nothing_started();
        assert!(r.sup.cancel(1));
        assert_eq!(r.finish(1).outcome, Outcome::Cancelled);
        assert!(!r.sup.cancel(999), "unknown run");
    }

    #[test]
    fn the_trays_cancel_hits_interactive_runs_only() {
        let r = rig(4);
        r.sup.submit(run(shot("plain"))).unwrap();
        r.sup.submit(run(region("overlay"))).unwrap();
        r.started();
        r.started();
        assert_eq!(r.sup.cancel_interactive(), 1);
        assert_eq!(r.finish(2).outcome, Outcome::Cancelled);
        assert!(!r.sup.is_idle(), "the plain run is still going");
        r.release(1, Outcome::Success);
        r.finish(1);
    }

    #[test]
    fn a_panicking_run_fails_that_run_and_the_supervisor_lives_on() {
        let r = rig(4);
        *r.runner.panic_on.lock().unwrap() = Some(1);
        r.sup.submit(run(shot("a"))).unwrap();
        let s = r.finish(1);
        assert_eq!(s.outcome, Outcome::Failed);
        assert!(
            s.message.contains("internal error") && s.message.contains("still running"),
            "{}",
            s.message
        );
        r.sup.submit(run(shot("b"))).unwrap();
        assert_eq!(r.started(), 2);
        r.release(2, Outcome::Success);
        r.finish(2);
    }

    #[test]
    fn wait_handles_finished_unknown_and_slow_runs() {
        let r = rig_remembering(4, 3);
        assert_eq!(r.sup.wait(42, Some(Duration::from_millis(10))), Waited::Unknown);
        r.sup.submit(run(shot("a"))).unwrap();
        r.started();
        assert_eq!(r.sup.wait(1, Some(Duration::from_millis(30))), Waited::TimedOut);
        let sup = r.sup.clone();
        let waiter = std::thread::spawn(move || sup.wait(1, None));
        std::thread::sleep(Duration::from_millis(30));
        r.release(1, Outcome::PartialSuccess);
        let Waited::Finished(s) = waiter.join().unwrap() else { panic!() };
        assert_eq!(s.outcome, Outcome::PartialSuccess);
        // Late askers still get the answer; the memory is bounded (3 here).
        assert!(matches!(r.sup.wait(1, None), Waited::Finished(_)));
        for i in 2..=5 {
            r.sup.submit(run(shot("x"))).unwrap();
            r.started();
            r.release(i, Outcome::Success);
            r.finish(i);
        }
        assert_eq!(r.sup.wait(1, Some(Duration::from_millis(5))), Waited::Unknown, "forgotten");
    }

    #[test]
    fn recording_toggle_stop_and_the_workflow_continues() {
        let r = rig(4);
        let s = r.sup.submit(rec(record("rec"), true)).unwrap();
        assert_eq!(s, Submitted::Started { run_id: 1 });
        r.started();
        r.clock.advance(Duration::from_secs(3));
        assert!(matches!(r.sup.recording().view(), RecordingView::Recording { .. }));
        // The same hotkey again stops it.
        let mut again = rec(record("rec"), true);
        again.origin = Origin::Hotkey;
        assert_eq!(r.sup.submit(again).unwrap(), Submitted::StoppedRecording { run_id: 1 });
        assert_eq!(r.finish(1).outcome, Outcome::Success);
        assert!(!r.sup.recording().is_active());
        // ...and a new recording may start right after.
        r.sup.submit(rec(record("rec"), true)).unwrap();
        assert_eq!(r.started(), 2);
        assert!(r.sup.stop_recording().is_ok());
        r.finish(2);
        assert!(matches!(r.sup.stop_recording(), Err(Rejection::NotRunning(_))));
    }

    #[test]
    fn a_second_recording_start_is_refused_and_interactive_captures_wait_for_the_overlay() {
        let r = rig(4);
        r.sup.submit(rec(record("rec"), false)).unwrap();
        r.started();
        assert!(matches!(r.sup.submit(rec(record("rec"), false)), Err(Rejection::Busy(_))));
        assert!(matches!(r.sup.submit(rec(record("gif"), true)), Err(Rejection::Busy(_))));
        r.sup.stop_recording().unwrap();
        r.finish(1);
    }

    #[test]
    fn cancelling_the_selection_of_a_recording_aborts_the_start() {
        let r = rig(4);
        r.runner.selecting.store(true, std::sync::atomic::Ordering::SeqCst);
        r.sup.submit(rec(record("rec"), true)).unwrap();
        r.started();
        assert_eq!(r.sup.recording().view(), RecordingView::Selecting);
        // While the overlay is up nothing else that needs the overlay may start ...
        assert!(matches!(r.sup.submit(run(region("r"))), Err(Rejection::Busy(_))));
        // ... and the tray's Cancel closes it.
        assert_eq!(r.sup.cancel_interactive(), 1);
        assert_eq!(r.finish(1).outcome, Outcome::Cancelled);
        assert!(!r.sup.recording().is_active(), "the slot is released whatever way the run ended");

        // Stop (hotkey pressed again) during the selection does the same.
        r.sup.submit(rec(record("rec"), true)).unwrap();
        r.started();
        r.sup.stop_recording().unwrap();
        assert_eq!(r.finish(2).outcome, Outcome::Cancelled);

        // A run cancelled by id while recording is discarded and releases the slot as well.
        r.runner.selecting.store(false, std::sync::atomic::Ordering::SeqCst);
        r.sup.submit(rec(record("rec"), true)).unwrap();
        r.started();
        assert!(r.sup.cancel(3));
        assert_eq!(r.finish(3).outcome, Outcome::Cancelled);
        assert!(!r.sup.recording().is_active());
    }

    #[test]
    fn shutdown_stops_the_recording_first_then_cancels_the_rest() {
        let r = rig(1);
        r.sup.submit(rec(record("rec"), true)).unwrap();
        r.started();
        r.sup.submit(run(shot("upload"))).unwrap();
        r.started();
        r.sup.submit(run(shot("queued"))).unwrap();
        let sup = r.sup.clone();
        let grace = ShutdownGrace {
            recording: Duration::from_secs(5),
            runs: Duration::from_millis(100),
            cancelled: Duration::from_secs(5),
        };
        let t = std::thread::spawn(move || sup.shutdown(grace));
        assert!(t.join().unwrap(), "everything ended in time");
        // The recording run finished normally (graceful stop), the others were cancelled.
        assert_eq!(r.finish(1).outcome, Outcome::Success);
        assert_eq!(r.finish(2).outcome, Outcome::Cancelled);
        assert_eq!(r.finish(3).outcome, Outcome::Cancelled);
        assert!(matches!(r.sup.submit(run(shot("late"))), Err(Rejection::ShuttingDown)));
    }

    #[test]
    fn shutdown_gives_running_uploads_time_to_finish_before_cancelling() {
        let r = rig(4);
        r.sup.submit(run(shot("upload"))).unwrap();
        r.started();
        let sup = r.sup.clone();
        let grace = ShutdownGrace { runs: Duration::from_secs(5), ..ShutdownGrace::default() };
        let t = std::thread::spawn(move || sup.shutdown(grace));
        std::thread::sleep(Duration::from_millis(50));
        r.release(1, Outcome::Success);
        assert!(t.join().unwrap());
        assert_eq!(
            r.finish(1).outcome,
            Outcome::Success,
            "not cancelled: it finished within the grace"
        );
    }

    #[test]
    fn a_requested_run_id_is_honoured() {
        let r = rig(4);
        let mut q = files(shot("f"), &["/a"]);
        q.run_id = Some(77);
        assert_eq!(r.sup.submit(q).unwrap().run_id(), 77);
        r.started();
        r.release(77, Outcome::Success);
        r.finish(77);
    }

    #[test]
    fn items_helper_builds_summaries() {
        let items = items_of([(Some("/a".into()), Some("https://x".into()), None)]);
        assert_eq!(items[0].url.as_deref(), Some("https://x"));
    }
}
