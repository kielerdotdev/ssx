//! Error type for the X11 backend, and its mapping onto [`ssx_capture::CaptureError`].
//!
//! X errors are kept structured (rather than flattened to strings) so callers and the
//! backend itself can react to specific cases: a `BadWindow` is "the window is gone", a
//! broken socket means the session must be re-established on the next call.

use ssx_capture::CaptureError;
use x11rb::{
    errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError},
    protocol::ErrorKind,
};

/// Everything that can go wrong talking to an X server.
#[derive(Debug, thiserror::Error)]
pub enum X11Error {
    /// Could not open the display.
    #[error(
        "cannot connect to X display {display:?}: {source}; is DISPLAY set and the server running?"
    )]
    Connect { display: String, source: ConnectError },
    /// The connection broke (server exited or was killed). The backend reconnects on the
    /// next call.
    #[error("connection to the X server was lost: {0}")]
    ConnectionLost(String),
    /// The server answered a request with an X protocol error.
    #[error("X server rejected {context}: {kind:?} (bad value {bad_value:#x})")]
    Server { context: &'static str, kind: ErrorKind, bad_value: u32 },
    /// The window no longer exists.
    #[error("window {0:#x} does not exist (any more)")]
    NoSuchWindow(u32),
    /// The window exists but is not viewable, so there are no pixels to capture.
    #[error("window {0:#x} is not viewable (minimised, unmapped or on another workspace)")]
    NotViewable(u32),
    /// The window id string is not a number.
    #[error("{0:?} is not a valid X11 window id (expected e.g. 0x1a00003 or 27262979)")]
    BadWindowId(String),
    /// The screen's pixel format is one this backend does not decode.
    #[error("unsupported X visual: {0}")]
    UnsupportedVisual(String),
    /// A reply was shorter than the protocol requires.
    #[error("malformed reply from the X server: {0}")]
    Malformed(&'static str),
    /// Other connection-level failure that is not fatal to the session.
    #[error("X connection error: {0}")]
    Connection(String),
}

pub(crate) type X11Result<T> = Result<T, X11Error>;

impl X11Error {
    /// `true` when the session is unusable and must be re-established.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self, X11Error::ConnectionLost(_))
    }

    pub(crate) fn from_connection(e: &ConnectionError) -> Self {
        match e {
            ConnectionError::IoError(_) | ConnectionError::ParseError(_) => {
                X11Error::ConnectionLost(e.to_string())
            }
            other => X11Error::Connection(other.to_string()),
        }
    }

    pub(crate) fn from_reply(context: &'static str, e: ReplyError) -> Self {
        match e {
            ReplyError::ConnectionError(c) => Self::from_connection(&c),
            ReplyError::X11Error(x) => {
                X11Error::Server { context, kind: x.error_kind, bad_value: x.bad_value }
            }
        }
    }

    /// `true` for `BadWindow` / `BadDrawable` / `BadPixmap` errors: the target vanished.
    pub(crate) fn is_gone(&self) -> bool {
        matches!(
            self,
            X11Error::NoSuchWindow(_)
                | X11Error::Server {
                    kind: ErrorKind::Window | ErrorKind::Drawable | ErrorKind::Pixmap,
                    ..
                }
        )
    }
}

impl From<ConnectionError> for X11Error {
    fn from(e: ConnectionError) -> Self {
        Self::from_connection(&e)
    }
}

impl From<ReplyOrIdError> for X11Error {
    fn from(e: ReplyOrIdError) -> Self {
        match e {
            ReplyOrIdError::IdsExhausted => X11Error::Connection("X resource ids exhausted".into()),
            ReplyOrIdError::ConnectionError(c) => Self::from_connection(&c),
            ReplyOrIdError::X11Error(x) => {
                X11Error::Server { context: "request", kind: x.error_kind, bad_value: x.bad_value }
            }
        }
    }
}

impl From<ReplyError> for X11Error {
    fn from(e: ReplyError) -> Self {
        Self::from_reply("request", e)
    }
}

impl From<X11Error> for CaptureError {
    fn from(e: X11Error) -> Self {
        match e {
            X11Error::NoSuchWindow(id) => CaptureError::NotFound(format!("{id:#x}")),
            X11Error::BadWindowId(s) => CaptureError::NotFound(s),
            X11Error::Connect { .. } => CaptureError::NoBackend(e.to_string()),
            other => CaptureError::backend("x11", other),
        }
    }
}
