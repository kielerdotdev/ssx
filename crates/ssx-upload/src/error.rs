//! Error type shared by every uploader.
//!
//! The variants are deliberately coarse and *actionable*: callers (the workflow engine, the
//! UI) match on them to decide whether to retry, ask the user to re-authenticate, or show a
//! configuration hint. `is_retryable` is the single source of truth for the retry wrapper.

use std::time::Duration;

use crate::types::UploadKind;

/// Everything that can go wrong while uploading.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The request never produced an HTTP response (DNS, connect, TLS, reset, timeout...).
    #[error("network error: {message}")]
    Network {
        /// Human readable description (never contains secrets or query strings).
        message: String,
        /// The failure was a timeout.
        timed_out: bool,
    },
    /// The server answered with a non-success status that is neither 401/403 nor 429.
    #[error("{}", http_message(*.status, .message.as_deref(), .body_snippet))]
    Http {
        /// HTTP status code.
        status: u16,
        /// Service supplied error text (for `.sxcu`: the evaluated `ErrorMessage`).
        message: Option<String>,
        /// The first few hundred characters of the response body.
        body_snippet: String,
        /// Parsed `Retry-After`, if the server sent one.
        retry_after: Option<Duration>,
    },
    /// HTTP 429 (or a service specific equivalent).
    #[error("rate limited{}", retry_suffix(*.retry_after))]
    RateLimited {
        /// How long the server asked us to wait.
        retry_after: Option<Duration>,
    },
    /// Credentials are missing, expired or rejected (HTTP 401/403, missing secret...).
    #[error("authentication failed: {message}")]
    Auth {
        /// What went wrong and, where possible, how to fix it.
        message: String,
    },
    /// The caller cancelled the upload through the [`crate::UploadContext`] token.
    #[error("upload cancelled")]
    Cancelled,
    /// The server answered successfully but the response could not be interpreted.
    #[error("invalid response from server: {why}")]
    InvalidResponse {
        /// Explanation, including a short snippet of the offending response where useful.
        why: String,
    },
    /// The uploader is misconfigured or the request cannot be expressed with it.
    #[error("configuration error: {message}")]
    Config {
        /// What is wrong.
        message: String,
    },
    /// The uploader does not handle this kind of content.
    #[error("uploader '{uploader}' does not support {kind} uploads")]
    Unsupported {
        /// Uploader name.
        uploader: String,
        /// The rejected kind.
        kind: UploadKind,
    },
    /// Local I/O failure (reading the file to upload, writing a destination...).
    #[error("I/O error while {context}: {source}")]
    Io {
        /// What we were doing.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

fn retry_suffix(after: Option<Duration>) -> String {
    after.map(|d| format!(" (retry after {}s)", d.as_secs())).unwrap_or_default()
}

fn http_message(status: u16, message: Option<&str>, snippet: &str) -> String {
    match message {
        Some(m) if !m.is_empty() => format!("HTTP {status}: {m}"),
        _ if snippet.is_empty() => format!("HTTP {status}"),
        _ => format!("HTTP {status}: {snippet}"),
    }
}

impl UploadError {
    /// Convenience constructor for [`UploadError::Config`].
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config { message: message.into() }
    }

    /// Convenience constructor for [`UploadError::InvalidResponse`].
    pub fn invalid_response(why: impl Into<String>) -> Self {
        Self::InvalidResponse { why: why.into() }
    }

    /// Convenience constructor for [`UploadError::Io`].
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io { context: context.into(), source }
    }

    /// Whether repeating the *same* request has a realistic chance of succeeding.
    ///
    /// Transient transport failures, 408/425/429 and 5xx (except 501/505, which are
    /// permanent) are retryable. Auth, configuration, cancellation and unparsable
    /// responses are not: repeating them only wastes time or hammers a server.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { .. } | Self::RateLimited { .. } => true,
            Self::Http { status, .. } => {
                matches!(*status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 507..=599)
                    && !matches!(*status, 501 | 505)
            }
            Self::Auth { .. }
            | Self::Cancelled
            | Self::InvalidResponse { .. }
            | Self::Config { .. }
            | Self::Unsupported { .. }
            | Self::Io { .. } => false,
        }
    }

    /// The server supplied `Retry-After` hint, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// True for [`UploadError::Cancelled`].
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> UploadError {
        UploadError::Http { status, message: None, body_snippet: String::new(), retry_after: None }
    }

    #[test]
    fn retryability() {
        assert!(http(500).is_retryable());
        assert!(http(503).is_retryable());
        assert!(http(429).is_retryable());
        assert!(!http(501).is_retryable());
        assert!(!http(400).is_retryable());
        assert!(!http(404).is_retryable());
        assert!(UploadError::Network { message: "x".into(), timed_out: true }.is_retryable());
        assert!(UploadError::RateLimited { retry_after: None }.is_retryable());
        assert!(!UploadError::Cancelled.is_retryable());
        assert!(!UploadError::Auth { message: "x".into() }.is_retryable());
        assert!(!UploadError::config("x").is_retryable());
    }

    #[test]
    fn display_prefers_service_message() {
        let e = UploadError::Http {
            status: 400,
            message: Some("bad file".into()),
            body_snippet: "<html>".into(),
            retry_after: None,
        };
        assert_eq!(e.to_string(), "HTTP 400: bad file");
        assert_eq!(http(502).to_string(), "HTTP 502");
    }
}
