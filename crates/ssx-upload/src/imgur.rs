//! Imgur (API v3).
//!
//! * Images: `POST /3/image` with the file in the multipart field `image`.
//! * Videos: `POST /3/upload` with the file in the multipart field `video`.
//! * Auth is either anonymous (`Authorization: Client-ID <id>`, the app's client id) or an
//!   OAuth2 access token (`Authorization: Bearer <token>`, uploads land in the user's
//!   account). The token can be given inline or fetched from the secret store at upload
//!   time, which is how [`crate::oauth`] hands over refreshed tokens.
//! * Anonymous uploads return a `deletehash`; ShareX-compatible deletion page
//!   `https://imgur.com/delete/<deletehash>` becomes [`UploadResult::deletion_url`].
//!   Account uploads have no deletehash (delete through the API with the image id, kept in
//!   `extra["id"]`).
//! * Errors come as `{"data":{"error":"..."|{"message":..}},"success":false,"status":N}`;
//!   the message is surfaced. HTTP 429 maps to [`UploadError::RateLimited`], using
//!   `Retry-After` or the `X-RateLimit-*Reset` epoch when present.
//!
//! Endpoints are configurable (`api_base`, `image_base`, `site_base`) so tests can point at a
//! local server; the defaults are the real hosts.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::body::{BodyPlan, Payload};
use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http::{self, HttpResponse, RAW_RESPONSE_LIMIT, truncate_bytes};
use crate::multipart::{self, FilePart};
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader};

/// How requests authenticate.
#[derive(Debug, Clone)]
pub enum ImgurAuth {
    /// Anonymous uploads with the application's client id.
    Anonymous {
        /// The client id registered at <https://api.imgur.com/oauth2/addclient>.
        client_id: String,
    },
    /// OAuth2 bearer token given inline.
    Bearer {
        /// Access token.
        access_token: String,
    },
    /// OAuth2 bearer token read from the secret store on every upload (so refreshes are
    /// picked up without rebuilding the uploader).
    BearerStored {
        /// Secret store entry holding the access token.
        secret_key: String,
    },
}

/// Thumbnail size suffixes supported by i.imgur.com.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThumbnailSize {
    /// 90x90 square.
    Small,
    /// 160x160.
    Thumb,
    /// 320x320.
    #[default]
    Medium,
    /// 640x640.
    Large,
    /// 1024x1024.
    Huge,
}

impl ThumbnailSize {
    fn suffix(self) -> char {
        match self {
            Self::Small => 's',
            Self::Thumb => 't',
            Self::Medium => 'm',
            Self::Large => 'l',
            Self::Huge => 'h',
        }
    }
}

/// Imgur uploader configuration.
#[derive(Debug, Clone)]
pub struct ImgurConfig {
    /// Authentication.
    pub auth: ImgurAuth,
    /// Optional title sent with each upload.
    pub title: Option<String>,
    /// Optional description sent with each upload.
    pub description: Option<String>,
    /// Album id (OAuth) or album deletehash (anonymous) to add uploads to.
    pub album: Option<String>,
    /// Thumbnail size for [`UploadResult::thumbnail_url`].
    pub thumbnail_size: ThumbnailSize,
    /// API origin.
    pub api_base: String,
    /// Origin of image links used to build thumbnails.
    pub image_base: String,
    /// Origin of the site, used for deletion links.
    pub site_base: String,
}

impl ImgurConfig {
    /// Anonymous uploads with `client_id`.
    pub fn anonymous(client_id: impl Into<String>) -> Self {
        Self::with_auth(ImgurAuth::Anonymous { client_id: client_id.into() })
    }

    /// Account uploads with an OAuth2 access token.
    pub fn bearer(access_token: impl Into<String>) -> Self {
        Self::with_auth(ImgurAuth::Bearer { access_token: access_token.into() })
    }

    /// Account uploads with the access token stored under `secret_key`.
    pub fn bearer_stored(secret_key: impl Into<String>) -> Self {
        Self::with_auth(ImgurAuth::BearerStored { secret_key: secret_key.into() })
    }

