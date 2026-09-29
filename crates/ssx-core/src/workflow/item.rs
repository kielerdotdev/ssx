//! Per-item state while a run is in flight.

use std::path::{Path, PathBuf};

use ssx_types::Frame;

use super::{
    Outcome, RecordedVideo, SkipReason, StepReport, UploadOutcome,
    report::{ItemReport, Importance, StepStatus},
};
use crate::{history::EntryKind, settings::DestinationType};

/// Extensions treated as images.
pub(super) const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif"];
/// Extensions treated as video (routed to the video destination).
pub(super) const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

pub(super) fn extension_of(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase())
}

pub(super) fn is_image_ext(ext: Option<&str>) -> bool {
    ext.is_some_and(|e| IMAGE_EXTS.contains(&e))
}

pub(super) fn is_video_ext(ext: Option<&str>) -> bool {
    ext.is_some_and(|e| VIDEO_EXTS.contains(&e))
}

/// MIME type for an extension.
pub(super) fn mime_for(ext: Option<&str>) -> &'static str {
    match ext.unwrap_or_default() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "txt" => "text/plain",
        "zip" => "application/zip",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Where an item's content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    /// A screenshot (or an image supplied by the caller / clipboard).
    Image,
    /// A recording made by this run or handed in as "just recorded".
    Recording,
    /// A file the user supplied. Never deleted by ssx.
    UserFile,
    /// A folder the user supplied, zipped into an ephemeral archive.
    Folder,
    /// Text (clipboard text).
    Text,
}

/// An encoded image.
#[derive(Debug, Clone)]
pub(super) struct Encoded {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
}

#[derive(Debug)]
pub(super) struct Item {
    pub index: usize,
    pub origin: Origin,
    pub kind: EntryKind,
    pub input_path: Option<PathBuf>,
    /// The file on disk that represents the item right now.
    pub local_path: Option<PathBuf>,
    /// `local_path` was created by this workflow (so it may be deleted after upload).
    pub created: bool,
    /// A temporary archive to remove when processing ends.
    pub ephemeral: Option<PathBuf>,
    pub frame: Option<Frame>,
    pub text: Option<String>,
    pub encoded: Option<Encoded>,
    /// `local_path` holds exactly the current `frame`/`text` (so uploads may stream it).
    pub file_matches_content: bool,
    pub edited: bool,
    pub window_title: Option<String>,
    pub process_name: Option<String>,
    pub video: Option<RecordedVideo>,
    pub upload: Option<UploadOutcome>,
    pub uploader: Option<String>,
    pub confirmed: bool,
    /// The URL later steps operate on (the short URL after `shorten_url`).
    pub url: Option<String>,
    pub short_url: Option<String>,
    pub steps: Vec<StepReport>,
    /// Why the remaining steps are not running.
    pub halted: Option<SkipReason>,
    pub cancelled: bool,
    pub history_id: Option<i64>,
    /// The name used to present the item to an uploader when there is no file.
    pub display_name: Option<String>,
}

impl Item {
    pub fn new(index: usize, origin: Origin, kind: EntryKind) -> Self {
        Self {
            index,
            origin,
            kind,
            input_path: None,
            local_path: None,
            created: false,
            ephemeral: None,
            frame: None,
            text: None,
            encoded: None,
            file_matches_content: false,
            edited: false,
            window_title: None,
            process_name: None,
            video: None,
            upload: None,
            uploader: None,
            confirmed: false,
            url: None,
            short_url: None,
            steps: Vec::new(),
            halted: None,
            cancelled: false,
            history_id: None,
            display_name: None,
        }
    }

    pub fn push(&mut self, report: StepReport) {
        match report.status {
            StepStatus::Cancelled => {
                self.cancelled = true;
                self.halted.get_or_insert(SkipReason::Cancelled);
            }
            StepStatus::Failed(_) if report.kind.importance() == Importance::Critical => {
                self.halted.get_or_insert(SkipReason::ItemFailed);
            }
            _ => {}
        }
        self.steps.push(report);
    }

    pub fn is_image(&self) -> bool {
        self.frame.is_some()
            || (self.origin == Origin::UserFile
                && is_image_ext(self.input_path.as_deref().and_then(extension_of).as_deref()))
    }

    pub fn dimensions(&self) -> (Option<u32>, Option<u32>) {
        if let Some(f) = &self.frame {
            return (Some(f.width()), Some(f.height()));
        }
        match &self.video {
            Some(v) => (v.width, v.height),
            None => (None, None),
        }
    }

    /// The path templates and notifications should call "the file".
    pub fn best_path(&self) -> Option<&Path> {
        self.local_path.as_deref().or(self.input_path.as_deref())
    }

    /// Destination type used for uploading this item.
    pub fn destination_type(&self) -> DestinationType {
        let ext = self
            .local_path
            .as_deref()
            .or(self.input_path.as_deref())
            .and_then(extension_of);
        match self.origin {
            Origin::Text => DestinationType::Text,
            Origin::Image => DestinationType::Image,
            Origin::Recording => {
                if ext.as_deref() == Some("gif") {
                    DestinationType::Image
                } else {
                    DestinationType::Video
                }
            }
            Origin::UserFile if self.edited => DestinationType::Image,
            Origin::UserFile | Origin::Folder => {
                if is_video_ext(ext.as_deref()) {
                    DestinationType::Video
                } else {
                    DestinationType::File
                }
            }
        }
    }

    /// History kind.
    pub fn entry_kind(&self) -> EntryKind {
        self.kind
    }

    pub fn item_outcome(&self) -> Outcome {
        if self.cancelled {
            return Outcome::Cancelled;
        }
        let failed = |imp: Importance| {
            self.steps.iter().any(|s| s.status.is_failure() && s.kind.importance() == imp)
        };
        if failed(Importance::Critical) {
            return Outcome::Failed;
        }
        if failed(Importance::Normal) {
            let achieved = self.confirmed || (self.created && self.local_path.is_some());
            return if achieved { Outcome::PartialSuccess } else { Outcome::Failed };
        }
        Outcome::Success
    }

    pub fn into_report(self) -> ItemReport {
        let outcome = self.item_outcome();
        ItemReport {
            index: self.index,
            kind: self.kind,
            input_path: self.input_path,
            created_by_workflow: self.created && self.local_path.is_some(),
            local_path: self.local_path,
            url: self.upload.as_ref().map(|u| u.url.clone()),
            short_url: self.short_url,
            thumbnail_url: self.upload.as_ref().and_then(|u| u.thumbnail_url.clone()),
            deletion_url: self.upload.as_ref().and_then(|u| u.deletion_url.clone()),
            uploader: self.uploader,
            history_id: self.history_id,
            steps: self.steps,
            outcome,
        }
    }
}
