//! What a run reports: per-step results, per-item results and the overall outcome.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::history::EntryKind;

/// One kind of step the engine can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepKind {
    /// Taking a screenshot.
    Capture,
    /// Recording the screen.
    Record,
    /// Reading the clipboard as input.
    ReadClipboard,
    /// Checking / preparing an input file (decoding, folder handling).
    LoadFile,
    /// Zipping a folder.
    Zip,
    /// `open_editor`.
    OpenEditor,
    /// `copy_image_to_clipboard`.
    CopyImage,
    /// `save_to_file`.
    SaveToFile,
    /// `save_as_dialog`.
    SaveAsDialog,
    /// `pin_to_screen`.
    PinToScreen,
    /// `ocr`.
    Ocr,
    /// `upload`.
    Upload,
    /// `delete_local_file`.
    DeleteLocalFile,
    /// `copy_url`.
    CopyUrl,
    /// `copy_short_url`.
    CopyShortUrl,
    /// `open_url`.
    OpenUrl,
    /// `shorten_url`.
    ShortenUrl,
    /// `show_qr_code`.
    ShowQrCode,
    /// `show_notification`.
    ShowNotification,
    /// `run_command`.
    RunCommand,
    /// Writing the history entry.
    RecordHistory,
}

/// How much a failure of a step matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    /// Without it there is nothing to work on: the item (or run) stops.
    Critical,
    /// A failure is recorded and later steps still run, but the item is at best a
    /// [`Outcome::PartialSuccess`].
    Normal,
    /// A failure is recorded as a warning and never affects the outcome or later steps
    /// (clipboard, notifications, browser, history, …).
    Optional,
}

impl StepKind {
    /// Stable snake_case name (matches the settings names where one exists).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Record => "record",
            Self::ReadClipboard => "read_clipboard",
            Self::LoadFile => "load_file",
            Self::Zip => "zip",
            Self::OpenEditor => "open_editor",
            Self::CopyImage => "copy_image_to_clipboard",
            Self::SaveToFile => "save_to_file",
            Self::SaveAsDialog => "save_as_dialog",
            Self::PinToScreen => "pin_to_screen",
            Self::Ocr => "ocr",
            Self::Upload => "upload",
            Self::DeleteLocalFile => "delete_local_file",
            Self::CopyUrl => "copy_url",
            Self::CopyShortUrl => "copy_short_url",
            Self::OpenUrl => "open_url",
            Self::ShortenUrl => "shorten_url",
            Self::ShowQrCode => "show_qr_code",
            Self::ShowNotification => "show_notification",
            Self::RunCommand => "run_command",
            Self::RecordHistory => "record_history",
        }
    }

    /// How much a failure of this step matters.
    pub const fn importance(self) -> Importance {
        match self {
            Self::Capture
            | Self::Record
            | Self::ReadClipboard
            | Self::LoadFile
            | Self::Zip
            | Self::OpenEditor => Importance::Critical,
            Self::SaveToFile
            | Self::SaveAsDialog
            | Self::Upload
            | Self::DeleteLocalFile
            | Self::ShortenUrl
            | Self::RunCommand => Importance::Normal,
            Self::CopyImage
            | Self::PinToScreen
            | Self::Ocr
            | Self::CopyUrl
            | Self::CopyShortUrl
            | Self::OpenUrl
            | Self::ShowQrCode
            | Self::ShowNotification
            | Self::RecordHistory => Importance::Optional,
        }
    }
}

impl std::fmt::Display for StepKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Category of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The platform cannot do it.
    Unsupported,
    /// Missing configuration (no uploader chosen, no program set, …).
    NotConfigured,
    /// File-system trouble.
    Io,
    /// A service reported an error (upload rejected, editor crashed, …).
    Service,
    /// The workflow itself is wrong (bad template variable, wrong input type, …).
    Invalid,
    /// A service panicked (a bug in the service; the run continued).
    Internal,
}

/// A step failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    /// Category.
    pub kind: FailureKind,
    /// User-facing description.
    pub message: String,
    /// Whether retrying may help (from [`ServiceError::Failed`](super::ServiceError)).
    pub retryable: bool,
}

/// Why a step did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The run was cancelled before this step.
    Cancelled,
    /// An earlier critical step failed for this item.
    ItemFailed,
    /// The step does not apply here (with an explanation).
    NotApplicable(String),
    /// There is no URL yet (no upload, or it failed).
    NoUrl,
    /// There is no image to work on.
    NoImage,
    /// There is no local file.
    NoLocalFile,
    /// The upload was not confirmed, so deleting the file would lose data.
    UploadNotConfirmed,
    /// The file was not created by this workflow; ssx never deletes user files.
    NotCreatedByWorkflow,
    /// The user declined (closed the save dialog).
    UserDeclined,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::ItemFailed => f.write_str("an earlier step failed"),
            Self::NotApplicable(why) => write!(f, "not applicable: {why}"),
            Self::NoUrl => f.write_str("no URL (no successful upload)"),
            Self::NoImage => f.write_str("no image"),
            Self::NoLocalFile => f.write_str("no local file"),
            Self::UploadNotConfirmed => f.write_str("upload not confirmed"),
            Self::NotCreatedByWorkflow => f.write_str("file was not created by this workflow"),
            Self::UserDeclined => f.write_str("declined by the user"),
        }
    }
}

