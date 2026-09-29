//! The service traits the workflow engine is written against.
//!
//! `ssx-core` knows nothing about screens, uploaders, editors or notification daemons. It
//! defines what it *needs* as small, object-safe, `Send + Sync` traits, and the application
//! layer (`ssx-app`, `ssx-cli`) plugs real implementations into a [`Services`] bundle:
//!
//! | Trait | Real implementation lives in |
//! |---|---|
//! | [`Capturer`] | `ssx-platform` (capture backends + HDR tonemap) |
//! | [`Recorder`] | the recording crate |
//! | [`Editor`], [`Pinner`], [`SaveDialog`], [`Notifier`] | the GUI app |
//! | [`Uploaders`], [`UrlShortener`] | `ssx-upload` |
//! | [`Clipboard`], [`UrlOpener`], [`Ocr`] | platform integration |
//! | [`QrRenderer`], [`CommandRunner`], [`FileSystem`] | defaults provided here |
//! | [`Zipper`] | app layer (any zip writer) |
//!
//! Conventions every implementation must follow:
//!
//! * **Cancellation.** Calls that can block take a [`CancelToken`]. Check it periodically and
//!   return [`ServiceError::Cancelled`] promptly; a user dismissing a dialog is also
//!   `Cancelled`, not a failure.
//! * **No panics, no blocking the caller forever.** The engine survives a panicking service
//!   (it is reported as a failed step) but that is a last resort.
//! * **Error messages are user-facing.** They are shown verbatim in notifications.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use ssx_types::Frame;

use super::{CancelToken, ServiceError};
use crate::{history::History, settings::DestinationType, settings::HdrConfig};

// ---- capture -------------------------------------------------------------------------

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureTarget {
    /// Interactive region selection.
    Region,
    /// The whole virtual desktop.
    Fullscreen,
    /// The monitor under the cursor.
    Monitor,
    /// The active window.
    Window,
    /// The previously selected region, without an overlay.
    LastRegion,
}

/// A capture request. The configured capture delay has **already elapsed** when this is
/// sent (the engine sleeps, cancellably, first).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureRequest {
    /// What to capture.
    pub target: CaptureTarget,
    /// Include the mouse cursor.
    pub include_cursor: bool,
    /// Tone-mapping parameters. The returned frame **must be 8-bit sRGB**
    /// ([`Frame::is_sdr8`]); tone-mapping HDR captures is the capturer's job.
    pub hdr: HdrConfig,
}

/// The result of a capture.
#[derive(Debug, Clone)]
pub struct Captured {
    /// The pixels (8-bit sRGB).
    pub frame: Frame,
    /// Title of the captured window, if known (`%t`).
    pub window_title: Option<String>,
    /// Process name of the captured window, if known (`%pn`).
    pub process_name: Option<String>,
}

impl Captured {
    /// A capture with no window information.
    pub fn new(frame: Frame) -> Self {
        Self { frame, window_title: None, process_name: None }
    }
}

/// Takes screenshots.
pub trait Capturer: Send + Sync {
    /// Captures according to `req`. Return [`ServiceError::Cancelled`] if the user aborts
    /// (Esc in the region overlay).
    fn capture(&self, req: &CaptureRequest, cancel: &CancelToken) -> Result<Captured, ServiceError>;
}

// ---- recording -----------------------------------------------------------------------

/// Recording container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// MP4 (or whatever the recorder is configured for).
    Video,
    /// Animated GIF.
    Gif,
}

/// A recording request.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordRequest {
    /// Container.
    pub kind: RecordKind,
    /// Directory the file must be created in (already exists).
    pub output_dir: PathBuf,
    /// File name **without extension**; the recorder picks the extension and must not
    /// overwrite an existing file (use [`crate::pattern::create_unique`]).
    pub file_stem: String,
    /// Include the mouse cursor.
    pub include_cursor: bool,
}

/// A finished recording.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedVideo {
    /// The finished file.
    pub path: PathBuf,
    /// Pixel width, if known.
    pub width: Option<u32>,
    /// Pixel height, if known.
    pub height: Option<u32>,
    /// Length, if known.
    pub duration: Option<Duration>,
}

