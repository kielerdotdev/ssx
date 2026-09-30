//! Process-wide context shared by all commands, and the bundle needed to run workflows.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use ssx_core::{
    history::History,
    pattern::FileCounter,
    settings::{CONFIG_DIR_ENV, Paths, Settings},
    workflow::{CancelToken, Engine, Naming, Services},
};
use ssx_platform::BackendKind;
use ssx_services::{ProductionOptions, ProductionServices};

use crate::{
    cli::GlobalArgs,
    error::{CliError, CliResult},
    output::Style,
};

/// Everything a command needs from the process.
#[derive(Debug)]
pub struct App {
    /// The global options as given.
    pub global: GlobalArgs,
    /// Resolved settings / data directories.
    pub paths: Paths,
    /// Cancelled by Ctrl-C.
    pub cancel: CancelToken,
    /// Styling for stdout.
    pub out: Style,
    /// Styling for stderr.
    pub err: Style,
}

/// Resolves the directories, honouring `--config-dir` over `SSX_CONFIG_DIR` over the platform
/// default. Takes the environment as a function so tests need not touch the real one.
pub fn resolve_paths(
    config_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> CliResult<Paths> {
    let over = config_dir.map(|d| d.as_os_str().to_owned());
    Paths::discover_with(
        |k| if k == CONFIG_DIR_ENV { over.clone().or_else(|| env(k)) } else { env(k) },
    )
    .map_err(|e| CliError::new(e.to_string()).hint("pass --config-dir DIR or set SSX_CONFIG_DIR"))
}

impl App {
    /// Builds the context from the parsed global options.
    pub fn new(global: GlobalArgs, cancel: CancelToken) -> CliResult<Self> {
        let paths = resolve_paths(global.config_dir.as_deref(), |k| std::env::var_os(k))?;
        let out = Style::for_stream(global.color, false);
        let err = Style::for_stream(global.color, true);
        Ok(Self { global, paths, cancel, out, err })
    }

    /// The forced capture backend, if any.
    pub fn backend(&self) -> CliResult<Option<BackendKind>> {
        self.global
            .backend
            .as_deref()
            .map(|name| {
                BackendKind::from_name(name).ok_or_else(|| {
                    CliError::usage(format!("unknown capture backend {name:?}"))
                        .hint("use one of: windows, wayland, portal, x11")
                })
            })
            .transpose()
    }

    /// Loads `settings.toml` strictly (a missing file means defaults). Non-fatal findings are
    /// logged as warnings.
    pub fn load_settings(&self) -> CliResult<Settings> {
        let loaded = Settings::load(&self.paths.settings_file())?;
        for w in &loaded.warnings {
            tracing::warn!("{w}");
        }
        Ok(loaded.settings)
    }

    /// The real services for `settings`.
    pub fn services(&self, settings: &Settings) -> CliResult<ProductionServices> {
        Ok(ProductionServices::new(
            settings,
            &self.paths,
            ProductionOptions {
                backend: self.backend()?,
                // A CLI process exits right after copying: hand the data to a process that
                // keeps serving the clipboard (wl-copy / xclip) when there is one.
                prefer_external_clipboard: true,
                ..ProductionOptions::default()
            },
        ))
    }

    /// Opens the history database, creating it (and the data directory) if needed.
    pub fn open_history(&self) -> CliResult<History> {
        std::fs::create_dir_all(&self.paths.data_dir).map_err(|e| {
            CliError::new(format!("cannot create {}: {e}", self.paths.data_dir.display()))
        })?;
        Ok(History::open(&self.paths.history_db())?)
    }

    /// Path for messages: the given one, made absolute when that is cheap.
    pub fn display_path(path: &Path) -> String {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).display().to_string()
    }
}

/// Services, history and engine for running workflows.
#[derive(Debug)]
pub struct Session {
    /// The settings the engine uses (with any command-line overrides applied).
    pub settings: Settings,
    /// The real services.
    pub services: ProductionServices,
    /// The history, when enabled and usable.
    pub history: Option<History>,
    /// The workflow engine.
    pub engine: Engine,
    naming: Naming,
}

