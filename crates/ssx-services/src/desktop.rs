//! Desktop integration services: notifications, opening URLs, showing QR codes.
//!
//! All three are *optional* steps in the workflow engine (a failure is a warning), so the
//! job here is to be safe and to fail with a useful message when a headless machine has no
//! notification daemon or browser launcher.
//!
//! * [`DesktopNotifier`] uses `notify-rust` (`D-Bus` on Linux, `WinRT` toasts on Windows, the
//!   notification centre on macOS). Freedesktop notification servers interpret a small HTML
//!   subset in the body, and file names or URLs contain `&` and `<`, so the body is escaped.
//!   Clicking a notification to open its URL is not implemented: it needs a long-lived
//!   listener, which belongs to the tray app; the URL is part of the text instead.
//! * [`SystemOpener`] only opens `http` and `https` URLs. A URL comes from an upload
//!   server's response (or a `.sxcu` template), and handing arbitrary URI schemes to the OS
//!   would let a hostile server start any registered protocol handler.
//! * QR codes are shown by writing a PNG to the temp directory and opening it with the default
//!   image viewer; the file is left for the OS to clean up (the viewer may still be reading).

use std::{path::Path, sync::Arc};

use ssx_core::workflow::{Notification, NotificationLevel, Notifier, ServiceError, UrlOpener};
use ssx_types::{EncodeOptions, Frame, ImageFormat};

/// Longest notification body sent to the desktop.
const MAX_BODY_CHARS: usize = 600;

/// Shows a notification. Abstracted for tests.
pub trait Toaster: Send + Sync {
    /// Shows `title` and `body` (already escaped) with an icon for `level`.
    fn show(&self, title: &str, body: &str, level: NotificationLevel) -> Result<(), String>;
}

/// Opens files and URLs with the default handler. Abstracted for tests.
pub trait Launcher: Send + Sync {
    /// Opens `url` in the default browser.
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// Opens a local file with the default application.
    fn open_file(&self, path: &Path) -> Result<(), String>;
}

/// `notify-rust`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NotifyRustToaster;

impl Toaster for NotifyRustToaster {
    fn show(&self, title: &str, body: &str, level: NotificationLevel) -> Result<(), String> {
        let mut n = notify_rust::Notification::new();
        n.appname("ssx").summary(title).body(body);
        #[cfg(all(unix, not(target_os = "macos")))]
        n.icon(match level {
            NotificationLevel::Success => "dialog-information",
            NotificationLevel::Warning => "dialog-warning",
            NotificationLevel::Error => "dialog-error",
        });
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = level;
        n.timeout(notify_rust::Timeout::Milliseconds(6000));
        n.show().map(drop).map_err(|e| e.to_string())
    }
}

/// `opener`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenerLauncher;

impl Launcher for OpenerLauncher {
    fn open_url(&self, url: &str) -> Result<(), String> {
        opener::open_browser(url).map_err(|e| e.to_string())
    }

    fn open_file(&self, path: &Path) -> Result<(), String> {
        opener::open(path).map_err(|e| e.to_string())
    }
}

/// Escapes the characters a freedesktop notification server treats as markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Strips control characters (except newline) and caps the length.
fn clean(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| *c == '\n' || !c.is_control()).collect();
    if cleaned.chars().count() <= MAX_BODY_CHARS {
        return cleaned;
    }
    let mut cut: String = cleaned.chars().take(MAX_BODY_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

/// The [`Notifier`] service.
pub struct DesktopNotifier {
    toaster: Box<dyn Toaster>,
    launcher: Arc<dyn Launcher>,
}

impl std::fmt::Debug for DesktopNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopNotifier").finish_non_exhaustive()
    }
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self::new(Box::new(NotifyRustToaster), Arc::new(OpenerLauncher))
    }
}

impl DesktopNotifier {
    /// A notifier over explicit back ends (tests).
    pub fn new(toaster: Box<dyn Toaster>, launcher: Arc<dyn Launcher>) -> Self {
        Self { toaster, launcher }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, n: &Notification) -> Result<(), ServiceError> {
        let mut body = clean(&n.body);
        if let Some(url) = &n.url {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&clean(url));
        }
        if let Some(path) = &n.path {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&clean(&path.display().to_string()));
        }
        self.toaster.show(&clean(&n.title), &escape_markup(&body), n.level).map_err(|e| {
            ServiceError::failed(format!(
                "cannot show a desktop notification ({e}); is a notification daemon running?"
            ))
        })
    }

    fn show_qr(&self, _url: &str, image: &Frame) -> Result<(), ServiceError> {
        let png = image
            .encode(EncodeOptions::new(ImageFormat::Png))
            .map_err(|e| ServiceError::failed(format!("cannot encode the QR code: {e}")))?;
        let file = tempfile::Builder::new().prefix("ssx-qr-").suffix(".png").tempfile()?;
        std::fs::write(file.path(), png)?;
        // Keep it: the viewer opens it asynchronously and may take a while to read it.
        let (_, path) = file.keep().map_err(|e| ServiceError::Io(e.error))?;
        self.launcher
            .open_file(&path)
            .map_err(|e| ServiceError::failed(format!("cannot open the QR code image ({e})")))
    }
}

/// The [`UrlOpener`] service. See the [module docs](self) for the scheme restriction.
pub struct SystemOpener {
    launcher: Arc<dyn Launcher>,
}

