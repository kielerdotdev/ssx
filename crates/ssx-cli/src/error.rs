//! CLI errors and the documented exit codes.
//!
//! Every failure ends up as `error: <what went wrong> (hint: <what to do>)` on stderr and
//! one of the exit codes below, so scripts can tell "you called it wrong" from "it failed" from
//! "the user gave up".

use std::fmt;

use ssx_core::{
    history::HistoryError,
    settings::{PathsError, SettingsError},
    workflow::ServiceError,
};

/// Process exit codes (documented in `ssx --help`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Everything that was asked for worked.
    Ok = 0,
    /// The command failed (also: a workflow finished only partially).
    Error = 1,
    /// The command line was wrong (clap reports these itself).
    Usage = 2,
    /// The user cancelled (Ctrl-C, closed the editor or the region overlay).
    Cancelled = 3,
}

impl ExitCode {
    /// The numeric process exit code.
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// A failed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// What went wrong.
    pub message: String,
    /// What to do about it.
    pub hint: Option<String>,
    /// The exit code to end the process with.
    pub code: ExitCode,
}

/// Result alias for commands.
pub type CliResult<T> = Result<T, CliError>;

impl CliError {
    /// A failure with exit code 1.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None, code: ExitCode::Error }
    }

    /// A usage error (exit code 2) for problems clap cannot see (mutually dependent options).
    pub fn usage(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None, code: ExitCode::Usage }
    }

    /// The user cancelled (exit code 3).
    pub fn cancelled() -> Self {
        Self { message: "cancelled".to_owned(), hint: None, code: ExitCode::Cancelled }
    }

    /// Adds a hint.
    #[must_use]
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {}

impl From<ServiceError> for CliError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Cancelled => Self::cancelled(),
            ServiceError::Unsupported(what) => {
                Self::new(format!("{what} is not supported here")).hint("run `ssx doctor` to see what this system offers")
            }
            ServiceError::NotConfigured(msg) => Self::new(msg),
            other => Self::new(other.to_string()),
        }
    }
}

impl From<SettingsError> for CliError {
    fn from(e: SettingsError) -> Self {
        let hint = match &e {
            SettingsError::Parse { .. } => Some("fix the file with `ssx config edit`, check it with `ssx config validate`"),
            SettingsError::Invalid { .. } => Some("nothing was written; correct the value and try again"),
            SettingsError::Migrate(_) => Some("upgrade ssx, or move the settings file aside to start from defaults"),
            _ => None,
        };
        let mut err = Self::new(e.to_string());
        err.hint = hint.map(str::to_owned);
        err
    }
}

impl From<PathsError> for CliError {
    fn from(e: PathsError) -> Self {
        Self::new(e.to_string())
    }
}

impl From<HistoryError> for CliError {
    fn from(e: HistoryError) -> Self {
        let hint = match &e {
            HistoryError::Busy => Some("another ssx process is using the history; retry in a moment"),
            HistoryError::Corrupt { .. } => {
                Some("move the history database aside (its path is shown by `ssx config path --all`) to start a fresh one")
            }
            _ => None,
        };
        let mut err = Self::new(e.to_string());
        err.hint = hint.map(str::to_owned);
        err
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(format!("cannot encode JSON output: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uses_the_documented_error_format() {
        assert_eq!(CliError::new("boom").to_string(), "boom");
        assert_eq!(CliError::new("boom").hint("try again").to_string(), "boom (hint: try again)");
    }

    #[test]
    fn exit_codes_are_the_documented_ones() {
        assert_eq!(ExitCode::Ok.code(), 0);
        assert_eq!(ExitCode::Error.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::Cancelled.code(), 3);
        assert_eq!(CliError::cancelled().code, ExitCode::Cancelled);
        assert_eq!(CliError::usage("x").code, ExitCode::Usage);
        assert_eq!(CliError::new("x").code, ExitCode::Error);
    }

    #[test]
    fn service_errors_map_to_the_right_codes_and_hints() {
        assert_eq!(CliError::from(ServiceError::Cancelled).code, ExitCode::Cancelled);
        let e = CliError::from(ServiceError::Unsupported("window capture".into()));
        assert!(e.message.contains("window capture") && e.hint.unwrap().contains("doctor"));
        let e = CliError::from(ServiceError::NotConfigured("no uploader".into()));
        assert_eq!(e.message, "no uploader");
    }
}
