//! The real services and the [`JobRunner`] that drives the workflow engine.
//!
//! A [`Runtime`] owns everything that outlives a settings change (the history database, the
//! overlay selector, the recorder and its controller, the notifier, the `%i` counter) and a
//! swappable [`Snapshot`] of everything derived from the settings (the uploaders, the
//! capturer, the engine). [`Runtime::apply_settings`] builds a complete new snapshot first and
//! swaps it in with one pointer store, so a run never sees half-old, half-new configuration; runs
//! already in flight keep the snapshot they started with.
//!
//! [`EngineRunner`] is the production [`JobRunner`]: it turns a [`Job`] into the right engine
//! call (`post_screenshot`, `post_file`, `post_video`), maps the engine's events to [`UiEvent`]s
//! and its report to an IPC [`RunSummary`]. Frames live only inside the engine call: nothing
//! here keeps a captured image after the run ends, so memory use falls back after every run.

use std::{
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use ssx_core::{
    history::History,
    ipc::{ItemSummary, RunSummary},
    pattern::FileCounter,
    settings::{AfterCapture, InputKind, Paths, Settings, Workflow},
    workflow::{
        CancelToken, Engine, Event, EventSink, Importance, Naming, Outcome, RunReport, Services,
        StepKind, StepStatus, VideoSource,
    },
};
use ssx_platform::BackendKind;
use ssx_services::{
    OverlaySelector, PickMode, ProductionOptions, ProductionServices, RegionSelector,
};

use crate::{
    daemon::{Job, JobRunner, JobSpec, RunOutput, plain_summary},
    events::{UiEvent, UiSink},
    notify::DaemonNotifier,
    recording::{RecordingController, TrackedRecorder},
};

/// Everything derived from one settings value.
pub struct Snapshot {
    /// The settings.
    pub settings: Arc<Settings>,
    services: ProductionServices,
    engine: Engine,
    naming: Naming,
    history: Option<Arc<History>>,
    notifier: Arc<DaemonNotifier>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").finish_non_exhaustive()
    }
}

impl Snapshot {
    #[allow(clippy::too_many_arguments)] // one call site per constructor; a struct would only rename them
    fn build(
        settings: Settings,
        paths: &Paths,
        backend: Option<BackendKind>,
        selector: &Option<Arc<OverlaySelector>>,
        recorder: &Arc<dyn ssx_core::workflow::Recorder>,
        naming: &Naming,
        history: &Option<Arc<History>>,
        notifier: &Arc<DaemonNotifier>,
    ) -> Self {
        let selector: Option<Arc<dyn RegionSelector>> =
            selector.clone().map(|s| s as Arc<dyn RegionSelector>);
        let services = ProductionServices::new(
            &settings,
            paths,
            ProductionOptions {
                backend,
                // The daemon keeps running, so it can keep serving the clipboard itself.
                prefer_external_clipboard: false,
                selector,
                recorder: Some(Arc::clone(recorder)),
                ..ProductionOptions::default()
            },
        );
        let history = history.clone().filter(|_| settings.history.enabled);
        Self {
            engine: Engine::new(settings.clone(), naming.clone()),
            settings: Arc::new(settings),
            services,
            naming: naming.clone(),
            history,
            notifier: Arc::clone(notifier),
        }
    }

    /// The bundle the engine takes, with the daemon's notifier plugged in.
    pub fn services(&self) -> Services<'_> {
        let mut s = self.services.services(self.history.as_deref());
        s.notifier = &*self.notifier;
        s
    }

    /// The engine for these settings.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// An engine with modified settings (a delay override, images through the editor) that
    /// shares the naming sources, so file names and the `%i` counter stay consistent.
    pub fn engine_with(&self, settings: Settings) -> Engine {
        Engine::new(settings, self.naming.clone())
    }

    /// The real services (for `Status` and diagnostics).
    pub fn production(&self) -> &ProductionServices {
        &self.services
    }
}

/// Long-lived state. See the module docs.
pub struct Runtime {
    current: RwLock<Arc<Snapshot>>,
    paths: Paths,
    backend: Option<BackendKind>,
    history: Option<Arc<History>>,
    naming: Naming,
    notifier: Arc<DaemonNotifier>,
    selector: Option<Arc<OverlaySelector>>,
    recorder: Arc<dyn ssx_core::workflow::Recorder>,
    #[cfg(feature = "record")]
    service_recorder: Option<Arc<ssx_services::record::ServiceRecorder>>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("paths", &self.paths).finish_non_exhaustive()
    }
}

/// How to build a [`Runtime`].
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Directories.
    pub paths: Paths,
    /// Forced capture backend.
    pub backend: Option<BackendKind>,
    /// The overlay (None: region capture is unavailable).
    pub selector: Option<Arc<OverlaySelector>>,
    /// The notifier.
    pub notifier: Arc<DaemonNotifier>,
    /// The recording controller shared with the supervisor.
    pub controller: Arc<RecordingController>,
}

