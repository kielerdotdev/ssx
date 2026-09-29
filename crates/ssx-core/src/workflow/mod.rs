//! The workflow engine: ShareX's "post screenshot / post file / post video" pipeline.
//!
//! ```text
//!  Input ─▶ after-capture steps ─▶ upload ─▶ after-upload steps ─▶ history
//!  (capture,    (edit, copy, save,   (image / text /   (copy URL, open, shorten,
//!   record,      pin, OCR, upload,    file / video      QR, notify, run command)
//!   files)       delete local)        destination)
//! ```
//!
//! [`Engine::run`] executes a [`Workflow`](crate::settings::Workflow) synchronously against a
//! [`Services`] bundle, streaming [`Event`]s and returning a [`RunReport`]. The three typed
//! entry points ([`Engine::post_screenshot`], [`Engine::post_file`], [`Engine::post_video`])
//! only differ in how the input is acquired.
//!
//! # Semantics
//!
//! * **Ordered steps, each with its own result.** `after_capture` runs in the order written,
//!   then `after_upload`. Every step ends [`Succeeded`](StepStatus::Succeeded),
//!   [`Failed`](StepStatus::Failed), [`Skipped`](StepStatus::Skipped) (with a reason) or
//!   [`Cancelled`](StepStatus::Cancelled).
//! * **Importance.** [`StepKind::importance`] classifies steps. A failed *critical* step
//!   (getting the input, opening the editor) ends that item. A failed *normal* step (save,
//!   upload, shorten, run command, delete) is recorded, later steps still run, and the outcome
//!   degrades to [`Outcome::PartialSuccess`] if something useful was achieved. A failed
//!   *optional* step (clipboard, notification, browser, history) is only a warning.
//! * **A failed upload keeps the local file** and says precisely why; steps that need a URL
//!   are skipped with [`SkipReason::NoUrl`]; if the workflow has `show_notification` a failure
//!   notification (including where the file was kept) is sent instead.
//! * **`delete_local_file`** runs only after a *confirmed* upload (the uploader returned a
//!   non-empty URL), only for files this workflow created, never for files the caller passed
//!   in, and treats an already-missing file as success (idempotent).
//! * **Cancellation** is checked before every step and passed into every blocking service
//!   call. Remaining steps are reported as skipped-cancelled; the outcome is
//!   [`Outcome::Cancelled`]. Cancelling the editor cancels the item.
//! * **Multi-file posts** run items in parallel (limit: `post_file.max_parallel_uploads`),
//!   never let one failure stop the others, and report in input order. After-upload steps
//!   run once over the *successful* items, in the listed order: `copy_url` puts all URLs on
//!   the clipboard newline-joined, `show_notification` sends one summary, `run_command` and
//!   `open_url` run per item, and image-only steps (copy image, pin, OCR, QR) are skipped as
//!   ambiguous.
//! * **History** gets one entry per item that produced a file or a URL, also when cancelled or
//!   partially failed, and the retention policy is applied afterwards.
//! * **Panics** in a service are caught and reported as a failed step.

mod cancel;
mod engine;
mod error;
mod events;
mod item;
mod report;
mod services;
mod steps;
mod template;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;

pub use cancel::{CancelToken, Cancelled};
pub use engine::{Engine, Input, Naming, VideoSource};
pub use error::ServiceError;
pub use events::{CollectingSink, Event, EventSink, NullSink};
pub use report::{
    FailureKind, Importance, ItemReport, Outcome, RunReport, SkipReason, StepFailure, StepKind,
    StepReport, StepStatus,
};
pub use services::{
    CaptureRequest, CaptureTarget, Captured, Capturer, Clipboard, ClipboardContent, CommandOutput,
    CommandRunner, CommandSpec, EditResult, Editor, FileSystem, Notification, NotificationLevel,
    Notifier, Ocr, Pinner, ProcessCommandRunner, QrCodeRenderer, QrRenderer, RecordKind,
    RecordRequest, RecordedVideo, Recorder, RecordingSession, SaveDialog, Services, StdFileSystem,
    Unsupported, UploadOutcome, UploadProgress, UploadRequest, UploadSource, Uploaders, UrlOpener,
    UrlShortener, Zipper,
};
pub use template::{
    TemplateError, TemplateVars, expand as expand_template, expand_all as expand_templates,
};