    fn with_auth(auth: ImgurAuth) -> Self {
        Self {
            auth,
            title: None,
            description: None,
            album: None,
            thumbnail_size: ThumbnailSize::default(),
            api_base: "https://api.imgur.com".into(),
            image_base: "https://i.imgur.com".into(),
            site_base: "https://imgur.com".into(),
        }
    }
}

/// The Imgur uploader.
#[derive(Debug, Clone)]
pub struct ImgurUploader {
    cfg: ImgurConfig,
    name: String,
}

impl ImgurUploader {
    /// Build the uploader, rejecting an empty client id / token.
    pub fn new(cfg: ImgurConfig) -> Result<Self, UploadError> {
        let empty = match &cfg.auth {
            ImgurAuth::Anonymous { client_id } => client_id.trim().is_empty(),
            ImgurAuth::Bearer { access_token } => access_token.trim().is_empty(),
            ImgurAuth::BearerStored { secret_key } => secret_key.trim().is_empty(),
        };
        if empty {
            return Err(UploadError::config("Imgur needs a client id (anonymous) or an access token"));
        }
        let name = match cfg.auth {
            ImgurAuth::Anonymous { .. } => "Imgur (anonymous)",
            _ => "Imgur (account)",
        };
        Ok(Self { cfg, name: name.to_owned() })
    }

    fn authorization(&self, ctx: &UploadContext) -> Result<String, UploadError> {
        Ok(match &self.cfg.auth {
            ImgurAuth::Anonymous { client_id } => format!("Client-ID {}", client_id.trim()),
            ImgurAuth::Bearer { access_token } => format!("Bearer {}", access_token.trim()),
            ImgurAuth::BearerStored { secret_key } => format!("Bearer {}", ctx.require_secret(secret_key)?.trim()),
        })
    }
}

/// `data.error` may be a string or an object with a `message`.
fn error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let err = v.pointer("/data/error")?;
    match err {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn header_epoch_delay(headers: &HeaderMap) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    ["x-ratelimit-userreset", "x-ratelimit-clientreset"].iter().find_map(|h| {
        let reset: u64 = headers.get(*h)?.to_str().ok()?.trim().parse().ok()?;
        // Some deployments send seconds-until-reset, others an epoch; accept both.
        Some(Duration::from_secs(if reset > 1_000_000_000 { reset.saturating_sub(now) } else { reset }))
    })
}

#[async_trait]
impl Uploader for ImgurUploader {
    fn name(&self) -> &str {
        &self.name
    }

    fn supports(&self, kind: UploadKind) -> bool {
        matches!(kind, UploadKind::Image | UploadKind::Video)
    }

    async fn upload(&self, req: &UploadRequest, ctx: &UploadContext) -> Result<UploadResult, UploadError> {
        ctx.check_cancelled()?;
        if !self.supports(req.kind) {
            return Err(UploadError::Unsupported { uploader: self.name.clone(), kind: req.kind });
        }
        let (endpoint, field) = match req.kind {
            UploadKind::Video => ("/3/upload", "video"),
            _ => ("/3/image", "image"),
        };
        let filename = req.resolved_filename();
        let mut fields: Vec<(String, String)> = Vec::new();
        if let Some(t) = &self.cfg.title {
            fields.push(("title".into(), t.clone()));
        }
        if let Some(d) = &self.cfg.description {
            fields.push(("description".into(), d.clone()));
        }
        if let Some(a) = &self.cfg.album {
            fields.push(("album".into(), a.clone()));
        }
        fields.push(("name".into(), filename.clone()));
        let (content_type, plan): (String, BodyPlan) = multipart::plan(
            &fields,
            Some(FilePart {
                field: field.into(),
                filename,
                mime: req.resolved_mime(),
                payload: Payload::from_request(req).await?,
            }),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&self.authorization(ctx)?).map_err(|_| UploadError::config("credentials contain invalid header characters"))?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type).map_err(|_| UploadError::config("bad multipart content type"))?,
        );
        let len = plan.content_length();
        let (body, fault) = plan.into_body(ctx).await?;
        let url = format!("{}{endpoint}", self.cfg.api_base.trim_end_matches('/'));
        let rb = ctx.http.post(url).headers(headers).header(CONTENT_LENGTH, len).body(body);
        let resp = http::fetch(ctx, rb, Some(&fault)).await?;
        if !resp.is_success() {
            return Err(self.map_error(&resp));
        }
        self.parse_success(&resp)
    }
}

