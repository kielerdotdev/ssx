//! Assembling the daemon and running it.
//!
//! ```text
//!  hotkeys ─┐                   ┌──────────── ssx-ipc server (per-connection threads)
//!  tray ────┼─▶ App::handle_* ──┤                    │
//!  IPC ─────┘        │          │        IpcHandler ─┴─▶ Coalescer (PostFiles)
//!                    ▼          ▼                              │
//!               Supervisor ◀────┴──────────────────────────────┘
//!                    │  (admission, queue, cancel)
//!                    ▼
//!               EngineRunner ─▶ ssx_core::workflow::Engine ─▶ services (capture, overlay,
//!                    │                                         upload, clipboard, recorder ...)
//!                    ▼
//!                 UiHandle ─▶ tray refresh + notifications
//! ```
//!
//! [`App`] owns the pieces and implements what they call back into ([`AppControl`] for the IPC
//! handler; `handle_action` for the tray and hotkeys). [`run`] is the process: acquire the
//! single-instance lock, start everything, wait for a quit request (tray, IPC, signal), then shut
//! down in the order that keeps data safe: hand pending files to the supervisor, stop and
//! finalise a recording, give running uploads a moment, cancel the rest, stop accepting
//! connections, remove the tray icon.
//!
//! Threads and idle cost: the IPC accept loop, the coalescer, the UI worker, the settings
//! watcher, the tray and the signal handler all *block* (on sockets, condition variables or
//! channels). The hotkey thread wakes four times a second to look for commands. Nothing spins.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use ssx_core::{
    ipc::{DaemonStatus, Request, RequestEnvelope, Response, ResponseEnvelope, ShowTarget},
    settings::{CONFIG_DIR_ENV, Paths, Settings},
    workflow::NotificationLevel,
};
use ssx_ipc::{Acquired, Instance};
use ssx_platform::BackendKind;
use ssx_services::{OverlaySelector, helpers};

use crate::{
    cli::Args,
    clock::{Clock, SystemClock},
    coalesce::CoalesceConfig,
    daemon::{Limits, Origin, ShutdownGrace, Supervisor},
    events::UiSink,
    hotkeys_glue::{HotkeyControl, HotkeyStatus, HotkeyTarget, plan},
    ids::RunIds,
    ipc_server::{AppControl, FilesCoalescer, IpcHandler, files_coalescer},
    logging,
    menu::{Action, HotkeyView},
    notify::{DaemonNotifier, NoticeStore},
    recording::RecordingController,
    reload::{ReloadOutcome, ReloadState, SettingsWatcher, read_settings_text},
    requests::job_for_workflow,
    runtime::{EngineRunner, Runtime, RuntimeOptions, describe_paths},
    tray::{ActionSink, TrayHandle},
    ui::{TrayCell, UiDeps, UiHandle, UiInner, build_view},
};

/// The application id of the single-instance lock and socket (what `ssx` forwards to).
pub const APP_ID: &str = "ssx";

/// Why the daemon could not start.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The directories could not be determined.
    #[error("{0}")]
    Paths(String),
    /// The settings file cannot be used and cannot be set aside.
    #[error("{0}")]
    Settings(String),
    /// The IPC endpoint could not be created.
    #[error("cannot start the IPC server: {0}")]
    Ipc(String),
}

/// Resolves the directories: `--config-dir`, else `SSX_CONFIG_DIR`, else the platform default.
pub fn resolve_paths(config_dir: Option<&Path>) -> Result<Paths, AppError> {
    let over = config_dir.map(|d| d.as_os_str().to_owned());
    Paths::discover_with(|k| {
        if k == CONFIG_DIR_ENV {
            over.clone().or_else(|| std::env::var_os(k))
        } else {
            std::env::var_os(k)
        }
    })
    .map_err(|e| AppError::Paths(format!("{e} (pass --config-dir DIR or set SSX_CONFIG_DIR)")))
}