/// Result of a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// Done.
    Succeeded,
    /// Tried and failed.
    Failed(StepFailure),
    /// Not executed.
    Skipped(SkipReason),
    /// Interrupted by cancellation while running.
    Cancelled,
}

impl StepStatus {
    /// `true` for [`StepStatus::Succeeded`].
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
    /// `true` for [`StepStatus::Failed`].
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One executed (or skipped) step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// Which step.
    pub kind: StepKind,
    /// Item index (`None` for run-level steps such as capture or a joined clipboard copy).
    pub item: Option<usize>,
    /// Result.
    pub status: StepStatus,
    /// Short human-readable detail ("saved to /x/y.png", "uploaded via imgur").
    pub detail: Option<String>,
    /// How long it took (zero for skipped steps).
    pub duration: Duration,
}

/// Overall result of a run or of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything that was asked for worked (optional steps may have warned).
    Success,
    /// Something useful was achieved (file saved, uploaded) but a non-optional step failed.
    PartialSuccess,
    /// Nothing useful was achieved.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

/// What happened to one input item (one screenshot, one file, one recording).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemReport {
    /// Index in input order.
    pub index: usize,
    /// Kind recorded in history.
    pub kind: EntryKind,
    /// The path the caller supplied (`post_file`), if any.
    pub input_path: Option<PathBuf>,
    /// The local file that exists for this item at the end (saved screenshot, recording, the
    /// user's own file). `None` if there is none or it was deleted after a confirmed upload.
    pub local_path: Option<PathBuf>,
    /// `true` if [`local_path`](Self::local_path) was created by this workflow.
    pub created_by_workflow: bool,
    /// Public URL of the upload.
    pub url: Option<String>,
    /// Shortened URL, if `shorten_url` ran.
    pub short_url: Option<String>,
    /// Thumbnail URL from the uploader.
    pub thumbnail_url: Option<String>,
    /// Deletion URL from the uploader.
    pub deletion_url: Option<String>,
    /// Uploader that handled it.
    pub uploader: Option<String>,
    /// History row, if recorded.
    pub history_id: Option<i64>,
    /// This item's steps in execution order.
    pub steps: Vec<StepReport>,
    /// This item's outcome.
    pub outcome: Outcome,
}

impl ItemReport {
    /// The first failed step, if any.
    pub fn first_failure(&self) -> Option<&StepReport> {
        self.steps.iter().find(|s| s.status.is_failure())
    }
}

/// The complete result of [`Engine::run`](super::Engine::run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Id of the workflow that ran.
    pub workflow_id: String,
    /// Overall outcome.
    pub outcome: Outcome,
    /// Per-item results in input order (empty if the input could not be acquired).
    pub items: Vec<ItemReport>,
    /// Run-level steps in execution order (acquiring the input, joined clipboard copy,
    /// summary notification, …).
    pub steps: Vec<StepReport>,
}

impl RunReport {
    /// URLs of all successful uploads, in input order (the short URL where one exists).
    pub fn urls(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|i| i.short_url.as_deref().or(i.url.as_deref()))
            .collect()
    }

    /// Every step, run-level first, then each item's.
    pub fn all_steps(&self) -> impl Iterator<Item = &StepReport> {
        self.steps.iter().chain(self.items.iter().flat_map(|i| i.steps.iter()))
    }

    /// Failures of *optional* steps: things worth a warning that did not affect the outcome.
    pub fn warnings(&self) -> Vec<&StepReport> {
        self.all_steps()
            .filter(|s| s.status.is_failure() && s.kind.importance() == Importance::Optional)
            .collect()
    }

    /// Failures of critical and normal steps.
    pub fn errors(&self) -> Vec<&StepReport> {
        self.all_steps()
            .filter(|s| s.status.is_failure() && s.kind.importance() != Importance::Optional)
            .collect()
    }

    /// A one-paragraph, user-facing summary ("Uploaded 2 of 3 files; 1 failed: …").
    pub fn summary(&self) -> String {
        let total = self.items.len();
        let uploaded = self.items.iter().filter(|i| i.url.is_some()).count();
        let mut s = match self.outcome {
            Outcome::Cancelled => "Cancelled".to_owned(),
            Outcome::Success if total == 1 => "Done".to_owned(),
            Outcome::Success => format!("Done: {total} items"),
            Outcome::PartialSuccess | Outcome::Failed if total == 0 => "Failed".to_owned(),
            Outcome::PartialSuccess | Outcome::Failed => {
                format!("{uploaded} of {total} uploaded")
            }
        };
        let errors = self.errors();
        if !errors.is_empty() {
            let details: Vec<String> = errors
                .iter()
                .filter_map(|e| match &e.status {
                    StepStatus::Failed(f) => Some(format!("{}: {}", e.kind, f.message)),
                    _ => None,
                })
                .collect();
            s.push_str("; ");
            s.push_str(&details.join("; "));
        }
        s
    }
}