/// Starts recordings.
pub trait Recorder: Send + Sync {
    /// Starts recording (region selection etc. happens here) and returns immediately with a
    /// handle. Selection cancelled → [`ServiceError::Cancelled`].
    fn start(&self, req: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError>;
}

/// A running recording.
pub trait RecordingSession: Send {
    /// Stops, finalises the file and returns it.
    fn stop(self: Box<Self>) -> Result<RecordedVideo, ServiceError>;
    /// Stops and discards the partial file.
    fn abort(self: Box<Self>);
}

// ---- editing and interactive UI -----------------------------------------------------

/// Outcome of an editing session.
#[derive(Debug, Clone)]
pub enum EditResult {
    /// The user finished; use this image from now on.
    Edited(Frame),
    /// The user closed the editor without accepting (cancels the workflow / item).
    Cancelled,
}

/// The image editor.
pub trait Editor: Send + Sync {
    /// Opens `frame` for editing and blocks until the user is done.
    fn edit(&self, frame: &Frame, cancel: &CancelToken) -> Result<EditResult, ServiceError>;
}

/// The "always on top" pinned image window.
pub trait Pinner: Send + Sync {
    /// Shows `frame` pinned to the screen.
    fn pin(&self, frame: &Frame) -> Result<(), ServiceError>;
}

/// Optical character recognition.
pub trait Ocr: Send + Sync {
    /// Recognises the text in `frame`.
    fn recognize(&self, frame: &Frame, cancel: &CancelToken) -> Result<String, ServiceError>;
}

/// A "save as" file chooser.
pub trait SaveDialog: Send + Sync {
    /// Asks where to save; `Ok(None)` means the user cancelled (skips only that step).
    fn choose_path(&self, suggested: &Path) -> Result<Option<PathBuf>, ServiceError>;
}

/// How prominent a notification is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    /// Something worked.
    Success,
    /// Something worked partially or is worth a look.
    Warning,
    /// Something failed.
    Error,
}

/// A desktop notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Level.
    pub level: NotificationLevel,
    /// Headline.
    pub title: String,
    /// Details (may be multi-line).
    pub body: String,
    /// URL to open when the notification is clicked.
    pub url: Option<String>,
    /// Local file to show/open when the notification is clicked.
    pub path: Option<PathBuf>,
}

/// Desktop notifications.
pub trait Notifier: Send + Sync {
    /// Shows a notification.
    fn notify(&self, notification: &Notification) -> Result<(), ServiceError>;

    /// Shows a QR code for `url` (a small window or a notification image).
    fn show_qr(&self, _url: &str, _image: &Frame) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported("showing QR codes".to_owned()))
    }
}

/// Opens URLs in the default browser.
pub trait UrlOpener: Send + Sync {
    /// Opens `url`.
    fn open(&self, url: &str) -> Result<(), ServiceError>;
}

/// Renders QR codes.
pub trait QrRenderer: Send + Sync {
    /// Renders `text` as a black-on-white 8-bit sRGB image including the quiet zone.
    fn render(&self, text: &str) -> Result<Frame, ServiceError>;
}

// ---- network -------------------------------------------------------------------------

/// Where the bytes of an upload come from.
#[derive(Debug, Clone, Copy)]
pub enum UploadSource<'a> {
    /// A file on disk (stream it; it may be gigabytes).
    LocalFile(&'a Path),
    /// Bytes in memory (an unsaved screenshot, clipboard text).
    Bytes(&'a [u8]),
}

/// An upload request. `destination` has been chosen by the engine from the settings
/// (workflow override > extension override > default for `kind`).
#[derive(Debug, Clone, Copy)]
pub struct UploadRequest<'a> {
    /// Uploader name as configured in settings.
    pub destination: &'a str,
    /// Content type class; one of image, text, file, video.
    pub kind: DestinationType,
    /// File name to present to the service.
    pub file_name: &'a str,
    /// MIME type.
    pub mime: &'a str,
    /// The data.
    pub source: UploadSource<'a>,
}

/// Upload progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadProgress {
    /// Bytes sent so far.
    pub sent: u64,
    /// Total bytes, if known.
    pub total: Option<u64>,
}

/// What a successful upload returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    /// Public URL. Must not be empty.
    pub url: String,
    /// Thumbnail URL, if the service makes one.
    pub thumbnail_url: Option<String>,
    /// URL that deletes the upload, if the service offers it.
    pub deletion_url: Option<String>,
}

impl UploadOutcome {
    /// An outcome with only a URL.
    pub fn url(url: impl Into<String>) -> Self {
        Self { url: url.into(), thumbnail_url: None, deletion_url: None }
    }
}

