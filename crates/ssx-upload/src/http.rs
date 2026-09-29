//! Shared HTTP plumbing: the TLS-configured client, cancellable sends, bounded response
//! reads and status-to-error mapping.
//!
//! Decisions worth knowing:
//! * TLS is rustls with the **ring** provider and the platform certificate verifier, built
//!   here and handed to reqwest pre-configured. That keeps OpenSSL and aws-lc (a C/asm build
//!   dependency) out of the build, and honours corporate root CAs installed in the OS.
//! * Every send races the [`crate::UploadContext`] cancellation token. Dropping the request
//!   future aborts the connection, which is what makes cancelling a multi-GB upload prompt.
//! * Response bodies are read with a hard cap so a hostile or broken server cannot exhaust
//!   memory; templates and error snippets work on the (possibly truncated) capped text.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode};

use crate::context::UploadContext;
use crate::error::UploadError;

/// Largest response body we buffer (8 MiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// [`crate::UploadResult::raw_response`] is truncated to this many bytes.
pub const RAW_RESPONSE_LIMIT: usize = 64 * 1024;

/// Characters of the response kept in [`UploadError::Http::body_snippet`].
const SNIPPET_CHARS: usize = 300;

/// Build the default shared HTTP client: rustls (ring) + platform verifier, system proxy
/// settings, a 20 s connect timeout and no overall timeout (uploads may legitimately take
/// hours; cancellation is the caller's tool).
pub fn build_http_client() -> Result<reqwest::Client, UploadError> {
    client_builder()?
        .build()
        .map_err(|e| UploadError::config(format!("could not build HTTP client: {e}")))
}

/// Like [`build_http_client`] but returns the builder for further customisation (tests use
/// it to disable proxies).
pub fn client_builder() -> Result<reqwest::ClientBuilder, UploadError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| UploadError::config(format!("TLS protocol setup failed: {e}")))?,
    )
    .map_err(|e| UploadError::config(format!("platform certificate verifier unavailable: {e}")))?
    .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(Duration::from_secs(20))
        .user_agent(concat!("ssx-upload/", env!("CARGO_PKG_VERSION"))))
}

/// A slot where a body stream records a local I/O failure, so that the error surfaced by
/// reqwest ("error sending request") can be replaced by the real cause.
#[derive(Debug, Clone, Default)]
pub struct BodyFault(Arc<Mutex<Option<std::io::Error>>>);

impl BodyFault {
    pub(crate) fn set(&self, e: std::io::Error) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(e);
        }
    }

    pub(crate) fn take(&self) -> Option<std::io::Error> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// A fully received (capped) HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Final URL after redirects.
    pub url: String,
    /// Body decoded as UTF-8 (lossy), capped at [`MAX_RESPONSE_BYTES`].
    pub text: String,
    /// The body exceeded the cap and was cut.
    pub truncated: bool,
}

impl HttpResponse {
    /// Response header as text (multiple values joined with `, `).
    pub fn header(&self, name: &str) -> Option<String> {
        let vals: Vec<&str> =
            self.headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
        if vals.is_empty() { None } else { Some(vals.join(", ")) }
    }

    /// 2xx.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Convert a non-success response into the matching [`UploadError`]. `message` is a
    /// service supplied explanation (e.g. an evaluated `.sxcu` `ErrorMessage`).
    pub fn to_error(&self, message: Option<String>) -> UploadError {
        let retry_after = parse_retry_after(&self.headers);
        let snippet = snippet(&self.text);
        let code = self.status.as_u16();
        match code {
            401 | 403 => UploadError::Auth {
                message: message.filter(|m| !m.is_empty()).unwrap_or_else(|| {
                    format!("server rejected the credentials (HTTP {code}): {snippet}")
                }),
            },
            429 => UploadError::RateLimited { retry_after },
            _ => UploadError::Http { status: code, message, body_snippet: snippet, retry_after },
        }
    }
}

