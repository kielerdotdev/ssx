//! Everything the window needs from the machine, behind small traits.
//!
//! A page never touches the operating system directly. It asks the [`Host`]: open a URL,
//! pick a folder, read the credential store, enable autostart, list what the file managers
//! have installed, open the history database, run the diagnostics. [`Host::system`] wires the
//! real implementations; [`Host::sandboxed`] wires fakes and temp folders, which is what the
//! tests and the screenshot harness use, so a test run never opens a browser, a dialog or
//! the developer's real desktop configuration.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use ssx_cli::commands::doctor::Report as DoctorReport;
use ssx_core::{
    history::History,
    pattern::{Clock, Env, StaticEnv, SystemClock, SystemEnv},
    settings::Paths,
};
use ssx_hotkeys::{
    Environment, Platform,
    bindings::{CommandRunner, Dirs, RunOutput, SystemRunner},
};
use ssx_shell::{Context as ShellContext, Integrations, MemoryRegistry, Platform as ShellPlatform};

use crate::{
    autostart::{Autostart, AutostartCommand, FakeAutostart},
    secrets::{MemoryVault, SecretVault, SystemVault},
    uploader_registry::{FakeTester, RealTester, UploadTester},
};

// ---- opening things -----------------------------------------------------------------------

/// Opens links and files with the default application.
pub trait Opener: Send + Sync + fmt::Debug {
    /// Opens a web link (only `http` and `https`).
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// Opens a file or folder.
    fn open_path(&self, path: &Path) -> Result<(), String>;
}

/// The real opener (`opener` crate through `ssx-services`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemOpen;

impl Opener for SystemOpen {
    fn open_url(&self, url: &str) -> Result<(), String> {
        use ssx_core::workflow::UrlOpener;
        ssx_services::SystemOpener::default().open(url).map_err(|e| e.to_string())
    }

    fn open_path(&self, path: &Path) -> Result<(), String> {
        use ssx_services::desktop::{Launcher, OpenerLauncher};
        OpenerLauncher.open_file(path)
    }
}

/// Records what it was asked to open.
#[derive(Debug, Default)]
pub struct RecordingOpener {
    /// URLs, in order.
    pub urls: Mutex<Vec<String>>,
    /// Paths, in order.
    pub paths: Mutex<Vec<PathBuf>>,
}

impl Opener for RecordingOpener {
    fn open_url(&self, url: &str) -> Result<(), String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("refusing to open {url:?}: only http and https links are opened"));
        }
        self.urls.lock().unwrap_or_else(PoisonError::into_inner).push(url.to_owned());
        Ok(())
    }

    fn open_path(&self, path: &Path) -> Result<(), String> {
        self.paths.lock().unwrap_or_else(PoisonError::into_inner).push(path.to_path_buf());
        Ok(())
    }
}

// ---- file dialogs -------------------------------------------------------------------------

/// Native file dialogs. Called from worker threads (they block).
pub trait Dialogs: Send + Sync + fmt::Debug {
    /// Asks for a folder.
    fn pick_folder(&self, start: Option<&Path>) -> Option<PathBuf>;
    /// Asks for a file with one of `extensions` (without dots).
    fn pick_file(&self, title: &str, extensions: &[&str]) -> Option<PathBuf>;
}

/// `rfd`: the XDG portal on Linux, the native dialogs elsewhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDialogs;

impl Dialogs for SystemDialogs {
    fn pick_folder(&self, start: Option<&Path>) -> Option<PathBuf> {
        let mut d = rfd::FileDialog::new().set_title("Choose a folder");
        if let Some(s) = start.filter(|s| s.is_dir()) {
            d = d.set_directory(s);
        }
        d.pick_folder()
    }

    fn pick_file(&self, title: &str, extensions: &[&str]) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).add_filter("Files", extensions).pick_file()
    }
}

/// Answers dialogs from a script.
#[derive(Debug, Default)]
pub struct ScriptedDialogs {
    /// The next folder to "pick".
    pub folder: Mutex<Option<PathBuf>>,
    /// The next file to "pick".
    pub file: Mutex<Option<PathBuf>>,
}

