//! What a right-click entry does: [`Action`] and the file [`Filter`] deciding where it shows.
//!
//! Every entry runs the ssx CLI as `ssx <exec_args...> -- <paths...>`. The `--` is added by
//! the generators (never by `exec_args`) so a file called `-rf` can never become a flag.

use crate::error::{Result, ShellError};

/// Extensions treated as images by the built-in image filter (lower case, no dot).
pub const IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "jpe", "gif", "webp", "bmp", "tif", "tiff", "avif", "heic", "heif"];
/// MIME types treated as images by the built-in image filter.
pub const IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
    "image/avif",
    "image/heif",
    "image/heic",
];
/// Extensions treated as video by the built-in video filter.
pub const VIDEO_EXTENSIONS: &[&str] =
    &["mp4", "mkv", "webm", "mov", "avi", "wmv", "flv", "m4v", "ogv", "mpg", "mpeg"];
/// MIME types treated as video by the built-in video filter.
pub const VIDEO_MIME_TYPES: &[&str] = &[
    "video/mp4",
    "video/x-matroska",
    "video/webm",
    "video/quicktime",
    "video/x-msvideo",
    "video/x-ms-wmv",
    "video/x-flv",
    "video/x-m4v",
    "video/ogg",
    "video/mpeg",
];

/// Coarse class of files an action applies to. File managers with a "kind" vocabulary
/// (Thunar's `<image-files/>`, macOS UTIs, Windows perceived types) map this directly, which
/// is more robust than enumerating extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterKind {
    /// Any file (and, if [`Filter::directories`], folders).
    Any,
    /// Raster images.
    Images,
    /// Video files.
    Videos,
    /// Only what `mime_types` / `extensions` list.
    Custom,
}

/// Which selections an action is offered for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Coarse class; see [`FilterKind`].
    pub kind: FilterKind,
    /// Concrete MIME types (`type/subtype`, no wildcards). Empty means "not restricted by MIME".
    pub mime_types: Vec<String>,
    /// Extensions without the dot, lower case. Empty means "not restricted by extension".
    pub extensions: Vec<String>,
    /// Whether folders qualify.
    pub directories: bool,
}

impl Filter {
    /// Everything, including folders.
    pub fn any() -> Self {
        Self {
            kind: FilterKind::Any,
            mime_types: Vec::new(),
            extensions: Vec::new(),
            directories: true,
        }
    }

    /// Raster images.
    pub fn images() -> Self {
        Self {
            kind: FilterKind::Images,
            mime_types: IMAGE_MIME_TYPES.iter().map(|s| (*s).to_owned()).collect(),
            extensions: IMAGE_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect(),
            directories: false,
        }
    }

    /// Video files.
    pub fn videos() -> Self {
        Self {
            kind: FilterKind::Videos,
            mime_types: VIDEO_MIME_TYPES.iter().map(|s| (*s).to_owned()).collect(),
            extensions: VIDEO_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect(),
            directories: false,
        }
    }

    /// A custom list of MIME types and/or extensions.
    pub fn custom(mime_types: &[&str], extensions: &[&str], directories: bool) -> Self {
        Self {
            kind: FilterKind::Custom,
            mime_types: mime_types.iter().map(|s| (*s).to_owned()).collect(),
            extensions: extensions.iter().map(|s| s.to_ascii_lowercase()).collect(),
            directories,
        }
    }

    /// Whether the filter accepts every file.
    pub fn is_any(&self) -> bool {
        self.kind == FilterKind::Any
    }

    /// Whether a file with this name matches (case-insensitive extension test; the
    /// extension list is authoritative, MIME is only used when no extensions are set).
    pub fn matches_name(&self, name: &str) -> bool {
        if self.is_any() || self.extensions.is_empty() {
            return true;
        }
        name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && self.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
        })
    }

    fn validate(&self, id: &str) -> Result<()> {
        let bad = |reason: String| ShellError::InvalidAction { id: id.to_owned(), reason };
        for e in &self.extensions {
            if e.is_empty() || !e.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(bad(format!("extension {e:?} must be ASCII alphanumeric")));
            }
        }
        for m in &self.mime_types {
            let ok = m.split_once('/').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && m.bytes().all(|c| c.is_ascii_alphanumeric() || b"/+-._".contains(&c))
            });
            if !ok {
                return Err(bad(format!("mime type {m:?} is not of the form type/subtype")));
            }
        }
        Ok(())
    }
}

