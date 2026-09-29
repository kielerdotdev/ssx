//! Recording mock services for testing the engine (and applications that drive it).
//!
//! Every mock appends a line to one shared [`Log`], so a test can assert the exact
//! *cross-service* order of side effects (`capture`, then `editor.edit`, then `fs.write`, then
//! `upload`, …). [`TestWorld`] wires them all into a [`Services`] bundle with an in-memory
//! file system and history database, and a fixed clock so file names are deterministic.
//!
//! Enabled for this crate's tests and, for downstream crates, by the `testing` feature.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ssx_types::Frame;

use super::{
    CancelToken, CaptureRequest, Captured, Capturer, Clipboard, ClipboardContent, CommandOutput,
    CommandRunner, CommandSpec, EditResult, Editor, Engine, FileSystem, Naming, Notification,
    Notifier, Ocr, Pinner, QrCodeRenderer, RecordRequest, RecordedVideo, Recorder,
    RecordingSession, SaveDialog, ServiceError, Services, UploadOutcome, UploadProgress,
    UploadRequest, UploadSource, Uploaders, UrlOpener, UrlShortener, Zipper,
};
use crate::{
    history::History,
    pattern::{FixedClock, MemoryCounter, SeededRng, StaticEnv, candidate_name},
    settings::{DestinationType, Settings},
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A shared, ordered log of side effects.
#[derive(Debug, Clone, Default)]
pub struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    /// Appends a line.
    pub fn push(&self, line: impl Into<String>) {
        lock(&self.0).push(line.into());
    }
    /// All lines so far.
    pub fn lines(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
    /// Lines starting with `prefix`.
    pub fn with_prefix(&self, prefix: &str) -> Vec<String> {
        self.lines().into_iter().filter(|l| l.starts_with(prefix)).collect()
    }
    /// Index of the first line starting with `prefix`.
    pub fn position(&self, prefix: &str) -> Option<usize> {
        self.lines().iter().position(|l| l.starts_with(prefix))
    }
    /// Clears the log.
    pub fn clear(&self) {
        lock(&self.0).clear();
    }
}

/// A scripted failure.
#[derive(Debug, Clone)]
pub enum Fail {
    /// [`ServiceError::Cancelled`].
    Cancelled,
    /// [`ServiceError::Unsupported`].
    Unsupported(String),
    /// [`ServiceError::NotConfigured`].
    NotConfigured(String),
    /// [`ServiceError::failed`].
    Message(String),
    /// [`ServiceError::retryable`].
    Retryable(String),
}

impl Fail {
    /// Shorthand for [`Fail::Message`].
    pub fn msg(m: &str) -> Self {
        Self::Message(m.to_owned())
    }
    /// The error to return.
    pub fn error(&self) -> ServiceError {
        match self {
            Fail::Cancelled => ServiceError::Cancelled,
            Fail::Unsupported(s) => ServiceError::Unsupported(s.clone()),
            Fail::NotConfigured(s) => ServiceError::NotConfigured(s.clone()),
            Fail::Message(s) => ServiceError::failed(s.clone()),
            Fail::Retryable(s) => ServiceError::retryable(s.clone()),
        }
    }
}

/// A solid-colour test frame.
pub fn test_frame(width: u32, height: u32) -> Frame {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
    }
    Frame::from_rgba8(width, height, data).expect("exact-size buffer")
}

// ---- in-memory file system -------------------------------------------------------------

#[derive(Debug, Default)]
struct MemState {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
    fail_writes: Option<String>,
    fail_remove: Option<io::ErrorKind>,
}

/// An in-memory [`FileSystem`] with scriptable failures.
#[derive(Debug, Default, Clone)]
pub struct MemFs {
    state: Arc<Mutex<MemState>>,
    log: Log,
}