/// Send `rb`, racing cancellation, and buffer the (capped) response.
pub async fn fetch(
    ctx: &UploadContext,
    rb: RequestBuilder,
    fault: Option<&BodyFault>,
) -> Result<HttpResponse, UploadError> {
    ctx.check_cancelled()?;
    let work = async {
        let resp = rb.send().await.map_err(|e| map_send_error(ctx, &e, fault))?;
        read_response(ctx, resp).await
    };
    tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => Err(UploadError::Cancelled),
        r = work => r,
    }
}

async fn read_response(
    ctx: &UploadContext,
    mut resp: reqwest::Response,
) -> Result<HttpResponse, UploadError> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let url = resp.url().to_string();
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_RESPONSE_BYTES.saturating_sub(buf.len());
                if chunk.len() > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(map_send_error(ctx, &e, None)),
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        url,
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
    })
}

/// Map a reqwest failure to [`UploadError`], preferring the real local cause when the body
/// stream recorded one and reporting cancellation as such.
pub fn map_send_error(
    ctx: &UploadContext,
    e: &reqwest::Error,
    fault: Option<&BodyFault>,
) -> UploadError {
    if ctx.cancel.is_cancelled() {
        return UploadError::Cancelled;
    }
    if let Some(io) = fault.and_then(BodyFault::take) {
        return UploadError::io("streaming the upload body", io);
    }
    let mut message = String::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        let part = err.to_string();
        // reqwest embeds the URL (possibly with tokens in the query) in its top message.
        let part = strip_url(&part);
        if !part.is_empty() && !message.contains(&part) {
            if !message.is_empty() {
                message.push_str(": ");
            }
            message.push_str(&part);
        }
        cur = err.source();
    }
    if message.is_empty() {
        "request failed".clone_into(&mut message);
    }
    UploadError::Network { message, timed_out: e.is_timeout() }
}

fn strip_url(s: &str) -> String {
    // "error sending request for url (https://host/path?token=..)" -> "error sending request"
    match s.find(" for url (") {
        Some(i) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// Parse `Retry-After` (delta-seconds or HTTP-date).
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(v).ok()?;
    Some(when.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO))
}

/// First [`SNIPPET_CHARS`] characters of `text`, for error messages.
pub fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    let mut out: String = trimmed.chars().take(SNIPPET_CHARS).collect();
    if trimmed.chars().nth(SNIPPET_CHARS).is_some() {
        out.push('…');
    }
    out
}

/// Cut `s` to at most `max` bytes on a character boundary.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn retry_after_seconds_and_date() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(120)));
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&h), Some(Duration::ZERO), "past dates clamp to zero");
        h.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&h), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn snippet_and_truncate_respect_char_boundaries() {
        let s = "é".repeat(400);
        let sn = snippet(&s);
        assert_eq!(sn.chars().count(), SNIPPET_CHARS + 1);
        assert_eq!(truncate_bytes("aé", 2), "a");
        assert_eq!(truncate_bytes("abc", 10), "abc");
    }

    #[test]
    fn client_builds_without_openssl() {
        // Exercises the ring + platform verifier wiring (would panic without a provider).
        build_http_client().expect("client");
    }

    fn resp(status: u16, text: &str) -> HttpResponse {
        HttpResponse {
            status: StatusCode::from_u16(status).unwrap(),
            headers: HeaderMap::new(),
            url: "http://x/".into(),
            text: text.into(),
            truncated: false,
        }
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(resp(401, "no").to_error(None), UploadError::Auth { .. }));
        assert!(
            matches!(resp(403, "no").to_error(Some("bad key".into())), UploadError::Auth { message } if message == "bad key")
        );
        assert!(matches!(resp(429, "").to_error(None), UploadError::RateLimited { .. }));
        assert!(matches!(resp(500, "boom").to_error(None), UploadError::Http { status: 500, .. }));
    }
}
