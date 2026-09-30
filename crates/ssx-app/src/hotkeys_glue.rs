//! Global hotkeys: from settings to registrations to actions.
//!
//! * [`plan`] (pure): every workflow with a hotkey, plus the three app-level hotkeys of
//!   `[hotkeys]`, becomes a [`Binding`]; unusable ones (unparsable, duplicate) become
//!   [`Problem`]s, **never silently dropped**. It also lists the exact command line that runs
//!   each workflow without the daemon (`ssx run <name>`), which is what the user needs where
//!   no in-process mechanism exists (sway, Hyprland, GNOME without the portal).
//! * [`apply`]: unregister what is registered, register the plan, and report per binding what
//!   worked and why the rest did not ("Ctrl+PrintScreen is already used by another
//!   application ...").
//! * [`HotkeyRunner`] owns the manager. Managers are not `Send` (on Windows and macOS they
//!   must live on the thread that runs the UI loop), so the runner is created where it will be
//!   used: [`HotkeyThread`] does that on a private thread for Linux, the native tray loop does
//!   it on the main thread elsewhere.
//!
//! Which mechanism is used is `ssx_hotkeys::open_best_manager`'s decision (global-hotkey on
//! Windows/macOS/X11, the GlobalShortcuts portal on Wayland where available). When there is
//! none, the runner records *why* and the CLI commands, and [`HotkeyStatus`] carries them to the
//! one-time notification and the log.

use std::{
    collections::HashMap,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::JoinHandle,
    time::Duration,
};

use ssx_core::settings::Settings;
use ssx_hotkeys::{
    BackendKind, Chord, HotkeyError, HotkeyId, HotkeyManager, HotkeyState, OpenError,
};

/// What a hotkey does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyTarget {
    /// Run this workflow.
    Workflow(String),
    /// Show the history window.
    OpenHistory,
    /// Show the settings window.
    OpenSettings,
}

/// One hotkey to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The registration id (`wf.<workflow id>`, `app.open-history`, ...).
    pub id: HotkeyId,
    /// The key combination.
    pub chord: Chord,
    /// What it does.
    pub target: HotkeyTarget,
    /// Human name for messages.
    pub label: String,
}

/// A hotkey that cannot be registered, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Human name (workflow name or setting).
    pub label: String,
    /// The hotkey text as written in the settings.
    pub hotkey: String,
    /// What is wrong and what to do.
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.label, self.hotkey, self.message)
    }
}

/// A command that does what a hotkey would, for desktops where ssx cannot grab keys itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliBinding {
    /// Human name.
    pub label: String,
    /// The configured hotkey (a suggestion for the user's own binding).
    pub hotkey: String,
    /// The command line, ready to paste into a keybinding.
    pub command: String,
}

/// The result of [`plan`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// What to register.
    pub bindings: Vec<Binding>,
    /// What cannot be registered.
    pub problems: Vec<Problem>,
    /// The equivalent commands.
    pub commands: Vec<CliBinding>,
}

