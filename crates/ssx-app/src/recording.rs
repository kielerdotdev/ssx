//! Recording control: one recording at a time, started and stopped from the tray, a hotkey,
//! IPC or shutdown.
//!
//! ```text
//!            begin                started               request_stop           finished
//!   Idle ───────────▶ Selecting ───────────▶ Recording ───────────────▶ Stopping ─────────▶ Idle
//!                        │  request_stop (overlay open: abort the start)                       ▲
//!                        └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! * **Selecting**: the workflow engine has called `Recorder::start`, which is asking for a
//!   region on the overlay. Stopping then means *cancelling the run*.
//! * **Recording**: frames are captured. Stopping cancels the run's `stop` token, which the
//!   engine's `record` step turns into `RecordingSession::stop()`: the file is finalised and the
//!   run goes on with after-capture and after-upload steps (upload, copy the URL, ...).
//! * The state returns to **Idle** as soon as the *file is finalised*, not when the whole run
//!   ends, so the next recording may start while the previous one is still uploading.
//!
//! [`RecordingMachine`] is the pure state machine (time is an argument);
//! [`RecordingController`] adds the tokens, the clock and UI notifications;
//! [`TrackedRecorder`] wraps the real recorder so the controller learns about start and finish
//! from the only place that really knows.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use ssx_core::{
    ipc::RecordingStatus,
    workflow::{
        CancelToken, RecordRequest, RecordedVideo, Recorder, RecordingSession, ServiceError,
    },
};

use crate::{
    clock::Clock,
    events::{RecordingView, UiEvent, UiSink},
};

/// The phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecState {
    /// Nothing is recording.
    Idle,
    /// The region is being chosen.
    Selecting {
        /// The run that will record.
        run_id: u64,
        /// Its workflow id.
        workflow_id: String,
    },
    /// Capturing.
    Recording {
        /// The run.
        run_id: u64,
        /// Its workflow id.
        workflow_id: String,
        /// When capturing started.
        since: Duration,
    },
    /// Finalising the file after a stop request.
    Stopping {
        /// The run.
        run_id: u64,
        /// Its workflow id.
        workflow_id: String,
        /// When capturing started.
        since: Duration,
    },
}

/// Why a recording cannot start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingBusy {
    /// The workflow that is recording.
    pub workflow_id: String,
    /// The run that is recording.
    pub run_id: u64,
}

/// What [`RecordingMachine::request_stop`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAction {
    /// Nothing is recording.
    NotRunning,
    /// Ask the recorder to stop gracefully (finalise the file).
    Graceful,
    /// The overlay is still open: cancel the run.
    AbortSelection,
    /// Already stopping.
    AlreadyStopping,
}

/// The pure state machine. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingMachine {
    state: RecState,
}

impl Default for RecordingMachine {
    fn default() -> Self {
        Self { state: RecState::Idle }
    }
}

impl RecordingMachine {
    /// The current phase.
    pub fn state(&self) -> &RecState {
        &self.state
    }

    /// The run that owns the recording, if any.
    pub fn run_id(&self) -> Option<u64> {
        match &self.state {
            RecState::Idle => None,
            RecState::Selecting { run_id, .. }
            | RecState::Recording { run_id, .. }
            | RecState::Stopping { run_id, .. } => Some(*run_id),
        }
    }

    /// The workflow that owns the recording, if any.
    pub fn workflow_id(&self) -> Option<&str> {
        match &self.state {
            RecState::Idle => None,
            RecState::Selecting { workflow_id, .. }
            | RecState::Recording { workflow_id, .. }
            | RecState::Stopping { workflow_id, .. } => Some(workflow_id),
        }
    }

    /// A recording run is about to call `Recorder::start`. Only from `Idle`.
    pub fn begin(&mut self, run_id: u64, workflow_id: &str) -> Result<(), RecordingBusy> {
        if let (Some(run_id), Some(wf)) = (self.run_id(), self.workflow_id()) {
            return Err(RecordingBusy { workflow_id: wf.to_owned(), run_id });
        }
        self.state = RecState::Selecting { run_id, workflow_id: workflow_id.to_owned() };
        Ok(())
    }

    /// `Recorder::start` succeeded: frames are being captured from `now`.
    /// Returns `false` (and changes nothing) unless the machine was selecting.
    pub fn started(&mut self, now: Duration) -> bool {
        if let RecState::Selecting { run_id, workflow_id } = &self.state {
            self.state = RecState::Recording {
                run_id: *run_id,
                workflow_id: workflow_id.clone(),
                since: now,
            };
            true
        } else {
            false
        }
    }

