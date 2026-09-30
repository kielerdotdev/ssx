//! Error types.

use std::time::Duration;

/// Why an overlay could not be shown or its result could not be obtained.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    /// No windowing system could be used. `tried` lists each candidate and why it failed.
    #[error("no usable display backend for the overlay: {}", .tried.join("; "))]
    NoBackend {
        /// One entry per rejected backend.
        tried: Vec<String>,
    },
    /// The platform has no overlay implementation (macOS for now).
    #[error("the selection overlay is not implemented on this platform ({0})")]
    Unsupported(&'static str),
    /// The input is unusable (empty frame, non-8-bit frame, ...).
    #[error("invalid overlay input: {0}")]
    InvalidInput(String),
    /// The chosen windowing backend failed while running.
    #[error("{backend} overlay backend failed: {message}")]
    Backend {
        /// Backend name (`x11`, `wayland-layer-shell`, ...).
        backend: &'static str,
        /// What went wrong.
        message: String,
    },
    /// Talking to the helper process failed.
    #[error(transparent)]
    Helper(#[from] HelperError),
    /// Frame conversion failed.
    #[error(transparent)]
    Frame(#[from] ssx_types::FrameError),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl OverlayError {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux backends report runtime errors this way
    pub(crate) fn backend(backend: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Backend { backend, message: err.to_string() }
    }
}

/// Failures specific to the helper-process client.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    /// The helper executable could not be started.
    #[error("cannot start overlay helper {path:?}: {source}")]
    Spawn {
        /// Executable that was tried.
        path: std::path::PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The user (or a stuck compositor) took longer than the timeout; the helper was killed.
    #[error("overlay helper did not answer within {0:?} and was killed")]
    Timeout(Duration),
    /// The helper died without answering (crash, kill, compositor gone).
    #[error("overlay helper exited with {status} without a result: {stderr}")]
    Crashed {
        /// Exit status description.
        status: String,
        /// Tail of the helper's stderr.
        stderr: String,
    },
    /// The helper reported an error of its own.
    #[error("overlay helper failed: {0}")]
    Reported(String),
    /// The helper's answer could not be parsed.
    #[error("overlay helper produced an unreadable answer: {0}")]
    BadAnswer(String),
}
