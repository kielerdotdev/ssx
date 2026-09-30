//! End-to-end test harness: spawns the real `ssx` binary in a hermetic environment.
//!
//! * Every test gets its own temp directory that holds the config dir, data dir and `HOME`, so
//!   nothing touches the developer's real settings, keyring or clipboard managers.
//! * `Xvfb` (and `sway`) are started per test; when the program is missing the test prints a
//!   SKIP line and returns, and CI installs them.
//! * The environment of the child is *replaced*, not inherited wholesale: display variables,
//!   `DBUS_SESSION_BUS_ADDRESS` and friends are removed so a developer's desktop session can
//!   never leak into a test (and the "no keyring, no notification daemon" paths are the ones
//!   exercised, exactly like in CI).

#![allow(dead_code)] // each test binary uses a different subset

pub mod mock;
pub mod x11;

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use ssx_types::Frame;
use tempfile::TempDir;

/// The binary under test.
pub fn ssx_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ssx")
}

/// `true` if `prog` is an executable in `PATH`.
pub fn have(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
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

    /// stdout lines.
    pub fn lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }

    /// stdout parsed as JSON.
    #[track_caller]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!("stdout is not JSON ({e}):\n{}\n--- stderr ---\n{}", self.stdout, self.stderr)
        })
    }
}

/// A hermetic environment for one test.
pub struct TestEnv {
    /// Holds everything below; removed on drop.
    pub dir: TempDir,
    /// `SSX_CONFIG_DIR`.
    pub cfg: PathBuf,
    /// `HOME` (and the XDG dirs below it).
    pub home: PathBuf,
    env: Vec<(String, String)>,
}

impl TestEnv {
    /// A fresh environment with no display.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("ssx-e2e").tempdir().expect("tempdir");
        let cfg = dir.path().join("cfg");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let mut e = Self { dir, cfg, home, env: Vec::new() };
        e.set("SSX_CONFIG_DIR", e.cfg.display().to_string());
        e.set("HOME", e.home.display().to_string());
        e.set("XDG_CONFIG_HOME", e.home.join(".config").display().to_string());
        e.set("XDG_DATA_HOME", e.home.join(".local/share").display().to_string());
        e.set("NO_COLOR", "1");
        e
    }

    /// Sets a variable for every command.
    pub fn set(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.env.retain(|(name, _)| name != k);
        self.env.push((k.to_owned(), v.into()));
        self
    }

    /// Points the child at an X server and forces the X11 backend.
    pub fn with_x11(mut self, display: &str) -> Self {
        self.set("DISPLAY", display);
        self.set("SSX_BACKEND", "x11");
        self
    }

    /// The base command: cleared environment plus this test's variables and a minimal `PATH`.
    pub fn command(&self) -> Command {
        let mut c = Command::new(ssx_bin());
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
        let mut c = self.command();
        c.args(args);
        Out::new(&c.output().expect("spawn ssx"))
    }

    /// Runs `ssx args...` with `input` on stdin.
    #[track_caller]
    pub fn ssx_with_stdin(&self, args: &[&str], input: &str) -> Out {
        use std::io::Write;
        let mut c = self.command();
        c.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().expect("spawn ssx");
        child.stdin.take().expect("stdin").write_all(input.as_bytes()).expect("write stdin");
        Out::new(&child.wait_with_output().expect("wait"))
    }

    /// Writes `settings.toml` (with the current `version`, like every real file has, unless the
    /// text sets one).
    pub fn write_settings(&self, text: &str) {
        std::fs::create_dir_all(&self.cfg).expect("cfg dir");
        let body = if text.contains("version =") {
            text.to_owned()
        } else {
            format!("version = 1\n{text}")
        };
        std::fs::write(self.cfg.join("settings.toml"), body).expect("write settings");
    }

    /// A path inside the test directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Decodes a PNG/JPEG file.
#[track_caller]
pub fn read_image(path: &Path) -> Frame {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    Frame::decode(&bytes).unwrap_or_else(|e| panic!("{} is not an image: {e}", path.display()))
}

/// RGB of pixel `(x, y)`.
pub fn rgb(f: &Frame, x: u32, y: u32) -> [u8; 3] {
    let row = f.row(y);
    let i = x as usize * 4;
    [row[i], row[i + 1], row[i + 2]]
}

/// The first pixel where `f` differs from `expected` (RGB triples in row-major order).
pub fn first_diff(f: &Frame, expected: &[[u8; 3]]) -> Option<String> {
    assert_eq!(
        expected.len(),
        (f.width() * f.height()) as usize,
        "size mismatch: {}x{}",
        f.width(),
        f.height()
    );
    for y in 0..f.height() {
        for x in 0..f.width() {
            let got = rgb(f, x, y);
            let want = expected[(y * f.width() + x) as usize];
            if got != want {
                return Some(format!("pixel ({x},{y}): got {got:?}, expected {want:?}"));
            }
        }
    }
    None
}

/// A `.sxcu` that POSTs multipart to `url`/upload and reads `{"link": ...}`.
pub fn sxcu_json(url: &str) -> String {
    format!(
        r#"{{"Version":"14.0.0","Name":"mock host","DestinationType":"ImageUploader, FileUploader",
"RequestMethod":"POST","RequestURL":"{url}/upload","Body":"MultipartFormData","FileFormName":"file",
"URL":"{{json:link}}","DeletionURL":"{{json:delete}}"}}"#
    )
}
