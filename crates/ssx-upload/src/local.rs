//! "Upload" to the local machine: exercise workflows without a network.
//!
//! With no destination directory nothing is written: the result URL is the source file's
//! `file://` URL (or `local://<name>` for in-memory data), which makes this a true no-op
//! stand-in for a real uploader in workflow tests. With a destination directory the payload
//! is copied there (streamed, with progress and cancellation) and the URL points at the copy,
//! or at `base_url/<name>` when the directory is served by a web server (a "shared folder"
//! destination, ShareX's `SharedFolder`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::body::CHUNK;
use crate::context::UploadContext;
use crate::error::UploadError;
use crate::types::{UploadKind, UploadRequest, UploadResult, UploadSource, Uploader};
use crate::util::url_encode;

/// The local/no-op uploader.
#[derive(Debug, Clone)]
pub struct LocalUploader {
    name: String,
    dest_dir: Option<PathBuf>,
    base_url: Option<String>,
}

impl Default for LocalUploader {
    fn default() -> Self {
        Self { name: "Local".to_owned(), dest_dir: None, base_url: None }
    }
}

impl LocalUploader {
    /// A pure no-op uploader.
    pub fn noop() -> Self {
        Self::default()
    }

    /// Copy uploads into `dir`.
    pub fn to_directory(dir: impl Into<PathBuf>) -> Self {
        Self { name: "Local folder".to_owned(), dest_dir: Some(dir.into()), base_url: None }
    }

    /// Report `base_url/<name>` instead of a `file://` URL.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    fn url_for(&self, file_name: &str, path: Option<&Path>) -> String {
        if let Some(base) = &self.base_url {
            return format!("{}/{}", base.trim_end_matches('/'), url_encode(file_name));
        }
        match path.and_then(|p| url::Url::from_file_path(p).ok()) {
            Some(u) => u.to_string(),
            None => format!("local://{}", url_encode(file_name)),
        }
    }
}

/// Keep only the last path component and neutralise anything that could escape the folder.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_owned();
    if cleaned.is_empty() { "upload.bin".to_owned() } else { cleaned }
}

/// `name`, or `name-1`, `name-2`... (before the extension) until unused.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..10_000)
        .map(|i| dir.join(format!("{stem}-{i}{ext}")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(format!("{stem}-{}{ext}", std::process::id())))
}

#[async_trait]
impl Uploader for LocalUploader {
    fn name(&self) -> &str {
        &self.name
    }

    fn supports(&self, _kind: UploadKind) -> bool {
        true
    }

    async fn upload(
        &self,
        req: &UploadRequest,
        ctx: &UploadContext,
    ) -> Result<UploadResult, UploadError> {
        ctx.check_cancelled()?;
        let file_name = safe_name(&req.resolved_filename());
        let total = req.payload_len().await?;
        let (url, dest) = match &self.dest_dir {
            None => {
                let source_path = match &req.source {
                    UploadSource::Path(p) => std::fs::canonicalize(p).ok(),
                    UploadSource::Bytes { .. } => None,
                };
                ctx.progress.report(total, Some(total));
                (self.url_for(&file_name, source_path.as_deref()), None)
            }
            Some(dir) => {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| UploadError::io(format!("creating {}", dir.display()), e))?;
                let target = unique_path(dir, &file_name);
                let final_name = target
                    .file_name()
                    .map_or_else(|| file_name.clone(), |n| n.to_string_lossy().into_owned());
                copy_payload(req, &target, total, ctx).await?;
                let abs = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                (self.url_for(&final_name, Some(&abs)), Some(target))
            }
        };
        let mut extra = std::collections::BTreeMap::new();
        if let Some(d) = dest {
            extra.insert("path".to_owned(), d.display().to_string());
        }
        Ok(UploadResult {
            url,
            thumbnail_url: None,
            deletion_url: None,
            raw_response: String::new(),
            uploader_name: self.name.clone(),
            extra,
        })
    }
}

async fn copy_payload(
    req: &UploadRequest,
    target: &Path,
    total: u64,
    ctx: &UploadContext,
) -> Result<(), UploadError> {
    let ioerr = |what: &str, e| UploadError::io(format!("{what} {}", target.display()), e);
    let mut out = tokio::fs::File::create(target).await.map_err(|e| ioerr("creating", e))?;
    let mut sent = 0u64;
    let result: Result<(), UploadError> = async {
        match &req.source {
            UploadSource::Bytes { data, .. } => {
                for chunk in data.chunks(CHUNK) {
                    ctx.check_cancelled()?;
                    out.write_all(chunk).await.map_err(|e| ioerr("writing", e))?;
                    sent += chunk.len() as u64;
                    ctx.progress.report(sent, Some(total));
                }
            }
            UploadSource::Path(p) => {
                let mut src = tokio::fs::File::open(p)
                    .await
                    .map_err(|e| UploadError::io(format!("opening {}", p.display()), e))?;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    ctx.check_cancelled()?;
                    let n = src
                        .read(&mut buf)
                        .await
                        .map_err(|e| UploadError::io(format!("reading {}", p.display()), e))?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n]).await.map_err(|e| ioerr("writing", e))?;
                    sent += n as u64;
                    ctx.progress.report(sent, Some(total));
                }
            }
        }
        out.flush().await.map_err(|e| ioerr("flushing", e))?;
        ctx.progress.report(sent, Some(total.max(sent)));
        Ok(())
    }
    .await;
    if result.is_err() {
        // Do not leave a truncated copy behind.
        drop(out);
        let _ = tokio::fs::remove_file(target).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitised() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("a\\b\\c.png"), "c.png");
        assert_eq!(safe_name("we:ird*?.png"), "we_ird__.png");
        assert_eq!(safe_name(".."), "upload.bin");
        assert_eq!(safe_name(""), "upload.bin");
        assert_eq!(safe_name("ünï.png"), "ünï.png");
    }

    #[test]
    fn unique_names() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(d.path(), "a.png"), d.path().join("a.png"));
        std::fs::write(d.path().join("a.png"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.png"), d.path().join("a-1.png"));
        std::fs::write(d.path().join("a-1.png"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.png"), d.path().join("a-2.png"));
        std::fs::write(d.path().join("noext"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "noext"), d.path().join("noext-1"));
    }
}
