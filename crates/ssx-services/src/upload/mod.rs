//! The [`Uploaders`] and [`UrlShortener`] services on top of `ssx-upload`.
//!
//! [`UploadService`] holds a *registry* of named destinations, built once from
//!
//! 1. the built-ins: `local` (a no-op that reports `file://` URLs, handy for trying
//!    workflows) and the `is.gd`, `v.gd` and `tinyurl` shorteners, which need no settings;
//! 2. every `*.sxcu` file in `<config dir>/uploaders/` (the file stem is the name); and
//! 3. the `[uploaders.<name>]` tables of `settings.toml` (see [`config`]).
//!
//! Later sources win on name clashes, so a settings table can deliberately replace a file or
//! a built-in; clashes between two *files* cannot happen (file names are unique).
//!
//! A destination that fails to load (typo in a table, corrupt `.sxcu`) does **not** fail the
//! service: it stays in the registry as *broken* with the reason, is reported by
//! `ssx uploaders list` and `ssx doctor`, and fails only the uploads that use it, with that
//! reason as the message. Nothing here does network I/O until an upload runs; the HTTP client
//! (which loads the platform certificate store) is created lazily.
//!
//! Every uploader is wrapped in [`RetryingUploader`]. Calls are synchronous (that is what the
//! core traits are); see [`SharedRuntime::block_on_cancellable`] for how cancellation and
//! progress cross the sync/async boundary.

pub mod config;
pub mod sxcu_files;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use ssx_core::{
    settings::{DestinationType, Settings},
    workflow::{
        CancelToken, ServiceError, UploadOutcome, UploadProgress, UploadRequest, UploadSource,
        Uploaders, UrlShortener as CoreShortener,
    },
};
use ssx_upload::{
    ProgressSink, RetryPolicy, RetryingUploader, SecretStore, UploadContext, UploadError,
    UploadKind, UploadRequest as NetRequest, Uploader, UrlShortener as NetShortener,
    build_http_client, local::LocalUploader, shorten::HttpShortener,
};

use crate::runtime::SharedRuntime;

pub use config::Built;
pub use sxcu_files::{ImportError, Imported, import_sxcu, remove_sxcu, sxcu_dir};

/// Where a registry entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Compiled in.
    Builtin,
    /// A table in `settings.toml`.
    Settings,
    /// A `.sxcu` file in the uploaders folder.
    SxcuFile(PathBuf),
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin => f.write_str("built-in"),
            Self::Settings => f.write_str("settings.toml"),
            Self::SxcuFile(p) => write!(f, "{}", p.display()),
        }
    }
}

/// A destination's description for listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploaderInfo {
    /// Name used in settings, workflows and `--to`.
    pub name: String,
    /// `imgur`, `s3`, `http`, `local`, `sxcu`, `shortener`, or `broken`.
    pub kind: String,
    /// Where it was defined.
    pub origin: String,
    /// Content kinds it uploads (`image`, `text`, `file`, `video`).
    pub uploads: Vec<&'static str>,
    /// Whether it can shorten URLs.
    pub shortens: bool,
    /// Why it cannot be used, if it is broken.
    pub error: Option<String>,
}

struct Entry {
    origin: Origin,
    built: Result<Built, String>,
}

/// The upload and URL-shortener services. See the [module docs](self).
pub struct UploadService {
    entries: BTreeMap<String, Entry>,
    runtime: SharedRuntime,
    secrets: Arc<dyn SecretStore>,
    context: OnceLock<Result<UploadContext, String>>,
}

