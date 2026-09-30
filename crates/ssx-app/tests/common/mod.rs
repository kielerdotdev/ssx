//! End-to-end harness for the daemon: spawns the real `ssx-app`, the real `ssx` CLI and the
//! real `ssx-overlay` helper in a hermetic environment.
//!
//! * Every test gets its own temp directory with the config dir, data dir, `HOME` and an
//!   `XDG_RUNTIME_DIR` (so the instance socket is private), and an environment that is
//!   *replaced*, not inherited: no developer display, D-Bus session or keyring leaks in.
//! * `Xvfb`, `sway`, `dbus-daemon`, `xdotool`, `xclip`, `ffprobe` are used where the test
//!   needs them; when one is missing the test prints a SKIP line and returns, and CI installs
//!   them.
//! * Every spawned process is killed on drop, so a failing test leaves nothing behind.
//! * The other binaries (`ssx`, `ssx-overlay`) belong to other packages, so cargo does not
//!   hand out `CARGO_BIN_EXE_*` for them: they are found next to `ssx-app` in the target
//!   directory and built on demand (once per test binary).

#![allow(dead_code)] // each test binary uses a different subset
#![cfg(unix)]

#[path = "../../../ssx-cli/tests/common/x11.rs"]
pub mod x11;

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Mutex, Once},
    time::{Duration, Instant},
};

use ssx_core::ipc::{
    ErrorCode, Request, RequestEnvelope, Response, ResponseEnvelope, decode_line, encode_line,
};
use ssx_ipc::{Client, ClientConfig, Location};
use tempfile::TempDir;

#[allow(unused_imports)]
pub use x11::{Painter, Xvfb, expected_scene, scene_server_with_ids};

/// `xdotool` against a test display.
pub trait Xdo {
    /// Runs `xdotool args...` and reports whether it succeeded.
    fn xdotool(&self, args: &[&str]) -> bool;
    /// One key chord, e.g. `ctrl+alt+F9`.
    fn xdotool_key(&self, chord: &str) -> bool {
        self.xdotool(&["key", chord])
    }
}

impl Xdo for Xvfb {
    fn xdotool(&self, args: &[&str]) -> bool {
        Command::new("xdotool")
            .env("DISPLAY", &self.display)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// `true` if `prog` is an executable in `PATH`.
pub fn have(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// Prints a SKIP line and returns `true` when any of `progs` is missing.
pub fn skip_unless(progs: &[&str]) -> bool {
    for p in progs {
        if !have(p) {
            eprintln!("SKIP: `{p}` is not installed (CI installs it with apt)");
            return true;
        }
    }
    false
}

/// The daemon under test.
pub fn app_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ssx-app"))
}

static BUILD: Once = Once::new();
static BUILD_LOCK: Mutex<()> = Mutex::new(());

/// Path of a binary of another package, built on demand.
pub fn sibling_bin(package: &str, name: &str) -> PathBuf {
    let dir = app_bin().parent().expect("target dir").to_path_buf();
    let path = dir.join(name);
    if path.is_file() {
        return path;
    }
    let _guard = BUILD_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !path.is_file() {
        BUILD.call_once(|| {});
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "-p", package, "--bin", name])
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p {package} failed");
    }
    assert!(path.is_file(), "{} was not produced", path.display());
    path
}

/// The `ssx` CLI.
pub fn ssx_bin() -> PathBuf {
    sibling_bin("ssx-cli", "ssx")
}

/// The overlay helper.
pub fn overlay_bin() -> PathBuf {
    sibling_bin("ssx-overlay", "ssx-overlay")
}

