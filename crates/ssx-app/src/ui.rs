//! What the user sees of the daemon: the tray state and the notifications that no workflow
//! produced.
//!
//! [`UiHandle`] is the [`UiSink`] the supervisor and the recording controller emit into. It only
//! *queues* the event (they may hold their locks while emitting, so a sink must never call back
//! into them); a worker thread applies each event to the [`UiInner`] state with the pure
//! [`reduce`] function, performs the notifications it asks for, and pushes a fresh
//! [`TrayView`] to the tray when something visible changed. The worker blocks on its channel:
//! it wakes once a second only while a recording is running (to advance the elapsed time in the
//! tooltip and menu) and not at all otherwise.
//!
//! Notification policy (what the *daemon* says; workflows notify through their own
//! `show_notification` step):
//! * a run that failed or only partly worked and whose workflow did not notify: an error
//!   notification with the summary;
//! * refusals ("another capture is still open"), background problems (a batch that could not
//!   start): a warning;
//! * nothing for successes, cancellations, or the start of a recording (a pop-up would end up in
//!   the video);
//! * all of it respects `general.show_notifications`.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};

use ssx_core::{
    settings::Settings,
    workflow::{NotificationLevel, Outcome},
};

use crate::{
    events::{RecordingView, UiEvent, UiSink},
    menu::{HotkeyView, UiState, build_menu, icon_kind, tooltip},
    notify::DaemonNotifier,
    recording::RecordingController,
    runtime::Runtime,
    tray::{TrayHandle, TrayView},
};

/// The state the tray is drawn from (besides settings and the recording).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiInner {
    /// The newest step text per running run.
    pub busy: BTreeMap<u64, String>,
    /// Runs that are waiting for the user.
    pub interactive: BTreeSet<u64>,
    /// Runs waiting for a slot.
    pub queued: BTreeSet<u64>,
    /// The newest failure, until the next run starts.
    pub last_error: Option<String>,
    /// Hotkeys.
    pub hotkeys: HotkeyView,
    /// The overlay helper exists.
    pub overlay_available: bool,
    /// This build can record.
    pub record_supported: bool,
    /// The settings window helper exists.
    pub settings_ui_available: bool,
}

impl Default for UiInner {
    fn default() -> Self {
        Self {
            busy: BTreeMap::new(),
            interactive: BTreeSet::new(),
            queued: BTreeSet::new(),
            last_error: None,
            hotkeys: HotkeyView { enabled: true, backend: None, problems: 0 },
            overlay_available: true,
            record_supported: true,
            settings_ui_available: false,
        }
    }
}

/// A notification the reducer wants shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Say {
    /// Prominence.
    pub level: NotificationLevel,
    /// Headline.
    pub title: String,
    /// Details.
    pub body: String,
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        let mut cut: String = line.chars().take(max).collect();
        cut.push('\u{2026}');
        cut
    }
}

/// Applies one event to the state and returns the notifications it calls for.
pub fn reduce(state: &mut UiInner, event: &UiEvent) -> Vec<Say> {
    match event {
        UiEvent::RunStarted { run_id, name, interactive } => {
            state.queued.remove(run_id);
            state.busy.insert(*run_id, format!("{name}..."));
            if *interactive {
                state.interactive.insert(*run_id);
            }
            state.last_error = None;
            Vec::new()
        }
        UiEvent::Step { run_id, text } => {
            if state.busy.contains_key(run_id) {
                state.busy.insert(*run_id, text.clone());
            }
            Vec::new()
        }
        UiEvent::RunQueued { run_id, .. } => {
            state.queued.insert(*run_id);
            Vec::new()
        }
        UiEvent::RunFinished { run_id, name, summary, notified } => {
            state.busy.remove(run_id);
            state.interactive.remove(run_id);
            state.queued.remove(run_id);
            match summary.outcome {
                Outcome::Success => {
                    state.last_error = None;
                    Vec::new()
                }
                Outcome::Cancelled => Vec::new(),
                Outcome::Failed | Outcome::PartialSuccess => {
                    state.last_error = Some(first_line(&summary.message, 120));
                    if *notified {
                        return Vec::new();
                    }
                    let (level, title) = if summary.outcome == Outcome::Failed {
                        (NotificationLevel::Error, format!("{name}: failed"))
                    } else {
                        (NotificationLevel::Warning, format!("{name}: partly done"))
                    };
                    vec![Say { level, title, body: summary.message.clone() }]
                }
            }
        }
        UiEvent::Recording(_) => Vec::new(),
        UiEvent::Notice { level, title, body } => {
            vec![Say { level: *level, title: title.clone(), body: body.clone() }]
        }
    }
}

