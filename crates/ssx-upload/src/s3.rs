//! S3-compatible uploader (AWS S3, Cloudflare R2, MinIO, Backblaze B2, Wasabi, ...).
//!
//! One `PutObject` per upload, signed with the in-crate SigV4 ([`crate::sigv4`]).
//!
//! * **Addressing**: virtual-hosted (`bucket.host/key`) or path-style (`host/bucket/key`).
//!   `Auto` picks path-style whenever a custom endpoint is configured (MinIO, most
//!   self-hosted stores) or the bucket name contains dots over HTTPS (the wildcard
//!   certificate would not match), virtual-hosted for plain AWS.
//! * **Payload signing**: `UNSIGNED-PAYLOAD` over HTTPS (the body is protected by TLS and
//!   nothing has to read a multi-GB file twice), otherwise the body's SHA-256 is computed by
//!   streaming the file once before the upload ("streaming hash"), because S3 rejects
//!   unsigned payloads over plain HTTP on some deployments and MinIO deployments behind
//!   HTTP are common in home labs. Override with [`PayloadSigning`].
//! * **Public URL**: derived from the addressing style, or a template such as
//!   `https://cdn.example.com/{key}` for buckets fronted by a CDN or R2 public domain.
//! * Credentials are looked up in the [`crate::SecretStore`] at upload time; the config only
//!   holds key *names*.
//!
//! Errors: S3 XML error bodies are parsed for `<Code>`/`<Message>`. Credential problems
//! (403, `ExpiredToken`, clock skew...) become [`UploadError::Auth`], `SlowDown` becomes
//! [`UploadError::RateLimited`], a wrong-region redirect becomes a configuration hint.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use crate::body::{BodyPlan, CHUNK, Payload};
use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http::{self, HttpResponse, RAW_RESPONSE_LIMIT, truncate_bytes};
use crate::nameparser::NameParser;
use crate::sigv4::{self, Credentials, SigningInput, UNSIGNED_PAYLOAD};
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader};

/// How the bucket appears in the request URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Addressing {
    /// See the module docs.
    #[default]
    Auto,
    /// `https://host/bucket/key`.
    PathStyle,
    /// `https://bucket.host/key`.
    VirtualHosted,
}

/// Whether the body is covered by the signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PayloadSigning {
    /// `UNSIGNED-PAYLOAD` over HTTPS, hashed over HTTP.
    #[default]
    Auto,
    /// Always `UNSIGNED-PAYLOAD`.
    Unsigned,
    /// Always sign the SHA-256 of the body (reads the file twice).
    Hashed,
}

/// Where the access keys come from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum S3Credentials {
    /// Keys held in memory. Convenient for tests; never serialised.
    #[serde(skip)]
    Static(Credentials),
    /// Names of entries in the [`crate::SecretStore`].
    Stored {
        /// Entry holding the access key id.
        access_key_id_key: String,
        /// Entry holding the secret access key.
        secret_access_key_key: String,
        /// Optional entry holding an STS session token.
        session_token_key: Option<String>,
    },
}

/// S3 uploader configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// Display name.
    pub name: String,
    /// Bucket name.
    pub bucket: String,
    /// Signing region (`auto` for Cloudflare R2).
    pub region: String,
    /// Custom endpoint URL (`https://host[:port][/base]`); `None` means AWS.
    pub endpoint: Option<String>,
    /// Addressing style.
    #[serde(default)]
    pub addressing: Addressing,
    /// Credentials.
    #[serde(default)]
    pub credentials: Option<S3Credentials>,
    /// Prepended to every object key (`screenshots/`).
    #[serde(default)]
    pub key_prefix: String,
    /// Object key template: `{filename}`, `{stem}`, `{ext}` and `%` name-parser codes
    /// (`%y/%mo/%rn{6}_{filename}`).
    #[serde(default = "default_key_template")]
    pub key_template: String,
    /// `x-amz-acl`, e.g. `public-read` (leave unset for buckets with ACLs disabled).
    #[serde(default)]
    pub acl: Option<String>,
    /// `x-amz-storage-class`, e.g. `STANDARD_IA`.
    #[serde(default)]
    pub storage_class: Option<String>,
    /// `Cache-Control` stored with the object.
    #[serde(default)]
    pub cache_control: Option<String>,
    /// `Content-Disposition` stored with the object (`inline` keeps browsers rendering).
    #[serde(default)]
    pub content_disposition: Option<String>,
    /// Extra headers (`x-amz-meta-*`, `x-amz-server-side-encryption`...), signed.
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// Public URL template: `{key}` (path-encoded), `{bucket}`, `{region}`, `{endpoint}`.
    #[serde(default)]
    pub public_url_template: Option<String>,
    /// Payload signing mode.
    #[serde(default)]
    pub payload_signing: PayloadSigning,
}