/// Builds the plan from the settings. `cli` is the program name to show in commands
/// (`ssx`).
pub fn plan(settings: &Settings, cli: &str) -> Plan {
    let mut out = Plan::default();
    let mut seen: HashMap<Chord, String> = HashMap::new();
    let mut add = |out: &mut Plan, id: String, label: String, hotkey: &str, target: HotkeyTarget| {
        let problem = |message: String| Problem {
            label: label.clone(),
            hotkey: hotkey.to_owned(),
            message,
        };
        let chord: Chord = match hotkey.parse() {
            Ok(c) => c,
            Err(e) => {
                out.problems.push(problem(format!("not a usable key combination: {e}")));
                return;
            }
        };
        let id = match HotkeyId::new(&id) {
            Ok(id) => id,
            Err(e) => {
                out.problems.push(problem(e.to_string()));
                return;
            }
        };
        if let Some(first) = seen.get(&chord) {
            out.problems.push(problem(format!(
                "{chord} is already bound to {first}; give this one a different key"
            )));
            return;
        }
        seen.insert(chord, label.clone());
        out.bindings.push(Binding { id, chord, target, label });
    };

    for wf in &settings.workflows {
        let Some(hotkey) = wf.trigger.hotkey.as_deref().map(str::trim).filter(|h| !h.is_empty())
        else {
            continue;
        };
        add(
            &mut out,
            format!("wf.{}", wf.id),
            wf.name.clone(),
            hotkey,
            HotkeyTarget::Workflow(wf.id.clone()),
        );
        let name = wf.trigger.cli_name.as_deref().unwrap_or(&wf.id);
        out.commands.push(CliBinding {
            label: wf.name.clone(),
            hotkey: hotkey.to_owned(),
            command: format!("{cli} run {name}"),
        });
    }
    for (setting, hotkey) in settings.hotkeys.entries() {
        let target = match setting {
            "hotkeys.open_history" => HotkeyTarget::OpenHistory,
            "hotkeys.open_settings" => HotkeyTarget::OpenSettings,
            // Pausing is not implemented by the recorder controls yet; say so instead of
            // pretending.
            other => {
                out.problems.push(Problem {
                    label: other.to_owned(),
                    hotkey: hotkey.to_owned(),
                    message: "this hotkey is not supported yet".to_owned(),
                });
                continue;
            }
        };
        let id = setting.trim_start_matches("hotkeys.").replace('_', "-");
        add(&mut out, format!("app.{id}"), setting.to_owned(), hotkey, target);
    }
    out
}

/// What the manager said about one binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Human name.
    pub label: String,
    /// The chord.
    pub chord: String,
    /// What to do about it.
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.label, self.chord, self.message)
    }
}

/// The outcome of registering a plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyReport {
    /// Registered `(label, chord)`.
    pub registered: Vec<(String, String)>,
    /// Refused by the manager.
    pub failures: Vec<Failure>,
}

/// An actionable sentence for a manager error.
pub fn describe_error(e: &HotkeyError) -> String {
    match e {
        HotkeyError::InUse(chord) => format!(
            "{chord} is already used by another application or by the desktop; choose another \
             key in the settings"
        ),
        HotkeyError::DuplicateChord { chord, existing } => {
            format!("{chord} is already registered as {existing}")
        }
        HotkeyError::DuplicateId(id) => format!("{id} is registered twice"),
        HotkeyError::UnknownId(id) => format!("{id} is not registered"),
        HotkeyError::Unavailable { reason, .. } => reason.clone(),
        HotkeyError::Backend { message, .. } => message.clone(),
    }
}

/// Registers `plan` on `mgr`, first removing every earlier registration.
pub fn apply(mgr: &mut dyn HotkeyManager, plan: &Plan) -> ApplyReport {
    for (id, _) in mgr.registered() {
        if let Err(e) = mgr.unregister(&id) {
            tracing::warn!("cannot unregister hotkey {id}: {e}");
        }
    }
    let mut report = ApplyReport::default();
    for b in &plan.bindings {
        match mgr.register(b.id.clone(), b.chord) {
            Ok(()) => report.registered.push((b.label.clone(), b.chord.to_string())),
            Err(e) => report.failures.push(Failure {
                label: b.label.clone(),
                chord: b.chord.to_string(),
                message: describe_error(&e),
            }),
        }
    }
    report
}

/// Why no in-process hotkey mechanism exists here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    /// One sentence for the user.
    pub reason: String,
    /// The strategies `ssx hotkeys detect` recommends instead.
    pub alternatives: Vec<String>,
}

/// The current state of hotkeys, for the tray, `Status` and the one-time notice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HotkeyStatus {
    /// The mechanism (`None` when there is none).
    pub backend: Option<String>,
    /// Why there is none.
    pub unavailable: Option<Unavailable>,
    /// Registered bindings.
    pub registered: usize,
    /// Problems (plan problems and registration failures), one line each.
    pub problems: Vec<String>,
}