impl std::fmt::Debug for RuntimeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOptions").field("paths", &self.paths).finish_non_exhaustive()
    }
}

impl Runtime {
    /// Opens the history and builds the first snapshot.
    pub fn new(opts: RuntimeOptions, settings: Settings) -> Self {
        let RuntimeOptions { paths, backend, selector, notifier, controller } = opts;
        if let Err(e) = std::fs::create_dir_all(&paths.data_dir) {
            tracing::warn!("cannot create {}: {e}", paths.data_dir.display());
        }
        let history = match History::open(&paths.history_db()) {
            Ok(h) => Some(Arc::new(h)),
            Err(e) => {
                tracing::warn!("history is unavailable, runs will not be recorded: {e}");
                None
            }
        };
        let naming = Naming::system(Arc::new(FileCounter::new(paths.counter_file())));

        #[cfg(feature = "record")]
        let (recorder, service_recorder): (
            Arc<dyn ssx_core::workflow::Recorder>,
            Option<Arc<ssx_services::record::ServiceRecorder>>,
        ) = {
            use ssx_services::record::{RecorderOptions, ServiceRecorder};
            let real = Arc::new(ServiceRecorder::new(RecorderOptions {
                backend,
                // A recording must survive the daemon dying: fragmented MP4 is playable up
                // to the last fragment even after a crash or a kill.
                mp4: ssx_services::record::Mp4Mode::Fragmented,
                selector: selector.clone(),
                ..RecorderOptions::default()
            }));
            (
                Arc::new(TrackedRecorder::new(real.clone(), Arc::clone(&controller))),
                Some(real),
            )
        };
        #[cfg(not(feature = "record"))]
        let recorder: Arc<dyn ssx_core::workflow::Recorder> = Arc::new(TrackedRecorder::new(
            Arc::new(ssx_services::stubs::StubRecorder),
            Arc::clone(&controller),
        ));

        let first = Arc::new(Snapshot::build(
            settings, &paths, backend, &selector, &recorder, &naming, &history, &notifier,
        ));
        Self {
            current: RwLock::new(first),
            paths,
            backend,
            history,
            naming,
            notifier,
            selector,
            recorder,
            #[cfg(feature = "record")]
            service_recorder,
        }
    }

    fn build(&self, settings: Settings) -> Arc<Snapshot> {
        Arc::new(Snapshot::build(
            settings,
            &self.paths,
            self.backend,
            &self.selector,
            &self.recorder,
            &self.naming,
            &self.history,
            &self.notifier,
        ))
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The current settings.
    pub fn settings(&self) -> Arc<Settings> {
        Arc::clone(&self.snapshot().settings)
    }

    /// Builds services for `settings` and swaps them in atomically.
    pub fn apply_settings(&self, settings: Settings) {
        let next = self.build(settings);
        *self.current.write().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    /// The directories.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The overlay selector, if the helper was found.
    pub fn selector(&self) -> Option<&Arc<OverlaySelector>> {
        self.selector.as_ref()
    }

    /// The history database (for the history window / `Status`).
    pub fn history(&self) -> Option<&Arc<History>> {
        self.history.as_ref()
    }

    /// The recorder, if this build has one.
    #[cfg(feature = "record")]
    pub fn service_recorder(&self) -> Option<&Arc<ssx_services::record::ServiceRecorder>> {
        self.service_recorder.as_ref()
    }

    /// The directory captures are saved in.
    pub fn captures_dir(&self) -> std::path::PathBuf {
        self.settings().general.resolve_save_dir()
    }
}

/// The phrase shown in the tray while `step` runs; `None` for steps too quick to mention.
pub fn step_text(step: StepKind) -> Option<&'static str> {
    Some(match step {
        StepKind::Capture => "Capturing...",
        StepKind::Record => "Recording...",
        StepKind::OpenEditor => "Editing...",
        StepKind::Upload => "Uploading...",
        StepKind::Zip => "Zipping...",
        StepKind::ShortenUrl => "Shortening the link...",
        StepKind::Ocr => "Recognising text...",
        StepKind::SaveAsDialog => "Waiting for a file name...",
        _ => return None,
    })
}

/// Forwards engine events to the UI and remembers whether the workflow notified the user.
struct EngineSink<'a> {
    run_id: u64,
    ui: &'a dyn UiSink,
    notified: AtomicBool,
}

