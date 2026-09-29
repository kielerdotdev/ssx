//! A small, configuration-driven "upload with PUT/POST" uploader for services that do not
//! warrant a `.sxcu` file: WebDAV shares, transfer.sh clones, object stores behind a plain
//! HTTP endpoint, internal upload services.
//!
//! The request URL is a template with `{filename}`, `{stem}` and `{ext}` (percent-encoded)
//! and ShareX `%` codes (`%y/%mo/%d`, `%rn{8}`). The body is either the raw bytes or a
//! multipart form with one file field. Where the public URL comes from is explicit
//! ([`ResultUrl`]) instead of guessed.
//!
//! Secrets never live in the config: [`HttpAuth`] names secret-store entries.

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::body::{BodyPlan, Payload};
use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http::{self, RAW_RESPONSE_LIMIT, truncate_bytes};
use crate::multipart::{self, FilePart};
use crate::nameparser::NameParser;
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader};

/// Request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UploadMethod {
    /// `PUT` (object-store style: the URL names the object).
    #[default]
    Put,
    /// `POST`.
    Post,
    /// `PATCH`.
    Patch,
}

/// Request body shape.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HttpBody {
    /// The file bytes as the request body, `Content-Type` from the file name.
    #[default]
    Raw,
    /// `multipart/form-data` with the file in `field` and optional extra text fields.
    Multipart {
        /// Field carrying the file.
        field: String,
        /// Additional text fields (`%` codes and `{filename}` are expanded).
        #[serde(default)]
        fields: Vec<(String, String)>,
    },
}

/// Authentication, resolved from the secret store at upload time.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HttpAuth {
    /// No authentication.
    #[default]
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer {
        /// Secret store entry with the token.
        secret_key: String,
    },
    /// `Authorization: Basic base64(username:secret)`.
    Basic {
        /// User name (not secret).
        username: String,
        /// Secret store entry with the password / API key.
        secret_key: String,
    },
    /// An arbitrary header whose value is the secret (`X-API-Key`).
    Header {
        /// Header name.
        name: String,
        /// Secret store entry with the value.
        secret_key: String,
    },
}

/// Where the resulting public URL comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ResultUrl {
    /// The URL the file was sent to (WebDAV, S3-style PUT).
    #[default]
    RequestUrl,
    /// The response body, trimmed.
    Body,
    /// A response header such as `Location`.
    Header(String),
    /// A JSON pointer (RFC 6901, e.g. `/data/link`) into the JSON response.
    JsonPointer(String),
    /// A template with `{filename}`, `{stem}`, `{ext}`, `{request_url}` (public CDN URL for a
    /// private upload endpoint).
    Template(String),
}

/// Configuration of an [`HttpUploader`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpUploaderConfig {
    /// Display name.
    pub name: String,
    /// Method.
    #[serde(default)]
    pub method: UploadMethod,
    /// URL template.
    pub url: String,
    /// Extra static headers (values may use `%` codes).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Body shape.
    #[serde(default)]
    pub body: HttpBody,
    /// Authentication.
    #[serde(default)]
    pub auth: HttpAuth,
    /// Result URL source.
    #[serde(default)]
    pub result: ResultUrl,
}

impl HttpUploaderConfig {
    /// `PUT` raw bytes to `url`, returning the request URL as the public URL.
    pub fn put(name: &str, url: &str) -> Self {
        Self {
            name: name.to_owned(),
            method: UploadMethod::Put,
            url: url.to_owned(),
            headers: Vec::new(),
            body: HttpBody::Raw,
            auth: HttpAuth::None,
            result: ResultUrl::RequestUrl,
        }
    }

    /// `POST` a multipart form with the file in `field`, taking the URL from the body.
    pub fn post_multipart(name: &str, url: &str, field: &str) -> Self {
        Self {
            name: name.to_owned(),
            method: UploadMethod::Post,
            url: url.to_owned(),
            headers: Vec::new(),
            body: HttpBody::Multipart { field: field.to_owned(), fields: Vec::new() },
            auth: HttpAuth::None,
            result: ResultUrl::Body,
        }
    }
}

/// Generic HTTP uploader.
#[derive(Debug, Clone)]
pub struct HttpUploader {
    cfg: HttpUploaderConfig,
}

fn encode(s: &str) -> String {
    crate::util::url_encode(s)
}

impl HttpUploader {
    /// Validate and build.
    pub fn new(cfg: HttpUploaderConfig) -> Result<Self, UploadError> {
        let u = url::Url::parse(&cfg.url.replace(['{', '}'], "x").replace('%', "x"))
            .map_err(|e| UploadError::config(format!("invalid URL template '{}': {e}", cfg.url)))?;
        if !matches!(u.scheme(), "http" | "https") {
            return Err(UploadError::config(format!("URL '{}' must use http or https", cfg.url)));
        }
        if let HttpBody::Multipart { field, .. } = &cfg.body {
            if field.is_empty() {
                return Err(UploadError::config("multipart body needs a file field name"));
            }
        }
        Ok(Self { cfg })
    }

