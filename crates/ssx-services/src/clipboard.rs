//! The [`Clipboard`] service.
//!
//! Two kinds of backends are chained ([`SystemClipboard`] tries them in order until one
//! works):
//!
//! * [`ArboardBackend`], the `arboard` crate: Windows, macOS, X11, and Wayland through the
//!   `wlr-data-control` protocol (feature `wayland-data-control`). It is the only backend
//!   that can *read*.
//! * [`ExternalBackend`], the `wl-copy` (Wayland) / `xclip` (X11) command-line tools.
//!
//! Why both? Because of who owns the clipboard. On X11 and Wayland the *source* application
//! serves the data to whoever pastes, so it must stay alive. A short-lived CLI process that
//! copies with `arboard` and exits takes the clipboard contents with it (unless a clipboard
//! manager happens to grab them first). `wl-copy` and `xclip` fork a background process that
//! keeps serving, which is what a one-shot `ssx capture --copy` needs. A long-running tray
//! app has no such problem, so it keeps `arboard` first. [`ClipboardOptions::prefer_external`]
//! selects the order.
//!
//! **File lists** ("copy files" in a file manager, to paste them elsewhere) differ per
//! desktop. Windows and macOS have native formats which `arboard` implements. On Linux,
//! `arboard` writes `text/uri-list`, which KDE (Dolphin) and most Qt apps accept; GNOME Files,
//! Nemo, Caja and Thunar want `x-special/gnome-copied-files` (`copy\nfile:///...`). So on
//! Linux desktops other than KDE the external tools write that format first ([`FileListFlavor`]),
//! falling back to `arboard`'s uri list.

use std::{
    borrow::Cow,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ssx_core::workflow::{CancelToken, Clipboard, ClipboardContent, ServiceError};
use ssx_types::{EncodeOptions, Frame, ImageFormat};

use crate::command::{ProcessSpec, find_in_path, run_process};

/// Why a backend could not do something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// This backend cannot do that on this system (try the next one, silently).
    Unsupported,
    /// It tried and failed.
    Failed(String),
}

/// How a file list is put on the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileListFlavor {
    /// `text/uri-list` (KDE, Qt applications, Windows/macOS use their native formats).
    UriList,
    /// `x-special/gnome-copied-files` (GNOME Files, Nemo, Caja, Thunar).
    GnomeCopiedFiles,
}

impl FileListFlavor {
    /// Chooses by `XDG_CURRENT_DESKTOP` (colon separated).
    pub fn detect(xdg_current_desktop: Option<&str>) -> Self {
        let kde = xdg_current_desktop.is_some_and(|d| {
            d.split(':').any(|t| t.eq_ignore_ascii_case("kde") || t.eq_ignore_ascii_case("plasma"))
        });
        if cfg!(target_os = "linux") && !kde { Self::GnomeCopiedFiles } else { Self::UriList }
    }
}

/// One way of talking to the system clipboard.
pub trait ClipboardBackend: Send {
    /// Short name for diagnostics and error messages.
    fn name(&self) -> &'static str;
    /// Puts an 8-bit sRGB image on the clipboard.
    fn set_image(&mut self, frame: &Frame) -> Result<(), BackendError>;
    /// Puts text on the clipboard.
    fn set_text(&mut self, text: &str) -> Result<(), BackendError>;
    /// Puts a file list on the clipboard in the given flavour.
    fn set_files(&mut self, paths: &[PathBuf], flavor: FileListFlavor) -> Result<(), BackendError>;
    /// Reads the clipboard.
    fn read(&mut self) -> Result<ClipboardContent, BackendError>;
}

/// Options for [`SystemClipboard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipboardOptions {
    /// Try the forking command-line tools before `arboard` (for short-lived processes).
    pub prefer_external: bool,
}

/// The [`Clipboard`] service: a chain of backends. See the [module docs](self).
pub struct SystemClipboard {
    /// `(backend, is_external)` in preference order.
    backends: Mutex<Vec<(Box<dyn ClipboardBackend>, bool)>>,
    flavor: FileListFlavor,
}

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemClipboard").field("flavor", &self.flavor).finish_non_exhaustive()
    }
}