impl EventSink for EngineSink<'_> {
    fn event(&self, event: Event) {
        match event {
            Event::StepStarted { step, .. } => {
                if let Some(text) = step_text(step) {
                    self.ui.emit(UiEvent::Step { run_id: self.run_id, text: text.to_owned() });
                }
            }
            Event::StepFinished { step: StepKind::ShowNotification, status, .. }
                if status.is_success() =>
            {
                self.notified.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

/// The IPC summary of an engine report.
pub fn summarize(run_id: u64, report: &RunReport) -> RunSummary {
    let mut items: Vec<ItemSummary> = report
        .items
        .iter()
        .map(|i| {
            let error = i
                .steps
                .iter()
                .filter(|s| s.kind.importance() != Importance::Optional)
                .find_map(|s| match &s.status {
                    StepStatus::Failed(f) => Some(format!("{}: {}", s.kind, f.message)),
                    _ => None,
                });
            ItemSummary {
                path: i.local_path.clone(),
                url: i.short_url.clone().or_else(|| i.url.clone()),
                error,
            }
        })
        .collect();
    // Run-level failures (the input could not be acquired) belong to no item.
    for step in &report.steps {
        if step.kind.importance() != Importance::Optional
            && let StepStatus::Failed(f) = &step.status
        {
            items.push(ItemSummary {
                path: None,
                url: None,
                error: Some(format!("{}: {}", step.kind, f.message)),
            });
        }
    }
    RunSummary { run_id, outcome: report.outcome, message: report.summary(), items }
}

/// The production [`JobRunner`].
pub struct EngineRunner {
    rt: Arc<Runtime>,
}

impl std::fmt::Debug for EngineRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineRunner").finish_non_exhaustive()
    }
}

impl EngineRunner {
    /// A runner over `rt`.
    pub fn new(rt: Arc<Runtime>) -> Self {
        Self { rt }
    }

    fn failed(run_id: u64, message: impl Into<String>) -> RunOutput {
        RunOutput { summary: plain_summary(run_id, Outcome::Failed, message), notified: false }
    }

    fn workflow_input_error(wf: &Workflow) -> Option<String> {
        (wf.input == InputKind::Files).then(|| {
            format!(
                "workflow {:?} takes files: use the tray's Upload files... or `ssx post-file`",
                wf.name
            )
        })
    }
}

impl JobRunner for EngineRunner {
    fn run(&self, job: &Job, ui: &dyn UiSink) -> RunOutput {
        let snap = self.rt.snapshot();
        let svc = snap.services();
        let sink = EngineSink { run_id: job.run_id, ui, notified: AtomicBool::new(false) };
        let report = match &job.spec {
            JobSpec::Workflow { workflow, delay_ms, mode } => {
                if let Some(msg) = Self::workflow_input_error(workflow) {
                    return Self::failed(job.run_id, msg);
                }
                if let Some(sel) = self.rt.selector() {
                    sel.set_mode(mode.map_or(PickMode::Rect, crate::requests::pick_mode));
                }
                let engine = match delay_ms {
                    Some(d) => {
                        let mut s = (*snap.settings).clone();
                        s.capture.delay_ms = *d;
                        snap.engine_with(s)
                    }
                    None => snap.engine().clone(),
                };
                if workflow.input.is_recording() {
                    // Started as a plain workflow (`RunWorkflow` on a record workflow via an
                    // older path): still needs a stop token.
                    engine.post_video(
                        workflow,
                        VideoSource::Record { stop: job.stop.clone() },
                        &svc,
                        &sink,
                        &job.cancel,
                    )
                } else {
                    engine.post_screenshot(workflow, &svc, &sink, &job.cancel)
                }
            }
            JobSpec::Files { workflow, paths, edit_first } => {
                let mut wf = workflow.clone();
                let mut settings = (*snap.settings).clone();
                if *edit_first {
                    if !wf.after_capture.contains(&AfterCapture::OpenEditor) {
                        wf.after_capture.insert(0, AfterCapture::OpenEditor);
                    }
                    settings.post_file.images_through_editor = true;
                }
                let engine =
                    if *edit_first { snap.engine_with(settings) } else { snap.engine().clone() };
                engine.post_file(&wf, paths.clone(), &svc, &sink, &job.cancel)
            }
            JobSpec::Record { workflow, plan, .. } => {
                #[cfg(feature = "record")]
                {
                    if let Some(rec) = self.rt.service_recorder() {
                        rec.set_plan(ssx_services::record::RecordingPlan {
                            target: plan.target.clone(),
                            audio: plan.audio,
                        });
                        rec.set_cancel(job.cancel.clone());
                    }
                }
                #[cfg(not(feature = "record"))]
                let _ = plan;
                if let Some(sel) = self.rt.selector() {
                    sel.set_mode(PickMode::Rect);
                }
                snap.engine().post_video(
                    workflow,
                    VideoSource::Record { stop: job.stop.clone() },
                    &svc,
                    &sink,
                    &job.cancel,
                )
            }
        };
        RunOutput {
            summary: summarize(job.run_id, &report),
            notified: sink.notified.load(Ordering::SeqCst),
        }
    }
}

/// Shows where the daemon keeps its files, for the log at start-up.
pub fn describe_paths(paths: &Paths, settings_file: &Path) -> String {
    format!(
        "config {} (settings {}), data {}",
        paths.config_dir.display(),
        settings_file.display(),
        paths.data_dir.display()
    )
}

/// A cancel token that is already cancelled (used to abort work at shutdown paths).
pub fn cancelled() -> CancelToken {
    let t = CancelToken::new();
    t.cancel();
    t
}