impl Session {
    /// Builds the session. A history that cannot be opened is a warning, not an error: an
    /// upload must not fail because of a locked or damaged database.
    pub fn new(app: &App, settings: Settings) -> CliResult<Self> {
        let services = app.services(&settings)?;
        let history = if settings.history.enabled {
            match app.open_history() {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!("history is unavailable, this run will not be recorded: {e}");
                    None
                }
            }
        } else {
            None
        };
        let naming = Naming::system(Arc::new(FileCounter::new(app.paths.counter_file())));
        let engine = Engine::new(settings.clone(), naming.clone());
        Ok(Self { settings, services, history, engine, naming })
    }

    /// An engine with different settings (a per-run format override) but the same naming
    /// sources, so file names and the shared `%i` counter stay consistent.
    pub fn engine_for(&self, settings: Settings) -> Engine {
        Engine::new(settings, self.naming.clone())
    }

    /// The bundle the engine takes.
    pub fn bundle(&self) -> Services<'_> {
        self.services.services(self.history.as_ref())
    }
}

/// Removes the verbatim prefix (`\\?\`) that Windows' `canonicalize` adds to drive paths:
/// registry entries and shortcuts should carry the ordinary `C:\...` spelling. UNC and
/// non-Windows paths are returned unchanged.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() > 2 && rest.as_bytes()[1] == b':' => PathBuf::from(rest),
        _ => path,
    }
}

/// The absolute path of the running executable (what menu entries and hotkeys should run).
pub fn exe_path() -> CliResult<PathBuf> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map(strip_verbatim)
        .map_err(|e| CliError::new(format!("cannot determine the path of this executable: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_flag_beats_environment_beats_default() {
        let env = |k: &str| (k == CONFIG_DIR_ENV).then(|| OsString::from("/from/env"));
        let p = resolve_paths(Some(Path::new("/from/flag")), env).unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/from/flag"));
        assert_eq!(p.data_dir, PathBuf::from("/from/flag/data"));
        let p = resolve_paths(None, env).unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/from/env"));
        let e = resolve_paths(None, |_| Some(OsString::from("  "))).unwrap_err();
        assert!(e.hint.unwrap().contains("--config-dir"));
    }

    #[test]
    fn windows_verbatim_prefixes_are_removed_from_drive_paths_only() {
        let strip = |s: &str| strip_verbatim(PathBuf::from(s));
        assert_eq!(
            strip(r"\\?\C:\Program Files\ssx\ssx.exe"),
            PathBuf::from(r"C:\Program Files\ssx\ssx.exe")
        );
        assert_eq!(
            strip(r"\\?\UNC\server\share\ssx.exe"),
            PathBuf::from(r"\\?\UNC\server\share\ssx.exe")
        );
        assert_eq!(strip("/usr/bin/ssx"), PathBuf::from("/usr/bin/ssx"));
        assert_eq!(strip(r"C:\ssx.exe"), PathBuf::from(r"C:\ssx.exe"));
    }

    #[test]
    fn backend_names_are_validated() {
        let global = |b: Option<&str>| GlobalArgs {
            verbose: 0,
            quiet: false,
            config_dir: None,
            backend: b.map(str::to_owned),
            color: crate::output::ColorChoice::Never,
        };
        let app = |b| App {
            global: global(b),
            paths: Paths::rooted_at("/x"),
            cancel: CancelToken::new(),
            out: Style::plain(),
            err: Style::plain(),
        };
        assert_eq!(app(None).backend().unwrap(), None);
        assert_eq!(app(Some("x11")).backend().unwrap(), Some(BackendKind::X11));
        let e = app(Some("nonsense")).backend().unwrap_err();
        assert_eq!(e.code, crate::error::ExitCode::Usage);
        assert!(e.hint.unwrap().contains("wayland"));
    }
}
