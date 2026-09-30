//! Error types of the recording crate.
//!
//! One enum per concern (`RecordError` for the session/pipeline, `AudioError` for audio,
//! `SourceError` for capture) so callers can match on what they care about: an
//! [`AudioError`] never aborts a recording (the session degrades to video-only), a
//! [`SourceError::PermissionDenied`] is something a UI should explain, while
//! [`RecordError::NoEncoder`] lists every candidate that was tried and why it failed.

use std::path::PathBuf;

/// Errors of a frame source (screen capture).
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// No capture path works in this environment (no display server, no portal, ...).
    #[error("no capture source is available: {0}")]
    Unavailable(String),
    /// The platform reports that this operation is not implemented for the requested
    /// selection (for example window capture on a compositor without the protocol).
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The user (or the system) denied screen capture, or dismissed the picker.
    #[error("screen capture was denied or cancelled: {0}")]
    PermissionDenied(String),
    /// The requested monitor, window or region does not exist (any more).
    #[error("capture target not found: {0}")]
    TargetNotFound(String),
    /// The captured surface is delivered in a form we cannot read (for example
    /// DMA-BUF-only PipeWire buffers).
    #[error("unsupported buffer type: {0}")]
    UnsupportedBuffer(String),
    /// The capture backend failed while running.
    #[error("{backend} capture failed: {message}")]
    Backend {
        /// Short backend name (`x11`, `pipewire`, `wgc`, ...).
        backend: &'static str,
        /// What went wrong.
        message: String,
    },
    /// A frame arrived that cannot be processed.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

impl SourceError {
    /// Shorthand for [`SourceError::Backend`].
    pub fn backend(backend: &'static str, message: impl Into<String>) -> Self {
        Self::Backend { backend, message: message.into() }
    }
}

impl From<ssx_capture::CaptureError> for SourceError {
    fn from(e: ssx_capture::CaptureError) -> Self {
        use ssx_capture::CaptureError as C;
        match e {
            C::NoBackend(m) => Self::Unavailable(m),
            C::NotFound(m) => Self::TargetNotFound(m),
            C::PermissionDenied(m) => Self::PermissionDenied(m),
            C::Cancelled => Self::PermissionDenied("cancelled".into()),
            C::Unsupported { backend, what } => Self::Unsupported(format!("{backend}: {what}")),
            C::InvalidRegion(r) => Self::TargetNotFound(format!("region {r:?}")),
            C::Backend { backend, message } => Self::Backend { backend, message },
            other => Self::Backend { backend: "capture", message: other.to_string() },
        }
    }
}

/// Errors of an audio source or the audio pipeline. Never fatal for a recording.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// No suitable device (no default input, no loopback / monitor source).
    #[error("no audio device: {0}")]
    NoDevice(String),
    /// The device exists but cannot deliver a usable format.
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    /// The host audio API failed.
    #[error("audio backend `{backend}` failed: {message}")]
    Backend {
        /// Backend name (`cpal-wasapi`, `pulse`, ...).
        backend: &'static str,
        /// What went wrong.
        message: String,
    },
    /// The resampler rejected its input.
    #[error("resampling failed: {0}")]
    Resample(String),
}

/// Errors of the recording session and the encoders.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The configuration is inconsistent (odd size, unknown container, ...).
    #[error("invalid recording configuration: {0}")]
    InvalidConfig(String),
    /// The build has no FFmpeg (the `ffmpeg` feature is off) but a video container was
    /// requested.
    #[error("this build has no FFmpeg support (enable the `system` or `static` feature)")]
    NoFfmpeg,
    /// The requested feature is not available in this build or on this platform.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// No candidate encoder could be opened.
    #[error("no usable video encoder for {codec} in {container}: {}", .tried.join("; "))]
    NoEncoder {
        /// Requested codec family.
        codec: String,
        /// Requested container.
        container: String,
        /// One `name: reason` line per candidate that was probed.
        tried: Vec<String>,
    },
    /// An encoder or muxer call failed.
    #[error("encoder `{encoder}` failed: {message}")]
    Encoder {
        /// Encoder name (`libx264`, `gifski`, ...).
        encoder: String,
        /// What went wrong.
        message: String,
    },
    /// The frame source failed.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// Pixel-format conversion or tone mapping failed.
    #[error("frame conversion failed: {0}")]
    Convert(String),
    /// File-system error (output file, partial file removal).
    #[error("i/o error on {path:?}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// No frame was captured before the recording ended, so there is nothing to save.
    #[error("the recording contains no video frames")]
    Empty,
    /// A pipeline thread panicked or vanished.
    #[error("internal pipeline failure: {0}")]
    Internal(String),
}

impl RecordError {
    /// Shorthand for [`RecordError::Encoder`].
    pub fn encoder(encoder: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Encoder { encoder: encoder.into(), message: message.into() }
    }
}

/// Result alias for the session and encoders.
pub type Result<T, E = RecordError> = std::result::Result<T, E>;
