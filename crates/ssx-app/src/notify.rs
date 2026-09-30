//! Desktop notifications from the daemon.
//!
//! * [`DaemonNotifier`] implements the engine's `Notifier` and additionally knows what a
//!   *click* should do: open the uploaded URL (only `http`/`https`, like the rest of ssx) or, for
//!   a saved file, the file. On Linux that is the freedesktop "default" action; notifications of
//!   toolkits without click support (Windows toasts through `notify-rust`) just show the text,
//!   with the URL in the body so it can be read and typed.
//! * [`prepare`] is the pure part: it cleans the text (control characters, length), escapes the
//!   markup notification servers interpret, and picks the click target. It is what the tests pin.
//! * [`NoticeStore`] remembers which *one-time* advice was already shown ("install the
//!   `AppIndicator` extension", "run `ssx hotkeys install`") in a small file in the data directory,
//!   so the user is told once per installation, not at every login.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use ssx_core::{
    settings::atomic_write,
    workflow::{Notification, NotificationLevel, Notifier, ServiceError},
};
use ssx_services::DesktopNotifier;
use ssx_types::Frame;

/// Longest body sent to the desktop.
const MAX_BODY_CHARS: usize = 600;

/// What a click on a notification does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    /// Open this `http(s)` URL in the browser.
    OpenUrl(String),
    /// Open this file with its default application.
    OpenPath(PathBuf),
}

/// A notification ready to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastSpec {
    /// Headline (cleaned).
    pub title: String,
    /// Body (cleaned; markup-escaped on desktops that interpret markup).
    pub body: String,
    /// Prominence.
    pub level: NotificationLevel,
    /// Click behaviour.
    pub click: Option<Click>,
}

fn clean(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| *c == '\n' || !c.is_control()).collect();
    if cleaned.chars().count() <= MAX_BODY_CHARS {
        return cleaned;
    }
    let mut cut: String = cleaned.chars().take(MAX_BODY_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn is_web_url(u: &str) -> bool {
    let u = u.trim();
    (u.starts_with("https://") || u.starts_with("http://")) && u.len() > "https://".len()
}

/// Cleans and escapes a notification and chooses its click target. A URL wins over a path
/// (the point of an upload notification is the link).
pub fn prepare(n: &Notification, escape: bool) -> ToastSpec {
    let mut body = clean(&n.body);
    let mut add = |line: &str| {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&clean(line));
    };
    if let Some(url) = &n.url {
        add(url);
    }
    if let Some(path) = &n.path {
        add(&path.display().to_string());
    }
    let click = n
        .url
        .as_deref()
        .filter(|u| is_web_url(u))
        .map(|u| Click::OpenUrl(u.trim().to_owned()))
        .or_else(|| n.path.clone().map(Click::OpenPath));
    ToastSpec {
        title: clean(&n.title),
        body: if escape { escape_markup(&body) } else { body },
        level: n.level,
        click,
    }
}

/// Shows a prepared notification. Abstracted so the notifier can be tested without a
/// notification daemon.
pub trait Toast: Send + Sync {
    /// Shows `spec`.
    fn show(&self, spec: &ToastSpec) -> Result<(), String>;
}

/// Opens things when a notification is clicked. Abstracted for tests.
pub trait ClickHandler: Send + Sync {
    /// Performs `click`.
    fn handle(&self, click: &Click);
}

/// Opens with the system's default handler (`opener`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClickHandler;

impl ClickHandler for SystemClickHandler {
    fn handle(&self, click: &Click) {
        let result = match click {
            Click::OpenUrl(u) => opener::open_browser(u),
            Click::OpenPath(p) => opener::open(p),
        };
        if let Err(e) = result {
            tracing::warn!("cannot open the clicked notification's target: {e}");
        }
    }
}

/// `notify-rust`, with a click action where the platform supports it.
pub struct NotifyRustToast {
    clicks: Arc<dyn ClickHandler>,
}

impl std::fmt::Debug for NotifyRustToast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyRustToast").finish_non_exhaustive()
    }
}

impl NotifyRustToast {
    /// A toast that opens click targets with `clicks`.
    pub fn new(clicks: Arc<dyn ClickHandler>) -> Self {
        Self { clicks }
    }
}

impl Toast for NotifyRustToast {
    fn show(&self, spec: &ToastSpec) -> Result<(), String> {
        let mut n = notify_rust::Notification::new();
        n.appname("ssx").summary(&spec.title).body(&spec.body);
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.icon(match spec.level {
                NotificationLevel::Success => "dialog-information",
                NotificationLevel::Warning => "dialog-warning",
                NotificationLevel::Error => "dialog-error",
            });
            if spec.click.is_some() {
                n.action("default", "Open");
            }
        }
        n.timeout(notify_rust::Timeout::Milliseconds(8000));
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let handle = n.show().map_err(|e| e.to_string())?;
            if let Some(click) = spec.click.clone() {
                let clicks = Arc::clone(&self.clicks);
                // `wait_for_action` blocks until the notification is closed or clicked; the
                // server closes it after the timeout, so the thread is short-lived.
                let _ =
                    std::thread::Builder::new().name("ssx-notify-click".into()).spawn(move || {
                        handle.wait_for_action(|action| {
                            if action == "default" {
                                clicks.handle(&click);
                            }
                        });
                    });
            }
            Ok(())
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            let _ = &self.clicks;
            n.show().map(drop).map_err(|e| e.to_string())
        }
    }
}