/// The upload subsystem.
pub trait Uploaders: Send + Sync {
    /// Uploads to the named destination. Call `progress` as data is sent (from the calling
    /// thread) and return [`ServiceError::Cancelled`] when `cancel` fires. Report retryable
    /// network trouble as [`ServiceError::retryable`].
    fn upload(
        &self,
        req: &UploadRequest<'_>,
        progress: &dyn Fn(UploadProgress),
        cancel: &CancelToken,
    ) -> Result<UploadOutcome, ServiceError>;
}

/// A URL shortening service.
pub trait UrlShortener: Send + Sync {
    /// Shortens `url` with the named provider.
    fn shorten(&self, provider: &str, url: &str, cancel: &CancelToken) -> Result<String, ServiceError>;
}

// ---- clipboard -----------------------------------------------------------------------

/// What is on the clipboard.
#[derive(Debug, Clone)]
pub enum ClipboardContent {
    /// An image.
    Image(Frame),
    /// Text.
    Text(String),
    /// A list of files (copied in a file manager).
    Files(Vec<PathBuf>),
    /// Nothing usable.
    Empty,
}

/// The system clipboard.
pub trait Clipboard: Send + Sync {
    /// Puts an image on the clipboard.
    fn set_image(&self, frame: &Frame) -> Result<(), ServiceError>;
    /// Puts text on the clipboard.
    fn set_text(&self, text: &str) -> Result<(), ServiceError>;
    /// Puts a file list on the clipboard (paste into a file manager).
    fn set_files(&self, paths: &[PathBuf]) -> Result<(), ServiceError>;
    /// Reads the clipboard (for the `clipboard` input).
    fn read(&self) -> Result<ClipboardContent, ServiceError>;
}

// ---- processes and files -------------------------------------------------------------

/// A program to run. **There is no shell**: `program` is executed directly and each element
/// of `args` is exactly one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable.
    pub program: String,
    /// Arguments, already template-expanded.
    pub args: Vec<String>,
    /// Kill the process after this long.
    pub timeout: Duration,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code (`None` if killed by a signal).
    pub exit_code: Option<i32>,
    /// `true` for exit code 0.
    pub success: bool,
    /// The last few KiB of standard error, for error messages.
    pub stderr_tail: String,
}

/// Runs external programs (the `run_command` after-upload task).
pub trait CommandRunner: Send + Sync {
    /// Runs `spec` and waits. A non-zero exit is *not* an `Err` (see [`CommandOutput`]);
    /// failing to start, timing out and cancellation are.
    fn run(&self, spec: &CommandSpec, cancel: &CancelToken) -> Result<CommandOutput, ServiceError>;
}

/// The subset of the file system the engine uses, abstracted for testing.
pub trait FileSystem: Send + Sync {
    /// Creates `dir` if needed and atomically creates a *new* file named `file_name` in it
    /// (` (2)`, ` (3)`, … on collisions), writing `bytes`. Returns the final path.
    fn write_unique(&self, dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<PathBuf>;
    /// Writes (creating or overwriting) exactly `path`, creating parent directories.
    fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Reads a whole file.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Opens a file for streaming reads.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// Removes a file. A missing file is `ErrorKind::NotFound`.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// `true` if something exists at `path`.
    fn exists(&self, path: &Path) -> bool;
    /// `true` if `path` is a directory.
    fn is_dir(&self, path: &Path) -> bool;
    /// Size of a file in bytes.
    fn file_len(&self, path: &Path) -> io::Result<u64>;
    /// Creates a directory and its parents.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
}

/// Zips folders for `post_file`.
pub trait Zipper: Send + Sync {
    /// Creates a zip archive of `folder` in a temporary location and returns its path. The
    /// engine deletes the archive when it has finished with it.
    fn zip_folder(&self, folder: &Path, cancel: &CancelToken) -> Result<PathBuf, ServiceError>;
}

// ---- the bundle ----------------------------------------------------------------------

/// Everything the engine talks to. Fields are public so an application can start from
/// [`Services::with_defaults`] and override only what it has.
#[derive(Clone, Copy)]
pub struct Services<'a> {
    /// Screenshots.
    pub capturer: &'a dyn Capturer,
    /// Screen recording.
    pub recorder: &'a dyn Recorder,
    /// Image editor.
    pub editor: &'a dyn Editor,
    /// Uploads.
    pub uploaders: &'a dyn Uploaders,
    /// URL shortening.
    pub shortener: &'a dyn UrlShortener,
    /// Clipboard.
    pub clipboard: &'a dyn Clipboard,
    /// Notifications.
    pub notifier: &'a dyn Notifier,
    /// Browser launching.
    pub opener: &'a dyn UrlOpener,
    /// QR code rendering.
    pub qr: &'a dyn QrRenderer,
    /// External programs.
    pub commands: &'a dyn CommandRunner,
    /// File system.
    pub fs: &'a dyn FileSystem,
    /// Folder zipping.
    pub zipper: &'a dyn Zipper,
    /// Save-as dialog.
    pub save_dialog: &'a dyn SaveDialog,
    /// Pin-to-screen window.
    pub pinner: &'a dyn Pinner,
    /// OCR.
    pub ocr: &'a dyn Ocr,
    /// History database; `None` disables recording.
    pub history: Option<&'a History>,
}

