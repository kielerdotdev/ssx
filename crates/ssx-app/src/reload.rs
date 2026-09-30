//! Settings hot-reload.
//!
//! Two halves:
//!
//! * [`ReloadState::evaluate`] is the pure decision: given the text of `settings.toml`, the
//!   settings in force and what was seen before, say whether to **apply** the new settings, do
//!   **nothing**, or **reject** them (keeping the old ones). It validates with core's
//!   `validate()`: an *error* rejects the file and the first issue is what the user is told;
//!   warnings do not block. The same rejected text is only reported once, so a broken file that
//!   the editor keeps re-saving does not produce a notification per save, and a file that is
//!   identical to what is running (an editor's touch, our own migration write-back) is ignored.
//! * [`SettingsWatcher`] tells the daemon *when* to look: the `notify` crate on the config
//!   directory (the directory, not the file, so atomic "write temp + rename" saves are seen),
//!   debounced, with mtime polling as the fallback when no watcher can be created. It sleeps on
//!   a channel: no polling at all while the watcher works.
//!
//! The daemon applies an accepted result **atomically** (see `app`): new services and a new
//! engine snapshot are built first and swapped in at once, then the tray menu and hotkeys are
//! refreshed; runs already in flight keep the snapshot they started with.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use notify::{RecursiveMode, Watcher};
use ssx_core::settings::{Settings, Severity};

/// The decision for one look at the file.
#[derive(Debug, Clone, PartialEq)]
pub enum ReloadOutcome {
    /// Nothing to do (same content as before, or same settings, or the file is absent).
    Unchanged,
    /// Switch to these settings.
    Apply {
        /// The new settings.
        settings: Box<Settings>,
        /// Non-fatal findings (unknown keys, validation warnings) for the log.
        warnings: Vec<String>,
    },
    /// Keep the old settings; tell the user this.
    Reject {
        /// One line for a notification (the first problem).
        first_issue: String,
        /// Every problem, for the log.
        all_issues: Vec<String>,
    },
}

fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// What was seen before. Cheap to keep; holds only hashes.
#[derive(Debug, Clone, Default)]
pub struct ReloadState {
    applied: Option<u64>,
    rejected: Option<u64>,
}

impl ReloadState {
    /// A state that knows the text currently in force (so an identical re-save is ignored).
    pub fn with_current(text: Option<&str>) -> Self {
        Self { applied: text.map(hash), rejected: None }
    }

    /// `true` while the newest content that was looked at is being rejected (the running
    /// settings are older than the file).
    pub fn is_rejecting(&self) -> bool {
        self.rejected.is_some()
    }

    /// Decides what to do about `text` (the file's current content; `None` = file missing).
    /// `current` are the settings in force.
    pub fn evaluate(&mut self, text: Option<&str>, current: &Settings) -> ReloadOutcome {
        // A deleted file (or the instant between an editor's unlink and rename) keeps the
        // running configuration; the next event brings the real content.
        let Some(text) = text else { return ReloadOutcome::Unchanged };
        let h = hash(text);
        if self.applied == Some(h) {
            // Back to what is running (a bad edit was undone): nothing is being rejected now.
            self.rejected = None;
            return ReloadOutcome::Unchanged;
        }
        if self.rejected == Some(h) {
            return ReloadOutcome::Unchanged;
        }
        let loaded = match Settings::from_toml_str(text) {
            Ok(l) => l,
            Err(e) => {
                self.rejected = Some(h);
                let full = e.to_string();
                let first = full.lines().next().unwrap_or("the settings file cannot be read");
                return ReloadOutcome::Reject {
                    first_issue: first.to_owned(),
                    all_issues: vec![full],
                };
            }
        };
        let issues = loaded.settings.validate();
        let errors: Vec<String> = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .map(ToString::to_string)
            .collect();
        if let Some(first) = errors.first() {
            self.rejected = Some(h);
            return ReloadOutcome::Reject { first_issue: first.clone(), all_issues: errors };
        }
        self.rejected = None;
        self.applied = Some(h);
        if loaded.settings == *current {
            return ReloadOutcome::Unchanged;
        }
        let mut warnings = loaded.warnings;
        warnings.extend(
            issues.iter().filter(|i| i.severity == Severity::Warning).map(ToString::to_string),
        );
        ReloadOutcome::Apply { settings: Box::new(loaded.settings), warnings }
    }
}

/// Reads the settings file for [`ReloadState::evaluate`]: `Ok(None)` when it does not exist.
pub fn read_settings_text(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// How the watcher found out that something changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// File-system notifications.
    Notify,
    /// Polling the modification time (no watcher could be created).
    Polling,
}

/// Debounce: how long the directory must stay quiet before the callback runs.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// Polling interval of the fallback.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Watches `settings.toml` and calls `on_change` (from the watcher's own thread) after
/// each debounced burst of changes. Dropping the watcher stops it.
pub struct SettingsWatcher {
    mode: WatchMode,
    stop: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    // Kept alive: dropping the notify watcher stops the events.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl std::fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWatcher").field("mode", &self.mode).finish_non_exhaustive()
    }
}

