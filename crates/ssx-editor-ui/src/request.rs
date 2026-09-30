//! The public request/outcome types: how the CLI or tray launches the editor and what it gets
//! back.
//!
//! The outcome is a small JSON document on stdout (`{"action":"save","path":"..."}`) plus an
//! exit code, so shell scripts and the tray can continue the workflow (`upload`, `copy`...)
//! without linking this crate.

use std::path::PathBuf;

use serde::Serialize;
use ssx_types::Frame;

/// What the editor opens with.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorInput {
    /// An image file or an `.ssxe` project.
    Path(PathBuf),
    /// Pixels the caller already has (a fresh capture).
    Frame(Frame),
    /// The image on the system clipboard.
    Clipboard,
}

/// Developer/test switches that never appear in `--help`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevOptions {
    /// Close the window after this many painted frames (smoke tests).
    pub exit_after_frames: Option<u32>,
    /// Run a scripted workload and print frame-time statistics to stderr as JSON.
    pub bench: Option<BenchMode>,
    /// Do not read or write the persistent state file.
    pub ephemeral_state: bool,
}

/// Scripted workloads for [`DevOptions::bench`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchMode {
    /// Zoom in and out and pan around the loaded image.
    PanZoom,
    /// Draw 100 rectangles/arrows through the real pointer path.
    Objects,
}

/// A request to edit an image.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorRequest {
    /// What to edit.
    pub input: EditorInput,
    /// Where finishing the session writes the PNG. Its presence puts the editor in
    /// *workflow mode*: "Done" writes here and reports back instead of asking for a path.
    pub output: Option<PathBuf>,
    /// Hidden developer switches.
    pub dev: DevOptions,
}

impl EditorRequest {
    /// A request to edit `input` with defaults.
    pub fn new(input: EditorInput) -> Self {
        Self { input, output: None, dev: DevOptions::default() }
    }
}

/// How the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeAction {
    /// The image was saved to `path`.
    Save,
    /// The image was copied to the clipboard (and written to `path` when there is one).
    Copy,
    /// The image was written to `path` for the caller to upload.
    Upload,
    /// The user closed the editor without keeping anything.
    Cancel,
}

/// The result of an editing session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditorOutcome {
    /// What the user chose.
    pub action: OutcomeAction,
    /// The written image, when there is one.
    pub path: Option<PathBuf>,
}

impl EditorOutcome {
    /// A cancelled session.
    pub fn cancelled() -> Self {
        Self { action: OutcomeAction::Cancel, path: None }
    }

    /// Process exit code: 0 on success, [`Self::EXIT_CANCELLED`] when cancelled.
    pub fn exit_code(&self) -> i32 {
        if self.action == OutcomeAction::Cancel { Self::EXIT_CANCELLED } else { 0 }
    }

    /// Exit code used when the user cancels.
    pub const EXIT_CANCELLED: i32 = 3;

    /// The one-line JSON the CLI prints.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"action":"cancel","path":null}"#.into())
    }
}

/// Reasons the editor could not start or run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The input could not be read.
    #[error("cannot open {path}: {reason}")]
    Open {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The clipboard holds no image.
    #[error("the clipboard does not contain an image")]
    NoClipboardImage,
    /// The frame is unusable (zero-sized...).
    #[error("cannot edit this image: {0}")]
    BadImage(String),
    /// The window system failed.
    #[error("the editor window failed: {0}")]
    Window(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_json_and_exit_codes() {
        let o = EditorOutcome { action: OutcomeAction::Upload, path: Some(PathBuf::from("/tmp/a.png")) };
        assert_eq!(o.to_json(), r#"{"action":"upload","path":"/tmp/a.png"}"#);
        assert_eq!(o.exit_code(), 0);
        let c = EditorOutcome::cancelled();
        assert_eq!(c.to_json(), r#"{"action":"cancel","path":null}"#);
        assert_eq!(c.exit_code(), 3);
        for (a, s) in [
            (OutcomeAction::Save, "save"),
            (OutcomeAction::Copy, "copy"),
            (OutcomeAction::Upload, "upload"),
            (OutcomeAction::Cancel, "cancel"),
        ] {
            assert!(EditorOutcome { action: a, path: None }.to_json().contains(s));
        }
    }

    #[test]
    fn errors_are_actionable() {
        let e = RunError::Open { path: PathBuf::from("/x.png"), reason: "no such file".into() };
        assert!(e.to_string().contains("/x.png") && e.to_string().contains("no such file"));
        assert!(RunError::NoClipboardImage.to_string().contains("clipboard"));
    }
}
