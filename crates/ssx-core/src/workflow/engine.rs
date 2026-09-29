//! The engine: acquiring input, running items (in parallel for multi-file posts), the
//! after-upload phase and report assembly. The per-step logic is in [`super::steps`].

use std::{
    collections::HashSet,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use super::{
    CancelToken, CaptureRequest, CaptureTarget, Captured, ClipboardContent, Event, EventSink,
    FailureKind, Outcome, RecordKind, RecordRequest, RunReport, ServiceError, Services, SkipReason,
    StepFailure, StepKind, StepReport, StepStatus,
    item::{Item, Origin, extension_of, is_image_ext, is_video_ext},
};
use crate::{
    history::EntryKind,
    pattern::{Clock, CounterStore, Env, Rng, SystemClock, SystemEnv, SystemRng},
    settings::{InputKind, Settings, Workflow},
};

/// Injectable sources for file naming (`%i`, `%rn`, time, …).
#[derive(Clone)]
pub struct Naming {
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Randomness.
    pub rng: Arc<dyn Rng>,
    /// User / machine identity.
    pub env: Arc<dyn Env>,
    /// The `%i` counter (use [`FileCounter`](crate::pattern::FileCounter) in the app).
    pub counter: Arc<dyn CounterStore>,
}

impl std::fmt::Debug for Naming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Naming").finish_non_exhaustive()
    }
}

impl Naming {
    /// System clock, RNG and environment with the given counter.
    pub fn system(counter: Arc<dyn CounterStore>) -> Self {
        Self {
            clock: Arc::new(SystemClock),
            rng: Arc::new(SystemRng::new()),
            env: Arc::new(SystemEnv),
            counter,
        }
    }
}

/// What to run the workflow on.
#[derive(Debug, Clone)]
pub enum Input {
    /// Acquire the content as the workflow's [`InputKind`] says: a screenshot through the
    /// [`Capturer`](super::Capturer) for the capture kinds, the clipboard for `clipboard`.
    /// (Recording and file inputs need their explicit variants.)
    Auto,
    /// An image captured elsewhere (for example by the overlay before the workflow was
    /// chosen).
    Image(Captured),
    /// Record the screen now; the run finishes recording when `stop` is cancelled.
    Record {
        /// Cancel this token to stop recording gracefully and keep the video. (The run's own
        /// [`CancelToken`] aborts and discards it.)
        stop: CancelToken,
    },
    /// A video that was just recorded by ssx (eligible for `delete_local_file`).
    Recorded(PathBuf),
    /// Existing files and folders (never deleted by ssx).
    Files(Vec<PathBuf>),
}

/// Where the video of [`Engine::post_video`] comes from.
#[derive(Debug, Clone)]
pub enum VideoSource {
    /// Record now; cancel `stop` to finish.
    Record {
        /// Stop signal.
        stop: CancelToken,
    },
    /// A recording that already exists.
    Recorded(PathBuf),
}

/// Runs workflows.
///
/// An `Engine` is cheap, immutable and shareable: it holds a settings snapshot and the naming
/// sources; all per-run state lives inside [`run`](Self::run). To pick up changed settings
/// build a new engine.
#[derive(Debug, Clone)]
pub struct Engine {
    pub(super) settings: Arc<Settings>,
    pub(super) naming: Naming,
}

/// Longest a `run_command` may run.
pub(super) const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// A failed step's ending, internal.
#[derive(Debug)]
pub(super) enum StepEnd {
    Failed(StepFailure),
    Skipped(SkipReason),
    Cancelled,
}

pub(super) type StepResult = Result<Option<String>, StepEnd>;

impl StepEnd {
    pub fn fail(kind: FailureKind, message: impl Into<String>) -> Self {
        Self::Failed(StepFailure { kind, message: message.into(), retryable: false })
    }
}

impl From<ServiceError> for StepEnd {
    fn from(e: ServiceError) -> Self {
        let (kind, retryable, message) = match e {
            ServiceError::Cancelled => return Self::Cancelled,
            ServiceError::Unsupported(w) => {
                (FailureKind::Unsupported, false, format!("{w} is not supported here"))
            }
            ServiceError::NotConfigured(w) => (FailureKind::NotConfigured, false, w),
            ServiceError::Io(e) => (FailureKind::Io, false, e.to_string()),
            ServiceError::Failed { message, retryable } => {
                (FailureKind::Service, retryable, message)
            }
        };
        Self::Failed(StepFailure { kind, message, retryable })
    }
}

impl From<std::io::Error> for StepEnd {
    fn from(e: std::io::Error) -> Self {
        Self::fail(FailureKind::Io, e.to_string())
    }
}

