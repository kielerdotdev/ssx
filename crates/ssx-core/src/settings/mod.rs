//! Typed, versioned application settings stored as TOML.
//!
//! Design decisions:
//!
//! * **Tolerant load.** Missing keys take their default (every struct is
//!   `#[serde(default)]`); unknown keys are reported as warnings and *kept out of* the typed
//!   value (so a downgrade never crashes), but the file on disk is only rewritten on an
//!   explicit [`Settings::save`].
//! * **Strict save.** [`Settings::save`] refuses to write settings that
//!   [`validate`](Settings::validate) reports errors for (notably plain-text secrets), and
//!   writes atomically (temp file + `fsync` + rename), so a crash never leaves a truncated
//!   file.
//! * **Versioned.** The file has a `version`; older files are migrated on load (with a
//!   `.vN.bak` copy of the original), newer files are refused untouched. See [`migrate`].
//! * **Opaque uploader config.** `[uploaders.<name>]` tables are kept as raw TOML so the
//!   upload crate owns their schema. Secrets are referenced as `keyring:<name>`, never
//!   stored.

mod destinations;
mod hotkey;
mod io;
pub mod migrate;
mod model;
mod paths;
mod validate;
pub mod workflow;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub use destinations::{DestinationOverride, DestinationType, Destinations};
pub use hotkey::{Hotkey, HotkeyError, Modifier};
pub use io::atomic_write;
pub use migrate::{CURRENT_VERSION, MigrateError};
pub use model::{
    CaptureSettings, FolderPolicy, General, HdrConfig, HistorySettings, Hotkeys, ImageFormatKind,
    PostFileSettings, TonemapOperator, TypeSubfolders,
};
pub use paths::{CONFIG_DIR_ENV, Paths, PathsError};
pub use validate::{KEYRING_PREFIX, Severity, ValidationIssue};
pub use workflow::{AfterCapture, AfterUpload, InputKind, Trigger, Workflow, builtin_workflows};

/// Everything that can go wrong loading or saving settings.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// Reading or writing the file failed.
    #[error("cannot access settings file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML or a value has the wrong type.
    #[error("cannot parse settings{}: {message}\nFix the mistake, or delete the file to start from defaults.", path.as_ref().map(|p| format!(" file {}", p.display())).unwrap_or_default())]
    Parse {
        /// The file, if it came from disk.
        path: Option<PathBuf>,
        /// Parser message, including line and column.
        message: String,
    },
    /// Version handling failed (file from a newer ssx, corrupt version key, …).
    #[error(transparent)]
    Migrate(#[from] MigrateError),
    /// Serialising failed (a bug: settings are always representable).
    #[error("cannot serialise settings: {0}")]
    Serialize(String),
    /// [`Settings::save`] refused because validation found errors.
    #[error("settings are invalid and were not saved:\n{}", issues.iter().map(|i| format!("  {i}")).collect::<Vec<_>>().join("\n"))]
    Invalid {
        /// The blocking issues.
        issues: Vec<ValidationIssue>,
    },
}

/// The complete configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Schema version of the file (see [`CURRENT_VERSION`]).
    pub version: u32,
    /// Saving, naming, formats.
    pub general: General,
    /// Capture behaviour and HDR defaults.
    pub capture: CaptureSettings,
    /// Default uploader per content type.
    pub destinations: Destinations,
    /// Opaque per-uploader configuration, keyed by uploader name.
    pub uploaders: BTreeMap<String, toml::Table>,
    /// History database behaviour.
    pub history: HistorySettings,
    /// `post_file` behaviour.
    pub post_file: PostFileSettings,
    /// The user's workflows (the built-in ones unless the file says otherwise).
    #[serde(default = "builtin_workflows")]
    pub workflows: Vec<Workflow>,
    /// Global hotkeys not tied to a workflow.
    pub hotkeys: Hotkeys,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            general: General::default(),
            capture: CaptureSettings::default(),
            destinations: Destinations::default(),
            uploaders: BTreeMap::new(),
            history: HistorySettings::default(),
            post_file: PostFileSettings::default(),
            workflows: builtin_workflows(),
            hotkeys: Hotkeys::default(),
        }
    }
}

/// The result of loading settings.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The settings (defaults for anything missing).
    pub settings: Settings,
    /// Non-fatal findings: unknown keys, migration notes.
    pub warnings: Vec<String>,
    /// `Some(v)` if the file was at version `v` and has been migrated.
    pub migrated_from: Option<u32>,
    /// `false` if the file did not exist and defaults were returned.
    pub existed: bool,
}

/// Paths of keys present in `original` but absent from `typed` (i.e. ignored by serde).
fn unknown_keys(original: &toml::Table, typed: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (k, v) in original {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        // Opaque uploader tables are preserved verbatim, so nothing inside is "unknown".
        if path.starts_with("uploaders.") || path == "uploaders" {
            continue;
        }
        match typed.get(k) {
            None => out.push(path),
            Some(t) => unknown_in_value(v, t, &path, out),
        }
    }
}

