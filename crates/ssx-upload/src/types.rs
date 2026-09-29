//! Request/response value types and the [`Uploader`] trait.
//!
//! `Uploader` is object safe (via `async-trait`) so the workflow engine can hold
//! `Arc<dyn Uploader>` and wrap it with [`crate::RetryingUploader`] or
//! [`crate::ShorteningUploader`] without knowing the concrete type. File-ish sources are
//! described lazily (a path, not its contents) so multi-GB videos are streamed from disk.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;

use crate::context::UploadContext;
use crate::error::UploadError;

/// Largest text payload we will read into memory to use as `{input}`.
pub const MAX_TEXT_INPUT_BYTES: u64 = 16 * 1024 * 1024;

/// What kind of content is being uploaded. Mirrors ShareX's destination categories; video
/// is separate because ssx allows a video-specific destination override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadKind {
    /// A screenshot or other raster image.
    Image,
    /// Text (pastebin style). The payload is UTF-8.
    Text,
    /// Any other file.
    File,
    /// A screen recording.
    Video,
    /// A URL to shorten. The payload is the URL as UTF-8.
    Url,
}

impl fmt::Display for UploadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Image => "image",
            Self::Text => "text",
            Self::File => "file",
            Self::Video => "video",
            Self::Url => "URL",
        })
    }
}

/// Where the bytes to upload come from.
#[derive(Debug, Clone)]
pub enum UploadSource {
    /// A file on disk, streamed at upload time (never read into memory as a whole).
    Path(PathBuf),
    /// In-memory data (screenshots that were never saved, text, URLs).
    Bytes {
        /// The data. `Bytes` is reference counted, so retries do not copy.
        data: Bytes,
        /// File name presented to the server.
        filename: String,
        /// MIME type; guessed from `filename` when `None`.
        mime: Option<String>,
    },
}

impl UploadSource {
    /// Whether the body can be produced again for a retry. True for every current variant;
    /// future one-shot sources (a pipe) must return false so retries are skipped.
    pub fn is_replayable(&self) -> bool {
        true
    }

    /// The file name embedded in the source, if any.
    pub fn file_name(&self) -> Option<String> {
        match self {
            Self::Path(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
            Self::Bytes { filename, .. } => Some(filename.clone()),
        }
    }
}

/// A single upload job.
#[derive(Debug, Clone)]
pub struct UploadRequest {
    /// Payload.
    pub source: UploadSource,
    /// Content category; decides which destination/endpoint is used.
    pub kind: UploadKind,
    /// Overrides the file name derived from `source`.
    pub filename: Option<String>,
}

impl UploadRequest {
    /// Upload a file from disk as `kind`.
    pub fn from_path(path: impl Into<PathBuf>, kind: UploadKind) -> Self {
        Self { source: UploadSource::Path(path.into()), kind, filename: None }
    }

    /// Upload in-memory bytes as `kind` under `filename`.
    pub fn from_bytes(data: impl Into<Bytes>, filename: impl Into<String>, kind: UploadKind) -> Self {
        Self {
            source: UploadSource::Bytes { data: data.into(), filename: filename.into(), mime: None },
            kind,
            filename: None,
        }
    }

    /// Upload text (`text.txt` unless overridden).
    pub fn text(text: impl Into<String>) -> Self {
        Self::from_bytes(Bytes::from(text.into()), "text.txt", UploadKind::Text)
    }

    /// Shorten a URL.
    pub fn url(url: impl Into<String>) -> Self {
        Self::from_bytes(Bytes::from(url.into()), "url.txt", UploadKind::Url)
    }

    /// Set an explicit file name.
    #[must_use]
    pub fn with_filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Set an explicit MIME type (only meaningful for in-memory sources).
    #[must_use]
    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        if let UploadSource::Bytes { mime: m, .. } = &mut self.source {
            *m = Some(mime.into());
        }
        self
    }