/// One right-click entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Stable identifier (`[a-z0-9-]`, at most 32 chars). Used in file names and registry keys.
    pub id: String,
    /// Menu text.
    pub label: String,
    /// Tooltip / `Comment=` text.
    pub description: String,
    /// Freedesktop icon name for Linux menus.
    pub icon: String,
    /// CLI arguments before the paths, e.g. `["post-file"]`. Must not contain `--`.
    pub exec_args: Vec<String>,
    /// Selections this entry is offered for.
    pub filter: Filter,
    /// Whether all selected files go to one invocation (otherwise one file per invocation,
    /// and the entry is offered for single selections where the file manager can express it).
    pub multi_select: bool,
}

impl Action {
    /// "Upload with ssx": `ssx post-file -- <paths...>`.
    pub fn upload() -> Self {
        Self {
            id: "upload".into(),
            label: "Upload with ssx".into(),
            description: "Upload the selected files with ssx".into(),
            icon: "document-send".into(),
            exec_args: vec!["post-file".into()],
            filter: Filter::any(),
            multi_select: true,
        }
    }

    /// "Edit image with ssx": `ssx edit -- <image>`.
    pub fn edit() -> Self {
        Self {
            id: "edit".into(),
            label: "Edit image with ssx".into(),
            description: "Open the selected image in the ssx editor".into(),
            icon: "applications-graphics".into(),
            exec_args: vec!["edit".into()],
            filter: Filter::images(),
            multi_select: false,
        }
    }

    /// "Upload video with ssx": `ssx post-video -- <file>`.
    pub fn upload_video() -> Self {
        Self {
            id: "upload-video".into(),
            label: "Upload video with ssx".into(),
            description: "Upload the selected video with ssx".into(),
            icon: "video-x-generic".into(),
            exec_args: vec!["post-video".into()],
            filter: Filter::videos(),
            multi_select: false,
        }
    }

    /// The built-in entries, in menu order.
    pub fn defaults() -> Vec<Self> {
        vec![Self::upload(), Self::edit(), Self::upload_video()]
    }

    /// Checks that the action can be embedded in every target format.
    pub fn validate(&self) -> Result<()> {
        let bad = |reason: &str| ShellError::InvalidAction {
            id: self.id.clone(),
            reason: reason.to_owned(),
        };
        let id_ok = (1..=32).contains(&self.id.len())
            && self.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !self.id.starts_with('-');
        if !id_ok {
            return Err(bad("id must be 1-32 chars of [a-z0-9-] and not start with '-'"));
        }
        let text_ok = |s: &str| !s.chars().any(char::is_control);
        if self.label.trim().is_empty() || !text_ok(&self.label) || !text_ok(&self.description) {
            return Err(bad("label/description must be non-empty single-line text"));
        }
        let icon_ok = !self.icon.is_empty()
            && self.icon.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
        if !icon_ok {
            return Err(bad("icon must be a plain freedesktop icon name"));
        }
        if self.exec_args.is_empty() {
            return Err(bad("exec_args needs at least the CLI subcommand"));
        }
        for a in &self.exec_args {
            if a == "--" || a.chars().any(char::is_control) || a.contains('%') {
                return Err(bad(
                    "exec_args must not contain '--', control characters or '%' (the generators add '--')",
                ));
            }
        }
        self.filter.validate(&self.id)
    }

    /// `PascalCase` identifier for formats that need an alphanumeric name (`ssxUploadVideo`).
    pub fn camel_id(&self) -> String {
        let mut out = String::from("ssx");
        for part in self.id.split('-').filter(|p| !p.is_empty()) {
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        for a in Action::defaults() {
            a.validate().unwrap_or_else(|e| panic!("{}: {e}", a.id));
        }
    }

    #[test]
    fn rejects_bad_actions() {
        let mut a = Action::upload();
        a.id = "Up load".into();
        assert!(a.validate().is_err());
        let mut a = Action::upload();
        a.exec_args = vec!["post-file".into(), "--".into()];
        assert!(a.validate().is_err());
        let mut a = Action::upload();
        a.exec_args = vec!["x\ny".into()];
        assert!(a.validate().is_err());
        let mut a = Action::edit();
        a.filter.extensions.push("p ng".into());
        assert!(a.validate().is_err());
        let mut a = Action::upload();
        a.label = String::new();
        assert!(a.validate().is_err());
    }

    #[test]
    fn camel_ids() {
        assert_eq!(Action::upload().camel_id(), "ssxUpload");
        assert_eq!(Action::upload_video().camel_id(), "ssxUploadVideo");
    }

    #[test]
    fn name_matching() {
        let f = Filter::images();
        assert!(f.matches_name("a.PNG"));
        assert!(f.matches_name("weird name.tar.jpeg"));
        assert!(!f.matches_name("a.txt"));
        assert!(!f.matches_name("png"));
        assert!(!f.matches_name(".png"));
        assert!(Filter::any().matches_name("anything"));
    }
}