fn default_key_template() -> String {
    "{filename}".to_owned()
}

impl S3Config {
    fn base(name: &str, bucket: &str, region: &str, endpoint: Option<String>, addressing: Addressing) -> Self {
        Self {
            name: name.to_owned(),
            bucket: bucket.to_owned(),
            region: region.to_owned(),
            endpoint,
            addressing,
            credentials: None,
            key_prefix: String::new(),
            key_template: default_key_template(),
            acl: None,
            storage_class: None,
            cache_control: None,
            content_disposition: None,
            extra_headers: Vec::new(),
            public_url_template: None,
            payload_signing: PayloadSigning::Auto,
        }
    }

    /// Amazon S3.
    pub fn aws(bucket: &str, region: &str) -> Self {
        Self::base("Amazon S3", bucket, region, None, Addressing::Auto)
    }

    /// Cloudflare R2 (`public_url_template` must point at the bucket's public domain).
    pub fn cloudflare_r2(account_id: &str, bucket: &str) -> Self {
        Self::base(
            "Cloudflare R2",
            bucket,
            "auto",
            Some(format!("https://{account_id}.r2.cloudflarestorage.com")),
            Addressing::PathStyle,
        )
    }

    /// Backblaze B2's S3-compatible API (`region` looks like `us-west-004`).
    pub fn backblaze_b2(bucket: &str, region: &str) -> Self {
        Self::base(
            "Backblaze B2",
            bucket,
            region,
            Some(format!("https://s3.{region}.backblazeb2.com")),
            Addressing::PathStyle,
        )
    }

    /// Wasabi.
    pub fn wasabi(bucket: &str, region: &str) -> Self {
        let host = if region == "us-east-1" { "s3.wasabisys.com".to_owned() } else { format!("s3.{region}.wasabisys.com") };
        Self::base("Wasabi", bucket, region, Some(format!("https://{host}")), Addressing::PathStyle)
    }

    /// MinIO or any other self-hosted endpoint (path-style).
    pub fn minio(endpoint: &str, bucket: &str) -> Self {
        Self::base("MinIO", bucket, "us-east-1", Some(endpoint.to_owned()), Addressing::PathStyle)
    }

    /// Set static credentials (tests, CLI flags).
    #[must_use]
    pub fn with_static_credentials(mut self, access_key_id: &str, secret_access_key: &str) -> Self {
        self.credentials = Some(S3Credentials::Static(Credentials {
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: None,
        }));
        self
    }

    /// Read credentials from the secret store under these entry names.
    #[must_use]
    pub fn with_stored_credentials(mut self, access_key_id_key: &str, secret_access_key_key: &str) -> Self {
        self.credentials = Some(S3Credentials::Stored {
            access_key_id_key: access_key_id_key.to_owned(),
            secret_access_key_key: secret_access_key_key.to_owned(),
            session_token_key: None,
        });
        self
    }
}

/// The S3 uploader.
#[derive(Clone)]
pub struct S3Uploader {
    cfg: Arc<S3Config>,
    endpoint: Option<url::Url>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl std::fmt::Debug for S3Uploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Uploader").field("bucket", &self.cfg.bucket).field("region", &self.cfg.region).finish_non_exhaustive()
    }
}

/// Resolved request target.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    url: String,
    host_header: String,
    path: String,
    origin: String,
}