    /// The file name presented to the server: override, then source, then `upload.bin`.
    pub fn resolved_filename(&self) -> String {
        self.filename
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| self.source.file_name())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "upload.bin".to_owned())
    }

    /// The MIME type for the payload: explicit, else guessed from the resolved file name.
    pub fn resolved_mime(&self) -> String {
        if let UploadSource::Bytes { mime: Some(m), .. } = &self.source {
            return m.clone();
        }
        mime_guess::from_path(self.resolved_filename())
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_owned()
    }

    /// Payload length in bytes.
    pub async fn payload_len(&self) -> Result<u64, UploadError> {
        match &self.source {
            UploadSource::Bytes { data, .. } => Ok(data.len() as u64),
            UploadSource::Path(p) => tokio::fs::metadata(p)
                .await
                .map(|m| m.len())
                .map_err(|e| UploadError::io(format!("reading metadata of {}", p.display()), e)),
        }
    }

    /// The payload as text, for `{input}`: only meaningful for [`UploadKind::Text`] and
    /// [`UploadKind::Url`] (empty for everything else, as in ShareX).
    pub async fn input_text(&self) -> Result<String, UploadError> {
        if !matches!(self.kind, UploadKind::Text | UploadKind::Url) {
            return Ok(String::new());
        }
        match &self.source {
            UploadSource::Bytes { data, .. } => Ok(String::from_utf8_lossy(data).into_owned()),
            UploadSource::Path(p) => {
                let len = self.payload_len().await?;
                if len > MAX_TEXT_INPUT_BYTES {
                    return Err(UploadError::config(format!(
                        "text input {} is {len} bytes, larger than the {MAX_TEXT_INPUT_BYTES} byte limit",
                        p.display()
                    )));
                }
                let data = tokio::fs::read(p)
                    .await
                    .map_err(|e| UploadError::io(format!("reading {}", p.display()), e))?;
                Ok(String::from_utf8_lossy(&data).into_owned())
            }
        }
    }
}

/// Successful upload outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadResult {
    /// Public URL of the uploaded item (or the shortened URL).
    pub url: String,
    /// Thumbnail URL, when the service provides one.
    pub thumbnail_url: Option<String>,
    /// URL (or endpoint) that deletes the item, when the service provides one.
    pub deletion_url: Option<String>,
    /// The server response, truncated to [`crate::RAW_RESPONSE_LIMIT`] bytes.
    pub raw_response: String,
    /// Which uploader produced this result.
    pub uploader_name: String,
    /// Uploader specific extras (Imgur `id`/`deletehash`, S3 `key`/`etag`, the URL before
    /// shortening as `original_url`...).
    pub extra: BTreeMap<String, String>,
}

/// An upload destination.
///
/// Implementations must be cheap to share (`Arc`), must honour `ctx.cancel` promptly, must
/// report progress through `ctx.progress`, and must stream file bodies.
#[async_trait]
pub trait Uploader: Send + Sync {
    /// Display name, also stored in [`UploadResult::uploader_name`].
    fn name(&self) -> &str;

    /// Whether this uploader can handle `kind`.
    fn supports(&self, kind: UploadKind) -> bool;

    /// Perform the upload.
    async fn upload(
        &self,
        req: &UploadRequest,
        ctx: &UploadContext,
    ) -> Result<UploadResult, UploadError>;
}

/// A URL shortening service, used for the "after upload: shorten URL" step.
#[async_trait]
pub trait UrlShortener: Send + Sync {
    /// Display name.
    fn name(&self) -> &str;

    /// Shorten `url`.
    async fn shorten(&self, url: &str, ctx: &UploadContext) -> Result<String, UploadError>;
}

#[async_trait]
impl<T: Uploader + ?Sized> Uploader for std::sync::Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn supports(&self, kind: UploadKind) -> bool {
        (**self).supports(kind)
    }

    async fn upload(
        &self,
        req: &UploadRequest,
        ctx: &UploadContext,
    ) -> Result<UploadResult, UploadError> {
        (**self).upload(req, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_resolution() {
        let r = UploadRequest::from_path("/tmp/a/b/shot.png", UploadKind::Image);
        assert_eq!(r.resolved_filename(), "shot.png");
        assert_eq!(r.resolved_mime(), "image/png");
        let r = r.with_filename("x.jpg");
        assert_eq!(r.resolved_filename(), "x.jpg");
        assert_eq!(r.resolved_mime(), "image/jpeg");
        let r = UploadRequest::from_bytes(vec![1u8], "", UploadKind::File);
        assert_eq!(r.resolved_filename(), "upload.bin");
        assert_eq!(r.resolved_mime(), "application/octet-stream");
        let r = UploadRequest::from_bytes(vec![1u8], "a.bin", UploadKind::File).with_mime("x/y");
        assert_eq!(r.resolved_mime(), "x/y");
    }

    #[tokio::test]
    async fn input_text_only_for_text_and_url() {
        assert_eq!(UploadRequest::text("héllo").input_text().await.unwrap(), "héllo");
        assert_eq!(UploadRequest::url("http://a").input_text().await.unwrap(), "http://a");
        let img = UploadRequest::from_bytes(vec![1u8], "a.png", UploadKind::Image);
        assert_eq!(img.input_text().await.unwrap(), "");
    }
}