/// Everything shared while one run executes.
pub(super) struct Run<'a> {
    pub engine: &'a Engine,
    pub wf: &'a Workflow,
    pub svc: &'a Services<'a>,
    pub sink: &'a dyn EventSink,
    pub cancel: &'a CancelToken,
    /// More than one item: clipboard/pin/OCR/QR steps are ambiguous and skipped.
    pub batch: bool,
    /// Paths the user handed in; never deleted.
    pub protected: HashSet<PathBuf>,
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

impl<'a> Run<'a> {
    pub fn settings(&self) -> &'a Settings {
        &self.engine.settings
    }

    fn emit(&self, e: Event) {
        self.sink.event(e);
    }

    /// Executes `f` as a step: emits events, converts its result and any panic into a
    /// [`StepReport`]. Does nothing (reports "skipped: cancelled") once the run is cancelled.
    pub fn step(
        &self,
        item: Option<usize>,
        kind: StepKind,
        f: impl FnOnce() -> StepResult,
    ) -> StepReport {
        self.step_impl(item, kind, true, f)
    }

    /// Like [`step`](Self::step) but runs even after cancellation (bookkeeping such as the
    /// history entry for what *did* get saved or uploaded).
    pub fn step_always(
        &self,
        item: Option<usize>,
        kind: StepKind,
        f: impl FnOnce() -> StepResult,
    ) -> StepReport {
        self.step_impl(item, kind, false, f)
    }

    fn step_impl(
        &self,
        item: Option<usize>,
        kind: StepKind,
        honor_cancel: bool,
        f: impl FnOnce() -> StepResult,
    ) -> StepReport {
        if honor_cancel && self.cancel.is_cancelled() {
            return self.skipped(item, kind, SkipReason::Cancelled);
        }
        self.emit(Event::StepStarted { item, step: kind });
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(f));
        let duration = started.elapsed();
        let (status, detail) = match outcome {
            Ok(Ok(detail)) => (StepStatus::Succeeded, detail),
            Ok(Err(StepEnd::Failed(f))) => (StepStatus::Failed(f), None),
            Ok(Err(StepEnd::Skipped(r))) => (StepStatus::Skipped(r), None),
            Ok(Err(StepEnd::Cancelled)) => (StepStatus::Cancelled, None),
            Err(p) => {
                tracing::error!(step = %kind, "service panicked");
                (
                    StepStatus::Failed(StepFailure {
                        kind: FailureKind::Internal,
                        message: format!(
                            "internal error: a service panicked ({})",
                            panic_message(&*p)
                        ),
                        retryable: false,
                    }),
                    None,
                )
            }
        };
        self.emit(Event::StepFinished { item, step: kind, status: status.clone(), duration });
        StepReport { kind, item, status, detail, duration }
    }

    /// A step that does not run.
    pub fn skipped(&self, item: Option<usize>, kind: StepKind, reason: SkipReason) -> StepReport {
        let status = StepStatus::Skipped(reason);
        self.emit(Event::StepFinished {
            item,
            step: kind,
            status: status.clone(),
            duration: Duration::ZERO,
        });
        StepReport { kind, item, status, detail: None, duration: Duration::ZERO }
    }

    pub fn progress(&self, item: Option<usize>, step: StepKind, done: u64, total: Option<u64>) {
        self.emit(Event::StepProgress { item, step, done, total });
    }
}

impl Engine {
    /// An engine using `settings` and `naming`.
    pub fn new(settings: Settings, naming: Naming) -> Self {
        Self { settings: Arc::new(settings), naming }
    }

    /// The settings snapshot in use.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Screenshot entry point: acquires an image as `wf.input` says and runs the workflow.
    pub fn post_screenshot(
        &self,
        wf: &Workflow,
        svc: &Services<'_>,
        sink: &dyn EventSink,
        cancel: &CancelToken,
    ) -> RunReport {
        self.run(wf, Input::Auto, svc, sink, cancel)
    }

    /// Runs `wf` on an image captured elsewhere.
    pub fn post_image(
        &self,
        wf: &Workflow,
        image: Captured,
        svc: &Services<'_>,
        sink: &dyn EventSink,
        cancel: &CancelToken,
    ) -> RunReport {
        self.run(wf, Input::Image(image), svc, sink, cancel)
    }

    /// File entry point (shell menu, CLI, drag and drop): every path becomes one item.
    /// Folders are zipped or rejected per `post_file.folders`. Items run in parallel (at most
    /// `post_file.max_parallel_uploads` at a time); one failing item never stops the others;
    /// results are in input order.
    pub fn post_file(
        &self,
        wf: &Workflow,
        paths: Vec<PathBuf>,
        svc: &Services<'_>,
        sink: &dyn EventSink,
        cancel: &CancelToken,
    ) -> RunReport {
        self.run(wf, Input::Files(paths), svc, sink, cancel)
    }