impl SystemClipboard {
    /// The real clipboard for this session.
    pub fn new(options: ClipboardOptions) -> Self {
        let external: Box<dyn ClipboardBackend> = Box::new(ExternalBackend::system());
        let native: Box<dyn ClipboardBackend> = Box::new(ArboardBackend::default());
        let backends = if options.prefer_external {
            vec![(external, true), (native, false)]
        } else {
            vec![(native, false), (external, true)]
        };
        Self {
            backends: Mutex::new(backends),
            flavor: FileListFlavor::detect(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()),
        }
    }

    /// A clipboard over explicit backends (tests, embedders). `flavor` picks the file-list
    /// format; the flag says whether a backend is an "external tool" (preferred for file
    /// lists on GNOME-style desktops).
    pub fn with_backends(
        backends: Vec<(Box<dyn ClipboardBackend>, bool)>,
        flavor: FileListFlavor,
    ) -> Self {
        Self { backends: Mutex::new(backends), flavor }
    }

    fn attempt(
        &self,
        what: &str,
        external_first: bool,
        mut f: impl FnMut(&mut dyn ClipboardBackend) -> Result<(), BackendError>,
    ) -> Result<(), ServiceError> {
        let mut backends = self.backends.lock().unwrap_or_else(PoisonError::into_inner);
        let mut order: Vec<usize> = (0..backends.len()).collect();
        if external_first {
            order.sort_by_key(|&i| !backends[i].1); // stable: externals first, order kept
        }
        let mut failures = Vec::new();
        for i in order {
            let (backend, _) = &mut backends[i];
            match f(backend.as_mut()) {
                Ok(()) => return Ok(()),
                Err(BackendError::Unsupported) => {}
                Err(BackendError::Failed(msg)) => {
                    tracing::debug!(backend = backend.name(), "clipboard {what} failed: {msg}");
                    failures.push(format!("{}: {msg}", backend.name()));
                }
            }
        }
        if failures.is_empty() {
            Err(ServiceError::Unsupported(format!("copying {what} on this system")))
        } else {
            Err(ServiceError::failed(format!(
                "could not copy {what} to the clipboard ({}). On Linux install `wl-clipboard` \
                 (Wayland) or `xclip` (X11)",
                failures.join("; ")
            )))
        }
    }
}

impl Clipboard for SystemClipboard {
    fn set_image(&self, frame: &Frame) -> Result<(), ServiceError> {
        if !frame.is_sdr8() {
            return Err(ServiceError::failed("cannot copy an image that has not been tone-mapped"));
        }
        self.attempt("the image", false, |b| b.set_image(frame))
    }

    fn set_text(&self, text: &str) -> Result<(), ServiceError> {
        self.attempt("the text", false, |b| b.set_text(text))
    }

    fn set_files(&self, paths: &[PathBuf]) -> Result<(), ServiceError> {
        if paths.is_empty() {
            return Err(ServiceError::failed("no files to copy"));
        }
        let flavor = self.flavor;
        self.attempt("the file list", flavor == FileListFlavor::GnomeCopiedFiles, |b| {
            b.set_files(paths, flavor)
        })
    }

    fn read(&self) -> Result<ClipboardContent, ServiceError> {
        let mut backends = self.backends.lock().unwrap_or_else(PoisonError::into_inner);
        let mut failures = Vec::new();
        for (backend, _) in backends.iter_mut() {
            match backend.read() {
                Ok(c) => return Ok(c),
                Err(BackendError::Unsupported) => {}
                Err(BackendError::Failed(m)) => failures.push(format!("{}: {m}", backend.name())),
            }
        }
        if failures.is_empty() {
            Err(ServiceError::Unsupported("reading the clipboard on this system".to_owned()))
        } else {
            Err(ServiceError::failed(format!(
                "could not read the clipboard ({})",
                failures.join("; ")
            )))
        }
    }
}

// ---- arboard ---------------------------------------------------------------------------

/// The `arboard` backend. The clipboard connection is opened on first use (there may be no
/// display when the bundle is built) and kept for the life of the process.
#[derive(Default)]
pub struct ArboardBackend {
    inner: Option<arboard::Clipboard>,
}

impl std::fmt::Debug for ArboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArboardBackend").field("open", &self.inner.is_some()).finish()
    }
}