impl MemFs {
    /// An empty file system logging to `log`.
    pub fn new(log: Log) -> Self {
        Self { state: Arc::default(), log }
    }
    /// Adds a file (and its parent directories).
    pub fn add_file(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        let path = path.into();
        let mut s = lock(&self.state);
        Self::add_parents(&mut s, &path);
        s.files.insert(path, bytes.into());
    }
    /// Adds a directory.
    pub fn add_dir(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let mut s = lock(&self.state);
        Self::add_parents(&mut s, &path);
        s.dirs.insert(path);
    }
    fn add_parents(s: &mut MemState, path: &Path) {
        for a in path.ancestors().skip(1) {
            if !a.as_os_str().is_empty() {
                s.dirs.insert(a.to_path_buf());
            }
        }
    }
    /// Contents of a file.
    pub fn contents(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        lock(&self.state).files.get(path.as_ref()).cloned()
    }
    /// Paths of all files.
    pub fn files(&self) -> Vec<PathBuf> {
        lock(&self.state).files.keys().cloned().collect()
    }
    /// Makes every write fail with this message.
    pub fn fail_writes(&self, message: Option<&str>) {
        lock(&self.state).fail_writes = message.map(str::to_owned);
    }
    /// Makes every removal fail with this error kind.
    pub fn fail_remove(&self, kind: Option<io::ErrorKind>) {
        lock(&self.state).fail_remove = kind;
    }
}