    fn parts(req: &UploadRequest) -> (String, String, String) {
        let filename = req.resolved_filename();
        let (stem, ext) = match filename.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_owned(), e.to_owned()),
            _ => (filename.clone(), String::new()),
        };
        (filename, stem, ext)
    }

    fn expand(template: &str, req: &UploadRequest, names: &NameParser, encode_parts: bool) -> String {
        let (filename, stem, ext) = Self::parts(req);
        let f = |s: String| if encode_parts { encode(&s) } else { s };
        names
            .parse(template)
            .replace("{filename}", &f(filename))
            .replace("{stem}", &f(stem))
            .replace("{ext}", &f(ext))
    }

    fn headers(&self, req: &UploadRequest, names: &NameParser, ctx: &UploadContext) -> Result<HeaderMap, UploadError> {
        let mut map = HeaderMap::new();
        let mut put = |name: &str, value: &str| -> Result<(), UploadError> {
            let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| UploadError::config(format!("invalid header name '{name}'")))?;
            let mut v = HeaderValue::from_str(value).map_err(|_| UploadError::config(format!("invalid value for header '{name}'")))?;
            if n == AUTHORIZATION || n.as_str().contains("key") || n.as_str().contains("token") {
                v.set_sensitive(true);
            }
            map.insert(n, v);
            Ok(())
        };
        for (k, v) in &self.cfg.headers {
            put(k, &Self::expand(v, req, names, false))?;
        }
        match &self.cfg.auth {
            HttpAuth::None => {}
            HttpAuth::Bearer { secret_key } => {
                put("authorization", &format!("Bearer {}", ctx.require_secret(secret_key)?))?;
            }
            HttpAuth::Basic { username, secret_key } => {
                use base64::Engine as _;
                let raw = format!("{username}:{}", ctx.require_secret(secret_key)?);
                put("authorization", &format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)))?;
            }
            HttpAuth::Header { name, secret_key } => put(name, &ctx.require_secret(secret_key)?)?,
        }
        Ok(map)
    }
}

#[async_trait]
impl Uploader for HttpUploader {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn supports(&self, kind: UploadKind) -> bool {
        kind != UploadKind::Url
    }

    async fn upload(&self, req: &UploadRequest, ctx: &UploadContext) -> Result<UploadResult, UploadError> {
        ctx.check_cancelled()?;
        if !self.supports(req.kind) {
            return Err(UploadError::Unsupported { uploader: self.cfg.name.clone(), kind: req.kind });
        }
        let names = NameParser::default();
        let url = Self::expand(&self.cfg.url, req, &names, true);
        let mut headers = self.headers(req, &names, ctx)?;
        let payload = Payload::from_request(req).await?;
        let (plan, content_type) = match &self.cfg.body {
            HttpBody::Raw => (BodyPlan::raw(payload), req.resolved_mime()),
            HttpBody::Multipart { field, fields } => {
                let fields: Vec<(String, String)> =
                    fields.iter().map(|(k, v)| (k.clone(), Self::expand(v, req, &names, false))).collect();
                let (ct, plan) = multipart::plan(
                    &fields,
                    Some(FilePart { field: field.clone(), filename: req.resolved_filename(), mime: req.resolved_mime(), payload }),
                );
                (plan, ct)
            }
        };
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_str(&content_type).map_err(|_| UploadError::config("invalid content type"))?,
            );
        }
        let method = match self.cfg.method {
            UploadMethod::Put => reqwest::Method::PUT,
            UploadMethod::Post => reqwest::Method::POST,
            UploadMethod::Patch => reqwest::Method::PATCH,
        };
        let len = plan.content_length();
        let (body, fault) = plan.into_body(ctx).await?;
        let rb = ctx.http.request(method, &url).headers(headers).header(CONTENT_LENGTH, len).body(body);
        let resp = http::fetch(ctx, rb, Some(&fault)).await?;
        if !resp.is_success() {
            return Err(resp.to_error(None));
        }
        let public = match &self.cfg.result {
            ResultUrl::RequestUrl => url.clone(),
            ResultUrl::Body => resp.text.trim().to_owned(),
            ResultUrl::Header(h) => resp.header(h).unwrap_or_default().trim().to_owned(),
            ResultUrl::JsonPointer(p) => {
                let v: serde_json::Value = serde_json::from_str(&resp.text).map_err(|e| {
                    UploadError::invalid_response(format!("expected a JSON response ({e}): {}", http::snippet(&resp.text)))
                })?;
                match v.pointer(p) {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                }
            }
            ResultUrl::Template(t) => Self::expand(t, req, &names, true).replace("{request_url}", &url),
        };
        if public.is_empty() {
            return Err(UploadError::invalid_response(format!(
                "the upload succeeded but the response has no URL ({:?}); the response began with: {}",
                self.cfg.result,
                http::snippet(&resp.text)
            )));
        }
        Ok(UploadResult {
            url: public,
            thumbnail_url: None,
            deletion_url: None,
            raw_response: truncate_bytes(&resp.text, RAW_RESPONSE_LIMIT),
            uploader_name: self.cfg.name.clone(),
            extra: std::collections::BTreeMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_expansion_encodes_file_parts_only() {
        let req = UploadRequest::from_bytes(vec![1], "my file.tar.gz", UploadKind::File);
        let n = NameParser::default();
        assert_eq!(
            HttpUploader::expand("https://h/{stem}/{filename}.{ext}", &req, &n, true),
            "https://h/my%20file.tar/my%20file.tar.gz.gz"
        );
        assert_eq!(HttpUploader::expand("{filename}", &req, &n, false), "my file.tar.gz");
    }

    #[test]
    fn validation() {
        assert!(HttpUploader::new(HttpUploaderConfig::put("x", "https://h/{filename}")).is_ok());
        assert!(HttpUploader::new(HttpUploaderConfig::put("x", "ftp://h/x")).is_err());
        assert!(HttpUploader::new(HttpUploaderConfig::put("x", "not a url")).is_err());
        assert!(HttpUploader::new(HttpUploaderConfig::post_multipart("x", "https://h/u", "")).is_err());
    }
}