    /// Video entry point: record now or take a finished recording.
    pub fn post_video(
        &self,
        wf: &Workflow,
        source: VideoSource,
        svc: &Services<'_>,
        sink: &dyn EventSink,
        cancel: &CancelToken,
    ) -> RunReport {
        let input = match source {
            VideoSource::Record { stop } => Input::Record { stop },
            VideoSource::Recorded(p) => Input::Recorded(p),
        };
        self.run(wf, input, svc, sink, cancel)
    }

    /// Runs `wf` on `input`, blocking until done. This is the one engine behind
    /// [`post_screenshot`](Self::post_screenshot), [`post_file`](Self::post_file) and
    /// [`post_video`](Self::post_video).
    ///
    /// Never panics and never returns an error: everything that goes wrong is a
    /// [`StepStatus::Failed`] in the [`RunReport`].
    pub fn run(
        &self,
        wf: &Workflow,
        input: Input,
        svc: &Services<'_>,
        sink: &dyn EventSink,
        cancel: &CancelToken,
    ) -> RunReport {
        let mut run =
            Run { engine: self, wf, svc, sink, cancel, batch: false, protected: HashSet::new() };
        sink.event(Event::RunStarted {
            workflow_id: wf.id.clone(),
            workflow_name: wf.name.clone(),
        });
        let mut run_steps: Vec<StepReport> = Vec::new();

        let items = self.acquire(&mut run, input, &mut run_steps);
        run.batch = items.len() > 1;

        let mut items = self.process_all(&run, items);
        run.after_upload_phase(&mut items, &mut run_steps);
        for item in &mut items {
            run.record_history(item);
        }

        let outcome = compute_outcome(&items, &run_steps, cancel);
        let report = RunReport {
            workflow_id: wf.id.clone(),
            outcome,
            items: items.into_iter().map(Item::into_report).collect(),
            steps: run_steps,
        };
        sink.event(Event::RunFinished { outcome });
        report
    }

    // ---- input ---------------------------------------------------------------------

    fn acquire(
        &self,
        run: &mut Run<'_>,
        input: Input,
        run_steps: &mut Vec<StepReport>,
    ) -> Vec<Item> {
        match input {
            Input::Image(captured) => vec![Self::image_item(0, captured)],
            Input::Files(paths) => Self::file_items(run, paths, run_steps),
            Input::Recorded(path) => {
                let mut item = Item::new(0, Origin::Recording, EntryKind::Video);
                item.input_path = Some(path.clone());
                item.local_path = Some(path);
                item.created = true;
                vec![item]
            }
            Input::Record { stop } => self.record(run, &stop, run_steps).into_iter().collect(),
            Input::Auto => match run.wf.input {
                InputKind::CaptureRegion
                | InputKind::CaptureFullscreen
                | InputKind::CaptureMonitor
                | InputKind::CaptureWindow
                | InputKind::CaptureLastRegion => {
                    self.capture(run, run_steps).into_iter().collect()
                }
                InputKind::Clipboard => Self::read_clipboard_input(run, run_steps),
                InputKind::RecordScreen | InputKind::RecordGif => {
                    Self::invalid_input(
                        run,
                        run_steps,
                        StepKind::Record,
                        "this is a recording workflow; start it with post_video (Input::Record) so it can be stopped",
                    );
                    Vec::new()
                }
                InputKind::Files => {
                    Self::invalid_input(
                        run,
                        run_steps,
                        StepKind::LoadFile,
                        "this workflow takes files; start it with post_file and a list of paths",
                    );
                    Vec::new()
                }
            },
        }
    }

    fn invalid_input(run: &Run<'_>, run_steps: &mut Vec<StepReport>, kind: StepKind, msg: &str) {
        run_steps.push(run.step(None, kind, || Err(StepEnd::fail(FailureKind::Invalid, msg))));
    }

    fn image_item(index: usize, captured: Captured) -> Item {
        let mut item = Item::new(index, Origin::Image, EntryKind::Image);
        item.frame = Some(captured.frame);
        item.window_title = captured.window_title;
        item.process_name = captured.process_name;
        item
    }

