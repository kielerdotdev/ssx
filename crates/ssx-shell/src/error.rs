//! Error type for the installers.

use std::io;
use std::path::PathBuf;

/// Errors from installing, removing or inspecting a file-manager integration.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    /// The `ssx` executable path cannot be embedded safely.
    #[error("invalid ssx executable path {path:?}: {reason}")]
    InvalidExe {
        /// The offending path.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// An action definition cannot be represented safely.
    #[error("invalid action {id:?}: {reason}")]
    InvalidAction {
        /// Action id.
        id: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A file we would write already exists and was not created by ssx.
    #[error("{path} already exists and was not created by ssx; remove or rename it first")]
    NotManaged {
        /// The existing file.
        path: PathBuf,
    },

    /// An existing configuration file could not be parsed, so we refuse to modify it.
    #[error("cannot parse {path}: {reason}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// Parser message.
        reason: String,
    },

    /// This integration is not available in the given environment.
    #[error("{0}")]
    Unavailable(String),

    /// Filesystem error with the path involved.
    #[error("{op} {path}: {source}")]
    Io {
        /// What was being done (`reading`, `writing`, ...).
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },

    /// Registry error (Windows).
    #[error("registry {op} {key}: {source}")]
    Registry {
        /// What was being done.
        op: &'static str,
        /// The registry key path (relative to HKCU).
        key: String,
        /// The OS error.
        #[source]
        source: io::Error,
    },
}

impl ShellError {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }

    pub(crate) fn registry(op: &'static str, key: &str) -> impl FnOnce(io::Error) -> Self {
        let key = key.to_owned();
        move |source| Self::Registry { op, key, source }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ShellError>;