    /// The user (or shutdown) wants the recording to end.
    pub fn request_stop(&mut self) -> StopAction {
        match std::mem::replace(&mut self.state, RecState::Idle) {
            RecState::Idle => StopAction::NotRunning,
            s @ RecState::Selecting { .. } => {
                self.state = s;
                StopAction::AbortSelection
            }
            RecState::Recording { run_id, workflow_id, since } => {
                self.state = RecState::Stopping { run_id, workflow_id, since };
                StopAction::Graceful
            }
            s @ RecState::Stopping { .. } => {
                self.state = s;
                StopAction::AlreadyStopping
            }
        }
    }

    /// The file is finalised (or the start failed / was cancelled): back to `Idle`.
    /// Returns `false` if there was nothing to finish.
    pub fn finished(&mut self) -> bool {
        !matches!(std::mem::replace(&mut self.state, RecState::Idle), RecState::Idle)
    }

    /// Run `run_id` ended for whatever reason: if it still owns the recording, release it.
    /// A safety net: no path may leave the machine stuck in a state whose run is gone.
    pub fn run_ended(&mut self, run_id: u64) -> bool {
        if self.run_id() == Some(run_id) {
            self.state = RecState::Idle;
            true
        } else {
            false
        }
    }

    /// What the UI shows at `now`.
    pub fn view(&self, now: Duration) -> RecordingView {
        match &self.state {
            RecState::Idle => RecordingView::Idle,
            RecState::Selecting { .. } => RecordingView::Selecting,
            RecState::Recording { since, .. } => {
                RecordingView::Recording { elapsed: now.saturating_sub(*since) }
            }
            RecState::Stopping { .. } => RecordingView::Stopping,
        }
    }

    /// The IPC description at `now`.
    pub fn status(&self, now: Duration) -> RecordingStatus {
        let elapsed_ms = match &self.state {
            RecState::Recording { since, .. } => {
                u64::try_from(now.saturating_sub(*since).as_millis()).unwrap_or(u64::MAX)
            }
            RecState::Stopping { since, .. } => {
                u64::try_from(now.saturating_sub(*since).as_millis()).unwrap_or(u64::MAX)
            }
            _ => 0,
        };
        RecordingStatus {
            active: !matches!(self.state, RecState::Idle),
            selecting: matches!(self.state, RecState::Selecting { .. }),
            workflow: self.workflow_id().map(str::to_owned),
            elapsed_ms,
            run_id: self.run_id(),
        }
    }
}

/// Why [`RecordingController::stop`] did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StopError {
    /// There is no recording.
    #[error("no recording is running")]
    NotRunning,
}

#[derive(Default)]
struct Tokens {
    /// Cancels the whole run (aborts a selection in progress, discards).
    run: Option<CancelToken>,
    /// Ends the recording gracefully (keeps the file).
    stop: Option<CancelToken>,
}

/// The machine plus the tokens, the clock and the UI. Shared by the supervisor (which admits
/// and stops recordings) and [`TrackedRecorder`] (which reports start and finish).
pub struct RecordingController {
    machine: Mutex<RecordingMachine>,
    tokens: Mutex<Tokens>,
    clock: Arc<dyn Clock>,
    ui: Arc<dyn UiSink>,
}

impl std::fmt::Debug for RecordingController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingController").field("machine", &self.lock()).finish_non_exhaustive()
    }
}

impl RecordingController {
    /// A controller in `Idle`.
    pub fn new(clock: Arc<dyn Clock>, ui: Arc<dyn UiSink>) -> Self {
        Self {
            machine: Mutex::new(RecordingMachine::default()),
            tokens: Mutex::new(Tokens::default()),
            clock,
            ui,
        }
    }