/// The engine's [`Notifier`] for the daemon. See the module docs.
pub struct DaemonNotifier {
    toast: Box<dyn Toast>,
    escape: bool,
    qr: DesktopNotifier,
}

impl std::fmt::Debug for DaemonNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonNotifier").finish_non_exhaustive()
    }
}

impl DaemonNotifier {
    /// The real notifier: `notify-rust` toasts, system opener for clicks.
    pub fn system() -> Self {
        Self::new(
            Box::new(NotifyRustToast::new(Arc::new(SystemClickHandler))),
            cfg!(all(unix, not(target_os = "macos"))),
        )
    }

    /// A notifier over any [`Toast`]; `escape` HTML-escapes the body (freedesktop servers).
    pub fn new(toast: Box<dyn Toast>, escape: bool) -> Self {
        Self { toast, escape, qr: DesktopNotifier::default() }
    }

    /// Shows a plain message (used for daemon-originated notices).
    pub fn say(&self, level: NotificationLevel, title: &str, body: &str) {
        let n = Notification {
            level,
            title: title.to_owned(),
            body: body.to_owned(),
            url: None,
            path: None,
        };
        if let Err(e) = self.notify(&n) {
            tracing::warn!("cannot show the notification {title:?}: {e}");
        }
    }
}

impl Notifier for DaemonNotifier {
    fn notify(&self, n: &Notification) -> Result<(), ServiceError> {
        let spec = prepare(n, self.escape);
        tracing::debug!(title = %spec.title, "notification");
        self.toast.show(&spec).map_err(|e| {
            ServiceError::failed(format!(
                "cannot show a desktop notification ({e}); is a notification daemon running?"
            ))
        })
    }

    fn show_qr(&self, url: &str, image: &Frame) -> Result<(), ServiceError> {
        self.qr.show_qr(url, image)
    }
}

/// Remembers which one-time notices were shown. See the module docs.
#[derive(Debug)]
pub struct NoticeStore {
    path: Option<PathBuf>,
    // Serialises read-modify-write within the process and remembers what this process showed
    // even when the file cannot be written.
    session: Mutex<BTreeSet<String>>,
}

const NOTICES_FILE: &str = "notices.json";

impl NoticeStore {
    /// A store in `dir` (`<dir>/notices.json`).
    pub fn in_dir(dir: &Path) -> Self {
        Self { path: Some(dir.join(NOTICES_FILE)), session: Mutex::new(BTreeSet::new()) }
    }

    /// A store that remembers nothing across runs (every notice is "new" once per process).
    pub fn in_memory() -> Self {
        Self { path: None, session: Mutex::new(BTreeSet::new()) }
    }

    fn load(&self) -> BTreeSet<String> {
        let Some(p) = &self.path else { return BTreeSet::new() };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default()
    }

    /// `true` the first time `key` is asked about (and remembers it), `false` afterwards.
    pub fn first_time(&self, key: &str) -> bool {
        let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        if !session.insert(key.to_owned()) {
            return false;
        }
        let mut seen = self.load();
        if !seen.insert(key.to_owned()) {
            return false;
        }
        if let Some(p) = &self.path {
            let text = serde_json::to_string(&seen.iter().collect::<Vec<_>>()).unwrap_or_default();
            if let Err(e) = atomic_write(p, text.as_bytes()) {
                tracing::warn!(
                    "cannot remember that {key:?} was shown ({e}); it may be shown again"
                );
            }
        }
        true
    }