/// Owns the manager and maps events to targets. Not `Send` (see the module docs).
pub struct HotkeyRunner {
    mgr: Option<Box<dyn HotkeyManager>>,
    unavailable: Option<Unavailable>,
    targets: HashMap<HotkeyId, HotkeyTarget>,
}

impl std::fmt::Debug for HotkeyRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeyRunner")
            .field("backend", &self.backend())
            .field("bindings", &self.targets.len())
            .finish_non_exhaustive()
    }
}

/// A one-line description of an [`OpenError`], with the alternatives it names.
fn unavailable_from(e: &OpenError) -> Unavailable {
    let OpenError::NoBackend { detection, failures } = e;
    let reasons: Vec<String> = failures.iter().map(ToString::to_string).collect();
    Unavailable {
        reason: if reasons.is_empty() {
            "this desktop does not let applications grab global keys".to_owned()
        } else {
            reasons.join("; ")
        },
        alternatives: detection.candidates.iter().map(|s| format!("{s:?}")).collect(),
    }
}

impl HotkeyRunner {
    /// Detects the best mechanism for this session.
    pub fn open() -> Self {
        match ssx_hotkeys::open_best_manager() {
            Ok(m) => Self::with_manager(m),
            Err(e) => {
                let u = unavailable_from(&e);
                tracing::info!("no in-process hotkey mechanism: {}", u.reason);
                Self { mgr: None, unavailable: Some(u), targets: HashMap::new() }
            }
        }
    }

    /// Uses an existing manager (tests).
    pub fn with_manager(mgr: Box<dyn HotkeyManager>) -> Self {
        Self { mgr: Some(mgr), unavailable: None, targets: HashMap::new() }
    }

    /// A runner that has no manager and says why (tests, `--no-hotkeys`).
    pub fn unavailable(reason: &str) -> Self {
        Self {
            mgr: None,
            unavailable: Some(Unavailable { reason: reason.to_owned(), alternatives: Vec::new() }),
            targets: HashMap::new(),
        }
    }

    /// The mechanism, if any.
    pub fn backend(&self) -> Option<BackendKind> {
        self.mgr.as_ref().map(|m| m.backend())
    }

    /// Why there is no mechanism.
    pub fn unavailable_reason(&self) -> Option<&Unavailable> {
        self.unavailable.as_ref()
    }

    /// Registers `plan` (or, with `enabled == false`, just clears every registration) and
    /// returns the status.
    pub fn apply(&mut self, plan: &Plan, enabled: bool) -> HotkeyStatus {
        let mut status = HotkeyStatus {
            backend: self.backend().map(|b| b.to_string()),
            unavailable: self.unavailable.clone(),
            registered: 0,
            problems: plan.problems.iter().map(ToString::to_string).collect(),
        };
        let Some(mgr) = self.mgr.as_mut() else { return status };
        let effective = if enabled { plan.clone() } else { Plan::default() };
        let report = apply(mgr.as_mut(), &effective);
        self.targets = effective
            .bindings
            .iter()
            .filter(|b| report.registered.iter().any(|(l, _)| *l == b.label))
            .map(|b| (b.id.clone(), b.target.clone()))
            .collect();
        status.registered = report.registered.len();
        if enabled {
            status.problems.extend(report.failures.iter().map(ToString::to_string));
        }
        status
    }

