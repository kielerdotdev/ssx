//! Data types of the history database.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What an entry represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A screenshot or other image.
    Image,
    /// A screen recording.
    Video,
    /// Any other file.
    File,
    /// A text snippet (see [`NewEntry::note`]).
    Text,
    /// A shortened or shared URL with no local file.
    Url,
}

impl EntryKind {
    /// The value stored in the database.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::File => "file",
            Self::Text => "text",
            Self::Url => "url",
        }
    }

    /// Inverse of [`as_str`](Self::as_str).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "image" => Self::Image,
            "video" => Self::Video,
            "file" => Self::File,
            "text" => Self::Text,
            "url" => Self::Url,
            _ => return None,
        })
    }
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// An entry to be inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    /// Creation time, Unix milliseconds. [`NewEntry::new`] sets it to now.
    pub created_at: i64,
    /// Kind.
    pub kind: EntryKind,
    /// The saved file, if any.
    pub local_path: Option<PathBuf>,
    /// PNG thumbnail (see [`crate::history::thumbnail_png`]); at most 1 MiB.
    pub thumbnail: Option<Vec<u8>>,
    /// Public URL of the upload.
    pub upload_url: Option<String>,
    /// Thumbnail URL of the upload.
    pub thumbnail_url: Option<String>,
    /// URL that deletes the upload, if the provider offers one.
    pub deletion_url: Option<String>,
    /// Name of the uploader used.
    pub uploader: Option<String>,
    /// Title of the captured window.
    pub window_title: Option<String>,
    /// Process of the captured window.
    pub process_name: Option<String>,
    /// Pixel width (images/videos).
    pub width: Option<u32>,
    /// Pixel height.
    pub height: Option<u32>,
    /// Size of the file in bytes.
    pub size_bytes: Option<u64>,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: Option<String>,
    /// Id of the workflow that produced the entry.
    pub workflow_id: Option<String>,
    /// Free text (text snippets, OCR output), truncated to 4 KiB.
    pub note: Option<String>,
}

impl NewEntry {
    /// An entry of `kind` created now, with everything else unset.
    pub fn new(kind: EntryKind) -> Self {
        Self {
            created_at: now_ms(),
            kind,
            local_path: None,
            thumbnail: None,
            upload_url: None,
            thumbnail_url: None,
            deletion_url: None,
            uploader: None,
            window_title: None,
            process_name: None,
            width: None,
            height: None,
            size_bytes: None,
            sha256: None,
            workflow_id: None,
            note: None,
        }
    }
}

/// A stored entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Database id (monotonically increasing, never reused).
    pub id: i64,
    /// Creation time, Unix milliseconds.
    pub created_at: i64,
    /// Kind.
    pub kind: EntryKind,
    /// The saved file, if any.
    pub local_path: Option<PathBuf>,
    /// PNG thumbnail; `None` if absent or not requested ([`Query::thumbnails`]).
    #[serde(skip)]
    pub thumbnail: Option<Vec<u8>>,
    /// Public URL.
    pub upload_url: Option<String>,
    /// Thumbnail URL.
    pub thumbnail_url: Option<String>,
    /// Deletion URL.
    pub deletion_url: Option<String>,
    /// Uploader name.
    pub uploader: Option<String>,
    /// Window title.
    pub window_title: Option<String>,
    /// Process name.
    pub process_name: Option<String>,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
    /// File size in bytes.
    pub size_bytes: Option<u64>,
    /// SHA-256 hex.
    pub sha256: Option<String>,
    /// Workflow id.
    pub workflow_id: Option<String>,
    /// Free text.
    pub note: Option<String>,
}

/// Upload result to attach to an existing entry (a re-upload from the history window).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadInfo {
    /// Public URL.
    pub url: Option<String>,
    /// Thumbnail URL.
    pub thumbnail_url: Option<String>,
    /// Deletion URL.
    pub deletion_url: Option<String>,
    /// Uploader name.
    pub uploader: Option<String>,
}

/// Search filter for [`History::list`](super::History::list) and `count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Free text; every whitespace-separated term must match (see the module docs for the
    /// FTS5 / `LIKE` semantics). `None` or blank = no text filter.
    pub text: Option<String>,
    /// Only these kinds; empty = all.
    pub kinds: Vec<EntryKind>,
    /// Only entries created at or after this Unix-ms instant.
    pub since: Option<i64>,
    /// Only entries created strictly before this Unix-ms instant.
    pub until: Option<i64>,
    /// Only entries that have an upload URL.
    pub uploaded_only: bool,
    /// Page size (clamped to 1..=1000).
    pub limit: usize,
    /// Rows to skip.
    pub offset: usize,
    /// Load thumbnails (skip for cheap listings).
    pub thumbnails: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            text: None,
            kinds: Vec::new(),
            since: None,
            until: None,
            uploaded_only: false,
            limit: 50,
            offset: 0,
            thumbnails: true,
        }
    }
}

/// What [`History::prune`](super::History::prune) should remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Keep only the newest this many entries.
    pub max_entries: Option<u32>,
    /// Remove entries older than this.
    pub max_age: Option<std::time::Duration>,
}

/// An entry whose file no longer exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    /// Entry id.
    pub id: i64,
    /// The missing path.
    pub path: PathBuf,
}

/// Hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Streaming hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    sha256_reader(&mut std::fs::File::open(path)?)
}

/// Streaming hex SHA-256 of everything `reader` yields.
pub fn sha256_reader(reader: &mut dyn Read) -> std::io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_round_trip() {
        for k in
            [EntryKind::Image, EntryKind::Video, EntryKind::File, EntryKind::Text, EntryKind::Url]
        {
            assert_eq!(EntryKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EntryKind::parse("bogus"), None);
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_matches_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert_eq!(sha256_file(&p).unwrap(), sha256_hex(&data));
        assert!(sha256_file(&tmp.path().join("missing")).is_err());
    }
}