impl std::fmt::Debug for Services<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").field("history", &self.history.is_some()).finish_non_exhaustive()
    }
}

impl Services<'static> {
    /// The real [`StdFileSystem`], [`ProcessCommandRunner`] and [`QrCodeRenderer`], with
    /// [`Unsupported`] for everything that needs the application. Override the fields you
    /// implement:
    ///
    /// ```
    /// # use ssx_core::workflow::Services;
    /// let services = Services { history: None, ..Services::with_defaults() };
    /// ```
    pub fn with_defaults() -> Self {
        static FS: StdFileSystem = StdFileSystem;
        static RUNNER: ProcessCommandRunner = ProcessCommandRunner;
        static QR: QrCodeRenderer = QrCodeRenderer;
        static NO: Unsupported = Unsupported;
        Self {
            capturer: &NO,
            recorder: &NO,
            editor: &NO,
            uploaders: &NO,
            shortener: &NO,
            clipboard: &NO,
            notifier: &NO,
            opener: &NO,
            qr: &QR,
            commands: &RUNNER,
            fs: &FS,
            zipper: &NO,
            save_dialog: &NO,
            pinner: &NO,
            ocr: &NO,
            history: None,
        }
    }
}

/// A stand-in for every service the application has not provided: each call fails with
/// [`ServiceError::Unsupported`], which the engine reports per step.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unsupported;

fn no<T>(what: &str) -> Result<T, ServiceError> {
    Err(ServiceError::Unsupported(what.to_owned()))
}

impl Capturer for Unsupported {
    fn capture(&self, _: &CaptureRequest, _: &CancelToken) -> Result<Captured, ServiceError> {
        no("screen capture")
    }
}
impl Recorder for Unsupported {
    fn start(&self, _: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError> {
        no("screen recording")
    }
}
impl Editor for Unsupported {
    fn edit(&self, _: &Frame, _: &CancelToken) -> Result<EditResult, ServiceError> {
        no("the image editor")
    }
}
impl Uploaders for Unsupported {
    fn upload(
        &self,
        _: &UploadRequest<'_>,
        _: &dyn Fn(UploadProgress),
        _: &CancelToken,
    ) -> Result<UploadOutcome, ServiceError> {
        no("uploading")
    }
}
impl UrlShortener for Unsupported {
    fn shorten(&self, _: &str, _: &str, _: &CancelToken) -> Result<String, ServiceError> {
        no("URL shortening")
    }
}
impl Clipboard for Unsupported {
    fn set_image(&self, _: &Frame) -> Result<(), ServiceError> {
        no("the clipboard")
    }
    fn set_text(&self, _: &str) -> Result<(), ServiceError> {
        no("the clipboard")
    }
    fn set_files(&self, _: &[PathBuf]) -> Result<(), ServiceError> {
        no("the clipboard")
    }
    fn read(&self) -> Result<ClipboardContent, ServiceError> {
        no("the clipboard")
    }
}
impl Notifier for Unsupported {
    fn notify(&self, _: &Notification) -> Result<(), ServiceError> {
        no("notifications")
    }
}
impl UrlOpener for Unsupported {
    fn open(&self, _: &str) -> Result<(), ServiceError> {
        no("opening URLs")
    }
}
impl Zipper for Unsupported {
    fn zip_folder(&self, _: &Path, _: &CancelToken) -> Result<PathBuf, ServiceError> {
        no("zipping folders")
    }
}
impl SaveDialog for Unsupported {
    fn choose_path(&self, _: &Path) -> Result<Option<PathBuf>, ServiceError> {
        no("the save dialog")
    }
}
impl Pinner for Unsupported {
    fn pin(&self, _: &Frame) -> Result<(), ServiceError> {
        no("pinning images to the screen")
    }
}
impl Ocr for Unsupported {
    fn recognize(&self, _: &Frame, _: &CancelToken) -> Result<String, ServiceError> {
        no("text recognition")
    }
}

// ---- default implementations ---------------------------------------------------------

/// [`FileSystem`] backed by `std::fs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn write_unique(&self, dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        crate::pattern::write_unique(dir, file_name, bytes)
    }
    fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, bytes)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
}