impl FileSystem for MemFs {
    fn write_unique(&self, dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut s = lock(&self.state);
        if let Some(m) = &s.fail_writes {
            return Err(io::Error::other(m.clone()));
        }
        for attempt in 0..10_000 {
            let path = dir.join(candidate_name(file_name, attempt));
            if !s.files.contains_key(&path) && !s.dirs.contains(&path) {
                Self::add_parents(&mut s, &path);
                s.files.insert(path.clone(), bytes.to_vec());
                drop(s);
                self.log.push(format!("fs.write:{}", path.display()));
                return Ok(path);
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name"))
    }
    fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut s = lock(&self.state);
        if let Some(m) = &s.fail_writes {
            return Err(io::Error::other(m.clone()));
        }
        Self::add_parents(&mut s, path);
        s.files.insert(path.to_path_buf(), bytes.to_vec());
        drop(s);
        self.log.push(format!("fs.write_file:{}", path.display()));
        Ok(())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        lock(&self.state)
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(self.read(path)?)))
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut s = lock(&self.state);
        if let Some(kind) = s.fail_remove {
            return Err(io::Error::new(kind, "scripted removal failure"));
        }
        let existed = s.files.remove(path).is_some();
        drop(s);
        self.log.push(format!("fs.remove:{}", path.display()));
        if existed { Ok(()) } else { Err(io::Error::new(io::ErrorKind::NotFound, "not found")) }
    }
    fn exists(&self, path: &Path) -> bool {
        let s = lock(&self.state);
        s.files.contains_key(path) || s.dirs.contains(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        lock(&self.state).dirs.contains(path)
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        lock(&self.state)
            .files
            .get(path)
            .map(|b| b.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.add_dir(path);
        Ok(())
    }
}

// ---- mocks -----------------------------------------------------------------------------

/// Scripted screen capturer.
#[derive(Debug)]
pub struct MockCapturer {
    log: Log,
    /// Results returned in order; when empty a 64x48 test frame is returned.
    pub script: Mutex<VecDeque<Result<Captured, Fail>>>,
    /// Size of default frames.
    pub size: Mutex<(u32, u32)>,
    /// Window title reported with default captures.
    pub window_title: Mutex<Option<String>>,
    /// Requests received.
    pub requests: Mutex<Vec<CaptureRequest>>,
    /// If set, returns a frame in this pixel format instead (to test rejection).
    pub hdr_frame: Mutex<bool>,
}

impl MockCapturer {
    fn new(log: Log) -> Self {
        Self {
            log,
            script: Mutex::default(),
            size: Mutex::new((64, 48)),
            window_title: Mutex::default(),
            requests: Mutex::default(),
            hdr_frame: Mutex::default(),
        }
    }
}

impl Capturer for MockCapturer {
    fn capture(
        &self,
        req: &CaptureRequest,
        _cancel: &CancelToken,
    ) -> Result<Captured, ServiceError> {
        self.log.push(format!("capture:{:?}", req.target));
        lock(&self.requests).push(*req);
        if let Some(next) = lock(&self.script).pop_front() {
            return next.map_err(|f| f.error());
        }
        if *lock(&self.hdr_frame) {
            let f = Frame::new(
                ssx_types::Size::new(4, 4),
                ssx_types::PixelFormat::Rgba16F,
                ssx_types::ColorSpace::ScRgbLinear,
            );
            return Ok(Captured::new(f));
        }
        let (w, h) = *lock(&self.size);
        let mut c = Captured::new(test_frame(w, h));
        c.window_title.clone_from(&lock(&self.window_title));
        Ok(c)
    }
}

/// Scripted recorder that writes a fake file through the [`MemFs`].
#[derive(Debug)]
pub struct MockRecorder {
    log: Log,
    fs: MemFs,
    /// Fail `start` with this.
    pub fail_start: Mutex<Option<Fail>>,
    /// Fail `stop` with this.
    pub fail_stop: Mutex<Option<Fail>>,
    /// Requests received.
    pub requests: Mutex<Vec<RecordRequest>>,
}

struct MockSession {
    log: Log,
    fs: MemFs,
    req: RecordRequest,
    fail_stop: Option<Fail>,
}

impl RecordingSession for MockSession {
    fn stop(self: Box<Self>) -> Result<RecordedVideo, ServiceError> {
        self.log.push("record.stop");
        if let Some(f) = &self.fail_stop {
            return Err(f.error());
        }
        let ext = match self.req.kind {
            super::RecordKind::Video => "mp4",
            super::RecordKind::Gif => "gif",
        };
        let path = self.fs.write_unique(
            &self.req.output_dir,
            &format!("{}.{ext}", self.req.file_stem),
            b"fake video bytes",
        )?;
        Ok(RecordedVideo {
            path,
            width: Some(640),
            height: Some(360),
            duration: Some(Duration::from_secs(3)),
        })
    }
    fn abort(self: Box<Self>) {
        self.log.push("record.abort");
    }
}

impl Recorder for MockRecorder {
    fn start(&self, req: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError> {
        self.log.push(format!("record.start:{:?}", req.kind));
        lock(&self.requests).push(req.clone());
        if let Some(f) = lock(&self.fail_start).as_ref() {
            return Err(f.error());
        }
        Ok(Box::new(MockSession {
            log: self.log.clone(),
            fs: self.fs.clone(),
            req: req.clone(),
            fail_stop: lock(&self.fail_stop).clone(),
        }))
    }
}

/// What the mock editor does.
#[derive(Debug, Clone)]
pub enum EditorMode {
    /// Returns the image with its top-left pixel painted white (so edits are detectable).
    Edit,
    /// The user closes the editor without accepting.
    Cancel,
    /// The editor fails.
    Fail(Fail),
}

/// Scripted image editor.
#[derive(Debug)]
pub struct MockEditor {
    log: Log,
    /// Behaviour.
    pub mode: Mutex<EditorMode>,
}

impl Editor for MockEditor {
    fn edit(&self, frame: &Frame, _cancel: &CancelToken) -> Result<EditResult, ServiceError> {
        self.log.push(format!("editor.edit:{}x{}", frame.width(), frame.height()));
        match lock(&self.mode).clone() {
            EditorMode::Edit => {
                let mut f = frame.clone();
                if let Some(px) = f.row_mut(0).get_mut(..4) {
                    px.copy_from_slice(&[255, 255, 255, 255]);
                }
                Ok(EditResult::Edited(f))
            }
            EditorMode::Cancel => Ok(EditResult::Cancelled),
            EditorMode::Fail(f) => Err(f.error()),
        }
    }
}

/// Decrements the in-flight counter when an upload ends, however it ends.
struct Done<'a>(&'a AtomicUsize);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One upload the mock received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRecord {
    /// Destination name.
    pub destination: String,
    /// Content kind.
    pub kind: DestinationType,
    /// File name presented.
    pub file_name: String,
    /// MIME type.
    pub mime: String,
    /// Path if a file was streamed.
    pub path: Option<PathBuf>,
    /// Size if bytes were sent from memory.
    pub bytes: Option<usize>,
}

type UploadHook = Box<dyn Fn(&UploadRequest<'_>) -> Option<Fail> + Send + Sync>;

/// Scripted uploader. URLs look like `https://<destination>.test/<file name>`.
pub struct MockUploaders {
    log: Log,
    /// Everything uploaded, in call order.
    pub uploads: Mutex<Vec<UploadRecord>>,
    /// Inspects each request and may fail it.
    pub hook: Mutex<Option<UploadHook>>,
    /// File names (substring) whose upload returns an empty URL although "successful".
    pub empty_url_for: Mutex<Vec<String>>,
    /// Sleep this long per upload (cancellable) to simulate a slow network.
    pub delay: Mutex<Duration>,
    /// Cancel this token as soon as an upload starts (simulates the user pressing cancel
    /// mid-upload); the bool says whether the upload still completes successfully.
    pub cancel_on_start: Mutex<Option<(CancelToken, bool)>>,
    current: AtomicUsize,
    /// Highest number of simultaneous uploads observed.
    pub max_concurrent: AtomicUsize,
}

impl std::fmt::Debug for MockUploaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockUploaders").finish_non_exhaustive()
    }
}