fn uri_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl S3Uploader {
    /// Validate `cfg` and build the uploader.
    pub fn new(cfg: S3Config) -> Result<Self, UploadError> {
        if cfg.bucket.is_empty() || cfg.bucket.contains(['/', ' ', '\\']) {
            return Err(UploadError::config(format!("invalid bucket name '{}'", cfg.bucket)));
        }
        if cfg.region.is_empty() {
            return Err(UploadError::config("region must be set (use 'auto' for Cloudflare R2)"));
        }
        let endpoint = match &cfg.endpoint {
            None => None,
            Some(e) => {
                let u = url::Url::parse(e).map_err(|err| UploadError::config(format!("invalid endpoint '{e}': {err}")))?;
                if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
                    return Err(UploadError::config(format!("endpoint '{e}' must be an http(s) URL with a host")));
                }
                Some(u)
            }
        };
        Ok(Self { cfg: Arc::new(cfg), endpoint, clock: Arc::new(Utc::now) })
    }

    /// Replace the clock (tests pin the signing time).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    fn https(&self) -> bool {
        self.endpoint.as_ref().is_none_or(|e| e.scheme() == "https")
    }

    fn effective_addressing(&self) -> Addressing {
        match self.cfg.addressing {
            Addressing::Auto => {
                if self.endpoint.is_some() || (self.cfg.bucket.contains('.') && self.https()) {
                    Addressing::PathStyle
                } else {
                    Addressing::VirtualHosted
                }
            }
            other => other,
        }
    }

    fn target(&self, key: &str) -> Result<Target, UploadError> {
        let (scheme, host, port, base_path) = match &self.endpoint {
            Some(e) => (
                e.scheme().to_owned(),
                e.host_str().unwrap_or_default().to_owned(),
                e.port(),
                e.path().trim_end_matches('/').to_owned(),
            ),
            None => ("https".to_owned(), format!("s3.{}.amazonaws.com", self.cfg.region), None, String::new()),
        };
        let virtual_hosted = self.effective_addressing() == Addressing::VirtualHosted;
        let host = if virtual_hosted { format!("{}.{host}", self.cfg.bucket) } else { host };
        let host_header = match port {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        };
        let mut path = base_path;
        if !virtual_hosted {
            path.push('/');
            path.push_str(&uri_encode_path(&self.cfg.bucket));
        }
        path.push('/');
        path.push_str(&uri_encode_path(key));
        let origin = format!("{scheme}://{host_header}");
        Ok(Target { url: format!("{origin}{path}"), host_header, path, origin })
    }

    fn object_key(&self, req: &UploadRequest, names: &NameParser) -> Result<String, UploadError> {
        let filename = req.resolved_filename();
        let filename: String = filename.chars().map(|c| if c.is_control() || c == '\\' { '_' } else { c }).collect();
        let filename = filename.trim_start_matches('/').to_owned();
        let (stem, ext) = match filename.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_owned(), e.to_owned()),
            _ => (filename.clone(), String::new()),
        };
        // Expand `%` codes first so a file name containing `%y` is never re-interpreted.
        let template = names.parse(&self.cfg.key_template);
        let rendered = template
            .replace("{filename}", &filename)
            .replace("{stem}", &stem)
            .replace("{ext}", &ext);
        let key = format!("{}{}", self.cfg.key_prefix, rendered);
        let key = key.trim_start_matches('/').to_owned();
        if key.is_empty() {
            return Err(UploadError::config("the object key template produced an empty key"));
        }
        if key.len() > 1024 {
            return Err(UploadError::config(format!("object key is {} bytes; S3 allows at most 1024", key.len())));
        }
        Ok(key)
    }

    fn public_url(&self, target: &Target, key: &str) -> String {
        match &self.cfg.public_url_template {
            Some(t) => t
                .replace("{key}", &uri_encode_path(key))
                .replace("{bucket}", &self.cfg.bucket)
                .replace("{region}", &self.cfg.region)
                .replace("{endpoint}", &target.origin),
            None => target.url.clone(),
        }
    }

    fn credentials(&self, ctx: &UploadContext) -> Result<Credentials, UploadError> {
        match &self.cfg.credentials {
            Some(S3Credentials::Static(c)) => Ok(c.clone()),
            Some(S3Credentials::Stored { access_key_id_key, secret_access_key_key, session_token_key }) => {
                Ok(Credentials {
                    access_key_id: ctx.require_secret(access_key_id_key)?,
                    secret_access_key: ctx.require_secret(secret_access_key_key)?,
                    session_token: match session_token_key {
                        Some(k) => Some(ctx.require_secret(k)?),
                        None => None,
                    },
                })
            }
            None => Err(UploadError::Auth { message: "no S3 credentials configured".into() }),
        }
    }

    async fn payload_hash(&self, req: &UploadRequest, ctx: &UploadContext) -> Result<String, UploadError> {
        let unsigned = match self.cfg.payload_signing {
            PayloadSigning::Unsigned => true,
            PayloadSigning::Hashed => false,
            PayloadSigning::Auto => self.https(),
        };
        if unsigned {
            return Ok(UNSIGNED_PAYLOAD.to_owned());
        }
        hash_source(req, ctx).await
    }
}