/// A finished command.
#[derive(Debug)]
pub struct Out {
    /// Exit code (`-1` if killed by a signal).
    pub code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

impl Out {
    fn new(o: &Output) -> Self {
        Self {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// Standard output as JSON.
    #[track_caller]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.stdout, self.stderr
            )
        })
    }

    /// Panics with both streams unless the command exited with 0.
    #[track_caller]
    pub fn ok(self) -> Self {
        assert_eq!(
            self.code, 0,
            "expected success\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }

    /// Panics unless the command exited with `code`.
    #[track_caller]
    pub fn code(self, code: i32) -> Self {
        assert_eq!(
            self.code, code,
            "expected exit code {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }
}

/// A hermetic environment for one test.
pub struct TestEnv {
    /// Holds everything below; removed on drop.
    pub dir: TempDir,
    /// `SSX_CONFIG_DIR`.
    pub cfg: PathBuf,
    /// `HOME`.
    pub home: PathBuf,
    /// `XDG_RUNTIME_DIR`.
    pub run: PathBuf,
    env: Vec<(String, String)>,
}

impl TestEnv {
    /// A fresh environment with no display.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("ssx-app").tempdir().expect("tempdir");
        let cfg = dir.path().join("cfg");
        let home = dir.path().join("home");
        let run = dir.path().join("run");
        for d in [&home, &run] {
            std::fs::create_dir_all(d).expect("dir");
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
        let mut e = Self { dir, cfg, home, run, env: Vec::new() };
        e.set("SSX_CONFIG_DIR", e.cfg.display().to_string());
        e.set("HOME", e.home.display().to_string());
        e.set("XDG_RUNTIME_DIR", e.run.display().to_string());
        e.set("XDG_CONFIG_HOME", e.home.join(".config").display().to_string());
        e.set("XDG_DATA_HOME", e.home.join(".local/share").display().to_string());
        // Hermetic: helpers only where a test asks for them.
        e.set("SSX_EDITOR_UI", "none");
        e.set("SSX_OVERLAY", "none");
        e.set("SSX_SETTINGS_UI", "none");
        e.set("NO_COLOR", "1");
        e
    }

    /// Sets a variable for every command.
    pub fn set(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.env.retain(|(name, _)| name != k);
        self.env.push((k.to_owned(), v.into()));
        self
    }

    /// Removes a variable.
    pub fn unset(&mut self, k: &str) -> &mut Self {
        self.env.retain(|(name, _)| name != k);
        self
    }

    /// Points everything at an X server and forces the X11 backend.
    pub fn with_x11(mut self, display: &str) -> Self {
        self.set("DISPLAY", display);
        self.set("SSX_BACKEND", "x11");
        self
    }

    /// Uses `dir` as `XDG_RUNTIME_DIR` (a compositor's socket lives there).
    pub fn with_run_dir(mut self, dir: &Path) -> Self {
        self.run = dir.to_path_buf();
        self.set("XDG_RUNTIME_DIR", dir.display().to_string());
        self
    }

    /// The data directory (`<cfg>/data`).
    pub fn data(&self) -> PathBuf {
        self.cfg.join("data")
    }

    /// A path inside the test directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A command with the cleared environment plus this test's variables.
    pub fn command(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        c.env_clear();
        for var in ["PATH", "LANG", "LC_ALL", "TMPDIR"] {
            if let Some(v) = std::env::var_os(var) {
                c.env(var, v);
            }
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c.stdin(Stdio::null());
        c
    }

    /// Runs `ssx args...`.
    #[track_caller]
    pub fn ssx(&self, args: &[&str]) -> Out {
        let mut c = self.command(&ssx_bin());
        c.args(args);
        Out::new(&c.output().expect("spawn ssx"))
    }

    /// Spawns `ssx args...` without waiting.
    pub fn spawn_ssx(&self, args: &[&str]) -> Child {
        let mut c = self.command(&ssx_bin());
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        c.spawn().expect("spawn ssx")
    }

    /// Writes `settings.toml` (with the current `version`).
    pub fn write_settings(&self, text: &str) {
        std::fs::create_dir_all(&self.cfg).expect("cfg dir");
        let body = if text.contains("version =") {
            text.to_owned()
        } else {
            format!("version = 1\n{text}")
        };
        std::fs::write(self.cfg.join("settings.toml"), body).expect("write settings");
    }

    /// Writes an uploader `.sxcu` that posts to `base_url`/upload and reads `{"link": ...}`.
    pub fn write_uploader(&self, base_url: &str) {
        let dir = self.cfg.join("uploaders");
        std::fs::create_dir_all(&dir).expect("uploaders dir");
        std::fs::write(dir.join("mock.sxcu"), x11_sxcu(base_url)).expect("write sxcu");
    }

    /// The instance socket location (the same one the daemon and the CLI use).
    pub fn ipc_location(&self) -> Location {
        Location::in_dir(self.run.join("ssx"))
    }

    /// A client for the daemon.
    pub fn client(&self) -> Client {
        Client::at(&self.ipc_location(), "ssx").expect("client").with_config(ClientConfig {
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(120),
            ..ClientConfig::default()
        })
    }

    /// Sends one request to the daemon.
    #[track_caller]
    pub fn request(&self, r: &Request) -> Response {
        request_with(&self.client(), r)
    }

    /// Reads the log files the daemon wrote (all of them, concatenated).
    pub fn daemon_log(&self) -> String {
        let dir = self.data().join("logs");
        let mut out = String::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut files: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
            files.sort();
            for f in files {
                out.push_str(&std::fs::read_to_string(f).unwrap_or_default());
            }
        }
        out
    }
}

/// A `.sxcu` that POSTs multipart to `url`/upload and reads `{"link": ...}`.
fn x11_sxcu(url: &str) -> String {
    format!(
        r#"{{"Version":"14.0.0","Name":"mock host","DestinationType":"ImageUploader, FileUploader",
"RequestMethod":"POST","RequestURL":"{url}/upload","Body":"MultipartFormData","FileFormName":"file",
"URL":"{{json:link}}","DeletionURL":"{{json:delete}}"}}"#
    )
}

/// Sends `r` with `client`.
#[track_caller]
pub fn request_with(client: &Client, r: &Request) -> Response {
    let line = encode_line(&RequestEnvelope::new(1, r.clone())).expect("encode");
    let reply =
        client.request(line.trim_end()).unwrap_or_else(|e| panic!("request {r:?} failed: {e}"));
    decode_line::<ResponseEnvelope>(&reply).expect("decode reply").response
}

/// The daemon process.
pub struct Daemon {
    child: Child,
    pub env_dir: PathBuf,
}

impl Daemon {
    /// Starts `ssx-app` with `args` and waits until it answers a ping. `None` (with a printed
    /// reason) if it did not come up.
    #[track_caller]
    pub fn start(env: &TestEnv, args: &[&str]) -> Daemon {
        let mut c = env.command(&app_bin());
        c.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = c.spawn().expect("spawn ssx-app");
        let client = env.client();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut err);
                }
                panic!(
                    "ssx-app exited during start ({status})\n--- stderr ---\n{err}\n--- log ---\n{}",
                    env.daemon_log()
                );
            }
            if let Ok(reply) = client.request(
                encode_line(&RequestEnvelope::new(1, Request::Ping)).expect("ping").trim_end(),
            ) && let Ok(ResponseEnvelope { response: Response::Pong { .. }, .. }) =
                decode_line::<ResponseEnvelope>(&reply)
            {
                return Daemon { child, env_dir: env.dir.path().to_path_buf() };
            }
            assert!(Instant::now() < deadline, "ssx-app never answered\n{}", env.daemon_log());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends SIGTERM.
    pub fn sigterm(&self) {
        let _ = Command::new("kill").args(["-TERM", &self.child.id().to_string()]).status();
    }

    /// Waits for the process to exit; `None` on timeout.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return Some(s);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `true` while the process runs.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// CPU time used so far (user + system) from `/proc`, in clock ticks, and the resident
    /// set size in KiB.
    pub fn proc_usage(&self) -> Option<(u64, u64)> {
        proc_usage(self.child.id())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `(utime + stime in clock ticks, VmRSS in KiB)` of process `pid`.
pub fn proc_usage(pid: u32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses: fields start after the last ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<u64>().ok())?;
    Some((ticks, rss))
}