fn mtime_and_len(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl SettingsWatcher {
    /// Starts watching `file`. `force_polling` skips the notify backend (tests, odd file
    /// systems).
    pub fn start(file: &Path, force_polling: bool, on_change: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let dir: PathBuf = file.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let name = file.file_name().map(std::ffi::OsStr::to_os_string);

        let mut watcher = None;
        if !force_polling {
            let events = tx.clone();
            let wanted = name.clone();
            let created = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                let hits_file = ev.paths.iter().any(|p| p.file_name() == wanted.as_deref());
                if hits_file || ev.paths.is_empty() {
                    let _ = events.send(());
                }
            });
            match created {
                Ok(mut w) => {
                    // The directory may not exist yet (first run): create it so the watch works.
                    let _ = std::fs::create_dir_all(&dir);
                    match w.watch(&dir, RecursiveMode::NonRecursive) {
                        Ok(()) => watcher = Some(w),
                        Err(e) => {
                            tracing::warn!("cannot watch {}: {e}; polling instead", dir.display());
                        }
                    }
                }
                Err(e) => tracing::warn!("no file watcher available ({e}); polling instead"),
            }
        }
        let mode = if watcher.is_some() { WatchMode::Notify } else { WatchMode::Polling };

        let thread_stop = Arc::clone(&stop);
        let file = file.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("ssx-settings-watch".into())
            .spawn(move || match mode {
                WatchMode::Notify => watch_loop(&rx, &thread_stop, &on_change),
                WatchMode::Polling => poll_loop(&file, &rx, &thread_stop, &on_change),
            })
            .map_err(|e| tracing::error!("cannot start the settings watcher thread: {e}"))
            .ok();
        Self { mode, stop, wake: tx, thread, _watcher: watcher }
    }

    /// How changes are detected.
    pub fn mode(&self) -> WatchMode {
        self.mode
    }

    /// Asks for a check now (the `ReloadSettings` IPC request).
    pub fn trigger(&self) {
        let _ = self.wake.send(());
    }
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Blocks on the channel; after the first event waits for quiet, then calls back.
fn watch_loop(rx: &mpsc::Receiver<()>, stop: &AtomicBool, on_change: &dyn Fn()) {
    while rx.recv().is_ok() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(()) if stop.load(Ordering::SeqCst) => return,
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        on_change();
    }
}

