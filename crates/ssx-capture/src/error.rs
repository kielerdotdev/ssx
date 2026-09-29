use ssx_types::{FrameError, Rect};

pub type Result<T> = std::result::Result<T, CaptureError>;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The backend cannot do this (see [`crate::Capabilities`]).
    #[error("{backend}: {what} is not supported")]
    Unsupported { backend: &'static str, what: &'static str },
    /// The OS refused (macOS TCC, portal denial, Wayland compositor policy…).
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The user dismissed a picker or permission dialog.
    #[error("capture cancelled")]
    Cancelled,
    /// Unknown monitor or window id.
    #[error("no monitor or window with id {0:?}")]
    NotFound(String),
    /// The requested region is empty or does not overlap any monitor.
    #[error("region {0:?} is empty or outside every monitor")]
    InvalidRegion(Rect),
    /// No backend could be initialised in this session.
    #[error("no usable capture backend: {0}")]
    NoBackend(String),
    /// Anything else, with the originating backend's name.
    #[error("{backend}: {message}")]
    Backend { backend: &'static str, message: String },
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Composite(#[from] crate::composite::CompositeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl CaptureError {
    pub fn unsupported(backend: &'static str, what: &'static str) -> Self {
        Self::Unsupported { backend, what }
    }

    pub fn backend(backend: &'static str, message: impl ToString) -> Self {
        Self::Backend { backend, message: message.to_string() }
    }
}