/// Waits until `f` returns `Some`, up to `timeout`.
pub fn wait_until<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Unwraps a `Finished` response.
#[track_caller]
pub fn finished(r: Response) -> ssx_core::ipc::RunSummary {
    match r {
        Response::Finished(s) => s,
        other => panic!("expected Finished, got {other:?}"),
    }
}

/// Asserts an error response with `code`.
#[track_caller]
pub fn expect_error(r: &Response, code: ErrorCode) -> String {
    match r {
        Response::Error { code: c, message } if *c == code => message.clone(),
        other => panic!("expected error {code:?}, got {other:?}"),
    }
}

// ---- the mock upload server -----------------------------------------------------------------

use wiremock::{
    Mock, MockServer, Request as HttpRequest, Respond, ResponseTemplate, matchers::method,
};

/// A local HTTP server (wiremock on its own runtime) that answers every upload with a unique
/// `{"link": "http://mock.test/N"}` and remembers the file names it received.
pub struct UploadMock {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

struct Numbered(std::sync::atomic::AtomicUsize);

impl Respond for Numbered {
    fn respond(&self, _: &HttpRequest) -> ResponseTemplate {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        ResponseTemplate::new(200).set_body_string(format!(
            "{{\"link\":\"http://mock.test/u{n}\",\"delete\":\"http://mock.test/d{n}\"}}"
        ))
    }
}

impl UploadMock {
    /// Starts a server on a free localhost port.
    pub fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        rt.block_on(
            Mock::given(method("POST"))
                .respond_with(Numbered(std::sync::atomic::AtomicUsize::new(0)))
                .mount(&server),
        );
        Self { rt, server }
    }

