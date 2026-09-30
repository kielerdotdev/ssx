//! Everything an installer needs from its environment, injectable for tests.
//!
//! Installers never read `$HOME`, `PATH`, the registry or spawn processes directly; they go
//! through [`Context`]. Tests build a [`Context::sandboxed`] one rooted in a temp dir with a
//! [`RecordingRunner`], so a test run cannot touch the developer's real desktop.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::action::Action;
use crate::error::{Result, ShellError};

/// Operating system family an integration targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// Linux and other freedesktop systems.
    Linux,
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
}

impl Platform {
    /// The platform this binary was compiled for.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
        })
    }
}

/// Result of running an external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status was zero.
    pub success: bool,
    /// Captured standard output (lossy UTF-8).
    pub stdout: String,
}

/// Runs helper programs (`update-desktop-database`, `pbs -update`, ...). Injectable so tests
/// never execute anything.
pub trait CommandRunner: Send + Sync + fmt::Debug {
    /// Runs `program` with `args`, waiting for it to finish.
    fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// Runs real processes (stdin closed, output captured).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()?;
        Ok(CommandOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
}

/// Records calls instead of running anything; every call "succeeds".
#[derive(Debug, Default)]
pub struct RecordingRunner {
    calls: Mutex<Vec<Vec<String>>>,
}

impl RecordingRunner {
    /// The calls so far, each as `[program, args...]`.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        if let Ok(mut calls) = self.calls.lock() {
            let mut call = vec![program.to_owned()];
            call.extend(args.iter().cloned());
            calls.push(call);
        }
        Ok(CommandOutput { success: true, stdout: String::new() })
    }
}

/// The environment and configuration for installers.
#[derive(Debug, Clone)]
pub struct Context {
    /// Target OS family (normally [`Platform::current`]).
    pub platform: Platform,
    /// Absolute path of the `ssx` CLI the menu entries launch.
    pub ssx_exe: PathBuf,
    /// The user's home directory.
    pub home: PathBuf,
    /// `$XDG_DATA_HOME` (default `~/.local/share`).
    pub data_home: PathBuf,
    /// `$XDG_CONFIG_HOME` (default `~/.config`).
    pub config_home: PathBuf,
    /// Directories searched for executables during detection.
    pub path_dirs: Vec<PathBuf>,
    /// System library directories searched for `nautilus-python`.
    pub lib_dirs: Vec<PathBuf>,
    /// Lower-cased entries of `$XDG_CURRENT_DESKTOP`.
    pub desktops: Vec<String>,
    /// `%APPDATA%` (Windows), used for the Send To folder.
    pub appdata: Option<PathBuf>,
    /// The entries to install.
    pub actions: Vec<Action>,
    /// Helper-process runner.
    pub runner: Arc<dyn CommandRunner>,
}