/// [`CommandRunner`] that spawns a process directly (never through a shell), with stdin and
/// stdout closed, a bounded stderr capture, a timeout and cancellation.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessCommandRunner;

const STDERR_TAIL_BYTES: usize = 4096;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, spec: &CommandSpec, cancel: &CancelToken) -> Result<CommandOutput, ServiceError> {
        use std::process::{Command, Stdio};

        if spec.program.trim().is_empty() {
            return Err(ServiceError::NotConfigured("run_command has no program".to_owned()));
        }
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| {
            ServiceError::failed(format!("cannot start {:?}: {e}", spec.program))
        })?;
        let mut stderr = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut tail: Vec<u8> = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let excess = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..excess);
                    }
                }
            }
            String::from_utf8_lossy(&tail).trim().to_owned()
        });

        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    return Err(ServiceError::Io(e));
                }
            }
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(ServiceError::Cancelled);
            }
            if started.elapsed() >= spec.timeout {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(ServiceError::failed(format!(
                    "{:?} did not finish within {} seconds and was stopped",
                    spec.program,
                    spec.timeout.as_secs()
                )));
            }
            cancel.wait_timeout(Duration::from_millis(15));
        };
        let stderr_tail = reader.join().unwrap_or_default();
        Ok(CommandOutput { exit_code: status.code(), success: status.success(), stderr_tail })
    }
}

/// [`QrRenderer`] using the `qrcode` crate: black modules on white, 4-module quiet zone,
/// 8 pixels per module.
#[derive(Debug, Default, Clone, Copy)]
pub struct QrCodeRenderer;