impl MockUploaders {
    fn new(log: Log) -> Self {
        Self {
            log,
            uploads: Mutex::default(),
            hook: Mutex::default(),
            empty_url_for: Mutex::default(),
            delay: Mutex::default(),
            cancel_on_start: Mutex::default(),
            current: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
        }
    }

    /// Fails uploads whose file name contains `needle`.
    pub fn fail_file(&self, needle: &str, fail: Fail) {
        let needle = needle.to_owned();
        *lock(&self.hook) =
            Some(Box::new(move |r| r.file_name.contains(&needle).then(|| fail.clone())));
    }

    /// Fails every upload.
    pub fn fail_all(&self, fail: Fail) {
        *lock(&self.hook) = Some(Box::new(move |_| Some(fail.clone())));
    }
}

impl Uploaders for MockUploaders {
    fn upload(
        &self,
        req: &UploadRequest<'_>,
        progress: &dyn Fn(UploadProgress),
        cancel: &CancelToken,
    ) -> Result<UploadOutcome, ServiceError> {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now, Ordering::SeqCst);
        let _done = Done(&self.current);

        let (path, bytes) = match req.source {
            UploadSource::LocalFile(p) => (Some(p.to_path_buf()), None),
            UploadSource::Bytes(b) => (None, Some(b.len())),
        };
        self.log.push(format!(
            "upload:{}:{:?}:{}:{}",
            req.destination,
            req.kind,
            req.file_name,
            if path.is_some() { "file" } else { "bytes" }
        ));
        lock(&self.uploads).push(UploadRecord {
            destination: req.destination.to_owned(),
            kind: req.kind,
            file_name: req.file_name.to_owned(),
            mime: req.mime.to_owned(),
            path,
            bytes,
        });
        let cancel_hook = lock(&self.cancel_on_start).clone();
        if let Some((token, complete)) = cancel_hook {
            token.cancel();
            if !complete {
                return Err(ServiceError::Cancelled);
            }
        }
        let total = bytes.map_or(1000, |b| b as u64);
        progress(UploadProgress { sent: total / 2, total: Some(total) });
        let delay = *lock(&self.delay);
        if !delay.is_zero() && !cancel.sleep(delay) {
            return Err(ServiceError::Cancelled);
        }
        if let Some(hook) = lock(&self.hook).as_ref()
            && let Some(f) = hook(req)
        {
            return Err(f.error());
        }
        progress(UploadProgress { sent: total, total: Some(total) });
        if lock(&self.empty_url_for).iter().any(|n| req.file_name.contains(n.as_str())) {
            return Ok(UploadOutcome::url(""));
        }
        Ok(UploadOutcome {
            url: format!("https://{}.test/{}", req.destination, req.file_name),
            thumbnail_url: Some(format!("https://{}.test/t/{}", req.destination, req.file_name)),
            deletion_url: Some(format!("https://{}.test/del/{}", req.destination, req.file_name)),
        })
    }
}

/// Scripted URL shortener (`https://sho.rt/<n>` with a per-call counter).
#[derive(Debug)]
pub struct MockShortener {
    log: Log,
    /// Fails every call.
    pub fail: Mutex<Option<Fail>>,
    /// Returns an empty string.
    pub empty: Mutex<bool>,
    calls: AtomicUsize,
}