impl Context {
    /// Builds a context from the process environment with the built-in actions.
    pub fn from_env(ssx_exe: impl Into<PathBuf>) -> Result<Self> {
        let platform = Platform::current();
        let env_path = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = env_path("HOME")
            .or_else(|| env_path("USERPROFILE"))
            .ok_or_else(|| ShellError::Unavailable("cannot determine the home directory".into()))?;
        let ctx = Self {
            platform,
            ssx_exe: ssx_exe.into(),
            data_home: env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            config_home: env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            home,
            path_dirs: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
            lib_dirs: [
                "/usr/lib",
                "/usr/lib64",
                "/usr/local/lib",
                "/usr/lib/x86_64-linux-gnu",
                "/usr/lib/aarch64-linux-gnu",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
            desktops: parse_desktops(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()),
            appdata: env_path("APPDATA"),
            actions: Action::defaults(),
            runner: Arc::new(SystemRunner),
        };
        ctx.validate()?;
        Ok(ctx)
    }

    /// A context whose every path lives under `root` and whose runner records instead of
    /// executing. Used by the tests and suitable for dry runs.
    pub fn sandboxed(root: &Path, ssx_exe: impl Into<PathBuf>) -> Self {
        let home = root.join("home");
        Self {
            platform: Platform::current(),
            ssx_exe: ssx_exe.into(),
            data_home: home.join(".local/share"),
            config_home: home.join(".config"),
            path_dirs: vec![root.join("bin")],
            lib_dirs: vec![root.join("usr/lib")],
            desktops: Vec::new(),
            appdata: Some(home.join("AppData/Roaming")),
            home,
            actions: Action::defaults(),
            runner: Arc::new(RecordingRunner::default()),
        }
    }

    /// Checks the executable path and every action.
    pub fn validate(&self) -> Result<()> {
        let s = self.ssx_exe.to_string_lossy();
        let bad = |reason: &str| ShellError::InvalidExe {
            path: self.ssx_exe.clone(),
            reason: reason.to_owned(),
        };
        if s.is_empty() || s.chars().any(char::is_control) {
            return Err(bad("empty or contains control characters"));
        }
        if self.ssx_exe.to_str().is_none() {
            return Err(bad("not valid UTF-8"));
        }
        if !is_absolute_for(self.platform, &s) {
            return Err(bad("must be an absolute path"));
        }
        let mut seen = std::collections::HashSet::new();
        for a in &self.actions {
            a.validate()?;
            if !seen.insert(a.id.as_str()) {
                return Err(ShellError::InvalidAction {
                    id: a.id.clone(),
                    reason: "duplicate action id".into(),
                });
            }
        }
        Ok(())
    }

    /// The executable path as UTF-8 (validated by [`validate`](Self::validate)).
    pub(crate) fn exe_str(&self) -> Result<&str> {
        self.ssx_exe.to_str().ok_or_else(|| ShellError::InvalidExe {
            path: self.ssx_exe.clone(),
            reason: "not valid UTF-8".into(),
        })
    }

    /// Whether `name` is an executable file in any of [`path_dirs`](Self::path_dirs).
    pub fn has_binary(&self, name: &str) -> bool {
        self.path_dirs.iter().any(|d| is_executable_file(&d.join(name)))
    }

    /// Whether `$XDG_CURRENT_DESKTOP` lists `name` (case-insensitive).
    pub fn desktop_is(&self, name: &str) -> bool {
        self.desktops.iter().any(|d| d.eq_ignore_ascii_case(name))
    }
}

/// Splits `$XDG_CURRENT_DESKTOP` (`GNOME:ubuntu`) into lower-case entries.
pub fn parse_desktops(value: &str) -> Vec<String> {
    value.split(':').filter(|s| !s.is_empty()).map(str::to_ascii_lowercase).collect()
}

fn is_absolute_for(platform: Platform, s: &str) -> bool {
    match platform {
        Platform::Windows => {
            let b = s.as_bytes();
            let drive = b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && matches!(b[2], b'\\' | b'/');
            drive || s.starts_with("\\\\")
        }
        _ => s.starts_with('/'),
    }
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktops_parse() {
        assert_eq!(parse_desktops("GNOME:ubuntu"), ["gnome", "ubuntu"]);
        assert_eq!(parse_desktops(""), Vec::<String>::new());
        assert_eq!(parse_desktops("::KDE:"), ["kde"]);
    }

    #[test]
    fn exe_validation() {
        let tmp = Path::new("/tmp/x");
        let mut c = Context::sandboxed(tmp, "/usr/bin/ssx");
        c.platform = Platform::Linux;
        assert!(c.validate().is_ok());
        c.ssx_exe = PathBuf::from("ssx");
        assert!(c.validate().is_err());
        c.ssx_exe = PathBuf::from("/usr/bin/ss\nx");
        assert!(c.validate().is_err());
        c.platform = Platform::Windows;
        c.ssx_exe = PathBuf::from(r"C:\Program Files\ssx\ssx.exe");
        assert!(c.validate().is_ok());
        c.ssx_exe = PathBuf::from(r"\\server\share\ssx.exe");
        assert!(c.validate().is_ok());
        c.ssx_exe = PathBuf::from("/usr/bin/ssx");
        assert!(c.validate().is_err());
    }

    #[test]
    fn duplicate_action_ids_rejected() {
        let mut c = Context::sandboxed(Path::new("/tmp/x"), "/usr/bin/ssx");
        c.platform = Platform::Linux;
        c.actions.push(Action::upload());
        assert!(c.validate().is_err());
    }
}