    fn lock(&self) -> MutexGuard<'_, RecordingMachine> {
        self.machine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tokens(&self) -> MutexGuard<'_, Tokens> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn announce(&self) {
        let view = self.lock().view(self.clock.now());
        self.ui.emit(UiEvent::Recording(view));
    }

    /// Claims the recording slot for `run_id`. `run` cancels the whole run, `stop` ends the
    /// recording gracefully.
    pub fn begin(
        &self,
        run_id: u64,
        workflow_id: &str,
        run: CancelToken,
        stop: CancelToken,
    ) -> Result<(), RecordingBusy> {
        self.lock().begin(run_id, workflow_id)?;
        *self.tokens() = Tokens { run: Some(run), stop: Some(stop) };
        self.announce();
        Ok(())
    }

    /// Frames are being captured now.
    pub fn mark_started(&self) {
        let now = self.clock.now();
        if self.lock().started(now) {
            self.announce();
        }
    }

    /// The file is finalised or the start failed.
    pub fn mark_finished(&self) {
        if self.lock().finished() {
            *self.tokens() = Tokens::default();
            self.announce();
        }
    }

    /// Run `run_id` ended: release the slot if it still holds it.
    pub fn run_ended(&self, run_id: u64) {
        if self.lock().run_ended(run_id) {
            *self.tokens() = Tokens::default();
            self.announce();
        }
    }

    /// Ends the recording: gracefully when frames are being captured, by cancelling the run
    /// while the region is still being chosen.
    pub fn stop(&self) -> Result<(), StopError> {
        let action = self.lock().request_stop();
        match action {
            StopAction::NotRunning => return Err(StopError::NotRunning),
            StopAction::Graceful => {
                if let Some(t) = &self.tokens().stop {
                    t.cancel();
                }
            }
            StopAction::AbortSelection => {
                if let Some(t) = &self.tokens().run {
                    t.cancel();
                }
            }
            StopAction::AlreadyStopping => {}
        }
        self.announce();
        Ok(())
    }

    /// The UI's view now.
    pub fn view(&self) -> RecordingView {
        self.lock().view(self.clock.now())
    }

    /// The IPC description now.
    pub fn status(&self) -> RecordingStatus {
        self.lock().status(self.clock.now())
    }

    /// The workflow that is recording.
    pub fn workflow_id(&self) -> Option<String> {
        self.lock().workflow_id().map(str::to_owned)
    }

    /// The run that is recording.
    pub fn run_id(&self) -> Option<u64> {
        self.lock().run_id()
    }

    /// `true` while any phase is active.
    pub fn is_active(&self) -> bool {
        self.view().is_active()
    }

    /// Blocks until the recording is `Idle` or `timeout` passes; `true` if idle. Polls a
    /// few times a second, and only ever during shutdown.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !self.is_active() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Wraps a [`Recorder`] and reports its start and finish to a [`RecordingController`].
pub struct TrackedRecorder {
    inner: Arc<dyn Recorder>,
    controller: Arc<RecordingController>,
}

impl std::fmt::Debug for TrackedRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedRecorder").finish_non_exhaustive()
    }
}

impl TrackedRecorder {
    /// Wraps `inner`.
    pub fn new(inner: Arc<dyn Recorder>, controller: Arc<RecordingController>) -> Self {
        Self { inner, controller }
    }
}

impl Recorder for TrackedRecorder {
    fn start(&self, req: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError> {
        match self.inner.start(req) {
            Ok(session) => {
                self.controller.mark_started();
                Ok(Box::new(TrackedSession {
                    inner: Some(session),
                    controller: Arc::clone(&self.controller),
                }))
            }
            Err(e) => {
                self.controller.mark_finished();
                Err(e)
            }
        }
    }
}

struct TrackedSession {
    inner: Option<Box<dyn RecordingSession>>,
    controller: Arc<RecordingController>,
}

impl RecordingSession for TrackedSession {
    fn stop(mut self: Box<Self>) -> Result<RecordedVideo, ServiceError> {
        let inner = self.inner.take();
        let result = match inner {
            Some(s) => s.stop(),
            None => Err(ServiceError::failed("the recording is already finished")),
        };
        self.controller.mark_finished();
        result
    }