/// The capture backend named by `--backend` or `SSX_BACKEND`.
pub fn resolve_backend(flag: Option<&str>) -> Option<BackendKind> {
    let name = flag.map(str::to_owned).or_else(|| std::env::var("SSX_BACKEND").ok())?;
    let kind = BackendKind::from_name(name.trim());
    if kind.is_none() {
        tracing::warn!(
            "unknown capture backend {name:?} ignored (use windows, wayland, portal or x11)"
        );
    }
    kind
}

/// A one-shot latch: `quit()` from anywhere, `wait()` on the thread that owns shutdown.
#[derive(Debug, Default)]
struct Latch {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Latch {
    fn set(&self) {
        *self.flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    fn is_set(&self) -> bool {
        *self.flag.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait(&self) {
        let mut g = self.flag.lock().unwrap_or_else(PoisonError::into_inner);
        while !*g {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The daemon. See the module docs.
pub struct App {
    args: Args,
    paths: Paths,
    settings_file: PathBuf,
    rt: Arc<Runtime>,
    sup: Arc<Supervisor>,
    files: Arc<FilesCoalescer>,
    ui: Arc<UiHandle>,
    notifier: Arc<DaemonNotifier>,
    notices: NoticeStore,
    tray: TrayCell,
    tray_running: AtomicBool,
    hotkeys: Mutex<Option<Box<dyn HotkeyControl>>>,
    hotkeys_enabled: AtomicBool,
    hotkey_status: Mutex<HotkeyStatus>,
    last_hotkey_problems: Mutex<Vec<String>>,
    reload: Mutex<ReloadState>,
    settings_problem: Mutex<Option<String>>,
    watcher: Mutex<Option<SettingsWatcher>>,
    quit: Latch,
    started: Instant,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("paths", &self.paths).finish_non_exhaustive()
    }
}

/// Lets long-lived callbacks reach the app without keeping it alive.
struct Control(Weak<App>);

impl AppControl for Control {
    fn settings(&self) -> Arc<Settings> {
        self.0.upgrade().map_or_else(|| Arc::new(Settings::default()), |a| a.rt.settings())
    }
    fn quit(&self) {
        if let Some(a) = self.0.upgrade() {
            a.request_quit();
        }
    }
    fn show(&self, target: ShowTarget) {
        if let Some(a) = self.0.upgrade() {
            a.show(target);
        }
    }
    fn reload_settings(&self) {
        if let Some(a) = self.0.upgrade() {
            a.reload_settings();
        }
    }
    fn status(&self) -> DaemonStatus {
        self.0.upgrade().map_or_else(
            || DaemonStatus {
                app_version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: std::process::id(),
                uptime_secs: 0,
                tray: false,
                hotkey_backend: "none".to_owned(),
                hotkeys_registered: 0,
                hotkey_problems: Vec::new(),
                active_runs: Vec::new(),
                queued_runs: 0,
                recording: ssx_core::ipc::RecordingStatus::default(),
                config_dir: String::new(),
                settings_problem: None,
            },
            |a| a.daemon_status(),
        )
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts a helper program detached from us (its exit is reaped by a tiny thread).
fn spawn_helper(path: &Path, args: &[&str]) -> std::io::Result<()> {
    let mut child =
        std::process::Command::new(path).args(args).stdin(std::process::Stdio::null()).spawn()?;
    let name = path.display().to_string();
    let _ = std::thread::Builder::new().name("ssx-helper-reaper".into()).spawn(move || match child
        .wait()
    {
        Ok(s) if !s.success() => tracing::debug!("{name} ended with {s}"),
        _ => {}
    });
    Ok(())
}

impl App {
    /// Builds everything except the IPC server, the tray and the hotkeys.
    pub fn new(args: Args, paths: Paths) -> Result<Arc<Self>, AppError> {
        let settings_file = paths.settings_file();
        std::fs::create_dir_all(&paths.config_dir)
            .and_then(|()| std::fs::create_dir_all(&paths.data_dir))
            .map_err(|e| {
                AppError::Paths(format!("cannot create the config/data directories: {e}"))
            })?;
        let loaded = Settings::load_or_recover(&settings_file)
            .map_err(|e| AppError::Settings(e.to_string()))?;
        for w in &loaded.warnings {
            tracing::warn!("settings: {w}");
        }
        let settings = loaded.settings;
        for issue in settings.validate() {
            tracing::warn!("settings: {issue}");
        }
        let current_text = read_settings_text(&settings_file).ok().flatten();

        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let notifier = Arc::new(DaemonNotifier::system());
        let selector = OverlaySelector::discover().map(Arc::new);
        match &selector {
            Some(s) => tracing::info!("selection overlay: {}", s.helper().display()),
            None => tracing::info!(
                "selection overlay: {}",
                OverlaySelector::discovery().describe(ssx_services::overlay::HELPER_ENV)
            ),
        }
        let settings_ui = helpers::discover("ssx-settings-ui", "SSX_SETTINGS_UI");

        let tray_cell: TrayCell = Arc::default();
        let inner = UiInner {
            overlay_available: selector.is_some(),
            record_supported: cfg!(feature = "record"),
            settings_ui_available: settings_ui.path().is_some(),
            hotkeys: HotkeyView { enabled: !args.no_hotkeys, backend: None, problems: 0 },
            ..UiInner::default()
        };

        let backend = resolve_backend(args.backend.as_deref());
        // The UI worker needs the recording controller (elapsed time) and the controller needs
        // a sink (phase changes): the controller gets a late-bound sink, bound once the UI
        // worker exists.
        let late = Arc::new(LateSink::default());
        let recording = Arc::new(RecordingController::new(Arc::clone(&clock), late.clone()));
        let rt = Arc::new(Runtime::new(
            RuntimeOptions {
                paths: paths.clone(),
                backend,
                selector,
                notifier: Arc::clone(&notifier),
                controller: Arc::clone(&recording),
            },
            settings,
        ));
        let ui = Arc::new(UiHandle::spawn(UiDeps {
            rt: Arc::clone(&rt),
            recording: Arc::clone(&recording),
            notifier: Arc::clone(&notifier),
            tray: Arc::clone(&tray_cell),
            initial: inner,
        }));
        late.bind(ui.clone());

        let ids = RunIds::new();
        let sup = Supervisor::new(
            Arc::new(EngineRunner::new(Arc::clone(&rt))),
            ui.clone(),
            Arc::clone(&clock),
            ids.clone(),
            Arc::clone(&recording),
            Limits::default(),
        );

        let started = Instant::now();
        let notices = NoticeStore::in_dir(&paths.data_dir);
        Ok(Arc::new_cyclic(|weak: &Weak<App>| {
            let control: Arc<dyn AppControl> = Arc::new(Control(weak.clone()));
            let files = Arc::new(files_coalescer(
                CoalesceConfig::default(),
                ids,
                clock,
                Arc::clone(&sup),
                control,
                ui.clone(),
            ));
            App {
                hotkeys_enabled: AtomicBool::new(!args.no_hotkeys),
                args,
                paths,
                settings_file,
                rt,
                sup,
                files,
                ui,
                notifier,
                notices,
                tray: tray_cell,
                tray_running: AtomicBool::new(false),
                hotkeys: Mutex::new(None),
                hotkey_status: Mutex::new(HotkeyStatus::default()),
                last_hotkey_problems: Mutex::new(Vec::new()),
                reload: Mutex::new(ReloadState::with_current(current_text.as_deref())),
                settings_problem: Mutex::new(None),
                watcher: Mutex::new(None),
                quit: Latch::default(),
                started,
            }
        }))
    }

    /// The directories.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// `--no-tray` was given.
    pub fn no_tray(&self) -> bool {
        self.args.no_tray
    }

    /// `--no-hotkeys` was given.
    pub fn no_hotkeys(&self) -> bool {
        self.args.no_hotkeys
    }

    /// The supervisor.
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.sup
    }

    /// The runtime.
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.rt
    }

    /// The UI handle (tests).
    pub fn ui(&self) -> &Arc<UiHandle> {
        &self.ui
    }

    // ---- quitting ---------------------------------------------------------------------------

    /// Asks the daemon to exit (tray Quit, IPC `Quit`, a signal). Returns at once.
    pub fn request_quit(&self) {
        tracing::info!("quit requested");
        self.quit.set();
    }

    /// `true` once a quit was requested.
    pub fn quitting(&self) -> bool {
        self.quit.is_set()
    }

    /// Blocks until a quit is requested.
    pub fn wait_quit(&self) {
        self.quit.wait();
    }

    // ---- actions ----------------------------------------------------------------------------

    /// Handles a clicked tray entry or a hotkey. Never blocks: long work is handed to threads.
    pub fn handle_action(self: &Arc<Self>, action: Action, origin: Origin) {
        tracing::debug!(?action, ?origin, "action");
        match action {
            Action::RunWorkflow(id) => {
                let settings = self.rt.settings();
                let Some(wf) = settings.workflow_by_id(&id).cloned() else {
                    self.say(
                        NotificationLevel::Warning,
                        "ssx",
                        &format!("the workflow {id:?} no longer exists"),
                    );
                    return;
                };
                if wf.input == ssx_core::settings::InputKind::Files {
                    self.upload_files_dialog();
                    return;
                }
                match job_for_workflow(wf, origin) {
                    // Refusals were already announced by the supervisor for people.
                    Ok(job) => drop(self.sup.submit(job)),
                    Err(r) => self.say(NotificationLevel::Warning, "ssx", &r.to_string()),
                }
            }
            Action::StopRecording => {
                if let Err(e) = self.sup.stop_recording() {
                    self.say(NotificationLevel::Warning, "ssx", &e.to_string());
                }
            }
            Action::CancelCurrent => {
                self.sup.cancel_interactive();
            }
            Action::UploadFiles => self.upload_files_dialog(),
            Action::OpenEditor => self.open_helper(
                "ssx-editor-ui",
                "SSX_EDITOR_UI",
                &[],
                "The image editor (ssx-editor-ui) was not found next to ssx or on PATH.",
            ),
            Action::OpenHistory => self.show(ShowTarget::History),
            Action::OpenSettings => self.show(ShowTarget::Settings),
            Action::ToggleHotkeys => {
                let now = !self.hotkeys_enabled.load(Ordering::SeqCst);
                self.hotkeys_enabled.store(now, Ordering::SeqCst);
                tracing::info!(enabled = now, "hotkeys toggled from the tray");
                self.apply_hotkeys();
            }
            Action::OpenCapturesFolder => {
                let dir = self.rt.captures_dir();
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = opener::open(&dir) {
                    self.say(
                        NotificationLevel::Warning,
                        "ssx",
                        &format!("cannot open {}: {e}", dir.display()),
                    );
                }
            }
            Action::Quit => self.request_quit(),
        }
    }

    /// Handles a pressed hotkey.
    pub fn handle_hotkey(self: &Arc<Self>, target: HotkeyTarget) {
        match target {
            HotkeyTarget::Workflow(id) => {
                self.handle_action(Action::RunWorkflow(id), Origin::Hotkey)
            }
            HotkeyTarget::OpenHistory => self.handle_action(Action::OpenHistory, Origin::Hotkey),
            HotkeyTarget::OpenSettings => self.handle_action(Action::OpenSettings, Origin::Hotkey),
        }
    }

    fn say(&self, level: NotificationLevel, title: &str, body: &str) {
        self.notifier.say(level, title, body);
    }

    fn open_helper(&self, base: &str, env: &str, args: &[&str], missing: &str) {
        match helpers::discover(base, env).into_path() {
            Some(p) => {
                if let Err(e) = spawn_helper(&p, args) {
                    self.say(
                        NotificationLevel::Error,
                        "ssx",
                        &format!("cannot start {}: {e}", p.display()),
                    );
                }
            }
            None => self.say(NotificationLevel::Warning, "ssx", missing),
        }
    }

    /// Settings / history / editor windows (IPC `Show`, tray, hotkeys).
    pub fn show(&self, target: ShowTarget) {
        match target {
            ShowTarget::Editor => self.open_helper(
                "ssx-editor-ui",
                "SSX_EDITOR_UI",
                &[],
                "The image editor (ssx-editor-ui) was not found next to ssx or on PATH.",
            ),
            ShowTarget::History => self.open_helper(
                "ssx-settings-ui",
                "SSX_SETTINGS_UI",
                &["--page", "history"],
                "The history window needs ssx-settings-ui, which was not found. `ssx history list` shows the history in a terminal.",
            ),
            ShowTarget::Settings => {
                if let Some(p) = helpers::discover("ssx-settings-ui", "SSX_SETTINGS_UI").into_path() {
                    if let Err(e) = spawn_helper(&p, &[]) {
                        self.say(NotificationLevel::Error, "ssx", &format!("cannot start {}: {e}", p.display()));
                    }
                    return;
                }
                // No settings window: open the file, creating it with the defaults first so
                // there is something to edit.
                if !self.settings_file.exists()
                    && let Err(e) = Settings::default().save(&self.settings_file)
                {
                    tracing::warn!("cannot create {}: {e}", self.settings_file.display());
                }
                if let Err(e) = opener::open(&self.settings_file) {
                    self.say(
                        NotificationLevel::Warning,
                        "ssx",
                        &format!("cannot open {}: {e}", self.settings_file.display()),
                    );
                }
            }
        }
    }

    /// Picks files with the native chooser (on its own thread) and uploads them through the
    /// same coalescing path as the file-manager entries.
    fn upload_files_dialog(self: &Arc<Self>) {
        let app = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("ssx-file-dialog".into()).spawn(move || {
            match pick_files() {
                Ok(paths) if paths.is_empty() => {}
                Ok(paths) => {
                    app.files.add(&ssx_core::ipc::PostAction::Upload, paths, None);
                }
                Err(why) => app.say(NotificationLevel::Warning, "Upload files", &why),
            }
        });
        if let Err(e) = spawned {
            tracing::error!("cannot start the file dialog thread: {e}");
        }
    }

    // ---- hotkeys ----------------------------------------------------------------------------

    /// Gives the app its hotkey owner and registers the current plan.
    pub fn attach_hotkeys(self: &Arc<Self>, control: Box<dyn HotkeyControl>) {
        *lock(&self.hotkeys) = Some(control);
        self.apply_hotkeys();
    }

    /// (Re-)registers the hotkeys for the current settings and reports the outcome.
    pub fn apply_hotkeys(&self) {
        let guard = lock(&self.hotkeys);
        let Some(control) = guard.as_ref() else { return };
        let settings = self.rt.settings();
        let plan = plan(&settings, "ssx");
        let enabled = self.hotkeys_enabled.load(Ordering::SeqCst);
        let status = control.apply(plan.clone(), enabled);
        drop(guard);

        if let Some(u) = &status.unavailable {
            tracing::info!("hotkeys: not registered here: {}", u.reason);
            if !self.args.no_hotkeys {
                let mut body = format!(
                    "ssx cannot grab global keys on this desktop ({}). Run `ssx hotkeys install` \
                     to bind them in your desktop's own settings. The commands:\n",
                    u.reason
                );
                for c in &plan.commands {
                    tracing::info!("hotkey command: {} ({}): {}", c.label, c.hotkey, c.command);
                    body.push_str(&format!("{}: {}\n", c.hotkey, c.command));
                }
                if self.notices.first_time("hotkeys-cli") {
                    self.say(NotificationLevel::Warning, "ssx: global hotkeys", body.trim_end());
                }
            }
        } else {
            tracing::info!(
                backend = ?status.backend,
                registered = status.registered,
                problems = status.problems.len(),
                "hotkeys registered"
            );
        }
        for p in &status.problems {
            tracing::warn!("hotkey problem: {p}");
        }
        {
            // Tell the user about *new* problems only: a reload that changes nothing about
            // them should not pop the same notification again.
            let mut last = lock(&self.last_hotkey_problems);
            if enabled && status.problems != *last && !status.problems.is_empty() {
                self.say(
                    NotificationLevel::Warning,
                    "ssx: some hotkeys are not working",
                    &status.problems.join("\n"),
                );
            }
            last.clone_from(&status.problems);
        }
        let view = HotkeyView {
            enabled,
            backend: status.backend.clone(),
            problems: status.problems.len(),
        };
        *lock(&self.hotkey_status) = status;
        self.ui.update(|u| u.hotkeys = view);
    }

    // ---- settings ---------------------------------------------------------------------------

    /// Switches to new settings: services and engine first (atomically), then hotkeys, then the
    /// tray.
    pub fn apply_settings(&self, settings: Settings) {
        self.rt.apply_settings(settings);
        self.apply_hotkeys();
        self.ui.refresh();
        tracing::info!("settings reloaded");
    }

    /// Looks at the settings file now (the watcher, `ReloadSettings`).
    pub fn reload_settings(&self) {
        let text = match read_settings_text(&self.settings_file) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", self.settings_file.display());
                return;
            }
        };
        let current = self.rt.settings();
        let outcome = lock(&self.reload).evaluate(text.as_deref(), &current);
        match outcome {
            ReloadOutcome::Unchanged => {
                if !lock(&self.reload).is_rejecting() {
                    *lock(&self.settings_problem) = None;
                }
            }
            ReloadOutcome::Apply { settings, warnings } => {
                for w in &warnings {
                    tracing::warn!("settings: {w}");
                }
                *lock(&self.settings_problem) = None;
                self.apply_settings(*settings);
            }
            ReloadOutcome::Reject { first_issue, all_issues } => {
                for i in &all_issues {
                    tracing::warn!("settings rejected: {i}");
                }
                *lock(&self.settings_problem) = Some(first_issue.clone());
                // A rejected file is important whatever `show_notifications` says: the user
                // just edited it and expects the change to take effect.
                self.say(
                    NotificationLevel::Error,
                    "ssx: settings not reloaded",
                    &format!("{first_issue}\nThe previous settings stay in effect."),
                );
            }
        }
    }

    /// Starts watching `settings.toml`.
    pub fn watch_settings(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let watcher = SettingsWatcher::start(&self.settings_file, false, move || {
            if let Some(app) = weak.upgrade() {
                app.reload_settings();
            }
        });
        tracing::info!(mode = ?watcher.mode(), "watching {}", self.settings_file.display());
        *lock(&self.watcher) = Some(watcher);
    }

    // ---- status -----------------------------------------------------------------------------

    /// Everything `Status` reports except the load (the IPC handler adds that).
    pub fn daemon_status(&self) -> DaemonStatus {
        let hk = lock(&self.hotkey_status).clone();
        DaemonStatus {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            tray: self.tray_running.load(Ordering::SeqCst),
            hotkey_backend: hk.backend.unwrap_or_else(|| "none".to_owned()),
            hotkeys_registered: hk.registered,
            hotkey_problems: hk.problems,
            active_runs: Vec::new(),
            queued_runs: 0,
            recording: ssx_core::ipc::RecordingStatus::default(),
            config_dir: self.paths.config_dir.display().to_string(),
            settings_problem: lock(&self.settings_problem).clone(),
        }
    }

    // ---- tray -------------------------------------------------------------------------------

    /// The current tray view.
    pub fn tray_view(&self) -> crate::tray::TrayView {
        build_view(&self.rt.settings(), &self.ui.snapshot(), self.sup.recording().view())
    }

    /// The action sink backends call.
    pub fn action_sink(self: &Arc<Self>) -> ActionSink {
        let weak = Arc::downgrade(self);
        Arc::new(move |action| {
            if let Some(app) = weak.upgrade() {
                app.handle_action(action, Origin::Tray);
            }
        })
    }

    /// Records that a tray is showing (or not) and stores its handle for refreshes.
    pub fn set_tray(&self, handle: Option<Arc<dyn TrayHandle>>) {
        self.tray_running.store(handle.is_some(), Ordering::SeqCst);
        if let Some(h) = &handle {
            h.refresh(&self.tray_view());
        }
        *lock(&self.tray) = handle;
    }

    /// Shows a one-time notice (tray advice) if it was not shown before.
    pub fn notice_once(&self, key: &str, title: &str, body: &str) {
        tracing::info!("{title}: {body}");
        if self.notices.first_time(key) {
            self.say(NotificationLevel::Warning, title, body);
        }
    }

    // ---- shutdown ---------------------------------------------------------------------------

    /// Winds everything down. See the module docs for the order.
    pub fn shutdown(&self) {
        tracing::info!("shutting down");
        *lock(&self.watcher) = None;
        self.files.shutdown();
        let clean = self.sup.shutdown(ShutdownGrace::default());
        if !clean {
            tracing::error!("some runs did not finish before the shutdown deadline");
        }
        *lock(&self.hotkeys) = None;
        if let Some(t) = lock(&self.tray).take() {
            t.shutdown();
        }
        self.ui.stop();
    }
}

/// An event sink whose target is bound after construction (the UI needs the recording
/// controller, which needs a sink).
#[derive(Default)]
struct LateSink(Mutex<Option<Arc<dyn UiSink>>>);

impl LateSink {
    fn bind(&self, sink: Arc<dyn UiSink>) {
        *lock(&self.0) = Some(sink);
    }
}

impl UiSink for LateSink {
    fn emit(&self, event: crate::events::UiEvent) {
        let sink = lock(&self.0).clone();
        if let Some(s) = sink {
            s.emit(event);
        }
    }
}

#[cfg(feature = "file-dialog")]
fn pick_files() -> Result<Vec<PathBuf>, String> {
    Ok(rfd::FileDialog::new().set_title("Upload files with ssx").pick_files().unwrap_or_default())
}

#[cfg(not(feature = "file-dialog"))]
fn pick_files() -> Result<Vec<PathBuf>, String> {
    Err("this build has no file chooser; use `ssx upload FILE...` or the file manager entry"
        .to_owned())
}

/// A second `ssx-app` launch: tell the running one to show its settings and leave.
fn forward_to_running(client: &ssx_ipc::Client) -> ExitCode {
    let line = match ssx_core::ipc::encode_line(&RequestEnvelope::new(
        1,
        Request::Show { target: ShowTarget::Settings },
    )) {
        Ok(l) => l.trim_end().to_owned(),
        Err(e) => {
            eprintln!("ssx-app: cannot build the request: {e}");
            return ExitCode::FAILURE;
        }
    };
    match client.request(&line) {
        Ok(reply) => match ssx_core::ipc::decode_line::<ResponseEnvelope>(&reply) {
            Ok(env) if env.response == Response::Ok => {
                eprintln!("ssx-app: already running; asked it to show its settings");
                ExitCode::SUCCESS
            }
            Ok(env) => {
                eprintln!("ssx-app: already running (it answered {:?})", env.response);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ssx-app: already running, but its answer was unreadable: {e}");
                ExitCode::SUCCESS
            }
        },
        Err(e) => {
            eprintln!("ssx-app: another instance holds the lock but did not answer: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the daemon until it is told to quit. Returns the process exit code.
pub fn run(args: Args) -> ExitCode {
    let paths = match resolve_paths(args.config_dir.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ssx-app: {e}");
            return ExitCode::from(2);
        }
    };
    let _log = logging::init(&paths.data_dir, &args);
    tracing::info!("ssx-app {} starting (pid {})", env!("CARGO_PKG_VERSION"), std::process::id());

    let server = match Instance::acquire(APP_ID) {
        Ok(Acquired::Primary(server)) => server,
        Ok(Acquired::Secondary(client)) => return forward_to_running(&client),
        Err(e) => {
            eprintln!("ssx-app: cannot set up the single-instance lock: {e}");
            tracing::error!("cannot acquire the instance lock: {e}");
            return ExitCode::FAILURE;
        }
    };

    let app = match App::new(args.clone(), paths) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ssx-app: {e}");
            tracing::error!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("{}", describe_paths(app.paths(), &app.settings_file));
    logging::install_panic_hook(Arc::clone(&app.notifier));

    // IPC.
    let handler = Arc::new(IpcHandler::new(
        Arc::clone(&app.sup),
        Arc::clone(&app.files),
        Arc::new(Control(Arc::downgrade(&app))),
    ));
    let serving = {
        let h = Arc::clone(&handler);
        match server.serve(move |line| h.handle_line(&line)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ssx-app: {}", AppError::Ipc(e.to_string()));
                return ExitCode::FAILURE;
            }
        }
    };

    // Signals: the first asks for a graceful exit, the second forces it.
    {
        let weak = Arc::downgrade(&app);
        let hits = std::sync::atomic::AtomicU32::new(0);
        if let Err(e) = ctrlc::set_handler(move || {
            if hits.fetch_add(1, Ordering::SeqCst) >= 1 {
                std::process::exit(130);
            }
            match weak.upgrade() {
                Some(a) => a.request_quit(),
                None => std::process::exit(0),
            }
        }) {
            tracing::warn!("cannot install the signal handler: {e}");
        }
    }

    app.watch_settings();
    let code = park(&app);

    tracing::info!("stopping the IPC server");
    app.shutdown();
    serving.shutdown();
    tracing::info!("bye");
    code
}

/// Starts the tray and the hotkeys, then blocks until quit. Linux: everything runs on its
/// own threads and the main thread only waits.
#[cfg(not(any(windows, target_os = "macos")))]
fn park(app: &Arc<App>) -> ExitCode {
    use crate::{
        hotkeys_glue::{HotkeyRunner, HotkeyThread},
        tray::NoTray,
    };
    // Hotkeys: the manager lives on its own thread.
    let hk = {
        let weak = Arc::downgrade(app);
        let no_hotkeys = app.args.no_hotkeys;
        HotkeyThread::spawn(
            move || {
                if no_hotkeys {
                    HotkeyRunner::unavailable("disabled with --no-hotkeys")
                } else {
                    HotkeyRunner::open()
                }
            },
            move |target| {
                if let Some(a) = weak.upgrade() {
                    a.handle_hotkey(target);
                }
            },
        )
    };
    app.attach_hotkeys(Box::new(hk));

    if app.args.no_tray {
        app.set_tray(Some(Arc::new(NoTray)));
        app.tray_running.store(false, Ordering::SeqCst);
        tracing::info!("tray disabled (--no-tray)");
    } else {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        match crate::tray_ksni::start(&app.tray_view(), &app.action_sink(), &desktop) {
            crate::tray_ksni::Started::Running(h) => {
                tracing::info!("tray icon registered");
                app.set_tray(Some(h));
            }
            crate::tray_ksni::Started::Waiting(h, problem) => {
                app.set_tray(Some(h));
                app.tray_running.store(false, Ordering::SeqCst);
                app.notice_once(
                    problem.notice_key,
                    "ssx: no tray icon yet",
                    &format!("{}. {}", problem.reason, problem.advice),
                );
            }
            crate::tray_ksni::Started::Unavailable(why) => {
                tracing::warn!("no tray icon: {why}");
            }
        }
    }
    app.wait_quit();
    ExitCode::SUCCESS
}

/// Windows and macOS: the tray and the hotkeys need a native event loop on this thread.
#[cfg(any(windows, target_os = "macos"))]
fn park(app: &Arc<App>) -> ExitCode {
    crate::tray_native::run(Arc::clone(app));
    ExitCode::SUCCESS
}