impl Dialogs for ScriptedDialogs {
    fn pick_folder(&self, _: Option<&Path>) -> Option<PathBuf> {
        self.folder.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
    fn pick_file(&self, _: &str, _: &[&str]) -> Option<PathBuf> {
        self.file.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

// ---- diagnostics --------------------------------------------------------------------------

/// Produces the `ssx doctor` report.
pub trait DoctorSource: Send + Sync + fmt::Debug {
    /// Probes the machine (can take a few seconds; call from a worker).
    fn gather(&self, paths: &Paths) -> Result<DoctorReport, String>;
}

/// The real diagnostics, straight from the CLI library.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDoctor;

impl DoctorSource for SystemDoctor {
    fn gather(&self, paths: &Paths) -> Result<DoctorReport, String> {
        use ssx_cli::{
            app::App,
            cli::GlobalArgs,
            output::{ColorChoice, Style},
        };
        let app = App {
            global: GlobalArgs {
                verbose: 0,
                quiet: true,
                config_dir: Some(paths.config_dir.clone()),
                backend: None,
                color: ColorChoice::Never,
            },
            paths: paths.clone(),
            cancel: ssx_core::workflow::CancelToken::new(),
            first_interrupt: Default::default(),
            out: Style::plain(),
            err: Style::plain(),
        };
        ssx_cli::commands::doctor::gather(&app).map_err(|e| e.message)
    }
}

/// Returns a prepared report.
#[derive(Debug)]
pub struct FixedDoctor(pub Result<DoctorReport, String>);

impl DoctorSource for FixedDoctor {
    fn gather(&self, _: &Paths) -> Result<DoctorReport, String> {
        self.0.clone().map_err(|e| e.clone())
    }
}

// ---- history database ---------------------------------------------------------------------

/// Opens the history database.
pub trait HistorySource: Send + Sync + fmt::Debug {
    /// Opens (creating it if needed).
    fn open(&self) -> Result<Arc<History>, String>;
}

/// The database in the data directory. A damaged file is reported, never deleted or moved.
#[derive(Debug, Clone)]
pub struct SystemHistory(pub Paths);

impl HistorySource for SystemHistory {
    fn open(&self) -> Result<Arc<History>, String> {
        std::fs::create_dir_all(&self.0.data_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.0.data_dir.display()))?;
        History::open(&self.0.history_db()).map(Arc::new).map_err(|e| e.to_string())
    }
}

/// Hands out one shared handle (tests, screenshots: often an in-memory database).
pub struct SharedHistory(pub Arc<History>);

impl fmt::Debug for SharedHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedHistory")
    }
}

impl HistorySource for SharedHistory {
    fn open(&self) -> Result<Arc<History>, String> {
        Ok(self.0.clone())
    }
}

/// Always fails with a message.
#[derive(Debug)]
pub struct BrokenHistory(pub String);

impl HistorySource for BrokenHistory {
    fn open(&self) -> Result<Arc<History>, String> {
        Err(self.0.clone())
    }
}

// ---- file-manager integration -------------------------------------------------------------

/// The file-manager installers and the context they run in.
#[derive(Debug, Clone)]
pub struct ShellHost {
    /// The installers for this platform.
    pub integrations: Integrations,
    /// Their context, or why it could not be built (no home directory, bad executable path).
    pub context: Result<ShellContext, String>,
}

impl ShellHost {
    /// The real ones for `ssx_exe`.
    pub fn system(ssx_exe: &Path) -> Self {
        Self {
            integrations: Integrations::native(),
            context: ShellContext::from_env(ssx_exe).map_err(|e| e.to_string()),
        }
    }

    /// Installers whose every path lives below `root`, and whose helper programs are only
    /// recorded.
    pub fn sandboxed(root: &Path, ssx_exe: &Path) -> Self {
        Self {
            integrations: Integrations::for_platform_with_registry(
                ShellPlatform::current(),
                Some(Arc::new(MemoryRegistry::new())),
            ),
            context: Ok(ShellContext::sandboxed(root, ssx_exe)),
        }
    }
}

// ---- the host -----------------------------------------------------------------------------