impl QrRenderer for QrCodeRenderer {
    fn render(&self, text: &str) -> Result<Frame, ServiceError> {
        const SCALE: usize = 8;
        const QUIET: usize = 4;
        let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| {
            ServiceError::failed(format!("cannot make a QR code for this text: {e}"))
        })?;
        let modules = code.width();
        let colors = code.to_colors();
        let side = (modules + 2 * QUIET) * SCALE;
        let mut rgba = vec![255u8; side * side * 4];
        for (i, c) in colors.iter().enumerate() {
            if *c != qrcode::Color::Dark {
                continue;
            }
            let (mx, my) = (i % modules + QUIET, i / modules + QUIET);
            for dy in 0..SCALE {
                let row = (my * SCALE + dy) * side + mx * SCALE;
                for px in rgba[row * 4..(row + SCALE) * 4].chunks_exact_mut(4) {
                    px[..3].fill(0);
                }
            }
        }
        let side = u32::try_from(side).map_err(|_| ServiceError::failed("QR code too large"))?;
        Frame::from_rgba8(side, side, rgba).map_err(|e| ServiceError::failed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_are_thread_safe() {
        fn check<T: Send + Sync>() {}
        check::<Services<'static>>();
    }

    #[test]
    fn qr_code_is_square_black_on_white_with_quiet_zone() {
        let f = QrCodeRenderer.render("https://example.com/abc").unwrap();
        assert_eq!(f.width(), f.height());
        assert!(f.is_sdr8());
        assert_eq!(f.width() % 8, 0);
        // top-left pixel is inside the quiet zone: white
        assert_eq!(&f.row(0)[..4], &[255, 255, 255, 255]);
        // and there are dark pixels
        let dark = (0..f.height()).any(|y| f.row(y).chunks_exact(4).any(|p| p[0] == 0));
        assert!(dark);
        // first module (finder pattern corner) at (4,4) is dark
        let y = 4 * 8;
        assert_eq!(&f.row(y)[4 * 8 * 4..4 * 8 * 4 + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn qr_code_rejects_oversized_input() {
        let big = "x".repeat(5000);
        assert!(matches!(QrCodeRenderer.render(&big), Err(ServiceError::Failed { .. })));
    }

    #[test]
    fn unsupported_reports_what_is_missing() {
        let e = Unsupported.open("https://x").unwrap_err();
        assert!(matches!(&e, ServiceError::Unsupported(w) if w.contains("URLs")));
    }

    #[test]
    fn std_fs_basic_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = StdFileSystem;
        let a = fs.write_unique(&tmp.path().join("d"), "a.png", b"1").unwrap();
        let b = fs.write_unique(&tmp.path().join("d"), "a.png", b"2").unwrap();
        assert_ne!(a, b);
        assert_eq!(fs.file_len(&a).unwrap(), 1);
        assert!(fs.exists(&a) && !fs.is_dir(&a) && fs.is_dir(&tmp.path().join("d")));
        let mut s = String::new();
        fs.open_read(&b).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "2");
        fs.remove_file(&a).unwrap();
        assert_eq!(fs.remove_file(&a).unwrap_err().kind(), io::ErrorKind::NotFound);
        let p = tmp.path().join("x/y/z.txt");
        fs.write_file(&p, b"hi").unwrap();
        fs.write_file(&p, b"over").unwrap();
        assert_eq!(fs.read(&p).unwrap(), b"over");
    }

    #[cfg(unix)]
    mod process {
        use super::*;

        fn spec(program: &str, args: &[&str]) -> CommandSpec {
            CommandSpec {
                program: program.to_owned(),
                args: args.iter().map(|s| (*s).to_owned()).collect(),
                timeout: Duration::from_secs(10),
            }
        }

        #[test]
        fn runs_without_a_shell() {
            // If a shell were involved, `;` would start a second command.
            let out = ProcessCommandRunner
                .run(&spec("sh", &["-c", "echo \"$0\" >&2; exit 3", "a; touch /nonexistent/x"]), &CancelToken::new())
                .unwrap();
            assert_eq!(out.exit_code, Some(3));
            assert!(!out.success);
            assert_eq!(out.stderr_tail, "a; touch /nonexistent/x");
        }

        #[test]
        fn arguments_are_not_word_split_or_expanded() {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("out.txt");
            let arg = "two words; $(echo injected) `id` *";
            let out = ProcessCommandRunner
                .run(
                    &spec("sh", &["-c", "printf %s \"$1\" > \"$2\"", "sh", arg, file.to_str().unwrap()]),
                    &CancelToken::new(),
                )
                .unwrap();
            assert!(out.success);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), arg);
        }

        #[test]
        fn success_and_missing_program() {
            assert!(ProcessCommandRunner.run(&spec("true", &[]), &CancelToken::new()).unwrap().success);
            let e = ProcessCommandRunner
                .run(&spec("definitely-not-a-program-ssx", &[]), &CancelToken::new())
                .unwrap_err();
            assert!(e.to_string().contains("definitely-not-a-program-ssx"), "{e}");
            let e = ProcessCommandRunner.run(&spec("  ", &[]), &CancelToken::new()).unwrap_err();
            assert!(matches!(e, ServiceError::NotConfigured(_)));
        }

        #[test]
        fn timeout_kills_the_process() {
            let mut s = spec("sleep", &["30"]);
            s.timeout = Duration::from_millis(100);
            let started = std::time::Instant::now();
            let e = ProcessCommandRunner.run(&s, &CancelToken::new()).unwrap_err();
            assert!(e.to_string().contains("did not finish"), "{e}");
            assert!(started.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn cancellation_kills_the_process() {
            let cancel = CancelToken::new();
            let c2 = cancel.clone();
            let h = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                c2.cancel();
            });
            let started = std::time::Instant::now();
            let e = ProcessCommandRunner.run(&spec("sleep", &["30"]), &cancel).unwrap_err();
            assert!(e.is_cancelled());
            assert!(started.elapsed() < Duration::from_secs(5));
            h.join().unwrap();
            // already-cancelled tokens never spawn
            assert!(ProcessCommandRunner.run(&spec("true", &[]), &cancel).unwrap_err().is_cancelled());
        }

        #[test]
        fn stderr_is_bounded() {
            let out = ProcessCommandRunner
                .run(&spec("sh", &["-c", "head -c 100000 /dev/zero | tr '\\0' x >&2"]), &CancelToken::new())
                .unwrap();
            assert!(out.stderr_tail.len() <= STDERR_TAIL_BYTES);
            assert!(out.stderr_tail.len() > 1000);
        }
    }
}