impl ImgurUploader {
    fn map_error(&self, resp: &HttpResponse) -> UploadError {
        let message = error_message(&resp.text);
        if resp.status.as_u16() == 429 {
            let retry_after = http::parse_retry_after(&resp.headers).or_else(|| header_epoch_delay(&resp.headers));
            return UploadError::RateLimited { retry_after };
        }
        resp.to_error(message)
    }

    fn parse_success(&self, resp: &HttpResponse) -> Result<UploadResult, UploadError> {
        let v: Value = serde_json::from_str(&resp.text)
            .map_err(|e| UploadError::invalid_response(format!("Imgur did not return JSON ({e}): {}", http::snippet(&resp.text))))?;
        if v.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(UploadError::Http {
                status: v.get("status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).unwrap_or(400),
                message: error_message(&resp.text),
                body_snippet: http::snippet(&resp.text),
                retry_after: None,
            });
        }
        let data = v.get("data").ok_or_else(|| UploadError::invalid_response("Imgur response has no 'data' object"))?;
        let link = data
            .get("link")
            .and_then(Value::as_str)
            .ok_or_else(|| UploadError::invalid_response(format!("Imgur response has no link: {}", http::snippet(&resp.text))))?;
        // Imgur occasionally answers with http:// links; everything is served over https.
        let url = link.strip_prefix("http://").map_or_else(|| link.to_owned(), |rest| format!("https://{rest}"));
        let id = data.get("id").and_then(Value::as_str).unwrap_or_default();
        let deletehash = data.get("deletehash").and_then(Value::as_str).filter(|s| !s.is_empty());
        let mut extra = BTreeMap::new();
        if !id.is_empty() {
            extra.insert("id".to_owned(), id.to_owned());
        }
        if let Some(d) = deletehash {
            extra.insert("deletehash".to_owned(), d.to_owned());
        }
        Ok(UploadResult {
            url,
            thumbnail_url: (!id.is_empty()).then(|| {
                format!("{}/{id}{}.jpg", self.cfg.image_base.trim_end_matches('/'), self.cfg.thumbnail_size.suffix())
            }),
            deletion_url: deletehash.map(|d| format!("{}/delete/{d}", self.cfg.site_base.trim_end_matches('/'))),
            raw_response: truncate_bytes(&resp.text, RAW_RESPONSE_LIMIT),
            uploader_name: self.name.clone(),
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_credentials() {
        assert!(ImgurUploader::new(ImgurConfig::anonymous("  ")).is_err());
        assert!(ImgurUploader::new(ImgurConfig::bearer("")).is_err());
        assert!(ImgurUploader::new(ImgurConfig::bearer_stored("")).is_err());
        assert!(ImgurUploader::new(ImgurConfig::anonymous("abc")).is_ok());
    }

    #[test]
    fn error_message_shapes() {
        assert_eq!(error_message(r#"{"data":{"error":"Bad"}}"#).as_deref(), Some("Bad"));
        assert_eq!(error_message(r#"{"data":{"error":{"message":"Obj","code":1}}}"#).as_deref(), Some("Obj"));
        assert_eq!(error_message("nope"), None);
    }

    #[test]
    fn epoch_or_relative_reset_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-userreset", HeaderValue::from_static("120"));
        assert_eq!(header_epoch_delay(&h), Some(Duration::from_secs(120)));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        h.insert("x-ratelimit-userreset", HeaderValue::from_str(&(now + 90).to_string()).unwrap());
        let d = header_epoch_delay(&h).unwrap();
        assert!(d >= Duration::from_secs(88) && d <= Duration::from_secs(90), "{d:?}");
    }
}