    /// `http://127.0.0.1:PORT`.
    pub fn url(&self) -> String {
        self.server.uri()
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.rt.block_on(self.server.received_requests()).unwrap_or_default()
    }
}

/// A settings file with the mock uploader as the default destination for everything.
pub fn settings_with_mock_uploader(extra: &str) -> String {
    format!(
        "[destinations]\nimage = \"mock\"\nfile = \"mock\"\nvideo = \"mock\"\ntext = \"mock\"\n\n{extra}\n"
    )
}

// ---- the standard Xvfb fixture -------------------------------------------------------

/// Everything a test needs.
pub struct Fixture {
    pub x: Xvfb,
    pub _painter: Painter,
    pub _ids: Vec<u32>,
    pub env: TestEnv,
    pub mock: UploadMock,
    pub save: PathBuf,
}

pub const WORKFLOWS: &str = r#"
[[workflows]]
id = "shot-upload"
name = "Shot and upload"
input = "capture_fullscreen"
after_capture = ["save_to_file", "upload"]
after_upload = ["copy_url"]
[workflows.trigger]
cli_name = "shot"

[[workflows]]
id = "shot-local"
name = "Shot only"
input = "capture_fullscreen"
after_capture = ["save_to_file"]
[workflows.trigger]
cli_name = "local"

[[workflows]]
id = "region-save"
name = "Region"
input = "capture_region"
after_capture = ["save_to_file"]
[workflows.trigger]
cli_name = "region"

[[workflows]]
id = "upload-files"
name = "Upload files"
input = "files"
after_capture = ["upload"]
after_upload = ["copy_url"]
[workflows.trigger]
cli_name = "upload"

[[workflows]]
id = "rec"
name = "Record"
input = "record_screen"
after_capture = []
[workflows.trigger]
cli_name = "rec"
"#;

pub fn fixture_sized(screen: &str, workflows: &str) -> Option<Fixture> {
    if skip_unless(&["Xvfb"]) {
        return None;
    }
    let x = Xvfb::start(screen)?;
    let painter = Painter::new(&x.display);
    let ids: Vec<u32> = x11::SCENE
        .iter()
        .map(|&(sx, sy, w, h, c)| {
            painter.window(
                i16::try_from(sx).unwrap(),
                i16::try_from(sy).unwrap(),
                u16::try_from(w).unwrap(),
                u16::try_from(h).unwrap(),
                c,
            )
        })
        .collect();
    painter.publish_windows(&ids, *ids.last().unwrap());
    let env = TestEnv::new().with_x11(&x.display);
    let mock = UploadMock::start();
    env.write_uploader(&mock.url());
    let save = env.path("shots");
    env.write_settings(&settings_with_mock_uploader(&format!(
        "[general]\nsave_dir = \"{}\"\nuse_type_subfolders = false\nfolder_pattern = \"\"\n\
         file_name_pattern = \"shot_%i\"\nshow_notifications = false\n\n{workflows}",
        save.display()
    )));
    Some(Fixture { x, _painter: painter, _ids: ids, env, mock, save })
}

pub fn fixture() -> Option<Fixture> {
    fixture_sized("800x600x24", WORKFLOWS)
}