impl std::fmt::Debug for SystemOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOpener").finish_non_exhaustive()
    }
}

impl Default for SystemOpener {
    fn default() -> Self {
        Self::new(Arc::new(OpenerLauncher))
    }
}

impl SystemOpener {
    /// An opener over an explicit launcher (tests).
    pub fn new(launcher: Arc<dyn Launcher>) -> Self {
        Self { launcher }
    }
}

impl UrlOpener for SystemOpener {
    fn open(&self, url: &str) -> Result<(), ServiceError> {
        let parsed = url::Url::parse(url.trim())
            .map_err(|e| ServiceError::failed(format!("{url:?} is not a valid URL: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(ServiceError::failed(format!(
                "refusing to open {url:?}: only http and https links are opened automatically"
            )));
        }
        self.launcher
            .open_url(parsed.as_str())
            .map_err(|e| ServiceError::failed(format!("cannot open the browser ({e})")))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Mutex};

    use super::*;

    #[derive(Default)]
    struct Recorder {
        toasts: Mutex<Vec<(String, String, NotificationLevel)>>,
        urls: Mutex<Vec<String>>,
        files: Mutex<Vec<PathBuf>>,
        fail: bool,
    }

    struct Shared(Arc<Recorder>);

    impl Toaster for Shared {
        fn show(&self, t: &str, b: &str, l: NotificationLevel) -> Result<(), String> {
            if self.0.fail {
                return Err("no such interface".into());
            }
            self.0.toasts.lock().unwrap().push((t.into(), b.into(), l));
            Ok(())
        }
    }

    impl Launcher for Shared {
        fn open_url(&self, url: &str) -> Result<(), String> {
            self.0.urls.lock().unwrap().push(url.into());
            Ok(())
        }
        fn open_file(&self, path: &Path) -> Result<(), String> {
            self.0.files.lock().unwrap().push(path.into());
            Ok(())
        }
    }

    fn notifier(rec: &Arc<Recorder>) -> DesktopNotifier {
        DesktopNotifier::new(Box::new(Shared(rec.clone())), Arc::new(Shared(rec.clone())))
    }

    fn note(body: &str) -> Notification {
        Notification {
            level: NotificationLevel::Success,
            title: "Uploaded".into(),
            body: body.into(),
            url: Some("https://x.example/?a=1&b=<2>".into()),
            path: Some(PathBuf::from("/tmp/a & b.png")),
        }
    }

    #[test]
    fn notifications_are_escaped_and_carry_the_url_and_path() {
        let rec = Arc::new(Recorder::default());
        notifier(&rec).notify(&note("1 file <done>")).unwrap();
        let toasts = rec.toasts.lock().unwrap();
        let (title, body, level) = &toasts[0];
        assert_eq!(title, "Uploaded");
        assert_eq!(*level, NotificationLevel::Success);
        assert_eq!(
            body,
            "1 file &lt;done&gt;\nhttps://x.example/?a=1&amp;b=&lt;2&gt;\n/tmp/a &amp; b.png"
        );
    }

    #[test]
    fn control_characters_are_stripped_and_long_bodies_cut() {
        let rec = Arc::new(Recorder::default());
        let n = Notification {
            url: None,
            path: None,
            ..note(&format!("a\u{1b}[31mb\0{}", "x".repeat(2000)))
        };
        notifier(&rec).notify(&n).unwrap();
        let body = rec.toasts.lock().unwrap()[0].1.clone();
        assert!(body.starts_with("a[31mb") && !body.contains('\u{1b}') && !body.contains('\0'));
        assert!(body.chars().count() <= MAX_BODY_CHARS + 1);
        assert!(body.ends_with('\u{2026}'));
    }

    #[test]
    fn a_missing_daemon_is_a_helpful_error() {
        let rec = Arc::new(Recorder { fail: true, ..Recorder::default() });
        let e = notifier(&rec).notify(&note("x")).unwrap_err();
        assert!(e.to_string().contains("notification daemon"), "{e}");
    }

    #[test]
    fn qr_codes_are_saved_as_png_and_opened() {
        let rec = Arc::new(Recorder::default());
        let qr = ssx_core::workflow::QrCodeRenderer;
        let frame = ssx_core::workflow::QrRenderer::render(&qr, "https://x.example/abc").unwrap();
        notifier(&rec).show_qr("https://x.example/abc", &frame).unwrap();
        let files = rec.files.lock().unwrap();
        let bytes = std::fs::read(&files[0]).unwrap();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.size(), frame.size());
        assert!(files[0].to_string_lossy().contains("ssx-qr-"));
        std::fs::remove_file(&files[0]).unwrap();
    }

    #[test]
    fn only_http_and_https_urls_are_opened() {
        let rec = Arc::new(Recorder::default());
        let opener = SystemOpener::new(Arc::new(Shared(rec.clone())));
        opener.open("https://example.com/a?b=c d").unwrap();
        opener.open("  http://example.com/x  ").unwrap();
        assert_eq!(
            *rec.urls.lock().unwrap(),
            ["https://example.com/a?b=c%20d", "http://example.com/x"]
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ms-msdt:/id PCWDiagnostic",
            "smb://host/share",
            "not a url",
            "https://",
            "",
        ] {
            assert!(opener.open(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(rec.urls.lock().unwrap().len(), 2, "nothing else reached the launcher");
    }
}
