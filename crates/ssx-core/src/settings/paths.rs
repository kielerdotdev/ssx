//! Where ssx keeps its files.
//!
//! Uses the platform conventions from the `directories` crate (`~/.config/ssx` on Linux,
//! `%APPDATA%\ssx\config` on Windows, `~/Library/Application Support/ssx` on macOS).
//! `SSX_CONFIG_DIR` relocates *everything* (config at the given directory, data in its
//! `data/` subfolder), which makes portable installs and hermetic tests trivial.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

/// Environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "SSX_CONFIG_DIR";

/// Why the directories could not be determined.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// No home directory (headless service accounts, containers) and no override.
    #[error(
        "cannot determine a per-user config directory (no home directory); set {CONFIG_DIR_ENV} to a writable folder"
    )]
    NoHome,
    /// The override is set but blank.
    #[error("{CONFIG_DIR_ENV} is set but empty; unset it or point it at a folder")]
    EmptyOverride,
}

/// Resolved ssx directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Holds `settings.toml`.
    pub config_dir: PathBuf,
    /// Holds the history database and counter (state that is not hand-edited).
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolves the directories from the process environment.
    pub fn discover() -> Result<Self, PathsError> {
        Self::discover_with(|k| std::env::var_os(k))
    }

    /// Like [`discover`](Self::discover) with an injectable environment lookup.
    pub fn discover_with(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, PathsError> {
        if let Some(over) = env(CONFIG_DIR_ENV) {
            if over.to_string_lossy().trim().is_empty() {
                return Err(PathsError::EmptyOverride);
            }
            return Ok(Self::rooted_at(PathBuf::from(over)));
        }
        let dirs = ProjectDirs::from("", "", "ssx").ok_or(PathsError::NoHome)?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    /// Everything under one root: config in `root`, data in `root/data`.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self { data_dir: root.join("data"), config_dir: root }
    }

    /// `settings.toml`.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    /// The history database.
    pub fn history_db(&self) -> PathBuf {
        self.data_dir.join("history.sqlite3")
    }

    /// The `%i` counter file.
    pub fn counter_file(&self) -> PathBuf {
        self.data_dir.join("counter")
    }

    /// Creates both directories.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)
    }
}

impl AsRef<Path> for Paths {
    fn as_ref(&self) -> &Path {
        &self.config_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_relocates_everything() {
        let p = Paths::discover_with(|k| {
            (k == CONFIG_DIR_ENV).then(|| OsString::from("/tmp/ssx-test"))
        })
        .unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/tmp/ssx-test"));
        assert_eq!(p.data_dir, PathBuf::from("/tmp/ssx-test/data"));
        assert_eq!(p.settings_file(), PathBuf::from("/tmp/ssx-test/settings.toml"));
        assert_eq!(p.history_db(), PathBuf::from("/tmp/ssx-test/data/history.sqlite3"));
        assert_eq!(p.counter_file(), PathBuf::from("/tmp/ssx-test/data/counter"));
    }

    #[test]
    fn blank_override_is_an_error() {
        let e = Paths::discover_with(|_| Some(OsString::from("  "))).unwrap_err();
        assert!(matches!(e, PathsError::EmptyOverride));
        assert!(e.to_string().contains(CONFIG_DIR_ENV));
    }

    #[test]
    fn platform_defaults_end_in_ssx() {
        // Either resolves to a per-user dir mentioning ssx, or reports NoHome (CI sandboxes).
        match Paths::discover_with(|_| None) {
            Ok(p) => {
                assert!(p.config_dir.to_string_lossy().to_lowercase().contains("ssx"));
                assert!(p.data_dir.to_string_lossy().to_lowercase().contains("ssx"));
            }
            Err(e) => assert!(matches!(e, PathsError::NoHome)),
        }
    }

    #[test]
    fn ensure_creates_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::rooted_at(tmp.path().join("a/b"));
        p.ensure().unwrap();
        assert!(p.config_dir.is_dir() && p.data_dir.is_dir());
    }
}