/// Builds what the tray shows.
pub fn build_view(settings: &Settings, inner: &UiInner, recording: RecordingView) -> TrayView {
    let ui = UiState {
        recording,
        interactive_open: !inner.interactive.is_empty()
            || matches!(recording, RecordingView::Selecting),
        busy: inner.busy.values().next_back().cloned(),
        queued: inner.queued.len(),
        last_error: inner.last_error.clone(),
        overlay_available: inner.overlay_available,
        record_supported: inner.record_supported,
        settings_ui_available: inner.settings_ui_available,
        hotkeys: inner.hotkeys.clone(),
    };
    TrayView { menu: build_menu(settings, &ui), tooltip: tooltip(&ui), icon: icon_kind(&ui) }
}

/// Where the tray handle lives once the tray is up (it starts after the worker).
pub type TrayCell = Arc<Mutex<Option<Arc<dyn TrayHandle>>>>;

enum Msg {
    Event(UiEvent),
    Refresh,
    Stop,
}

/// The sink and the levers of the UI worker.
pub struct UiHandle {
    tx: Sender<Msg>,
    inner: Arc<Mutex<UiInner>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for UiHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiHandle").finish_non_exhaustive()
    }
}

impl UiSink for UiHandle {
    fn emit(&self, event: UiEvent) {
        let _ = self.tx.send(Msg::Event(event));
    }
}

/// What the worker needs.
pub struct UiDeps {
    /// For the current settings.
    pub rt: Arc<Runtime>,
    /// For the recording phase.
    pub recording: Arc<RecordingController>,
    /// Shows notifications.
    pub notifier: Arc<DaemonNotifier>,
    /// The tray, once it exists.
    pub tray: TrayCell,
    /// Initial state.
    pub initial: UiInner,
}

impl std::fmt::Debug for UiDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiDeps").finish_non_exhaustive()
    }
}