    fn capture(&self, run: &Run<'_>, run_steps: &mut Vec<StepReport>) -> Option<Item> {
        let target = match run.wf.input {
            InputKind::CaptureFullscreen => CaptureTarget::Fullscreen,
            InputKind::CaptureMonitor => CaptureTarget::Monitor,
            InputKind::CaptureWindow => CaptureTarget::Window,
            InputKind::CaptureLastRegion => CaptureTarget::LastRegion,
            _ => CaptureTarget::Region,
        };
        let cap = &self.settings.capture;
        let delay = Duration::from_millis(u64::from(cap.delay_ms));
        let req = CaptureRequest { target, include_cursor: cap.show_cursor, hdr: cap.hdr };
        let mut captured: Option<Captured> = None;
        let report = run.step(None, StepKind::Capture, || {
            if !delay.is_zero() && !run.cancel.sleep(delay) {
                return Err(StepEnd::Cancelled);
            }
            let c = run.svc.capturer.capture(&req, run.cancel)?;
            if !c.frame.is_sdr8() {
                return Err(StepEnd::fail(
                    FailureKind::Invalid,
                    format!(
                        "the capture backend returned a {:?}/{:?} frame; frames must be tone-mapped to 8-bit sRGB before they reach the workflow",
                        c.frame.format(),
                        c.frame.color_space()
                    ),
                ));
            }
            if c.frame.size().is_empty() {
                return Err(StepEnd::fail(FailureKind::Invalid, "the capture is empty (0 pixels)"));
            }
            let detail = format!("{}x{}", c.frame.width(), c.frame.height());
            captured = Some(c);
            Ok(Some(detail))
        });
        run_steps.push(report);
        captured.map(|c| Self::image_item(0, c))
    }

    fn read_clipboard_input(run: &mut Run<'_>, run_steps: &mut Vec<StepReport>) -> Vec<Item> {
        let mut content: Option<ClipboardContent> = None;
        let report = run.step(None, StepKind::ReadClipboard, || {
            let c = run.svc.clipboard.read()?;
            let detail = match &c {
                ClipboardContent::Image(_) => "image",
                ClipboardContent::Text(t) if t.trim().is_empty() => {
                    return Err(StepEnd::fail(FailureKind::Invalid, "the clipboard text is empty"));
                }
                ClipboardContent::Text(_) => "text",
                ClipboardContent::Files(f) if f.is_empty() => {
                    return Err(StepEnd::fail(FailureKind::Invalid, "the clipboard has no files"));
                }
                ClipboardContent::Files(_) => "files",
                ClipboardContent::Empty => {
                    return Err(StepEnd::fail(
                        FailureKind::Invalid,
                        "the clipboard is empty; copy an image, text or files first",
                    ));
                }
            };
            content = Some(c);
            Ok(Some(detail.to_owned()))
        });
        run_steps.push(report);
        match content {
            Some(ClipboardContent::Image(frame)) => {
                if frame.is_sdr8() {
                    vec![Self::image_item(0, Captured::new(frame))]
                } else {
                    run_steps.push(run.step(None, StepKind::ReadClipboard, || {
                        Err(StepEnd::fail(
                            FailureKind::Invalid,
                            "the clipboard image is not 8-bit sRGB",
                        ))
                    }));
                    Vec::new()
                }
            }
            Some(ClipboardContent::Text(text)) => {
                let mut item = Item::new(0, Origin::Text, EntryKind::Text);
                item.text = Some(text);
                vec![item]
            }
            Some(ClipboardContent::Files(paths)) => Self::file_items(run, paths, run_steps),
            Some(ClipboardContent::Empty) | None => Vec::new(),
        }
    }

