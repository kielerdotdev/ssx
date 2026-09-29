//! Per-upload environment handed to every [`crate::Uploader`].

use std::fmt;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::UploadError;
use crate::progress::{NullProgress, ProgressSink};
use crate::secrets::{InMemorySecretStore, SecretError, SecretStore};

/// Everything an uploader needs besides its own configuration. Cheap to clone.
#[derive(Clone)]
pub struct UploadContext {
    /// Progress receiver (wrap in [`crate::ThrottledProgress`] for UI use).
    pub progress: Arc<dyn ProgressSink>,
    /// Cancelling this aborts the in-flight request promptly and yields
    /// [`UploadError::Cancelled`].
    pub cancel: CancellationToken,
    /// Shared HTTP client (connection pooling, proxy settings, TLS roots).
    pub http: reqwest::Client,
    /// Where secrets referenced by uploader configuration are resolved.
    pub secrets: Arc<dyn SecretStore>,
}

impl fmt::Debug for UploadContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadContext")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl UploadContext {
    /// Context with no progress reporting, a fresh cancellation token and an empty
    /// in-memory secret store.
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            progress: Arc::new(NullProgress),
            cancel: CancellationToken::new(),
            http,
            secrets: Arc::new(InMemorySecretStore::new()),
        }
    }

    /// Use `progress` as the progress sink.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Use `cancel` as the cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Use `secrets` as the secret store.
    #[must_use]
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretStore>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Fail fast when already cancelled.
    pub fn check_cancelled(&self) -> Result<(), UploadError> {
        if self.cancel.is_cancelled() { Err(UploadError::Cancelled) } else { Ok(()) }
    }

    /// Look up a required secret, mapping absence to an actionable [`UploadError::Auth`].
    pub fn require_secret(&self, key: &str) -> Result<String, UploadError> {
        match self.secrets.get(key) {
            Ok(Some(v)) if !v.is_empty() => Ok(v),
            Ok(_) => Err(UploadError::Auth {
                message: format!("secret '{key}' is not set; add it in the uploader settings"),
            }),
            Err(SecretError(e)) => {
                Err(UploadError::Auth { message: format!("could not read secret '{key}': {e}") })
            }
        }
    }
}