impl std::fmt::Debug for UploadService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadService")
            .field("destinations", &self.entries.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn wrap_retry(built: Built, policy: &RetryPolicy) -> Built {
    Built {
        kind: built.kind,
        uploader: built
            .uploader
            .map(|u| Arc::new(RetryingUploader::new(u, policy.clone())) as Arc<dyn Uploader>),
        shortener: built.shortener,
    }
}

fn builtin(name: &str) -> Option<Built> {
    match name {
        "local" => Some(Built {
            kind: "local",
            uploader: Some(Arc::new(LocalUploader::noop())),
            shortener: None,
        }),
        "is.gd" => Some(shortener("shortener", HttpShortener::is_gd(None))),
        "v.gd" => Some(shortener("shortener", HttpShortener::v_gd(None))),
        "tinyurl" => Some(shortener("shortener", HttpShortener::tiny_url(None))),
        _ => None,
    }
}

fn shortener(kind: &'static str, s: HttpShortener) -> Built {
    Built { kind, uploader: None, shortener: Some(Arc::new(s)) }
}

/// Names of the built-in destinations.
pub const BUILTIN_NAMES: [&str; 4] = ["local", "is.gd", "v.gd", "tinyurl"];

impl UploadService {
    /// Builds the registry from `settings` and the `.sxcu` files under `config_dir`.
    pub fn new(
        settings: &Settings,
        config_dir: &Path,
        secrets: Arc<dyn SecretStore>,
        policy: &RetryPolicy,
    ) -> Self {
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        for name in BUILTIN_NAMES {
            if let Some(b) = builtin(name) {
                entries.insert(
                    name.to_owned(),
                    Entry { origin: Origin::Builtin, built: Ok(wrap_retry(b, policy)) },
                );
            }
        }
        for (name, path, result) in sxcu_files::load_dir(config_dir) {
            entries.insert(
                name,
                Entry {
                    origin: Origin::SxcuFile(path),
                    built: result.map(|b| wrap_retry(b, policy)),
                },
            );
        }
        for (name, table) in &settings.uploaders {
            let built = config::build(name, table, config_dir).map(|b| wrap_retry(b, policy));
            entries.insert(name.clone(), Entry { origin: Origin::Settings, built });
        }
        Self { entries, runtime: SharedRuntime::new(), secrets, context: OnceLock::new() }
    }

    /// Uses `runtime` instead of a private one (embedders that already own one).
    #[must_use]
    pub fn with_runtime(mut self, runtime: SharedRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Every destination, sorted by name.
    pub fn list(&self) -> Vec<UploaderInfo> {
        self.entries
            .iter()
            .map(|(name, e)| {
                let (kind, uploads, shortens, error) = match &e.built {
                    Ok(b) => (
                        b.kind.to_owned(),
                        b.uploader.as_ref().map_or_else(Vec::new, |u| {
                            [
                                ("image", UploadKind::Image),
                                ("text", UploadKind::Text),
                                ("file", UploadKind::File),
                                ("video", UploadKind::Video),
                            ]
                            .into_iter()
                            .filter(|(_, k)| u.supports(*k))
                            .map(|(n, _)| n)
                            .collect()
                        }),
                        b.shortener.is_some(),
                        None,
                    ),
                    Err(msg) => ("broken".to_owned(), Vec::new(), false, Some(msg.clone())),
                };
                UploaderInfo {
                    name: name.clone(),
                    kind,
                    origin: e.origin.to_string(),
                    uploads,
                    shortens,
                    error,
                }
            })
            .collect()
    }

    /// Names of all destinations.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    fn unknown(&self, name: &str) -> ServiceError {
        ServiceError::NotConfigured(format!(
            "there is no uploader called {name:?}; available: {}. Define it under \
             [uploaders.{name}] in settings.toml or import a .sxcu with `ssx uploaders import`",
            self.names().join(", ")
        ))
    }

    fn built(&self, name: &str) -> Result<&Built, ServiceError> {
        match self.entries.get(name) {
            None => Err(self.unknown(name)),
            Some(Entry { built: Err(msg), .. }) => Err(ServiceError::NotConfigured(msg.clone())),
            Some(Entry { built: Ok(b), .. }) => Ok(b),
        }
    }

    /// The template context: HTTP client (created on first use) and secret store.
    fn context(&self) -> Result<UploadContext, ServiceError> {
        self.context
            .get_or_init(|| {
                build_http_client()
                    .map(|http| UploadContext::new(http).with_secrets(self.secrets.clone()))
                    .map_err(|e| format!("cannot set up HTTPS: {e}"))
            })
            .clone()
            .map_err(ServiceError::failed)
    }

    /// Uploads a tiny test payload to `name` (a 1x1 PNG, or a text note for text-only
    /// destinations) and returns the outcome. Shorteners shorten `https://example.com/`.
    pub fn test(&self, name: &str, cancel: &CancelToken) -> Result<UploadOutcome, ServiceError> {
        let built = self.built(name)?;
        if let Some(up) = &built.uploader {
            let png = test_png();
            let (kind, file_name, mime, data): (DestinationType, &str, &str, &[u8]) =
                if up.supports(UploadKind::Image) {
                    (DestinationType::Image, "ssx-test.png", "image/png", &png)
                } else if up.supports(UploadKind::Text) {
                    (DestinationType::Text, "ssx-test.txt", "text/plain", b"ssx test upload\n")
                } else {
                    (
                        DestinationType::File,
                        "ssx-test.bin",
                        "application/octet-stream",
                        b"ssx test upload\n",
                    )
                };
            let req = UploadRequest {
                destination: name,
                kind,
                file_name,
                mime,
                source: UploadSource::Bytes(data),
            };
            return self.upload(&req, &|_| {}, cancel);
        }
        let short = self.shorten(name, "https://example.com/", cancel)?;
        Ok(UploadOutcome::url(short))
    }
}

/// A valid 1x1 PNG for [`UploadService::test`].
fn test_png() -> Vec<u8> {
    ssx_types::Frame::from_rgba8(1, 1, vec![0x1e, 0x90, 0xff, 0xff])
        .ok()
        .and_then(|f| f.encode(ssx_types::EncodeOptions::default()).ok())
        .unwrap_or_default()
}

fn net_kind(kind: DestinationType) -> UploadKind {
    match kind {
        DestinationType::Image => UploadKind::Image,
        DestinationType::Text => UploadKind::Text,
        DestinationType::File => UploadKind::File,
        DestinationType::Video => UploadKind::Video,
        DestinationType::UrlShortener | DestinationType::UrlSharing => UploadKind::Url,
    }
}

/// Maps `ssx-upload` errors onto the engine's, keeping the retry hint and adding the fix.
pub fn map_upload_error(destination: &str, e: UploadError) -> ServiceError {
    match e {
        UploadError::Cancelled => ServiceError::Cancelled,
        UploadError::Config { message } => {
            ServiceError::NotConfigured(format!("uploader {destination:?}: {message}"))
        }
        UploadError::Auth { message } => ServiceError::failed(format!(
            "{destination}: authentication failed: {message}. Check the credentials of the uploader \
             (secrets are set with `ssx uploaders secret set <name>`)"
        )),
        other if other.is_retryable() => ServiceError::retryable(format!("{destination}: {other}")),
        other => ServiceError::failed(format!("{destination}: {other}")),
    }
}

/// Latest progress report, handed from the uploader's task to the calling thread.
#[derive(Default)]
struct LatestProgress(Mutex<Option<(u64, Option<u64>)>>);

impl ProgressSink for LatestProgress {
    fn report(&self, sent: u64, total: Option<u64>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((sent, total));
    }
}

impl LatestProgress {
    fn take(&self) -> Option<(u64, Option<u64>)> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

impl Uploaders for UploadService {
    fn upload(
        &self,
        req: &UploadRequest<'_>,
        progress: &dyn Fn(UploadProgress),
        cancel: &CancelToken,
    ) -> Result<UploadOutcome, ServiceError> {
        let built = self.built(req.destination)?;
        let Some(uploader) = built.uploader.clone() else {
            return Err(ServiceError::NotConfigured(format!(
                "{:?} is a URL shortener, not an uploader; choose another destination",
                req.destination
            )));
        };
        let kind = net_kind(req.kind);
        if !uploader.supports(kind) {
            return Err(ServiceError::failed(format!(
                "uploader {:?} cannot upload {kind} content; pick another destination for it \
                 (`ssx uploaders list` shows what each one accepts)",
                req.destination
            )));
        }
        let net_req = match req.source {
            UploadSource::LocalFile(p) => {
                NetRequest::from_path(p, kind).with_filename(req.file_name)
            }
            UploadSource::Bytes(b) => {
                NetRequest::from_bytes(b.to_vec(), req.file_name, kind).with_mime(req.mime)
            }
        };
        let base = self.context()?;
        let latest = Arc::new(LatestProgress::default());
        let ctx = base.with_progress(latest.clone());
        let flush = || {
            if let Some((sent, total)) = latest.take() {
                progress(UploadProgress { sent, total });
            }
        };
        let result = self.runtime.block_on_cancellable(cancel, flush, |token| {
            let ctx = ctx.with_cancel(token);
            async move { uploader.upload(&net_req, &ctx).await }
        })?;
        match result {
            Ok(r) if r.url.trim().is_empty() => Err(ServiceError::failed(format!(
                "{}: the server accepted the upload but no URL could be read from its response",
                req.destination
            ))),
            Ok(r) => Ok(UploadOutcome {
                url: r.url,
                thumbnail_url: r.thumbnail_url.filter(|u| !u.is_empty()),
                deletion_url: r.deletion_url.filter(|u| !u.is_empty()),
            }),
            Err(e) => Err(map_upload_error(req.destination, e)),
        }
    }
}

impl CoreShortener for UploadService {
    fn shorten(
        &self,
        provider: &str,
        url: &str,
        cancel: &CancelToken,
    ) -> Result<String, ServiceError> {
        let built = self.built(provider)?;
        let Some(short): Option<Arc<dyn NetShortener>> = built.shortener.clone() else {
            return Err(ServiceError::NotConfigured(format!(
                "{provider:?} cannot shorten URLs; use is.gd, v.gd, tinyurl, a `shortener` table or a \
                 .sxcu file with the URLShortener destination"
            )));
        };
        let ctx = self.context()?;
        let url = url.to_owned();
        let result = self.runtime.block_on_cancellable(
            cancel,
            || {},
            |token| {
                let ctx = ctx.with_cancel(token);
                async move { short.shorten(&url, &ctx).await }
            },
        )?;
        result.map_err(|e| map_upload_error(provider, e))
    }
}

#[cfg(test)]
mod tests;