impl ArboardBackend {
    fn open(&mut self) -> Result<&mut arboard::Clipboard, BackendError> {
        if self.inner.is_none() {
            self.inner =
                Some(arboard::Clipboard::new().map_err(|e| BackendError::Failed(e.to_string()))?);
        }
        self.inner.as_mut().ok_or(BackendError::Unsupported)
    }
}

fn arboard_err(e: &arboard::Error) -> BackendError {
    match e {
        arboard::Error::ClipboardNotSupported => BackendError::Unsupported,
        other => BackendError::Failed(other.to_string()),
    }
}

/// Straight RGBA bytes of an sRGB frame (borrowed when it already is tightly packed RGBA).
fn rgba_bytes(frame: &Frame) -> Result<Cow<'_, [u8]>, BackendError> {
    if frame.format() == ssx_types::PixelFormat::Rgba8
        && frame.stride() == frame.width() as usize * 4
    {
        return Ok(Cow::Borrowed(frame.data()));
    }
    frame
        .clone()
        .into_rgba8()
        .map(|f| Cow::Owned(f.into_data()))
        .map_err(|e| BackendError::Failed(e.to_string()))
}

impl ClipboardBackend for ArboardBackend {
    fn name(&self) -> &'static str {
        "arboard"
    }

    fn set_image(&mut self, frame: &Frame) -> Result<(), BackendError> {
        let bytes = rgba_bytes(frame)?;
        let data = arboard::ImageData {
            width: frame.width() as usize,
            height: frame.height() as usize,
            bytes,
        };
        self.open()?.set_image(data).map_err(|e| arboard_err(&e))
    }

    fn set_text(&mut self, text: &str) -> Result<(), BackendError> {
        self.open()?.set_text(text.to_owned()).map_err(|e| arboard_err(&e))
    }

    fn set_files(
        &mut self,
        paths: &[PathBuf],
        _flavor: FileListFlavor,
    ) -> Result<(), BackendError> {
        self.open()?.set().file_list(paths).map_err(|e| arboard_err(&e))
    }

    fn read(&mut self) -> Result<ClipboardContent, BackendError> {
        let cb = self.open()?;
        match cb.get().file_list() {
            Ok(files) if !files.is_empty() => return Ok(ClipboardContent::Files(files)),
            Ok(_)
            | Err(arboard::Error::ContentNotAvailable | arboard::Error::ConversionFailure) => {}
            Err(e) => tracing::debug!("clipboard file list unavailable: {e}"),
        }
        match cb.get_image() {
            Ok(img) => {
                let (w, h) = (
                    u32::try_from(img.width)
                        .map_err(|_| BackendError::Failed("image too wide".into()))?,
                    u32::try_from(img.height)
                        .map_err(|_| BackendError::Failed("image too tall".into()))?,
                );
                let frame = Frame::from_rgba8(w, h, img.bytes.into_owned())
                    .map_err(|e| BackendError::Failed(e.to_string()))?;
                return Ok(ClipboardContent::Image(frame));
            }
            Err(arboard::Error::ContentNotAvailable | arboard::Error::ConversionFailure) => {}
            Err(e) => tracing::debug!("clipboard image unavailable: {e}"),
        }
        match cb.get_text() {
            Ok(t) if !t.is_empty() => Ok(ClipboardContent::Text(t)),
            Ok(_)
            | Err(arboard::Error::ContentNotAvailable | arboard::Error::ConversionFailure) => {
                Ok(ClipboardContent::Empty)
            }
            Err(e) => Err(arboard_err(&e)),
        }
    }
}

// ---- wl-copy / xclip -------------------------------------------------------------------

/// Which display server the external tools should talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// `WAYLAND_DISPLAY` is set: use `wl-copy`.
    Wayland,
    /// Only `DISPLAY` is set: use `xclip`.
    X11,
    /// Neither (or not Linux).
    None,
}

impl Session {
    /// Reads the process environment.
    pub fn from_env() -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        if !cfg!(unix) || cfg!(target_os = "macos") {
            Self::None
        } else if set("WAYLAND_DISPLAY") {
            Self::Wayland
        } else if set("DISPLAY") {
            Self::X11
        } else {
            Self::None
        }
    }
}