    /// Waits up to `timeout` for hotkey presses and returns their targets, in order. Key
    /// releases and events for unknown ids are ignored.
    pub fn poll(&mut self, timeout: Duration) -> Vec<HotkeyTarget> {
        let Some(mgr) = self.mgr.as_ref() else {
            std::thread::sleep(timeout);
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut take = |ev: ssx_hotkeys::HotkeyEvent, targets: &HashMap<HotkeyId, HotkeyTarget>| {
            if ev.state == HotkeyState::Pressed
                && let Some(t) = targets.get(&ev.id)
            {
                out.push(t.clone());
            }
        };
        match mgr.events().recv_timeout(timeout) {
            Ok(ev) => take(ev, &self.targets),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return out,
        }
        while let Ok(ev) = mgr.events().try_recv() {
            take(ev, &self.targets);
        }
        out
    }

    /// Non-blocking [`poll`](Self::poll), for a UI loop that calls it on every iteration.
    pub fn try_poll(&mut self) -> Vec<HotkeyTarget> {
        self.poll(Duration::ZERO)
    }
}

enum Cmd {
    Apply { plan: Box<Plan>, enabled: bool, reply: Sender<HotkeyStatus> },
    Shutdown,
}

/// A [`HotkeyRunner`] on its own thread (Linux, where the manager may live anywhere).
pub struct HotkeyThread {
    tx: Sender<Cmd>,
    join: Option<JoinHandle<()>>,
    initial: HotkeyStatus,
}

impl std::fmt::Debug for HotkeyThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeyThread").field("backend", &self.initial.backend).finish_non_exhaustive()
    }
}

/// How often the hotkey thread looks for commands between key events. Only bounds the latency
/// of `apply`; a key press wakes the thread at once.
const COMMAND_LATENCY: Duration = Duration::from_millis(250);

impl HotkeyThread {
    /// Starts the thread. `make` builds the runner *on that thread*; `on_target` is called
    /// there for each key press and must return quickly.
    pub fn spawn(
        make: impl FnOnce() -> HotkeyRunner + Send + 'static,
        on_target: impl Fn(HotkeyTarget) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<HotkeyStatus>();
        let join = std::thread::Builder::new()
            .name("ssx-hotkeys".into())
            .spawn(move || {
                let mut runner = make();
                let _ = ready_tx.send(HotkeyStatus {
                    backend: runner.backend().map(|b| b.to_string()),
                    unavailable: runner.unavailable.clone(),
                    ..HotkeyStatus::default()
                });
                loop {
                    for t in runner.poll(COMMAND_LATENCY) {
                        on_target(t);
                    }
                    loop {
                        match rx.try_recv() {
                            Ok(Cmd::Apply { plan, enabled, reply }) => {
                                let _ = reply.send(runner.apply(&plan, enabled));
                            }
                            Ok(Cmd::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
                            Err(mpsc::TryRecvError::Empty) => break,
                        }
                    }
                }
            })
            .map_err(|e| tracing::error!("cannot start the hotkey thread: {e}"))
            .ok();
        let initial = ready_rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| HotkeyStatus {
            unavailable: Some(Unavailable {
                reason: "the hotkey thread did not start".to_owned(),
                alternatives: Vec::new(),
            }),
            ..HotkeyStatus::default()
        });
        Self { tx, join, initial }
    }

    /// What the runner found at start (mechanism or reason).
    pub fn initial_status(&self) -> &HotkeyStatus {
        &self.initial
    }

    /// Registers `plan` (or clears with `enabled == false`) and returns the status.
    pub fn apply(&self, plan: Plan, enabled: bool) -> HotkeyStatus {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Apply { plan: Box::new(plan), enabled, reply }).is_err() {
            return self.initial.clone();
        }
        rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| self.initial.clone())
    }
}

/// What the app needs from whatever owns the hotkey manager (a [`HotkeyThread`] on Linux; the
/// UI loop's runner on Windows and macOS).
pub trait HotkeyControl: Send + Sync {
    /// What was found at start (mechanism or reason).
    fn initial_status(&self) -> HotkeyStatus;
    /// Registers `plan` (or clears with `enabled == false`) and returns the status.
    fn apply(&self, plan: Plan, enabled: bool) -> HotkeyStatus;
}

impl HotkeyControl for HotkeyThread {
    fn initial_status(&self) -> HotkeyStatus {
        self.initial.clone()
    }

    fn apply(&self, plan: Plan, enabled: bool) -> HotkeyStatus {
        HotkeyThread::apply(self, plan, enabled)
    }
}

impl Drop for HotkeyThread {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ssx_core::settings::{Trigger, Workflow};
    use ssx_hotkeys::HotkeyEvent;