    /// Forgets everything (`ssx-app --reset-notices`, tests).
    pub fn forget_all(&self) {
        self.session.lock().unwrap_or_else(PoisonError::into_inner).clear();
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    fn note(body: &str, url: Option<&str>, path: Option<&str>) -> Notification {
        Notification {
            level: NotificationLevel::Success,
            title: "Uploaded".into(),
            body: body.into(),
            url: url.map(Into::into),
            path: path.map(PathBuf::from),
        }
    }

    #[test]
    fn urls_and_paths_are_appended_and_markup_is_escaped_where_asked() {
        let n = note("1 file <done>", Some("https://x.example/?a=1&b=<2>"), Some("/tmp/a & b.png"));
        let s = prepare(&n, true);
        assert_eq!(
            s.body,
            "1 file &lt;done&gt;\nhttps://x.example/?a=1&amp;b=&lt;2&gt;\n/tmp/a &amp; b.png"
        );
        let raw = prepare(&n, false);
        assert!(raw.body.contains("a=1&b=<2>"), "{}", raw.body);
    }

    #[test]
    fn a_click_opens_the_url_first_then_the_file() {
        let both = prepare(&note("x", Some("https://x.example/a"), Some("/tmp/a.png")), true);
        assert_eq!(both.click, Some(Click::OpenUrl("https://x.example/a".into())));
        let file = prepare(&note("x", None, Some("/tmp/a.png")), true);
        assert_eq!(file.click, Some(Click::OpenPath("/tmp/a.png".into())));
        assert_eq!(prepare(&note("x", None, None), true).click, None);
    }

    #[test]
    fn only_web_urls_become_click_targets() {
        for bad in
            ["file:///etc/passwd", "javascript:alert(1)", "smb://h/s", "not a url", "https://", ""]
        {
            let s = prepare(&note("x", Some(bad), None), true);
            assert_eq!(s.click, None, "{bad:?}");
        }
        // A hostile URL still appears as text (escaped), but is never opened by a click.
        let s = prepare(&note("x", Some("javascript:alert(1)"), Some("/tmp/a.png")), true);
        assert_eq!(s.click, Some(Click::OpenPath("/tmp/a.png".into())), "falls back to the file");
    }

    #[test]
    fn control_characters_are_stripped_and_long_text_is_cut() {
        let n = note(&format!("a\u{1b}[31mb\0{}", "x".repeat(2000)), None, None);
        let s = prepare(&n, true);
        assert!(
            s.body.starts_with("a[31mb") && !s.body.contains('\u{1b}') && !s.body.contains('\0')
        );
        assert!(s.body.chars().count() <= MAX_BODY_CHARS + 1 && s.body.ends_with('\u{2026}'));
        let t = Notification { title: "T\u{7}itle\nline".into(), ..note("", None, None) };
        assert_eq!(prepare(&t, true).title, "Title\nline");
    }

    #[derive(Default)]
    struct Recorder(StdMutex<Vec<ToastSpec>>, bool);
    impl Toast for Arc<Recorder> {
        fn show(&self, spec: &ToastSpec) -> Result<(), String> {
            if self.1 {
                return Err("no such interface".into());
            }
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
    }

    #[test]
    fn the_notifier_shows_prepared_toasts_and_explains_a_missing_daemon() {
        let rec = Arc::new(Recorder::default());
        let n = DaemonNotifier::new(Box::new(Arc::clone(&rec)), true);
        n.notify(&note("body & more", Some("https://x.example/1"), None)).unwrap();
        n.say(NotificationLevel::Warning, "ssx", "careful");
        let shown = rec.0.lock().unwrap().clone();
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].body, "body &amp; more\nhttps://x.example/1");
        assert_eq!(shown[1].level, NotificationLevel::Warning);

        let broken = Arc::new(Recorder(StdMutex::default(), true));
        let n = DaemonNotifier::new(Box::new(broken), true);
        let e = n.notify(&note("x", None, None)).unwrap_err();
        assert!(e.to_string().contains("notification daemon"), "{e}");
        n.say(NotificationLevel::Error, "still no panic", "x");
    }

    #[test]
    fn one_time_notices_are_shown_once_and_remembered_across_restarts() {
        let d = tempfile::tempdir().unwrap();
        let a = NoticeStore::in_dir(d.path());
        assert!(a.first_time("tray-gnome"));
        assert!(!a.first_time("tray-gnome"));
        assert!(a.first_time("hotkeys-sway"), "keys are independent");
        let b = NoticeStore::in_dir(d.path()); // "the next login"
        assert!(!b.first_time("tray-gnome"));
        assert!(!b.first_time("hotkeys-sway"));
        b.forget_all();
        assert!(NoticeStore::in_dir(d.path()).first_time("tray-gnome"));
    }

    #[test]
    fn a_corrupt_or_unwritable_store_never_blocks_the_notice() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(NOTICES_FILE), "{{{ not json").unwrap();
        let s = NoticeStore::in_dir(d.path());
        assert!(s.first_time("x"), "a damaged file counts as empty");
        assert!(!s.first_time("x"), "and is repaired");
        let unwritable = NoticeStore::in_dir(Path::new("/proc/definitely/not/writable"));
        assert!(unwritable.first_time("y"), "the notice is shown even if it cannot be remembered");
        let mem = NoticeStore::in_memory();
        assert!(mem.first_time("z") && !mem.first_time("z"));
    }

    #[test]
    fn concurrent_askers_see_exactly_one_first_time() {
        let d = tempfile::tempdir().unwrap();
        let s = Arc::new(NoticeStore::in_dir(d.path()));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || s.first_time("race"))
            })
            .collect();
        let firsts =
            handles.into_iter().filter(|_| true).map(|h| h.join().unwrap()).filter(|b| *b).count();
        assert_eq!(firsts, 1);
    }
}