/// Runs the clipboard tools. Abstracted so tests can record instead of spawning.
pub trait ToolRunner: Send + Sync {
    /// `true` if `program` can be started.
    fn available(&self, program: &str) -> bool;
    /// Runs `program args`, feeding `stdin`. `Err` carries a message.
    fn run(&self, program: &str, args: &[String], stdin: &[u8]) -> Result<(), String>;
}

/// Spawns the real tools.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemTools;

impl ToolRunner for SystemTools {
    fn available(&self, program: &str) -> bool {
        find_in_path(program).is_some()
    }

    fn run(&self, program: &str, args: &[String], stdin: &[u8]) -> Result<(), String> {
        let out = run_process(
            &ProcessSpec {
                stdin: Some(stdin),
                timeout: Duration::from_secs(10),
                capture_stderr: false,
                ..ProcessSpec::new(program, args)
            },
            &CancelToken::new(),
        )
        .map_err(|e| e.to_string())?;
        if out.success { Ok(()) } else { Err(format!("{program} exited with {:?}", out.exit_code)) }
    }
}

/// `wl-copy` / `xclip`. See the [module docs](self) for why.
pub struct ExternalBackend {
    session: Session,
    tools: Arc<dyn ToolRunner>,
}

impl std::fmt::Debug for ExternalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalBackend").field("session", &self.session).finish_non_exhaustive()
    }
}

impl ExternalBackend {
    /// The real tools for the current session.
    pub fn system() -> Self {
        Self { session: Session::from_env(), tools: Arc::new(SystemTools) }
    }

    /// Explicit session and tool runner (tests).
    pub fn new(session: Session, tools: Arc<dyn ToolRunner>) -> Self {
        Self { session, tools }
    }

    fn copy(&self, mime: &str, data: &[u8]) -> Result<(), BackendError> {
        let (program, args): (&str, Vec<String>) = match self.session {
            Session::Wayland => ("wl-copy", vec!["--type".into(), mime.into()]),
            Session::X11 => {
                ("xclip", ["-selection", "clipboard", "-in", "-t", mime].map(String::from).to_vec())
            }
            Session::None => return Err(BackendError::Unsupported),
        };
        if !self.tools.available(program) {
            return Err(BackendError::Unsupported);
        }
        self.tools.run(program, &args, data).map_err(BackendError::Failed)
    }
}

/// Percent-encodes a path for a `file://` URI (RFC 3986 unreserved characters and `/` stay).
pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/'); // Windows drive paths: file:///C:/...
    }
    for b in text.replace('\\', "/").bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(char::from(b));
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The clipboard payload for a file list in `flavor`, and its MIME type.
pub fn file_list_payload(paths: &[PathBuf], flavor: FileListFlavor) -> (&'static str, String) {
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    match flavor {
        FileListFlavor::GnomeCopiedFiles => {
            ("x-special/gnome-copied-files", format!("copy\n{}", uris.join("\n")))
        }
        // RFC 2483: CRLF-terminated lines.
        FileListFlavor::UriList => (
            "text/uri-list",
            uris.iter().fold(String::new(), |mut acc, u| {
                acc.push_str(u);
                acc.push_str("\r\n");
                acc
            }),
        ),
    }
}

/// Absolute, existing paths only: file managers cannot paste a relative or missing path.
fn absolute_existing(paths: &[PathBuf]) -> Result<Vec<PathBuf>, BackendError> {
    paths
        .iter()
        .map(|p| {
            std::fs::canonicalize(p).map_err(|e| {
                BackendError::Failed(format!("cannot copy {} to the clipboard: {e}", p.display()))
            })
        })
        .collect()
}