/// Records the commands the hotkey generators would run, answering like an empty system.
#[derive(Debug, Default)]
pub struct RecordingRunner {
    /// The commands run, one string each.
    pub calls: Mutex<Vec<String>>,
}

impl CommandRunner for RecordingRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<RunOutput> {
        let line = format!("{program} {}", args.join(" "));
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(line.clone());
        Ok(if line.starts_with("gsettings get") && line.ends_with("custom-keybindings") {
            RunOutput::ok("@as []")
        } else if line.starts_with("gsettings get") {
            RunOutput::failed("no such key")
        } else {
            RunOutput::ok("")
        })
    }
}

/// The machine, as far as the window is concerned.
#[derive(Clone)]
pub struct Host {
    /// Where settings and data live.
    pub paths: Paths,
    /// "Now" for previews.
    pub clock: Arc<dyn Clock>,
    /// User / machine names for pattern previews.
    pub env: Arc<dyn Env>,
    /// Autostart at login.
    pub autostart: Arc<dyn Autostart>,
    /// What autostart would launch.
    pub autostart_command: AutostartCommand,
    /// Opening links and files.
    pub opener: Arc<dyn Opener>,
    /// File dialogs.
    pub dialogs: Arc<dyn Dialogs>,
    /// Uploader secrets.
    pub vault: Arc<dyn SecretVault>,
    /// Test uploads and re-uploads.
    pub uploads: Arc<dyn UploadTester>,
    /// The session's environment variables that decide how hotkeys are delivered.
    pub hotkey_env: Environment,
    /// The operating system family for hotkey detection.
    pub hotkey_platform: Platform,
    /// Where compositor configs live (`None`: no home directory).
    pub hotkey_dirs: Option<Dirs>,
    /// Runs `gsettings` / `kwriteconfig`.
    pub hotkey_runner: Arc<dyn RunnerBox>,
    /// The `ssx` program that generated bindings run.
    pub ssx_exe: PathBuf,
    /// File-manager entries.
    pub shell: ShellHost,
    /// Diagnostics.
    pub doctor: Arc<dyn DoctorSource>,
    /// The history database.
    pub history: Arc<dyn HistorySource>,
}

/// `CommandRunner` that can be shared between threads.
pub trait RunnerBox: CommandRunner + Send + Sync + fmt::Debug {}
impl<T: CommandRunner + Send + Sync + fmt::Debug> RunnerBox for T {}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("paths", &self.paths).finish_non_exhaustive()
    }
}

/// The `ssx` command-line program: next to this executable, else the first one on `PATH`,
/// else where it would be next to this executable (an absolute path, so the Integration page
/// can say "not found" instead of refusing to work), else the bare name.
pub fn discover_ssx_exe() -> PathBuf {
    let name = format!("ssx{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.parent().map(|d| d.join(&name)));
    sibling
        .clone()
        .filter(|p| p.is_file())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join(&name))
                    .find(|c| c.is_absolute() && c.is_file())
            })
        })
        .or(sibling)
        .unwrap_or_else(|| PathBuf::from(name))
}

impl Host {
    /// The real machine.
    pub fn system(paths: Paths) -> Self {
        let exe = discover_ssx_exe();
        let cmd = AutostartCommand::discover();
        Self {
            history: Arc::new(SystemHistory(paths.clone())),
            paths,
            clock: Arc::new(SystemClock),
            env: Arc::new(SystemEnv),
            autostart: crate::autostart::system(&cmd),
            autostart_command: cmd,
            opener: Arc::new(SystemOpen),
            dialogs: Arc::new(SystemDialogs),
            vault: Arc::new(SystemVault::open()),
            uploads: Arc::new(RealTester),
            hotkey_env: Environment::from_env(),
            hotkey_platform: Platform::current(),
            hotkey_dirs: Dirs::from_env(),
            hotkey_runner: Arc::new(SystemRunnerBox),
            shell: ShellHost::system(&exe),
            ssx_exe: exe,
            doctor: Arc::new(SystemDoctor),
        }
    }

