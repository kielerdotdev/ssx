//! URL shortening: the "after upload, shorten the URL" workflow step.
//!
//! [`HttpShortener`] covers the common "GET/POST a URL, get the short one back as plain text
//! or in JSON" services (is.gd, v.gd, TinyURL, YOURLS in `simple` format, self-hosted
//! shorteners) through configuration. Anything fancier is a `.sxcu` file
//! ([`crate::sxcu::SxcuUploader`] implements [`UrlShortener`] too).
//!
//! [`ShorteningUploader`] chains an uploader and a shortener. A failing shortener must not
//! throw away a completed upload, so by default it *fails open*: the original URL is
//! returned and the reason is recorded in `extra["shorten_error"]`. Cancellation always
//! propagates.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};

use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http;
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader, UrlShortener};
use crate::util::url_encode;

/// Request method of a shortening endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortenMethod {
    /// Parameters in the query string.
    Get,
    /// Parameters as a form-urlencoded body.
    Post,
}

/// How to read the short URL out of the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortenResponse {
    /// The trimmed body is the short URL.
    PlainText,
    /// A JSON pointer (`/shorturl`, `/data/url`) into a JSON response.
    JsonPointer(String),
}

/// Configuration of an [`HttpShortener`].
#[derive(Debug, Clone)]
pub struct HttpShortenerConfig {
    /// Display name.
    pub name: String,
    /// Endpoint URL.
    pub endpoint: String,
    /// Method.
    pub method: ShortenMethod,
    /// Parameter carrying the long URL.
    pub url_param: String,
    /// Fixed extra parameters (`format=simple`, `signature=...`).
    pub params: Vec<(String, String)>,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Response format.
    pub response: ShortenResponse,
}

/// Config-driven shortener.
#[derive(Debug, Clone)]
pub struct HttpShortener {
    cfg: HttpShortenerConfig,
}

impl HttpShortener {
    /// Validate and build.
    pub fn new(cfg: HttpShortenerConfig) -> Result<Self, UploadError> {
        let u = url::Url::parse(&cfg.endpoint).map_err(|e| UploadError::config(format!("invalid shortener endpoint '{}': {e}", cfg.endpoint)))?;
        if !matches!(u.scheme(), "http" | "https") {
            return Err(UploadError::config("shortener endpoint must be http(s)"));
        }
        if cfg.url_param.is_empty() {
            return Err(UploadError::config("shortener needs the name of the parameter that carries the URL"));
        }
        Ok(Self { cfg })
    }

    fn simple(name: &str, endpoint: &str) -> Self {
        Self {
            cfg: HttpShortenerConfig {
                name: name.to_owned(),
                endpoint: endpoint.to_owned(),
                method: ShortenMethod::Get,
                url_param: "url".to_owned(),
                params: Vec::new(),
                headers: Vec::new(),
                response: ShortenResponse::PlainText,
            },
        }
    }

    /// is.gd (`create.php?format=simple`). Pass a different `endpoint` for tests or mirrors.
    pub fn is_gd(endpoint: Option<&str>) -> Self {
        let mut s = Self::simple("is.gd", endpoint.unwrap_or("https://is.gd/create.php"));
        s.cfg.params.push(("format".into(), "simple".into()));
        s
    }

    /// v.gd, is.gd's sibling that shows a preview page.
    pub fn v_gd(endpoint: Option<&str>) -> Self {
        let mut s = Self::simple("v.gd", endpoint.unwrap_or("https://v.gd/create.php"));
        s.cfg.params.push(("format".into(), "simple".into()));
        s
    }

    /// TinyURL's `api-create.php`.
    pub fn tiny_url(endpoint: Option<&str>) -> Self {
        Self::simple("TinyURL", endpoint.unwrap_or("https://tinyurl.com/api-create.php"))
    }
}

fn looks_like_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

