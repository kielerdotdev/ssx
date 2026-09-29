//! Uploaders for ssx.
//!
//! * [`Uploader`] / [`UrlShortener`]: the object-safe async traits every destination
//!   implements; [`UploadContext`] carries progress, cancellation, the shared HTTP client and
//!   the secret store.
//! * [`RetryingUploader`]: backoff + jitter + `Retry-After` around any uploader.
//! * `sxcu` (feature): ShareX `.sxcu` custom uploaders, template language included.
//! * [`imgur`], `s3` (feature, with in-crate `sigv4`), [`http_uploader`], [`shorten`],
//!   [`local`]: built-in destinations.
//! * `oauth` (feature): loopback + PKCE OAuth2 helper.
//!
//! SFTP/FTP are intentionally absent (see the README); implement [`Uploader`] to add them.
#![forbid(unsafe_code)]

pub mod body;
pub mod context;
pub mod error;
pub mod http;
pub mod http_uploader;
pub mod imgur;
pub mod local;
pub mod multipart;
pub mod nameparser;
#[cfg(feature = "oauth")]
pub mod oauth;
pub mod progress;
pub mod retry;
#[cfg(feature = "s3")]
pub mod s3;
pub mod secrets;
pub mod shorten;
#[cfg(feature = "s3")]
pub mod sigv4;
#[cfg(feature = "sxcu")]
pub mod sxcu;
pub mod types;
pub mod util;

pub use context::UploadContext;
pub use error::UploadError;
pub use http::{RAW_RESPONSE_LIMIT, build_http_client};
pub use progress::{NullProgress, ProgressSink, ThrottledProgress};
pub use retry::{Jitter, RetryPolicy, RetryingUploader};
pub use secrets::{InMemorySecretStore, SecretError, SecretStore};
pub use shorten::ShorteningUploader;
pub use types::{UploadKind, UploadRequest, UploadResult, UploadSource, Uploader, UrlShortener};