impl UrlShortener for MockShortener {
    fn shorten(
        &self,
        provider: &str,
        url: &str,
        _cancel: &CancelToken,
    ) -> Result<String, ServiceError> {
        self.log.push(format!("shorten:{provider}:{url}"));
        if let Some(f) = lock(&self.fail).as_ref() {
            return Err(f.error());
        }
        if *lock(&self.empty) {
            return Ok(String::new());
        }
        Ok(format!("https://sho.rt/{}", self.calls.fetch_add(1, Ordering::SeqCst) + 1))
    }
}

/// Scripted clipboard.
#[derive(Debug)]
pub struct MockClipboard {
    log: Log,
    /// Last image set.
    pub image: Mutex<Option<Frame>>,
    /// Last text set.
    pub text: Mutex<Option<String>>,
    /// Last file list set.
    pub files: Mutex<Option<Vec<PathBuf>>>,
    /// What `read` returns.
    pub content: Mutex<Option<ClipboardContent>>,
    /// Every write fails with this.
    pub fail: Mutex<Option<Fail>>,
}

impl MockClipboard {
    fn check(&self) -> Result<(), ServiceError> {
        match lock(&self.fail).as_ref() {
            Some(f) => Err(f.error()),
            None => Ok(()),
        }
    }
}

impl Clipboard for MockClipboard {
    fn set_image(&self, frame: &Frame) -> Result<(), ServiceError> {
        self.log.push(format!("clipboard.image:{}x{}", frame.width(), frame.height()));
        self.check()?;
        *lock(&self.image) = Some(frame.clone());
        Ok(())
    }
    fn set_text(&self, text: &str) -> Result<(), ServiceError> {
        self.log.push(format!("clipboard.text:{}", text.replace('\n', "\\n")));
        self.check()?;
        *lock(&self.text) = Some(text.to_owned());
        Ok(())
    }
    fn set_files(&self, paths: &[PathBuf]) -> Result<(), ServiceError> {
        self.log.push(format!("clipboard.files:{}", paths.len()));
        self.check()?;
        *lock(&self.files) = Some(paths.to_vec());
        Ok(())
    }
    fn read(&self) -> Result<ClipboardContent, ServiceError> {
        self.log.push("clipboard.read");
        Ok(lock(&self.content).clone().unwrap_or(ClipboardContent::Empty))
    }
}

/// Scripted notifier.
#[derive(Debug)]
pub struct MockNotifier {
    log: Log,
    /// Notifications shown.
    pub shown: Mutex<Vec<Notification>>,
    /// QR codes shown (URL, side length).
    pub qr: Mutex<Vec<(String, u32)>>,
    /// Fails every call.
    pub fail: Mutex<Option<Fail>>,
}

impl Notifier for MockNotifier {
    fn notify(&self, n: &Notification) -> Result<(), ServiceError> {
        self.log.push(format!("notify:{:?}:{}", n.level, n.title));
        if let Some(f) = lock(&self.fail).as_ref() {
            return Err(f.error());
        }
        lock(&self.shown).push(n.clone());
        Ok(())
    }
    fn show_qr(&self, url: &str, image: &Frame) -> Result<(), ServiceError> {
        self.log.push(format!("qr:{url}"));
        if let Some(f) = lock(&self.fail).as_ref() {
            return Err(f.error());
        }
        lock(&self.qr).push((url.to_owned(), image.width()));
        Ok(())
    }
}

/// Scripted browser launcher.
#[derive(Debug)]
pub struct MockOpener {
    log: Log,
    /// Fails every call.
    pub fail: Mutex<Option<Fail>>,
}

impl UrlOpener for MockOpener {
    fn open(&self, url: &str) -> Result<(), ServiceError> {
        self.log.push(format!("open:{url}"));
        match lock(&self.fail).as_ref() {
            Some(f) => Err(f.error()),
            None => Ok(()),
        }
    }
}