/// The fallback: look at the modification time every [`POLL_INTERVAL`] (an explicit trigger
/// is honoured at once).
fn poll_loop(file: &Path, rx: &mpsc::Receiver<()>, stop: &AtomicBool, on_change: &dyn Fn()) {
    let mut last = mtime_and_len(file);
    loop {
        let triggered = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = mtime_and_len(file);
        if triggered || now != last {
            last = now;
            // Let a writer that is still busy finish (the same debounce as the notify path).
            std::thread::sleep(DEBOUNCE);
            on_change();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn text(settings: &Settings) -> String {
        settings.to_toml_string().unwrap()
    }

    fn state_for(s: &Settings) -> ReloadState {
        ReloadState::with_current(Some(&text(s)))
    }

    // ---- the decision -----------------------------------------------------------------------

    #[test]
    fn identical_content_and_missing_files_change_nothing() {
        let cur = Settings::default();
        let mut st = state_for(&cur);
        assert_eq!(st.evaluate(Some(&text(&cur)), &cur), ReloadOutcome::Unchanged);
        assert_eq!(
            st.evaluate(None, &cur),
            ReloadOutcome::Unchanged,
            "deleted file keeps the old config"
        );
    }

    #[test]
    fn a_valid_edit_is_applied_once() {
        let cur = Settings::default();
        let mut st = state_for(&cur);
        let mut edited = cur.clone();
        edited.general.image_quality = 55;
        edited.workflows[0].trigger.hotkey = Some("Ctrl+Alt+K".into());
        let t = text(&edited);
        let ReloadOutcome::Apply { settings, warnings } = st.evaluate(Some(&t), &cur) else {
            panic!("expected apply")
        };
        assert_eq!(*settings, edited);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(st.evaluate(Some(&t), &edited), ReloadOutcome::Unchanged, "the same text again");
    }

    #[test]
    fn content_that_differs_only_in_formatting_is_recognised_as_no_change() {
        let cur = Settings::default();
        let mut st = ReloadState::default(); // never saw the running text
        let reformatted = format!("# a comment\n{}\n\n", text(&cur));
        assert_eq!(st.evaluate(Some(&reformatted), &cur), ReloadOutcome::Unchanged);
    }

    #[test]
    fn a_broken_file_is_rejected_with_its_first_problem_and_only_reported_once() {
        let cur = Settings::default();
        let mut st = state_for(&cur);
        let broken = "version = 1\n[general\nimage_quality = ";
        let ReloadOutcome::Reject { first_issue, all_issues } = st.evaluate(Some(broken), &cur)
        else {
            panic!("expected reject")
        };
        assert!(!first_issue.is_empty() && !first_issue.contains('\n'), "{first_issue:?}");
        assert!(!all_issues.is_empty());
        assert_eq!(
            st.evaluate(Some(broken), &cur),
            ReloadOutcome::Unchanged,
            "no notification storm"
        );
        // Fixing it works, and going back to the broken text is reported again.
        let mut fixed = cur.clone();
        fixed.general.image_quality = 61;
        assert!(matches!(st.evaluate(Some(&text(&fixed)), &cur), ReloadOutcome::Apply { .. }));
        assert!(matches!(st.evaluate(Some(broken), &fixed), ReloadOutcome::Reject { .. }));
        assert!(st.is_rejecting());
        // Undoing the bad edit (back to the text that is running) ends the rejection.
        assert_eq!(st.evaluate(Some(&text(&fixed)), &fixed), ReloadOutcome::Unchanged);
        assert!(!st.is_rejecting());
    }

    #[test]
    fn validation_errors_reject_and_warnings_do_not() {
        let cur = Settings::default();
        let mut st = state_for(&cur);
        // Two workflows on the same hotkey is an error in core's validation? Use a bad
        // hotkey, which definitely is.
        let mut bad = cur.clone();
        bad.workflows[0].trigger.hotkey = Some("Ctrl+".into());
        let out = st.evaluate(Some(&text(&bad)), &cur);
        let ReloadOutcome::Reject { first_issue, .. } = out else { panic!("{out:?}") };
        assert!(first_issue.contains("hotkey"), "{first_issue}");

        // A warning-only change (an unknown key) still applies, with the warning attached.
        let mut ok = cur.clone();
        ok.general.image_quality = 70;
        let with_unknown = format!("{}\n[mystery]\nx = 1\n", text(&ok));
        let ReloadOutcome::Apply { warnings, .. } = st.evaluate(Some(&with_unknown), &cur) else {
            panic!("warnings must not block")
        };
        assert!(warnings.iter().any(|w| w.contains("mystery")), "{warnings:?}");
    }

    #[test]
    fn files_from_a_newer_ssx_are_rejected_not_applied() {
        let cur = Settings::default();
        let mut st = state_for(&cur);
        let newer = "version = 999\n";
        assert!(matches!(st.evaluate(Some(newer), &cur), ReloadOutcome::Reject { .. }));
    }

    #[test]
    fn read_settings_text_distinguishes_missing_from_broken() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_settings_text(&d.path().join("nope.toml")).unwrap(), None);
        let p = d.path().join("s.toml");
        std::fs::write(&p, "a = 1").unwrap();
        assert_eq!(read_settings_text(&p).unwrap().as_deref(), Some("a = 1"));
        assert!(
            read_settings_text(d.path()).is_err(),
            "a directory is an I/O error, not 'missing'"
        );
    }

    // ---- the watcher -----------------------------------------------------------------------

    fn counting() -> (Arc<AtomicUsize>, impl Fn() + Send + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        (n, move || {
            n2.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn wait_for(n: &AtomicUsize, at_least: usize, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if n.load(Ordering::SeqCst) >= at_least {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn the_notify_watcher_sees_edits_and_atomic_renames_and_debounces_bursts() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("settings.toml");
        std::fs::write(&file, "version = 1\n").unwrap();
        let (n, cb) = counting();
        let w = SettingsWatcher::start(&file, false, cb);
        if w.mode() != WatchMode::Notify {
            eprintln!("SKIP: no inotify-style watcher in this environment");
            return;
        }
        // A burst of writes is one callback.
        for i in 0..5 {
            std::fs::write(&file, format!("version = 1\n# {i}\n")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(wait_for(&n, 1, 5), "an edit is noticed");
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(n.load(Ordering::SeqCst), 1, "the burst was debounced into one callback");
        // An editor's atomic save: write a temp file, rename over the target.
        let tmp = d.path().join(".settings.toml.swp");
        std::fs::write(&tmp, "version = 1\n# atomic\n").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert!(wait_for(&n, 2, 5), "a rename over the file is noticed");
        // Unrelated files in the directory do not count.
        let before = n.load(Ordering::SeqCst);
        std::fs::write(d.path().join("other.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(n.load(Ordering::SeqCst), before);
        // trigger() is an explicit check.
        w.trigger();
        assert!(wait_for(&n, before + 1, 5));
    }

    #[test]
    fn the_polling_fallback_notices_changes_and_triggers() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("settings.toml");
        std::fs::write(&file, "version = 1\n").unwrap();
        let (n, cb) = counting();
        let w = SettingsWatcher::start(&file, true, cb);
        assert_eq!(w.mode(), WatchMode::Polling);
        w.trigger();
        assert!(wait_for(&n, 1, 5), "an explicit trigger is honoured at once");
        std::fs::write(&file, "version = 1\n# changed, and longer\n").unwrap();
        assert!(wait_for(&n, 2, 10), "a change is found within the poll interval");
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread_promptly() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("settings.toml");
        let (_, cb) = counting();
        let start = std::time::Instant::now();
        drop(SettingsWatcher::start(&file, false, cb));
        let (_, cb) = counting();
        drop(SettingsWatcher::start(&file, true, cb));
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    }
}