impl ClipboardBackend for ExternalBackend {
    fn name(&self) -> &'static str {
        match self.session {
            Session::Wayland => "wl-copy",
            Session::X11 => "xclip",
            Session::None => "external tools",
        }
    }

    fn set_image(&mut self, frame: &Frame) -> Result<(), BackendError> {
        if self.session == Session::None {
            return Err(BackendError::Unsupported);
        }
        let png = frame
            .encode(EncodeOptions { png_fast: true, ..EncodeOptions::new(ImageFormat::Png) })
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        self.copy("image/png", &png)
    }

    fn set_text(&mut self, text: &str) -> Result<(), BackendError> {
        self.copy("text/plain;charset=utf-8", text.as_bytes())
    }

    fn set_files(&mut self, paths: &[PathBuf], flavor: FileListFlavor) -> Result<(), BackendError> {
        if self.session == Session::None {
            return Err(BackendError::Unsupported);
        }
        let paths = absolute_existing(paths)?;
        let (mime, payload) = file_list_payload(&paths, flavor);
        self.copy(mime, payload.as_bytes())
    }

    fn read(&mut self) -> Result<ClipboardContent, BackendError> {
        Err(BackendError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ssx_types::{ColorSpace, PixelFormat, Size};

    use super::*;

    type Log = Arc<Mutex<Vec<String>>>;

    /// A backend that records calls and can be told to fail or decline.
    struct Fake {
        name: &'static str,
        log: Log,
        mode: Result<(), BackendError>,
        content: Option<ClipboardContent>,
    }

    impl Fake {
        fn boxed(
            name: &'static str,
            log: &Log,
            mode: Result<(), BackendError>,
        ) -> Box<dyn ClipboardBackend> {
            Box::new(Self { name, log: log.clone(), mode, content: None })
        }

        fn note(&self, what: &str) -> Result<(), BackendError> {
            self.log.lock().unwrap().push(format!("{}:{what}", self.name));
            self.mode.clone()
        }
    }

    impl ClipboardBackend for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn set_image(&mut self, f: &Frame) -> Result<(), BackendError> {
            self.note(&format!("image {}x{}", f.width(), f.height()))
        }
        fn set_text(&mut self, t: &str) -> Result<(), BackendError> {
            self.note(&format!("text {t}"))
        }
        fn set_files(&mut self, p: &[PathBuf], fl: FileListFlavor) -> Result<(), BackendError> {
            self.note(&format!("files {} {fl:?}", p.len()))
        }
        fn read(&mut self) -> Result<ClipboardContent, BackendError> {
            self.log.lock().unwrap().push(format!("{}:read", self.name));
            match &self.content {
                Some(c) => Ok(c.clone()),
                None => self.mode.clone().map(|()| ClipboardContent::Empty),
            }
        }
    }

    fn log() -> Log {
        Arc::default()
    }

    fn snapshot(l: &Log) -> Vec<String> {
        l.lock().unwrap().clone()
    }

    fn frame() -> Frame {
        Frame::from_rgba8(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]).unwrap()
    }

    #[test]
    fn the_first_working_backend_wins_and_later_ones_are_not_touched() {
        let l = log();
        let cb = SystemClipboard::with_backends(
            vec![(Fake::boxed("a", &l, Ok(())), false), (Fake::boxed("b", &l, Ok(())), false)],
            FileListFlavor::UriList,
        );
        cb.set_text("hello").unwrap();
        cb.set_image(&frame()).unwrap();
        assert_eq!(snapshot(&l), ["a:text hello", "a:image 2x1"]);
    }

    #[test]
    fn failures_and_declines_fall_through_to_the_next_backend() {
        let l = log();
        let cb = SystemClipboard::with_backends(
            vec![
                (Fake::boxed("declines", &l, Err(BackendError::Unsupported)), false),
                (Fake::boxed("breaks", &l, Err(BackendError::Failed("no display".into()))), false),
                (Fake::boxed("works", &l, Ok(())), false),
            ],
            FileListFlavor::UriList,
        );
        cb.set_text("x").unwrap();
        assert_eq!(snapshot(&l), ["declines:text x", "breaks:text x", "works:text x"]);
    }

    #[test]
    fn when_everything_fails_the_error_says_what_to_install() {
        let l = log();
        let cb = SystemClipboard::with_backends(
            vec![(
                Fake::boxed("arboard", &l, Err(BackendError::Failed("no display".into()))),
                false,
            )],
            FileListFlavor::UriList,
        );
        let e = cb.set_text("x").unwrap_err();
        let m = e.to_string();
        assert!(
            m.contains("arboard: no display") && m.contains("wl-clipboard") && m.contains("xclip"),
            "{m}"
        );
        assert!(matches!(e, ServiceError::Failed { retryable: false, .. }));

        let cb = SystemClipboard::with_backends(
            vec![(Fake::boxed("a", &l, Err(BackendError::Unsupported)), false)],
            FileListFlavor::UriList,
        );
        assert!(matches!(cb.set_text("x").unwrap_err(), ServiceError::Unsupported(_)));
        let none = SystemClipboard::with_backends(vec![], FileListFlavor::UriList);
        assert!(matches!(none.read().unwrap_err(), ServiceError::Unsupported(_)));
    }

    #[test]
    fn gnome_style_file_lists_prefer_the_external_tool_but_text_keeps_the_order() {
        let l = log();
        let backends = || {
            vec![
                (Fake::boxed("native", &l, Ok(())), false),
                (Fake::boxed("tool", &l, Ok(())), true),
            ]
        };
        let gnome = SystemClipboard::with_backends(backends(), FileListFlavor::GnomeCopiedFiles);
        gnome.set_files(&[PathBuf::from("/a")]).unwrap();
        gnome.set_text("t").unwrap();
        let kde = SystemClipboard::with_backends(backends(), FileListFlavor::UriList);
        kde.set_files(&[PathBuf::from("/a")]).unwrap();
        assert_eq!(
            snapshot(&l),
            ["tool:files 1 GnomeCopiedFiles", "native:text t", "native:files 1 UriList"]
        );
        assert!(gnome.set_files(&[]).is_err(), "an empty list is an error, not a silent no-op");
    }

    #[test]
    fn unconverted_hdr_frames_are_refused() {
        let hdr = Frame::new(Size::new(1, 1), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
        let cb = SystemClipboard::with_backends(vec![], FileListFlavor::UriList);
        assert!(cb.set_image(&hdr).unwrap_err().to_string().contains("tone-mapped"));
    }

    #[test]
    fn read_returns_the_first_backends_content() {
        let l = log();
        let mut reader = Fake { name: "r", log: l.clone(), mode: Ok(()), content: None };
        reader.content = Some(ClipboardContent::Text("pasted".into()));
        let cb = SystemClipboard::with_backends(
            vec![
                (Fake::boxed("tool", &l, Err(BackendError::Unsupported)), true),
                (Box::new(reader), false),
            ],
            FileListFlavor::UriList,
        );
        assert!(matches!(cb.read().unwrap(), ClipboardContent::Text(t) if t == "pasted"));
    }

    #[test]
    fn flavor_detection_by_desktop() {
        if cfg!(target_os = "linux") {
            assert_eq!(FileListFlavor::detect(Some("GNOME")), FileListFlavor::GnomeCopiedFiles);
            assert_eq!(
                FileListFlavor::detect(Some("ubuntu:GNOME")),
                FileListFlavor::GnomeCopiedFiles
            );
            assert_eq!(FileListFlavor::detect(Some("XFCE")), FileListFlavor::GnomeCopiedFiles);
            assert_eq!(FileListFlavor::detect(Some("KDE")), FileListFlavor::UriList);
            assert_eq!(FileListFlavor::detect(Some("plasma")), FileListFlavor::UriList);
            assert_eq!(FileListFlavor::detect(None), FileListFlavor::GnomeCopiedFiles);
        } else {
            assert_eq!(FileListFlavor::detect(Some("GNOME")), FileListFlavor::UriList);
        }
    }

    #[test]
    fn uris_are_percent_encoded() {
        assert_eq!(file_uri(Path::new("/tmp/a b/é#%.png")), "file:///tmp/a%20b/%C3%A9%23%25.png");
        assert_eq!(file_uri(Path::new("C:\\Users\\me\\x.png")), "file:///C:/Users/me/x.png");
        assert_eq!(file_uri(Path::new("/plain/x-y_z.~png")), "file:///plain/x-y_z.~png");
    }

    #[test]
    fn file_list_payloads_match_what_file_managers_expect() {
        let paths = [PathBuf::from("/a b"), PathBuf::from("/c")];
        let (mime, body) = file_list_payload(&paths, FileListFlavor::GnomeCopiedFiles);
        assert_eq!(mime, "x-special/gnome-copied-files");
        assert_eq!(
            body, "copy\nfile:///a%20b\nfile:///c",
            "no trailing newline (Nautilus is strict)"
        );
        let (mime, body) = file_list_payload(&paths, FileListFlavor::UriList);
        assert_eq!(mime, "text/uri-list");
        assert_eq!(body, "file:///a%20b\r\nfile:///c\r\n");
    }

    type ToolCall = (String, Vec<String>, Vec<u8>);

    /// Records tool invocations instead of spawning.
    struct RecordingTools {
        installed: Vec<&'static str>,
        calls: Mutex<Vec<ToolCall>>,
    }

    impl ToolRunner for RecordingTools {
        fn available(&self, program: &str) -> bool {
            self.installed.contains(&program)
        }
        fn run(&self, program: &str, args: &[String], stdin: &[u8]) -> Result<(), String> {
            self.calls.lock().unwrap().push((program.to_owned(), args.to_vec(), stdin.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn external_backend_speaks_wl_copy_and_xclip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        let real = std::fs::canonicalize(&file).unwrap();

        let tools = Arc::new(RecordingTools {
            installed: vec!["wl-copy", "xclip"],
            calls: Mutex::default(),
        });
        let mut wl = ExternalBackend::new(Session::Wayland, tools.clone());
        wl.set_text("hi").unwrap();
        wl.set_image(&frame()).unwrap();
        wl.set_files(std::slice::from_ref(&file), FileListFlavor::GnomeCopiedFiles).unwrap();
        let mut x = ExternalBackend::new(Session::X11, tools.clone());
        x.set_text("yo").unwrap();
        x.set_files(&[file], FileListFlavor::UriList).unwrap();

        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].0, "wl-copy");
        assert_eq!(calls[0].1, ["--type", "text/plain;charset=utf-8"]);
        assert_eq!(calls[0].2, b"hi");
        assert_eq!(calls[1].1, ["--type", "image/png"]);
        assert!(calls[1].2.starts_with(b"\x89PNG"), "images are sent as PNG");
        assert_eq!(calls[2].1, ["--type", "x-special/gnome-copied-files"]);
        assert_eq!(
            String::from_utf8(calls[2].2.clone()).unwrap(),
            format!("copy\n{}", file_uri(&real))
        );
        assert_eq!(calls[3].0, "xclip");
        assert_eq!(
            calls[3].1,
            ["-selection", "clipboard", "-in", "-t", "text/plain;charset=utf-8"]
        );
        assert_eq!(calls[4].1, ["-selection", "clipboard", "-in", "-t", "text/uri-list"]);
        assert!(String::from_utf8(calls[4].2.clone()).unwrap().ends_with("a%20b.txt\r\n"));
    }

    #[test]
    fn external_backend_declines_when_the_tool_or_session_is_missing() {
        let tools = Arc::new(RecordingTools { installed: vec![], calls: Mutex::default() });
        let mut b = ExternalBackend::new(Session::Wayland, tools.clone());
        assert_eq!(b.set_text("x"), Err(BackendError::Unsupported));
        let mut b = ExternalBackend::new(Session::None, tools.clone());
        assert_eq!(b.set_text("x"), Err(BackendError::Unsupported));
        assert_eq!(b.set_image(&frame()), Err(BackendError::Unsupported));
        assert_eq!(b.read().unwrap_err(), BackendError::Unsupported);
        assert!(tools.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_files_are_reported_by_name() {
        let tools = Arc::new(RecordingTools { installed: vec!["xclip"], calls: Mutex::default() });
        let mut b = ExternalBackend::new(Session::X11, tools);
        let e = b.set_files(&[PathBuf::from("/definitely/not/here.png")], FileListFlavor::UriList);
        assert!(matches!(e, Err(BackendError::Failed(m)) if m.contains("here.png")));
    }

    #[test]
    fn rgba_conversion_handles_bgra_and_stride() {
        let mut data = vec![0u8; 12]; // 1x2 BGRA with stride 8 (4 bytes padding)
        data[..4].copy_from_slice(&[3, 2, 1, 255]);
        data[8..12].copy_from_slice(&[6, 5, 4, 255]);
        let f = Frame::from_raw(Size::new(1, 2), 8, PixelFormat::Bgra8, ColorSpace::Srgb, data)
            .unwrap();
        assert_eq!(rgba_bytes(&f).unwrap().as_ref(), [1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(matches!(rgba_bytes(&frame()).unwrap(), Cow::Borrowed(_)), "no copy for RGBA");
    }
}