/// SHA-256 of the payload, streamed in [`CHUNK`]-sized reads (never the whole file at once).
async fn hash_source(req: &UploadRequest, ctx: &UploadContext) -> Result<String, UploadError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    match &req.source {
        crate::types::UploadSource::Bytes { data, .. } => hasher.update(data),
        crate::types::UploadSource::Path(p) => {
            let mut f = tokio::fs::File::open(p)
                .await
                .map_err(|e| UploadError::io(format!("opening {}", p.display()), e))?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                ctx.check_cancelled()?;
                let n = f.read(&mut buf).await.map_err(|e| UploadError::io(format!("hashing {}", p.display()), e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
    }
    Ok(sigv4::hex(&hasher.finalize()))
}

/// Text between `<tag>` and `</tag>` in an S3 XML error body.
fn xml_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{tag}>"))? + start;
    Some(body[start..end].trim())
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Map an S3 error response to an [`UploadError`].
fn map_error(resp: &HttpResponse) -> UploadError {
    let code = xml_tag(&resp.text, "Code").unwrap_or_default();
    let message = xml_tag(&resp.text, "Message").map(xml_unescape).unwrap_or_default();
    let detail = match (code.is_empty(), message.is_empty()) {
        (true, true) => None,
        (false, true) => Some(code.to_owned()),
        (true, false) => Some(message.clone()),
        (false, false) => Some(format!("{code}: {message}")),
    };
    match code {
        "SlowDown" | "RequestLimitExceeded" | "Throttling" | "ThrottlingException" => {
            UploadError::RateLimited { retry_after: http::parse_retry_after(&resp.headers) }
        }
        "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "AccessDenied" | "ExpiredToken" | "InvalidToken"
        | "TokenRefreshRequired" | "AccountProblem" => {
            let hint = match code {
                "SignatureDoesNotMatch" => " (check the secret key, region and endpoint)",
                "ExpiredToken" | "InvalidToken" => " (the session token expired; refresh the credentials)",
                _ => "",
            };
            UploadError::Auth { message: format!("{}{hint}", detail.unwrap_or_default()) }
        }
        "RequestTimeTooSkewed" => UploadError::Auth {
            message: format!("{} (your system clock differs from the server's by more than 15 minutes)", detail.unwrap_or_default()),
        },
        "NoSuchBucket" => UploadError::config(format!("{} (check the bucket name and region)", detail.unwrap_or_default())),
        "PermanentRedirect" | "AuthorizationHeaderMalformed" | "IllegalLocationConstraintException" => {
            UploadError::config(format!("{} (the bucket is in a different region or endpoint; fix the region setting)", detail.unwrap_or_default()))
        }
        _ if matches!(resp.status.as_u16(), 301 | 307) => UploadError::config(
            "the bucket lives in a different region or endpoint (S3 answered with a redirect); fix the region/endpoint setting",
        ),
        _ => resp.to_error(detail),
    }
}

#[async_trait]
impl Uploader for S3Uploader {
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
        let creds = self.credentials(ctx)?;
        let names = NameParser::default();
        let key = self.object_key(req, &names)?;
        let target = self.target(&key)?;
        let payload_hash = self.payload_hash(req, ctx).await?;
        let now = (self.clock)();
        let mime = req.resolved_mime();

        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), target.host_header.clone()),
            ("x-amz-date".into(), sigv4::amz_date(now)),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("content-type".into(), mime),
        ];
        if let Some(t) = &creds.session_token {
            headers.push(("x-amz-security-token".into(), t.clone()));
        }
        for (name, value) in [
            ("x-amz-acl", &self.cfg.acl),
            ("x-amz-storage-class", &self.cfg.storage_class),
            ("cache-control", &self.cfg.cache_control),
            ("content-disposition", &self.cfg.content_disposition),
        ] {
            if let Some(v) = value {
                headers.push((name.into(), v.clone()));
            }
        }
        headers.extend(self.cfg.extra_headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())));

        let signed = sigv4::sign(
            &creds,
            &SigningInput {
                method: "PUT",
                path: &target.path,
                query: "",
                headers: &headers,
                payload_hash: &payload_hash,
                region: &self.cfg.region,
                service: "s3",
                time: now,
                s3: true,
            },
        );

        let mut map = HeaderMap::new();
        for (k, v) in headers.iter().filter(|(k, _)| k != "host") {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| UploadError::config(format!("invalid header name '{k}'")))?;
            let value = HeaderValue::from_str(v).map_err(|_| UploadError::config(format!("invalid value for header '{k}'")))?;
            map.insert(name, value);
        }
        map.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&signed.authorization).map_err(|_| UploadError::config("authorization header is not valid"))?,
        );

        let plan = BodyPlan::raw(Payload::from_request(req).await?);
        let len = plan.content_length();
        let (body, fault) = plan.into_body(ctx).await?;
        let rb = ctx.http.put(&target.url).headers(map).header(CONTENT_LENGTH, len).body(body);
        let resp = http::fetch(ctx, rb, Some(&fault)).await?;
        if !resp.is_success() {
            return Err(map_error(&resp));
        }
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("key".to_owned(), key.clone());
        extra.insert("bucket".to_owned(), self.cfg.bucket.clone());
        if let Some(etag) = resp.header("etag") {
            extra.insert("etag".to_owned(), etag.trim_matches('"').to_owned());
        }
        Ok(UploadResult {
            url: self.public_url(&target, &key),
            thumbnail_url: None,
            deletion_url: None,
            raw_response: truncate_bytes(&resp.text, RAW_RESPONSE_LIMIT),
            uploader_name: self.cfg.name.clone(),
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(cfg: S3Config) -> S3Uploader {
        S3Uploader::new(cfg.with_static_credentials("AK", "SK")).expect("valid")
    }

    #[test]
    fn addressing_and_urls() {
        let t = up(S3Config::aws("my-bucket", "eu-west-1")).target("a b/c.png").unwrap();
        assert_eq!(t.url, "https://my-bucket.s3.eu-west-1.amazonaws.com/a%20b/c.png");
        assert_eq!(t.host_header, "my-bucket.s3.eu-west-1.amazonaws.com");

        // Dotted bucket names cannot use virtual hosting over HTTPS.
        let t = up(S3Config::aws("my.dotted.bucket", "us-east-1")).target("k").unwrap();
        assert_eq!(t.url, "https://s3.us-east-1.amazonaws.com/my.dotted.bucket/k");

        let t = up(S3Config::minio("http://localhost:9000", "pics")).target("dir/x.png").unwrap();
        assert_eq!(t.url, "http://localhost:9000/pics/dir/x.png");
        assert_eq!(t.host_header, "localhost:9000");
        assert_eq!(t.path, "/pics/dir/x.png");

        let t = up(S3Config::cloudflare_r2("acc123", "shots")).target("k").unwrap();
        assert_eq!(t.url, "https://acc123.r2.cloudflarestorage.com/shots/k");

        let mut c = S3Config::minio("https://s3.example.com/base/", "b");
        c.addressing = Addressing::VirtualHosted;
        let t = up(c).target("k").unwrap();
        assert_eq!(t.url, "https://b.s3.example.com/base/k");

        assert_eq!(up(S3Config::backblaze_b2("b", "us-west-004")).target("k").unwrap().url, "https://s3.us-west-004.backblazeb2.com/b/k");
        assert_eq!(up(S3Config::wasabi("b", "us-east-1")).target("k").unwrap().url, "https://s3.wasabisys.com/b/k");
        assert_eq!(up(S3Config::wasabi("b", "eu-central-1")).target("k").unwrap().url, "https://s3.eu-central-1.wasabisys.com/b/k");
    }

    #[test]
    fn object_keys() {
        let mut cfg = S3Config::aws("b", "us-east-1");
        cfg.key_prefix = "shots/".into();
        cfg.key_template = "{stem}-x.{ext}".into();
        let u = up(cfg);
        let req = UploadRequest::from_bytes(vec![1], "a.tar.gz", UploadKind::File);
        assert_eq!(u.object_key(&req, &NameParser::default()).unwrap(), "shots/a.tar-x.gz");
        let req = UploadRequest::from_bytes(vec![1], "/etc/pass\\wd\n", UploadKind::File);
        assert_eq!(u.object_key(&req, &NameParser::default()).unwrap(), "shots/etc/pass_wd_-x.");

        let mut cfg = S3Config::aws("b", "us-east-1");
        cfg.key_template = "%rn{5}/{filename}".into();
        let u = up(cfg);
        let key = u.object_key(&UploadRequest::from_bytes(vec![1], "f%y.png", UploadKind::Image), &NameParser::default()).unwrap();
        assert!(key.len() == 5 + 1 + 6 && key.ends_with("/f%y.png"), "file names are never re-expanded: {key}");

        let mut cfg = S3Config::aws("b", "us-east-1");
        cfg.key_template = "x".repeat(1100);
        let e = up(cfg).object_key(&UploadRequest::text("t"), &NameParser::default()).unwrap_err();
        assert!(e.to_string().contains("1024"));
    }

    #[test]
    fn public_url_template() {
        let mut cfg = S3Config::cloudflare_r2("acc", "shots");
        cfg.public_url_template = Some("https://cdn.example.com/{bucket}/{key}?r={region}".into());
        let u = up(cfg);
        let t = u.target("a b.png").unwrap();
        assert_eq!(u.public_url(&t, "a b.png"), "https://cdn.example.com/shots/a%20b.png?r=auto");
    }

    #[test]
    fn config_validation() {
        assert!(S3Uploader::new(S3Config::aws("", "us-east-1")).is_err());
        assert!(S3Uploader::new(S3Config::aws("a/b", "us-east-1")).is_err());
        assert!(S3Uploader::new(S3Config::aws("b", "")).is_err());
        assert!(S3Uploader::new(S3Config::minio("not a url", "b")).is_err());
        assert!(S3Uploader::new(S3Config::minio("ftp://host", "b")).is_err());
    }

    #[test]
    fn error_mapping() {
        use reqwest::StatusCode;
        let resp = |status: u16, body: &str| HttpResponse {
            status: StatusCode::from_u16(status).unwrap(),
            headers: HeaderMap::new(),
            url: String::new(),
            text: body.into(),
            truncated: false,
        };
        let e = map_error(&resp(403, "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match</Message></Error>"));
        assert!(matches!(&e, UploadError::Auth { message } if message.contains("SignatureDoesNotMatch") && message.contains("secret key")), "{e:?}");
        assert!(matches!(map_error(&resp(503, "<Error><Code>SlowDown</Code></Error>")), UploadError::RateLimited { .. }));
        assert!(matches!(map_error(&resp(404, "<Error><Code>NoSuchBucket</Code><Message>nope</Message></Error>")), UploadError::Config { .. }));
        assert!(matches!(map_error(&resp(301, "<Error><Code>PermanentRedirect</Code></Error>")), UploadError::Config { .. }));
        assert!(matches!(map_error(&resp(307, "")), UploadError::Config { .. }));
        assert!(matches!(map_error(&resp(403, "")), UploadError::Auth { .. }));
        match map_error(&resp(500, "<Error><Code>InternalError</Code><Message>a &amp; b</Message></Error>")) {
            UploadError::Http { status: 500, message: Some(m), .. } => assert_eq!(m, "InternalError: a & b"),
            other => panic!("{other:?}"),
        }
        assert!(map_error(&resp(500, "<html>")).is_retryable());
    }

    #[test]
    fn in_memory_secrets_cannot_be_serialised_but_stored_references_can() {
        let cfg = S3Config::aws("b", "r").with_static_credentials("AKIA", "TOPSECRET");
        assert!(serde_json::to_string(&cfg).is_err(), "static secrets must never reach a config file");
        assert!(!format!("{cfg:?}").contains("TOPSECRET"), "Debug output is redacted");
        let cfg = S3Config::aws("b", "r").with_stored_credentials("s3.id", "s3.secret");
        let json = serde_json::to_string(&cfg).unwrap();
        let back: S3Config = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.credentials, Some(S3Credentials::Stored { ref access_key_id_key, .. }) if access_key_id_key == "s3.id"));
        assert_eq!(back.key_template, "{filename}");
    }
}