/// Scripted command runner.
#[derive(Debug)]
pub struct MockCommands {
    log: Log,
    /// Commands received.
    pub runs: Mutex<Vec<CommandSpec>>,
    /// Exit code to report.
    pub exit_code: Mutex<i32>,
    /// Fails to start.
    pub fail: Mutex<Option<Fail>>,
}

impl CommandRunner for MockCommands {
    fn run(
        &self,
        spec: &CommandSpec,
        _cancel: &CancelToken,
    ) -> Result<CommandOutput, ServiceError> {
        self.log.push(format!("run:{} {:?}", spec.program, spec.args));
        lock(&self.runs).push(spec.clone());
        if let Some(f) = lock(&self.fail).as_ref() {
            return Err(f.error());
        }
        let code = *lock(&self.exit_code);
        Ok(CommandOutput {
            exit_code: Some(code),
            success: code == 0,
            stderr_tail: if code == 0 { String::new() } else { "boom".to_owned() },
        })
    }
}

/// Scripted zipper writing `/tmp/ssx-zip/<folder>.zip` into the [`MemFs`].
#[derive(Debug)]
pub struct MockZipper {
    log: Log,
    fs: MemFs,
    /// Fails every call.
    pub fail: Mutex<Option<Fail>>,
}

impl Zipper for MockZipper {
    fn zip_folder(&self, folder: &Path, _cancel: &CancelToken) -> Result<PathBuf, ServiceError> {
        self.log.push(format!("zip:{}", folder.display()));
        if let Some(f) = lock(&self.fail).as_ref() {
            return Err(f.error());
        }
        let name = folder
            .file_name()
            .map_or_else(|| "folder".to_owned(), |n| n.to_string_lossy().into_owned());
        Ok(self.fs.write_unique(
            Path::new("/tmp/ssx-zip"),
            &format!("{name}.zip"),
            b"PK fake zip",
        )?)
    }
}

/// Scripted save dialog.
#[derive(Debug)]
pub struct MockSaveDialog {
    log: Log,
    /// The path the "user" picks (`None` = cancels).
    pub choice: Mutex<Option<PathBuf>>,
}

impl SaveDialog for MockSaveDialog {
    fn choose_path(&self, suggested: &Path) -> Result<Option<PathBuf>, ServiceError> {
        self.log.push(format!("dialog.save:{}", suggested.display()));
        Ok(lock(&self.choice).clone())
    }
}

/// Scripted pin window.
#[derive(Debug)]
pub struct MockPinner {
    log: Log,
    /// Fails every call.
    pub fail: Mutex<Option<Fail>>,
}

impl Pinner for MockPinner {
    fn pin(&self, frame: &Frame) -> Result<(), ServiceError> {
        self.log.push(format!("pin:{}x{}", frame.width(), frame.height()));
        match lock(&self.fail).as_ref() {
            Some(f) => Err(f.error()),
            None => Ok(()),
        }
    }
}

/// Scripted OCR.
#[derive(Debug)]
pub struct MockOcr {
    log: Log,
    /// Text returned.
    pub text: Mutex<String>,
}

impl Ocr for MockOcr {
    fn recognize(&self, _frame: &Frame, _cancel: &CancelToken) -> Result<String, ServiceError> {
        self.log.push("ocr");
        Ok(lock(&self.text).clone())
    }
}

// ---- the bundle ------------------------------------------------------------------------

/// All mocks wired together.
#[derive(Debug)]
pub struct TestWorld {
    /// The shared side-effect log.
    pub log: Log,
    /// In-memory file system.
    pub fs: MemFs,
    /// Capturer.
    pub capturer: MockCapturer,
    /// Recorder.
    pub recorder: MockRecorder,
    /// Editor.
    pub editor: MockEditor,
    /// Uploaders.
    pub uploaders: MockUploaders,
    /// Shortener.
    pub shortener: MockShortener,
    /// Clipboard.
    pub clipboard: MockClipboard,
    /// Notifier.
    pub notifier: MockNotifier,
    /// Browser opener.
    pub opener: MockOpener,
    /// Command runner.
    pub commands: MockCommands,
    /// Zipper.
    pub zipper: MockZipper,
    /// Save dialog.
    pub save_dialog: MockSaveDialog,
    /// Pinner.
    pub pinner: MockPinner,
    /// OCR.
    pub ocr: MockOcr,
    /// In-memory history database.
    pub history: History,
    qr: QrCodeRenderer,
}