    use super::*;

    fn wf(id: &str, name: &str, hotkey: Option<&str>, cli: Option<&str>) -> Workflow {
        Workflow {
            id: id.into(),
            name: name.into(),
            trigger: Trigger { hotkey: hotkey.map(Into::into), cli_name: cli.map(Into::into) },
            ..Workflow::default()
        }
    }

    fn settings(workflows: Vec<Workflow>) -> Settings {
        Settings { workflows, ..Settings::default() }
    }

    // ---- plan ---------------------------------------------------------------------------------

    #[test]
    fn the_default_settings_bind_every_hotkey_and_list_the_cli_commands() {
        let p = plan(&Settings::default(), "ssx");
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let ids: Vec<&str> = p.bindings.iter().map(|b| b.id.as_str()).collect();
        assert!(ids.contains(&"wf.capture-region") && ids.contains(&"wf.record-screen"));
        assert_eq!(p.bindings.len(), p.commands.len());
        let region = p.commands.iter().find(|c| c.command == "ssx run region").expect("cli name");
        assert_eq!(region.hotkey, "Ctrl+PrintScreen");
        assert!(p.bindings.iter().all(|b| matches!(b.target, HotkeyTarget::Workflow(_))));
    }

    #[test]
    fn workflows_without_hotkeys_are_skipped_silently() {
        let p = plan(&settings(vec![wf("a", "A", None, None), wf("b", "B", Some("  "), None)]), "ssx");
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn bad_and_duplicate_hotkeys_become_problems_not_silence() {
        let s = settings(vec![
            wf("one", "First", Some("Ctrl+Alt+1"), Some("one")),
            wf("dup", "Second", Some("alt+ctrl+1"), None),
            wf("bad", "Third", Some("Ctrl+NoSuchKey"), None),
            wf("bare", "Fourth", Some("Ctrl"), None),
        ]);
        let p = plan(&s, "ssx");
        assert_eq!(p.bindings.len(), 1);
        assert_eq!(p.problems.len(), 3, "{:?}", p.problems);
        let dup = p.problems.iter().find(|p| p.label == "Second").unwrap();
        assert!(dup.message.contains("already bound to First"), "{dup}");
        assert!(p.problems.iter().any(|p| p.label == "Third" && p.message.contains("not a usable")));
        // Commands are still listed for every workflow that has a hotkey, so a user on sway
        // can bind them by hand.
        assert_eq!(p.commands.len(), 4);
        assert_eq!(p.commands[0].command, "ssx run one");
        assert_eq!(p.commands[1].command, "ssx run dup", "falls back to the id");
    }

    #[test]
    fn app_level_hotkeys_are_bound_and_pause_is_reported_as_unsupported() {
        let mut s = settings(vec![]);
        s.hotkeys.open_history = Some("Ctrl+Alt+H".into());
        s.hotkeys.open_settings = Some("Ctrl+Alt+S".into());
        s.hotkeys.pause_recording = Some("Ctrl+Alt+P".into());
        let p = plan(&s, "ssx");
        assert_eq!(p.bindings.len(), 2);
        assert_eq!(p.bindings[0].target, HotkeyTarget::OpenHistory);
        assert_eq!(p.bindings[1].id.as_str(), "app.open-settings");
        assert_eq!(p.problems.len(), 1);
        assert!(p.problems[0].message.contains("not supported"));
        assert!(p.commands.is_empty(), "no CLI equivalent for window hotkeys");
    }

    #[test]
    fn a_hotkey_shared_between_a_workflow_and_an_app_hotkey_is_a_conflict() {
        let mut s = settings(vec![wf("a", "A", Some("Ctrl+Alt+H"), None)]);
        s.hotkeys.open_history = Some("Ctrl+Alt+H".into());
        let p = plan(&s, "ssx");
        assert_eq!(p.bindings.len(), 1);
        assert_eq!(p.problems.len(), 1);
    }

    #[test]
    fn overlong_workflow_ids_are_a_problem_not_a_panic() {
        let long = "x".repeat(70);
        let p = plan(&settings(vec![wf(&long, "Long", Some("Ctrl+Alt+L"), None)]), "ssx");
        assert!(p.bindings.is_empty());
        assert_eq!(p.problems.len(), 1);
    }

    // ---- apply on a fake manager ---------------------------------------------------------------

    #[derive(Debug)]
    struct FakeMgr {
        registered: Vec<(HotkeyId, Chord)>,
        refuse: Vec<Chord>,
        events: mpsc::Receiver<HotkeyEvent>,
    }

    impl HotkeyManager for FakeMgr {
        fn backend(&self) -> BackendKind {
            BackendKind::GlobalHotkey
        }
        fn register(&mut self, id: HotkeyId, chord: Chord) -> Result<(), HotkeyError> {
            if self.refuse.contains(&chord) {
                return Err(HotkeyError::InUse(chord));
            }
            self.registered.push((id, chord));
            Ok(())
        }
        fn unregister(&mut self, id: &HotkeyId) -> Result<(), HotkeyError> {
            self.registered.retain(|(i, _)| i != id);
            Ok(())
        }
        fn registered(&self) -> Vec<(HotkeyId, Chord)> {
            self.registered.clone()
        }
        fn events(&self) -> &mpsc::Receiver<HotkeyEvent> {
            &self.events
        }
        fn reports_release(&self) -> bool {
            true
        }
    }

    fn fake(refuse: &[&str]) -> (HotkeyRunner, Sender<HotkeyEvent>) {
        let (tx, events) = mpsc::channel();
        let mgr = FakeMgr {
            registered: Vec::new(),
            refuse: refuse.iter().map(|c| c.parse().unwrap()).collect(),
            events,
        };
        (HotkeyRunner::with_manager(Box::new(mgr)), tx)
    }

    fn press(tx: &Sender<HotkeyEvent>, id: &str, state: HotkeyState) {
        tx.send(HotkeyEvent { id: HotkeyId::new(id).unwrap(), state }).unwrap();
    }

    #[test]
    fn a_conflict_is_reported_with_the_key_and_what_to_do_and_the_rest_still_works() {
        let (mut r, _tx) = fake(&["Ctrl+PrintScreen"]);
        let status = r.apply(&plan(&Settings::default(), "ssx"), true);
        assert_eq!(status.backend.as_deref(), Some("global-hotkey"));
        assert_eq!(status.problems.len(), 1, "{:?}", status.problems);
        assert!(status.problems[0].contains("Ctrl+Print") && status.problems[0].contains("another application"), "{}", status.problems[0]);
        let n = plan(&Settings::default(), "ssx").bindings.len();
        assert_eq!(status.registered, n - 1);
    }

    #[test]
    fn re_applying_replaces_the_registrations() {
        let (mut r, tx) = fake(&[]);
        let s1 = settings(vec![wf("a", "A", Some("Ctrl+Alt+1"), None)]);
        let s2 = settings(vec![wf("b", "B", Some("Ctrl+Alt+2"), None)]);
        assert_eq!(r.apply(&plan(&s1, "ssx"), true).registered, 1);
        press(&tx, "wf.a", HotkeyState::Pressed);
        assert_eq!(r.poll(Duration::from_millis(50)), [HotkeyTarget::Workflow("a".into())]);
        assert_eq!(r.apply(&plan(&s2, "ssx"), true).registered, 1);
        press(&tx, "wf.a", HotkeyState::Pressed);
        press(&tx, "wf.b", HotkeyState::Pressed);
        assert_eq!(
            r.poll(Duration::from_millis(50)),
            [HotkeyTarget::Workflow("b".into())],
            "the old binding is gone"
        );
    }

    #[test]
    fn disabling_unregisters_everything_and_enabling_brings_it_back() {
        let (mut r, tx) = fake(&[]);
        let p = plan(&Settings::default(), "ssx");
        let on = r.apply(&p, true);
        assert!(on.registered > 0);
        let off = r.apply(&p, false);
        assert_eq!(off.registered, 0);
        assert!(off.problems.is_empty(), "no failure noise while switched off");
        press(&tx, "wf.capture-region", HotkeyState::Pressed);
        assert!(r.poll(Duration::from_millis(30)).is_empty());
        assert_eq!(r.apply(&p, true).registered, on.registered);
    }

    #[test]
    fn only_presses_of_registered_keys_produce_targets_in_order() {
        let (mut r, tx) = fake(&[]);
        r.apply(&plan(&Settings::default(), "ssx"), true);
        press(&tx, "wf.capture-region", HotkeyState::Pressed);
        press(&tx, "wf.capture-region", HotkeyState::Released);
        press(&tx, "wf.nonsense", HotkeyState::Pressed);
        press(&tx, "wf.capture-fullscreen", HotkeyState::Pressed);
        assert_eq!(
            r.poll(Duration::from_millis(50)),
            [
                HotkeyTarget::Workflow("capture-region".into()),
                HotkeyTarget::Workflow("capture-fullscreen".into())
            ]
        );
        assert!(r.try_poll().is_empty());
    }

    #[test]
    fn plan_problems_are_part_of_the_status_even_when_registration_works() {
        let (mut r, _tx) = fake(&[]);
        let s = settings(vec![
            wf("a", "A", Some("Ctrl+Alt+1"), None),
            wf("b", "B", Some("Ctrl+Alt+1"), None),
        ]);
        let status = r.apply(&plan(&s, "ssx"), true);
        assert_eq!(status.registered, 1);
        assert_eq!(status.problems.len(), 1);
    }

    #[test]
    fn an_unavailable_runner_reports_why_and_never_registers() {
        let mut r = HotkeyRunner::unavailable("no X server");
        assert_eq!(r.backend(), None);
        let status = r.apply(&plan(&Settings::default(), "ssx"), true);
        assert_eq!(status.registered, 0);
        assert_eq!(status.unavailable.unwrap().reason, "no X server");
        assert!(r.poll(Duration::from_millis(5)).is_empty());
    }

    #[test]
    fn errors_are_phrased_as_actions() {
        let chord: Chord = "Ctrl+Alt+Z".parse().unwrap();
        assert!(describe_error(&HotkeyError::InUse(chord)).contains("choose another key"));
        let unavailable = HotkeyError::Unavailable {
            backend: BackendKind::Portal,
            reason: "the portal has no GlobalShortcuts".into(),
        };
        assert_eq!(describe_error(&unavailable), "the portal has no GlobalShortcuts");
    }

    // ---- the thread --------------------------------------------------------------------------------

    #[test]
    fn the_hotkey_thread_applies_plans_and_forwards_presses() {
        let (tx, events) = mpsc::channel();
        let got: Arc<Mutex<Vec<HotkeyTarget>>> = Arc::default();
        let got2 = Arc::clone(&got);
        let ht = HotkeyThread::spawn(
            move || {
                HotkeyRunner::with_manager(Box::new(FakeMgr {
                    registered: Vec::new(),
                    refuse: Vec::new(),
                    events,
                }))
            },
            move |t| got2.lock().unwrap().push(t),
        );
        assert_eq!(ht.initial_status().backend.as_deref(), Some("global-hotkey"));
        let status = ht.apply(plan(&Settings::default(), "ssx"), true);
        assert!(status.registered > 0);
        press(&tx, "wf.capture-fullscreen", HotkeyState::Pressed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*got.lock().unwrap(), [HotkeyTarget::Workflow("capture-fullscreen".into())]);
        drop(ht); // joins
    }

    #[test]
    fn an_unavailable_thread_still_answers_apply() {
        let ht = HotkeyThread::spawn(|| HotkeyRunner::unavailable("nope"), |_| {});
        assert_eq!(ht.initial_status().unavailable.as_ref().unwrap().reason, "nope");
        assert_eq!(ht.apply(Plan::default(), true).registered, 0);
    }
}