    fn abort(mut self: Box<Self>) {
        if let Some(s) = self.inner.take() {
            s.abort();
        }
        self.controller.mark_finished();
    }
}

impl Drop for TrackedSession {
    fn drop(&mut self) {
        // A session dropped without stop/abort (a panic unwinding through the engine):
        // dropping the inner session aborts it; the slot must not stay claimed.
        if self.inner.take().is_some() {
            self.controller.mark_finished();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{clock::FakeClock, events::CollectingUi};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- the machine ---------------------------------------------------------------------

    #[test]
    fn the_happy_path_walks_through_every_phase() {
        let mut m = RecordingMachine::default();
        assert_eq!(m.view(ms(0)), RecordingView::Idle);
        m.begin(7, "record-screen").unwrap();
        assert_eq!(m.view(ms(0)), RecordingView::Selecting);
        assert_eq!(m.status(ms(0)).selecting, true);
        assert!(m.started(ms(1_000)));
        assert_eq!(m.view(ms(3_500)), RecordingView::Recording { elapsed: ms(2_500) });
        let st = m.status(ms(3_500));
        assert_eq!(
            (st.active, st.selecting, st.elapsed_ms, st.run_id, st.workflow.as_deref()),
            (true, false, 2_500, Some(7), Some("record-screen"))
        );
        assert_eq!(m.request_stop(), StopAction::Graceful);
        assert_eq!(m.view(ms(4_000)), RecordingView::Stopping);
        assert_eq!(m.request_stop(), StopAction::AlreadyStopping);
        assert!(m.finished());
        assert_eq!(m.view(ms(4_000)), RecordingView::Idle);
        assert!(!m.finished(), "nothing left to finish");
    }

    #[test]
    fn only_one_recording_at_a_time() {
        let mut m = RecordingMachine::default();
        m.begin(1, "a").unwrap();
        let busy = m.begin(2, "b").unwrap_err();
        assert_eq!(busy, RecordingBusy { workflow_id: "a".into(), run_id: 1 });
        m.started(ms(0));
        assert!(m.begin(3, "c").is_err(), "also while recording");
        m.request_stop();
        assert!(m.begin(4, "d").is_err(), "also while finalising");
        m.finished();
        m.begin(5, "e").unwrap();
    }

    #[test]
    fn stopping_while_selecting_aborts_the_start_and_idle_has_nothing_to_stop() {
        let mut m = RecordingMachine::default();
        assert_eq!(m.request_stop(), StopAction::NotRunning);
        m.begin(1, "a").unwrap();
        assert_eq!(m.request_stop(), StopAction::AbortSelection);
        assert!(
            matches!(m.state(), RecState::Selecting { .. }),
            "still selecting until the run reports back"
        );
        assert!(m.finished());
        assert!(!m.started(ms(1)), "started() outside selecting is ignored");
    }

    #[test]
    fn a_finished_run_releases_only_its_own_recording() {
        let mut m = RecordingMachine::default();
        m.begin(1, "a").unwrap();
        assert!(!m.run_ended(2), "someone else's run");
        assert!(matches!(m.state(), RecState::Selecting { .. }));
        assert!(m.run_ended(1));
        assert_eq!(m.state(), &RecState::Idle);
        assert!(!m.run_ended(1));
    }

    #[test]
    fn elapsed_never_underflows() {
        let mut m = RecordingMachine::default();
        m.begin(1, "a").unwrap();
        m.started(ms(5_000));
        assert_eq!(m.view(ms(1_000)), RecordingView::Recording { elapsed: Duration::ZERO });
        assert_eq!(m.status(ms(1_000)).elapsed_ms, 0);
    }

    // ---- the controller ------------------------------------------------------------------

    fn controller() -> (Arc<RecordingController>, Arc<FakeClock>, Arc<CollectingUi>) {
        let clock = Arc::new(FakeClock::new());
        let ui = Arc::new(CollectingUi::new());
        (Arc::new(RecordingController::new(clock.clone(), ui.clone())), clock, ui)
    }

    #[test]
    fn stop_ends_a_recording_gracefully_and_a_selection_by_cancelling_the_run() {
        let (c, clock, ui) = controller();
        let (run, stop) = (CancelToken::new(), CancelToken::new());
        assert_eq!(c.stop(), Err(StopError::NotRunning));

        c.begin(1, "wf", run.clone(), stop.clone()).unwrap();
        c.stop().unwrap();
        assert!(run.is_cancelled() && !stop.is_cancelled(), "selection: abort the run");
        c.mark_finished();

        let (run, stop) = (CancelToken::new(), CancelToken::new());
        c.begin(2, "wf", run.clone(), stop.clone()).unwrap();
        clock.advance(ms(500));
        c.mark_started();
        clock.advance(ms(2_000));
        assert_eq!(c.view(), RecordingView::Recording { elapsed: ms(2_000) });
        c.stop().unwrap();
        assert!(stop.is_cancelled() && !run.is_cancelled(), "recording: keep the file");
        assert_eq!(c.view(), RecordingView::Stopping);
        c.mark_finished();
        assert!(!c.is_active());

        let phases: Vec<RecordingView> = ui
            .events()
            .into_iter()
            .filter_map(|e| if let UiEvent::Recording(v) = e { Some(v) } else { None })
            .collect();
        assert_eq!(phases.first(), Some(&RecordingView::Selecting));
        assert!(phases.contains(&RecordingView::Stopping));
        assert_eq!(phases.last(), Some(&RecordingView::Idle));
    }

    #[test]
    fn a_second_begin_reports_who_is_recording() {
        let (c, ..) = controller();
        c.begin(1, "a", CancelToken::new(), CancelToken::new()).unwrap();
        let e = c.begin(2, "b", CancelToken::new(), CancelToken::new()).unwrap_err();
        assert_eq!((e.run_id, e.workflow_id.as_str()), (1, "a"));
        assert_eq!(c.run_id(), Some(1));
        assert_eq!(c.workflow_id().as_deref(), Some("a"));
    }

    #[test]
    fn run_ended_is_a_safety_net() {
        let (c, ..) = controller();
        c.begin(1, "a", CancelToken::new(), CancelToken::new()).unwrap();
        c.mark_started();
        c.run_ended(1);
        assert!(!c.is_active());
        c.begin(2, "a", CancelToken::new(), CancelToken::new()).unwrap();
    }

    #[test]
    fn wait_idle_reports_whether_the_recording_ended_in_time() {
        let (c, ..) = controller();
        assert!(c.wait_idle(ms(1)));
        c.begin(1, "a", CancelToken::new(), CancelToken::new()).unwrap();
        assert!(!c.wait_idle(ms(60)));
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || {
            std::thread::sleep(ms(50));
            c2.mark_finished();
        });
        assert!(c.wait_idle(Duration::from_secs(5)));
        t.join().unwrap();
    }

    // ---- the tracked recorder ---------------------------------------------------------------

    struct FakeSession {
        stopped: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
        fail: bool,
    }
    impl RecordingSession for FakeSession {
        fn stop(self: Box<Self>) -> Result<RecordedVideo, ServiceError> {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(ServiceError::failed("disk full"))
            } else {
                Ok(RecordedVideo {
                    path: "/x.mp4".into(),
                    width: None,
                    height: None,
                    duration: None,
                })
            }
        }
        fn abort(self: Box<Self>) {
            self.aborted.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct FakeRecorder {
        fail_start: bool,
        fail_stop: bool,
        stopped: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
    }
    impl Recorder for FakeRecorder {
        fn start(&self, _: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError> {
            if self.fail_start {
                return Err(ServiceError::Cancelled);
            }
            Ok(Box::new(FakeSession {
                stopped: self.stopped.clone(),
                aborted: self.aborted.clone(),
                fail: self.fail_stop,
            }))
        }
    }

    fn request() -> RecordRequest {
        RecordRequest {
            kind: ssx_core::workflow::RecordKind::Video,
            output_dir: ".".into(),
            file_stem: "x".into(),
            include_cursor: false,
        }
    }

    fn tracked(
        fail_start: bool,
        fail_stop: bool,
    ) -> (TrackedRecorder, Arc<RecordingController>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (c, ..) = controller();
        let (stopped, aborted) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let r = TrackedRecorder::new(
            Arc::new(FakeRecorder {
                fail_start,
                fail_stop,
                stopped: stopped.clone(),
                aborted: aborted.clone(),
            }),
            c.clone(),
        );
        (r, c, stopped, aborted)
    }

    #[test]
    fn start_and_stop_drive_the_controller() {
        let (r, c, stopped, _) = tracked(false, false);
        c.begin(1, "wf", CancelToken::new(), CancelToken::new()).unwrap();
        let s = r.start(&request()).unwrap();
        assert!(matches!(c.view(), RecordingView::Recording { .. }));
        s.stop().unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(!c.is_active(), "released as soon as the file is finalised");
    }

    #[test]
    fn a_failed_stop_still_releases_the_slot() {
        let (r, c, ..) = tracked(false, true);
        c.begin(1, "wf", CancelToken::new(), CancelToken::new()).unwrap();
        let s = r.start(&request()).unwrap();
        assert!(s.stop().is_err());
        assert!(!c.is_active());
    }

    #[test]
    fn a_cancelled_or_failed_start_releases_the_slot() {
        let (r, c, ..) = tracked(true, false);
        c.begin(1, "wf", CancelToken::new(), CancelToken::new()).unwrap();
        assert!(r.start(&request()).map(|_| ()).unwrap_err().is_cancelled());
        assert!(!c.is_active());
    }

    #[test]
    fn abort_and_drop_release_the_slot_too() {
        let (r, c, _, aborted) = tracked(false, false);
        c.begin(1, "wf", CancelToken::new(), CancelToken::new()).unwrap();
        r.start(&request()).unwrap().abort();
        assert_eq!(aborted.load(Ordering::SeqCst), 1);
        assert!(!c.is_active());

        c.begin(2, "wf", CancelToken::new(), CancelToken::new()).unwrap();
        drop(r.start(&request()).unwrap());
        assert!(!c.is_active(), "a session dropped mid-panic must not wedge the recorder");
    }
}