impl Default for TestWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl TestWorld {
    /// Fresh mocks; capture succeeds, everything else does nothing special.
    pub fn new() -> Self {
        let log = Log::default();
        let fs = MemFs::new(log.clone());
        Self {
            capturer: MockCapturer::new(log.clone()),
            recorder: MockRecorder {
                log: log.clone(),
                fs: fs.clone(),
                fail_start: Mutex::default(),
                fail_stop: Mutex::default(),
                requests: Mutex::default(),
            },
            editor: MockEditor { log: log.clone(), mode: Mutex::new(EditorMode::Edit) },
            uploaders: MockUploaders::new(log.clone()),
            shortener: MockShortener {
                log: log.clone(),
                fail: Mutex::default(),
                empty: Mutex::default(),
                calls: AtomicUsize::new(0),
            },
            clipboard: MockClipboard {
                log: log.clone(),
                image: Mutex::default(),
                text: Mutex::default(),
                files: Mutex::default(),
                content: Mutex::default(),
                fail: Mutex::default(),
            },
            notifier: MockNotifier {
                log: log.clone(),
                shown: Mutex::default(),
                qr: Mutex::default(),
                fail: Mutex::default(),
            },
            opener: MockOpener { log: log.clone(), fail: Mutex::default() },
            commands: MockCommands {
                log: log.clone(),
                runs: Mutex::default(),
                exit_code: Mutex::default(),
                fail: Mutex::default(),
            },
            zipper: MockZipper { log: log.clone(), fs: fs.clone(), fail: Mutex::default() },
            save_dialog: MockSaveDialog { log: log.clone(), choice: Mutex::default() },
            pinner: MockPinner { log: log.clone(), fail: Mutex::default() },
            ocr: MockOcr { log: log.clone(), text: Mutex::new("recognised text".to_owned()) },
            history: History::open_in_memory().expect("in-memory history"),
            qr: QrCodeRenderer,
            fs,
            log,
        }
    }

    /// The services bundle (history included).
    pub fn services(&self) -> Services<'_> {
        Services {
            capturer: &self.capturer,
            recorder: &self.recorder,
            editor: &self.editor,
            uploaders: &self.uploaders,
            shortener: &self.shortener,
            clipboard: &self.clipboard,
            notifier: &self.notifier,
            opener: &self.opener,
            qr: &self.qr,
            commands: &self.commands,
            fs: &self.fs,
            zipper: &self.zipper,
            save_dialog: &self.save_dialog,
            pinner: &self.pinner,
            ocr: &self.ocr,
            history: Some(&self.history),
        }
    }

    /// The same bundle without history.
    pub fn services_without_history(&self) -> Services<'_> {
        Services { history: None, ..self.services() }
    }

    /// Settings that make everything resolvable: save dir `/shots`, every destination
    /// (image, text, file, video, URL shortener) set to `test` / `short`, notifications on.
    pub fn settings(&self) -> Settings {
        let mut s = Settings::default();
        s.general.save_dir = Some(PathBuf::from("/shots"));
        s.destinations.image = Some("test".into());
        s.destinations.text = Some("test".into());
        s.destinations.file = Some("test".into());
        s.destinations.video = Some("test".into());
        s.destinations.url_shortener = Some("short".into());
        s
    }

    /// An engine with a fixed clock (2024-03-09 14:05:06 UTC), seeded RNG and in-memory
    /// counter, so names are deterministic.
    pub fn engine(&self, settings: Settings) -> Engine {
        Engine::new(settings, Self::naming())
    }

    /// The deterministic naming sources.
    pub fn naming() -> Naming {
        Naming {
            clock: Arc::new(FixedClock::from_rfc3339("2024-03-09T14:05:06+00:00").expect("valid")),
            rng: Arc::new(SeededRng::new(7)),
            env: Arc::new(StaticEnv {
                user: "tester".into(),
                domain: "DOMAIN".into(),
                machine: "HOST".into(),
                files: BTreeMap::new(),
            }),
            counter: Arc::new(MemoryCounter::default()),
        }
    }
}