fn unknown_in_value(
    original: &toml::Value,
    typed: &toml::Value,
    path: &str,
    out: &mut Vec<String>,
) {
    match (original, typed) {
        (toml::Value::Table(o), toml::Value::Table(t)) => unknown_keys(o, t, path, out),
        (toml::Value::Array(o), toml::Value::Array(t)) => {
            for (i, (ov, tv)) in o.iter().zip(t).enumerate() {
                unknown_in_value(ov, tv, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

impl Settings {
    /// Parses settings from TOML text, migrating and collecting warnings.
    pub fn from_toml_str(text: &str) -> Result<Loaded, SettingsError> {
        Self::parse_inner(text, None)
    }

    fn parse_inner(text: &str, path: Option<&Path>) -> Result<Loaded, SettingsError> {
        let parse_err = |e: &dyn std::fmt::Display| SettingsError::Parse {
            path: path.map(Path::to_path_buf),
            message: e.to_string(),
        };
        let mut table: toml::Table = text.parse().map_err(|e| parse_err(&e))?;
        let from = migrate::migrate(&mut table)?;
        let migrated_from = (from < CURRENT_VERSION).then_some(from);
        let settings: Settings = table.clone().try_into().map_err(|e| parse_err(&e))?;

        let mut warnings = Vec::new();
        if let Ok(typed) = toml::Table::try_from(&settings) {
            let mut unknown = Vec::new();
            unknown_keys(&table, &typed, "", &mut unknown);
            for key in unknown {
                warnings.push(format!(
                    "unknown setting `{key}` was ignored (typo, or written by a different ssx version)"
                ));
            }
        }
        if let Some(v) = migrated_from {
            warnings.push(format!("settings migrated from version {v} to {CURRENT_VERSION}"));
        }
        Ok(Loaded { settings, warnings, migrated_from, existed: true })
    }

    /// Serialises to TOML text.
    pub fn to_toml_string(&self) -> Result<String, SettingsError> {
        toml::to_string_pretty(self).map_err(|e| SettingsError::Serialize(e.to_string()))
    }

    /// Loads `path`. A missing file yields defaults (`existed == false`). A file from an
    /// older version is migrated: the original is copied to `<file>.v<N>.bak` and the
    /// migrated file replaces it.
    pub fn load(path: &Path) -> Result<Loaded, SettingsError> {
        let io_err = |source| SettingsError::Io { path: path.to_path_buf(), source };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Loaded {
                    settings: Settings::default(),
                    warnings: Vec::new(),
                    migrated_from: None,
                    existed: false,
                });
            }
            Err(e) => return Err(io_err(e)),
        };
        let mut loaded = Self::parse_inner(&text, Some(path))?;
        if let Some(from) = loaded.migrated_from {
            let backup = path.with_file_name(format!(
                "{}.v{from}.bak",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            ));
            let result = atomic_write(&backup, text.as_bytes()).and_then(|()| {
                let migrated = loaded
                    .settings
                    .to_toml_string()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                atomic_write(path, migrated.as_bytes())
            });
            match result {
                Ok(()) => loaded
                    .warnings
                    .push(format!("the previous file was saved as {}", backup.display())),
                // Keep working from the in-memory migrated value; try again next start.
                Err(e) => loaded.warnings.push(format!(
                    "could not write the migrated settings back to {}: {e}",
                    path.display()
                )),
            }
        }
        Ok(loaded)
    }

    /// Like [`load`](Self::load), but a file that cannot be *parsed* is moved aside to
    /// `<file>.corrupt-<unix time>` and defaults are returned with a warning, so a broken
    /// hand edit never stops the app from starting. Files from a *newer* ssx and I/O errors
    /// are still reported as errors (those must not be clobbered).
    pub fn load_or_recover(path: &Path) -> Result<Loaded, SettingsError> {
        match Self::load(path) {
            Err(SettingsError::Parse { message, .. }) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let aside = path.with_file_name(format!(
                    "{}.corrupt-{stamp}",
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                ));
                std::fs::rename(path, &aside)
                    .map_err(|source| SettingsError::Io { path: path.to_path_buf(), source })?;
                Ok(Loaded {
                    settings: Settings::default(),
                    warnings: vec![format!(
                        "settings file was unreadable ({message}); it was moved to {} and defaults are in use",
                        aside.display()
                    )],
                    migrated_from: None,
                    existed: true,
                })
            }
            other => other,
        }
    }

    /// Validates and atomically writes to `path`. Warnings do not block saving.
    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let issues: Vec<_> =
            self.validate().into_iter().filter(|i| i.severity == Severity::Error).collect();
        if !issues.is_empty() {
            return Err(SettingsError::Invalid { issues });
        }
        let text = self.to_toml_string()?;
        atomic_write(path, text.as_bytes())
            .map_err(|source| SettingsError::Io { path: path.to_path_buf(), source })
    }

    /// Checks the settings for values that cannot work. See [`ValidationIssue`].
    pub fn validate(&self) -> Vec<ValidationIssue> {
        validate::validate(self)
    }

    /// The workflow with this id.
    pub fn workflow_by_id(&self, id: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.id == id)
    }

    /// The workflow reachable as `ssx run <name>`.
    pub fn workflow_by_cli_name(&self, name: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.trigger.cli_name.as_deref() == Some(name))
    }

    /// Looks a workflow up by id, then by CLI name, then by display name
    /// (case-insensitively). What `RunWorkflow { name }` uses.
    pub fn find_workflow(&self, id_or_name: &str) -> Option<&Workflow> {
        self.workflow_by_id(id_or_name)
            .or_else(|| self.workflow_by_cli_name(id_or_name))
            .or_else(|| self.workflows.iter().find(|w| w.name.eq_ignore_ascii_case(id_or_name)))
    }

    /// The workflow bound to `hotkey` (compared in canonical form).
    pub fn workflow_for_hotkey(&self, hotkey: &Hotkey) -> Option<&Workflow> {
        self.workflows.iter().find(|w| {
            w.trigger.hotkey.as_deref().and_then(|h| h.parse::<Hotkey>().ok()).as_ref()
                == Some(hotkey)
        })
    }
}

#[cfg(test)]
mod tests;