    fn file_items(
        run: &mut Run<'_>,
        paths: Vec<PathBuf>,
        run_steps: &mut Vec<StepReport>,
    ) -> Vec<Item> {
        if paths.is_empty() {
            Self::invalid_input(run, run_steps, StepKind::LoadFile, "no files were given");
            return Vec::new();
        }
        run.protected = paths.iter().cloned().collect();
        paths
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let ext = extension_of(&p);
                let kind = if is_image_ext(ext.as_deref()) {
                    EntryKind::Image
                } else if is_video_ext(ext.as_deref()) {
                    EntryKind::Video
                } else {
                    EntryKind::File
                };
                let mut item = Item::new(i, Origin::UserFile, kind);
                item.input_path = Some(p.clone());
                item.local_path = Some(p);
                item
            })
            .collect()
    }

    fn record(
        &self,
        run: &Run<'_>,
        stop: &CancelToken,
        run_steps: &mut Vec<StepReport>,
    ) -> Option<Item> {
        let kind =
            if run.wf.input == InputKind::RecordGif { RecordKind::Gif } else { RecordKind::Video };
        let mut video = None;
        let report = run.step(None, StepKind::Record, || {
            let inputs = crate::pattern::NameInputs::default();
            let dir = run
                .save_dir(EntryKind::Video, &inputs)
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            let stem = run
                .file_name(&inputs, "")
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            run.svc.fs.create_dir_all(&dir)?;
            let req = RecordRequest {
                kind,
                output_dir: dir,
                file_stem: stem,
                include_cursor: self.settings.capture.show_cursor,
            };
            let session = run.svc.recorder.start(&req)?;
            // Wait for the user to stop (graceful) or for the run to be cancelled (abort).
            loop {
                if run.cancel.is_cancelled() {
                    session.abort();
                    return Err(StepEnd::Cancelled);
                }
                if stop.wait_timeout(Duration::from_millis(10)) {
                    break;
                }
            }
            let recorded = session.stop()?;
            let detail = format!("recorded {}", recorded.path.display());
            video = Some(recorded);
            Ok(Some(detail))
        });
        run_steps.push(report);
        video.map(|v| {
            let mut item = Item::new(0, Origin::Recording, EntryKind::Video);
            item.local_path = Some(v.path.clone());
            item.created = true;
            item.video = Some(v);
            item
        })
    }

    // ---- per-item phase ------------------------------------------------------------

    fn process_all(&self, run: &Run<'_>, items: Vec<Item>) -> Vec<Item> {
        let n = items.len();
        let workers = (self.settings.post_file.max_parallel_uploads.clamp(1, 16) as usize).min(n);
        if workers <= 1 {
            return items.into_iter().map(|i| run.process_item(i)).collect();
        }
        let pending: Vec<Mutex<Option<Item>>> =
            items.into_iter().map(|i| Mutex::new(Some(i))).collect();
        let done: Vec<Mutex<Option<Item>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for w in 0..workers {
                let spawned = std::thread::Builder::new()
                    .name(format!("ssx-item-{w}"))
                    .spawn_scoped(scope, || {
                        loop {
                            let i = next.fetch_add(1, Ordering::SeqCst);
                            let Some(slot) = pending.get(i) else { break };
                            let taken = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                            if let Some(item) = taken {
                                let result = run.process_item(item);
                                *done[i].lock().unwrap_or_else(PoisonError::into_inner) =
                                    Some(result);
                            }
                        }
                    });
                if let Err(e) = spawned {
                    tracing::warn!(error = %e, "could not start a worker thread; continuing with fewer");
                }
            }
        });
        // Anything a failed spawn left behind is processed here.
        pending
            .into_iter()
            .zip(done)
            .enumerate()
            .map(|(index, (p, d))| {
                let finished = d.into_inner().unwrap_or_else(PoisonError::into_inner);
                if let Some(item) = finished {
                    item
                } else {
                    let left = p.into_inner().unwrap_or_else(PoisonError::into_inner);
                    if let Some(item) = left {
                        run.process_item(item)
                    } else {
                        // A worker took it but died outside a step: unreachable in
                        // practice because steps catch panics. Report, never lose it.
                        let mut lost = Item::new(index, Origin::UserFile, EntryKind::File);
                        lost.push(run.skipped(
                            Some(index),
                            StepKind::LoadFile,
                            SkipReason::ItemFailed,
                        ));
                        lost
                    }
                }
            })
            .collect()
    }
}

/// Derives the run outcome from the items and run-level steps.
pub(super) fn compute_outcome(
    items: &[Item],
    run_steps: &[StepReport],
    cancel: &CancelToken,
) -> Outcome {
    let saw_cancel = items.iter().flat_map(|i| i.steps.iter()).chain(run_steps.iter()).any(|s| {
        matches!(s.status, StepStatus::Cancelled | StepStatus::Skipped(SkipReason::Cancelled))
    });
    if saw_cancel
        && (cancel.is_cancelled() || items.is_empty() || items.iter().all(|i| i.cancelled))
    {
        return Outcome::Cancelled;
    }
    if items.is_empty() {
        return Outcome::Failed;
    }
    let outcomes: Vec<Outcome> = items.iter().map(Item::item_outcome).collect();
    if outcomes.iter().all(|o| *o == Outcome::Cancelled) {
        return Outcome::Cancelled;
    }
    let run_level_failure = run_steps.iter().any(|s| {
        s.status.is_failure() && s.kind.importance() != super::report::Importance::Optional
    });
    if outcomes.iter().all(|o| *o == Outcome::Success) && !run_level_failure {
        Outcome::Success
    } else if outcomes.iter().any(|o| matches!(o, Outcome::Success | Outcome::PartialSuccess)) {
        Outcome::PartialSuccess
    } else {
        Outcome::Failed
    }
}
