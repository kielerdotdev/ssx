//! Error type for the IPC transport.
//!
//! Variants are split so callers can react differently: `NotRunning` (start the app),
//! `Timeout` (app is hung), `LineTooLong`/`InvalidLine` (protocol abuse) and the security
//! variants (never retry, surface to the user).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// What was being waited for when a [`Error::Timeout`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// Waiting for the server socket to appear / accept.
    Connect,
    /// Waiting for a peer to send (or finish sending) a line.
    Read,
    /// Waiting for the peer to drain what we sent.
    Write,
    /// Waiting for a freshly spawned app to start listening.
    Startup,
}

impl std::fmt::Display for TimeoutKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "connecting",
            Self::Read => "reading",
            Self::Write => "writing",
            Self::Startup => "waiting for the application to start",
        })
    }
}

/// Errors returned by `ssx-ipc`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The application id contains characters that are unsafe in file/pipe names.
    #[error(
        "invalid application id {0:?}: use 1-48 characters from [A-Za-z0-9._-], not starting with '.'"
    )]
    InvalidAppId(String),

    /// No per-user runtime directory could be determined.
    #[error("cannot determine a per-user runtime directory: {0}")]
    NoRuntimeDir(String),

    /// The runtime directory exists but is not private to the current user.
    #[error("runtime directory {path} is not private to this user: {reason}")]
    InsecureDirectory {
        /// Offending directory.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// A socket (or other file) at our endpoint path belongs to somebody else.
    #[error("refusing endpoint {path}: {reason}")]
    ForeignEndpoint {
        /// Offending path.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// The socket path exceeds the OS limit for Unix socket addresses.
    #[error(
        "socket path {path} is too long ({len} bytes, limit {limit}); set a shorter runtime dir"
    )]
    PathTooLong {
        /// Offending path.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
        /// The limit we enforce.
        limit: usize,
    },

    /// The peer's credentials did not match the current user.
    #[error("peer rejected: {0}")]
    PeerRejected(String),

    /// No server is listening (and, for `send_or_spawn`, spawning did not help).
    #[error("no ssx instance is listening")]
    NotRunning,

    /// A line exceeded the configured maximum length.
    #[error("line exceeds the maximum of {max} bytes")]
    LineTooLong {
        /// Configured limit.
        max: usize,
    },

    /// A line to send contained a newline, or received bytes were not UTF-8.
    #[error("invalid line: {0}")]
    InvalidLine(&'static str),

    /// The peer closed the connection before completing a line.
    #[error("connection closed by peer")]
    ConnectionClosed,

    /// A time limit expired.
    #[error("timed out after {after:?} while {kind}")]
    Timeout {
        /// What we were doing.
        kind: TimeoutKind,
        /// The limit that expired.
        after: Duration,
    },

    /// The server was shut down.
    #[error("server has been shut down")]
    Shutdown,

    /// The `spawn` callback passed to `send_or_spawn` failed.
    #[error("failed to launch the application: {0}")]
    Spawn(#[source] io::Error),

    /// Underlying I/O error with context.
    #[error("{context}: {source}")]
    Io {
        /// What we were doing.
        context: &'static str,
        /// The OS error.
        #[source]
        source: io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { context, source }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