impl UiHandle {
    /// Starts the worker.
    pub fn spawn(deps: UiDeps) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let inner = Arc::new(Mutex::new(deps.initial.clone()));
        let worker_inner = Arc::clone(&inner);
        let join = std::thread::Builder::new()
            .name("ssx-ui".into())
            .spawn(move || worker(&rx, &worker_inner, &deps))
            .map_err(|e| tracing::error!("cannot start the UI thread: {e}"))
            .ok();
        Self { tx, inner, join: Mutex::new(join) }
    }

    fn lock(&self) -> MutexGuard<'_, UiInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and redraws.
    pub fn update(&self, f: impl FnOnce(&mut UiInner)) {
        f(&mut self.lock());
        let _ = self.tx.send(Msg::Refresh);
    }

    /// Redraws (settings changed).
    pub fn refresh(&self) {
        let _ = self.tx.send(Msg::Refresh);
    }

    /// A copy of the state.
    pub fn snapshot(&self) -> UiInner {
        self.lock().clone()
    }

    /// Stops the worker after it has handled what is queued.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
        let handle = self.join.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn worker(rx: &mpsc::Receiver<Msg>, inner: &Mutex<UiInner>, deps: &UiDeps) {
    let mut last_pushed: Option<TrayView> = None;
    loop {
        let recording = deps.recording.view();
        let msg = if matches!(recording, RecordingView::Recording { .. }) {
            // Advance the elapsed time shown in the tooltip and the Stop entry.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        };
        match msg {
            Some(Msg::Stop) => return,
            Some(Msg::Event(ev)) => {
                let says = reduce(&mut inner.lock().unwrap_or_else(PoisonError::into_inner), &ev);
                if !says.is_empty() && deps.rt.settings().general.show_notifications {
                    for s in says {
                        deps.notifier.say(s.level, &s.title, &s.body);
                    }
                }
            }
            Some(Msg::Refresh) | None => {}
        }
        let view = {
            let state = inner.lock().unwrap_or_else(PoisonError::into_inner);
            build_view(&deps.rt.settings(), &state, deps.recording.view())
        };
        if last_pushed.as_ref() != Some(&view) {
            let tray = deps.tray.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(t) = tray {
                t.refresh(&view);
            }
            last_pushed = Some(view);
        }
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::ipc::RunSummary;

    use super::*;

    fn finished(run_id: u64, outcome: Outcome, notified: bool) -> UiEvent {
        UiEvent::RunFinished {
            run_id,
            name: "Region".into(),
            summary: RunSummary {
                run_id,
                outcome,
                message: "upload: HTTP 500\nsecond line".into(),
                items: vec![],
            },
            notified,
        }
    }

    fn started(run_id: u64, interactive: bool) -> UiEvent {
        UiEvent::RunStarted { run_id, name: "Region".into(), interactive }
    }

    #[test]
    fn a_run_shows_progress_and_clears_when_it_ends() {
        let mut s = UiInner::default();
        assert!(reduce(&mut s, &started(1, true)).is_empty());
        assert_eq!(s.busy[&1], "Region...");
        assert!(s.interactive.contains(&1));
        reduce(&mut s, &UiEvent::Step { run_id: 1, text: "Uploading...".into() });
        assert_eq!(s.busy[&1], "Uploading...");
        reduce(&mut s, &UiEvent::Step { run_id: 99, text: "ghost".into() });
        assert!(!s.busy.contains_key(&99), "steps of unknown runs are ignored");
        assert!(reduce(&mut s, &finished(1, Outcome::Success, false)).is_empty());
        assert!(s.busy.is_empty() && s.interactive.is_empty() && s.last_error.is_none());
    }

    #[test]
    fn failures_notify_once_unless_the_workflow_already_did() {
        let mut s = UiInner::default();
        reduce(&mut s, &started(1, false));
        let says = reduce(&mut s, &finished(1, Outcome::Failed, false));
        assert_eq!(says.len(), 1);
        assert_eq!(says[0].level, NotificationLevel::Error);
        assert_eq!(says[0].title, "Region: failed");
        assert!(says[0].body.contains("HTTP 500"));
        assert_eq!(s.last_error.as_deref(), Some("upload: HTTP 500"), "the tooltip gets line one");

        reduce(&mut s, &started(2, false));
        assert!(s.last_error.is_none(), "a new run clears the old error");
        assert!(reduce(&mut s, &finished(2, Outcome::Failed, true)).is_empty(), "already notified");
        assert!(s.last_error.is_some(), "but the tray still shows it");

        let says = reduce(&mut s, &finished(3, Outcome::PartialSuccess, false));
        assert_eq!(says[0].level, NotificationLevel::Warning);
        assert_eq!(says[0].title, "Region: partly done");
    }

    #[test]
    fn cancelled_runs_are_silent_and_do_not_mark_an_error() {
        let mut s = UiInner::default();
        reduce(&mut s, &started(1, true));
        assert!(reduce(&mut s, &finished(1, Outcome::Cancelled, false)).is_empty());
        assert!(s.last_error.is_none());
    }

    #[test]
    fn queued_runs_are_counted_until_they_start_or_end() {
        let mut s = UiInner::default();
        reduce(&mut s, &UiEvent::RunQueued { run_id: 5, name: "x".into() });
        reduce(&mut s, &UiEvent::RunQueued { run_id: 6, name: "x".into() });
        assert_eq!(s.queued.len(), 2);
        reduce(&mut s, &started(5, false));
        assert_eq!(s.queued.len(), 1);
        reduce(&mut s, &finished(6, Outcome::Cancelled, true));
        assert!(s.queued.is_empty());
    }

    #[test]
    fn notices_become_notifications_verbatim() {
        let mut s = UiInner::default();
        let says = reduce(
            &mut s,
            &UiEvent::Notice {
                level: NotificationLevel::Warning,
                title: "ssx is busy".into(),
                body: "finish the overlay".into(),
            },
        );
        assert_eq!(
            says,
            [Say {
                level: NotificationLevel::Warning,
                title: "ssx is busy".into(),
                body: "finish the overlay".into()
            }]
        );
        assert!(reduce(&mut s, &UiEvent::Recording(RecordingView::Idle)).is_empty());
    }

    #[test]
    fn first_lines_are_trimmed_and_capped() {
        assert_eq!(first_line("  a b \nrest", 10), "a b");
        assert_eq!(first_line(&"x".repeat(50), 10), format!("{}\u{2026}", "x".repeat(10)));
        assert_eq!(first_line("", 10), "");
    }

    #[test]
    fn the_view_reflects_state_and_settings() {
        let settings = Settings::default();
        let mut s = UiInner::default();
        s.hotkeys =
            HotkeyView { enabled: true, backend: Some("global-hotkey".into()), problems: 0 };
        let v = build_view(&settings, &s, RecordingView::Idle);
        assert_eq!(v.tooltip, "ssx: Ready");
        assert_eq!(v.icon, crate::menu::IconKind::Idle);

        reduce(&mut s, &started(1, true));
        let v = build_view(&settings, &s, RecordingView::Idle);
        assert!(v.tooltip.contains("Region..."), "{}", v.tooltip);
        assert!(v.menu.entry(&crate::menu::Action::CancelCurrent).is_some());

        let v = build_view(
            &settings,
            &UiInner::default(),
            RecordingView::Recording { elapsed: Duration::from_secs(61) },
        );
        assert_eq!(v.icon, crate::menu::IconKind::Recording);
        assert!(v.tooltip.contains("1:01"));
        let v = build_view(&settings, &UiInner::default(), RecordingView::Selecting);
        assert!(v.menu.entry(&crate::menu::Action::StopRecording).is_some());
    }
}
