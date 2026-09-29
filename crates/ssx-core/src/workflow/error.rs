//! The error type shared by every service trait.

/// Why a service call failed.
///
/// The engine maps these onto [`FailureKind`](super::FailureKind)s in the run report, so
/// implementations should pick the most specific variant and put an *actionable* message in
/// it ("the imgur token has expired; sign in again in Settings > Uploaders").
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The user (or the cancellation token) aborted the operation. Never a failure.
    #[error("cancelled")]
    Cancelled,
    /// This platform/backend cannot do that (e.g. window capture on a compositor without
    /// the protocol).
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The feature needs configuration that is missing (no uploader chosen, no API key).
    #[error("not configured: {0}")]
    NotConfigured(String),
    /// A file-system operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Anything else. `retryable` hints that trying again may help (network timeouts).
    #[error("{message}")]
    Failed {
        /// Human-readable description.
        message: String,
        /// Whether a retry could succeed.
        retryable: bool,
    },
}

impl ServiceError {
    /// A non-retryable failure.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed { message: message.into(), retryable: false }
    }

    /// A failure that may succeed on retry.
    pub fn retryable(message: impl Into<String>) -> Self {
        Self::Failed { message: message.into(), retryable: true }
    }

    /// `true` for [`ServiceError::Cancelled`].
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}