    /// Fakes and temp folders below `root`; the clock and the environment are fixed so that
    /// what is rendered does not depend on when or where the test runs.
    pub fn sandboxed(root: &Path) -> Self {
        let paths = Paths::rooted_at(root.join("config"));
        let exe = PathBuf::from("/usr/local/bin/ssx");
        let cmd = AutostartCommand {
            program: PathBuf::from("/usr/local/bin/ssx-tray"),
            args: vec!["--background".to_owned()],
        };
        Self {
            history: Arc::new(SystemHistory(paths.clone())),
            paths,
            clock: Arc::new(
                ssx_core::pattern::FixedClock::from_rfc3339("2025-03-09T14:05:06.789+01:00")
                    .unwrap_or(ssx_core::pattern::FixedClock(chrono::Local::now().fixed_offset())),
            ),
            env: Arc::new(StaticEnv {
                user: "marius".into(),
                domain: "WORKGROUP".into(),
                machine: "laptop".into(),
                files: std::collections::BTreeMap::default(),
            }),
            autostart: Arc::new(FakeAutostart::new(false)),
            autostart_command: cmd,
            opener: Arc::new(RecordingOpener::default()),
            dialogs: Arc::new(ScriptedDialogs::default()),
            vault: Arc::new(MemoryVault::keyring()),
            uploads: Arc::new(FakeTester::default()),
            hotkey_env: Environment::from_pairs([
                ("XDG_CURRENT_DESKTOP", "sway"),
                ("XDG_SESSION_TYPE", "wayland"),
                ("SWAYSOCK", "/run/user/1000/sway-ipc.sock"),
                ("WAYLAND_DISPLAY", "wayland-1"),
            ]),
            hotkey_platform: Platform::Linux,
            hotkey_dirs: Some(Dirs::under(root.join("home"))),
            hotkey_runner: Arc::new(RecordingRunner::default()),
            shell: ShellHost::sandboxed(root, &exe),
            ssx_exe: exe,
            doctor: Arc::new(FixedDoctor(Err("diagnostics were not prepared".to_owned()))),
        }
    }

    /// The hotkey delivery strategy of this host's session.
    pub fn hotkey_detection(&self) -> ssx_hotkeys::Detection {
        ssx_hotkeys::detect(&self.hotkey_env, self.hotkey_platform)
    }
}

/// Runs real commands for the hotkey generators.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunnerBox;

impl CommandRunner for SystemRunnerBox {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<RunOutput> {
        SystemRunner.run(program, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxed_host_touches_only_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let h = Host::sandboxed(dir.path());
        assert!(h.paths.config_dir.starts_with(dir.path()));
        assert!(h.hotkey_dirs.as_ref().unwrap().config_home.starts_with(dir.path()));
        assert!(h.opener.open_url("https://example.com").is_ok());
        assert!(h.opener.open_url("javascript:alert(1)").is_err());
        assert!(h.dialogs.pick_folder(None).is_none());
        assert_eq!(h.hotkey_detection().candidates.len(), 1);
        assert!(h.shell.context.is_ok());
    }

    #[test]
    fn the_recording_runner_answers_like_an_empty_gnome() {
        let r = RecordingRunner::default();
        let out =
            r.run("gsettings", &["get".into(), "x".into(), "custom-keybindings".into()]).unwrap();
        assert_eq!(out.stdout, "@as []");
        assert!(!r.run("gsettings", &["get".into(), "x".into(), "name".into()]).unwrap().success);
        assert_eq!(r.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn scripted_dialogs_answer_once() {
        let d = ScriptedDialogs::default();
        *d.folder.lock().unwrap() = Some(PathBuf::from("/x"));
        assert_eq!(d.pick_folder(None), Some(PathBuf::from("/x")));
        assert_eq!(d.pick_folder(None), None);
    }

    #[test]
    fn the_ssx_program_lookup_never_panics() {
        let p = discover_ssx_exe();
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("ssx"));
    }

    #[test]
    fn shared_history_hands_out_the_same_database() {
        let h = Arc::new(History::open_in_memory().unwrap());
        let s = SharedHistory(h.clone());
        assert!(Arc::ptr_eq(&s.open().unwrap(), &h));
        assert_eq!(BrokenHistory("nope".into()).open().unwrap_err(), "nope");
    }
}