#[async_trait]
impl UrlShortener for HttpShortener {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    async fn shorten(&self, long_url: &str, ctx: &UploadContext) -> Result<String, UploadError> {
        ctx.check_cancelled()?;
        if !looks_like_url(long_url) {
            return Err(UploadError::config(format!("'{long_url}' is not an http(s) URL and cannot be shortened")));
        }
        let mut pairs: Vec<(String, String)> = self.cfg.params.clone();
        pairs.push((self.cfg.url_param.clone(), long_url.to_owned()));
        let query = pairs.iter().map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v))).collect::<Vec<_>>().join("&");
        let mut rb = match self.cfg.method {
            ShortenMethod::Get => {
                let sep = if self.cfg.endpoint.contains('?') { '&' } else { '?' };
                ctx.http.get(format!("{}{sep}{query}", self.cfg.endpoint))
            }
            ShortenMethod::Post => {
                ctx.http.post(&self.cfg.endpoint).header(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded")).body(query)
            }
        };
        for (k, v) in &self.cfg.headers {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| UploadError::config(format!("invalid header name '{k}'")))?;
            let value = HeaderValue::from_str(v).map_err(|_| UploadError::config(format!("invalid value for header '{k}'")))?;
            rb = rb.header(name, value);
        }
        let resp = http::fetch(ctx, rb, None).await?;
        if !resp.is_success() {
            let message = resp.text.trim().strip_prefix("Error:").map(|m| m.trim().to_owned());
            return Err(resp.to_error(message));
        }
        let short = match &self.cfg.response {
            ShortenResponse::PlainText => resp.text.trim().to_owned(),
            ShortenResponse::JsonPointer(p) => {
                let v: serde_json::Value = serde_json::from_str(&resp.text)
                    .map_err(|e| UploadError::invalid_response(format!("expected JSON from {} ({e}): {}", self.cfg.name, http::snippet(&resp.text))))?;
                v.pointer(p).and_then(|v| v.as_str()).unwrap_or_default().trim().to_owned()
            }
        };
        if !looks_like_url(&short) {
            return Err(UploadError::invalid_response(format!("{} did not return a URL: {}", self.cfg.name, http::snippet(&resp.text))));
        }
        Ok(short)
    }
}

/// Uploader followed by a shortening step.
pub struct ShorteningUploader {
    inner: Arc<dyn Uploader>,
    shortener: Arc<dyn UrlShortener>,
    fail_open: bool,
}

impl std::fmt::Debug for ShorteningUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShorteningUploader").field("inner", &self.inner.name()).field("shortener", &self.shortener.name()).finish()
    }
}

impl ShorteningUploader {
    /// Chain `inner` and `shortener`, failing open (see the module docs).
    pub fn new(inner: Arc<dyn Uploader>, shortener: Arc<dyn UrlShortener>) -> Self {
        Self { inner, shortener, fail_open: true }
    }

    /// Return the shortener's error instead of the unshortened result.
    #[must_use]
    pub fn fail_closed(mut self) -> Self {
        self.fail_open = false;
        self
    }
}

#[async_trait]
impl Uploader for ShorteningUploader {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn supports(&self, kind: UploadKind) -> bool {
        self.inner.supports(kind)
    }

    async fn upload(&self, req: &UploadRequest, ctx: &UploadContext) -> Result<UploadResult, UploadError> {
        let mut result = self.inner.upload(req, ctx).await?;
        if result.url.is_empty() {
            return Ok(result);
        }
        match self.shortener.shorten(&result.url, ctx).await {
            Ok(short) => {
                result.extra.insert("original_url".into(), std::mem::replace(&mut result.url, short));
                result.extra.insert("shortener".into(), self.shortener.name().to_owned());
                Ok(result)
            }
            Err(e) if e.is_cancelled() => Err(e),
            Err(e) if self.fail_open => {
                tracing::warn!(shortener = self.shortener.name(), error = %e, "URL shortening failed; keeping the original URL");
                result.extra.insert("shorten_error".into(), e.to_string());
                Ok(result)
            }
            Err(e) => Err(e),
        }
    }
}
