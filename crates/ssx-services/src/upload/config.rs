//! Turns the opaque `[uploaders.<name>]` settings tables into `ssx-upload` uploaders.
//!
//! `ssx-core` treats these tables as opaque; this module owns their schema. Every table has a
//! `type`:
//!
//! | `type` | What | Required keys |
//! |---|---|---|
//! | `imgur` | Imgur API v3 | `client_id` (anonymous) **or** `access_token = "keyring:<name>"` |
//! | `s3` | S3-compatible object store | `bucket`; for `preset = "custom"` (default) also `endpoint` |
//! | `http` | generic PUT/POST upload | `url` |
//! | `local` | copy into a folder / no-op | none |
//! | `sxcu` | a `ShareX` `.sxcu` file | `file` (relative to the config directory) |
//! | `shortener` | URL shortener | `service = "is.gd" \| "v.gd" \| "tinyurl"` or `endpoint` |
//!
//! Anything that looks like a credential must be a `keyring:<name>` reference (the core
//! validator enforces this on save); the referenced value is resolved through the
//! [`SecretStore`](ssx_upload::SecretStore) **at upload time**, so rotating a secret needs no
//! restart. Unknown keys are rejected with the list of valid ones, because a typo such as
//! `bukcet` would otherwise silently upload to the wrong place.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use serde::Deserialize;
use ssx_upload::{
    Uploader, UrlShortener,
    http_uploader::{
        HttpAuth, HttpBody, HttpUploader, HttpUploaderConfig, ResultUrl, UploadMethod,
    },
    imgur::{ImgurConfig, ImgurUploader, ThumbnailSize},
    local::LocalUploader,
    s3::{Addressing, PayloadSigning, S3Config, S3Credentials, S3Uploader},
    shorten::{HttpShortener, HttpShortenerConfig, ShortenMethod, ShortenResponse},
    sxcu::{CustomUploader, SxcuUploader},
};

/// Prefix of a secret reference (same constant `ssx-core` validates against).
pub const KEYRING_PREFIX: &str = ssx_core::settings::KEYRING_PREFIX;

/// A successfully built entry.
pub struct Built {
    /// Human-readable kind (`"imgur"`, `"s3"`, ...).
    pub kind: &'static str,
    /// The uploader, if this entry uploads.
    pub uploader: Option<Arc<dyn Uploader>>,
    /// The shortener, if this entry shortens.
    pub shortener: Option<Arc<dyn UrlShortener>>,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built")
            .field("kind", &self.kind)
            .field("uploader", &self.uploader.is_some())
            .field("shortener", &self.shortener.is_some())
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Table {
    Imgur(ImgurToml),
    S3(S3Toml),
    Http(HttpToml),
    Local(LocalToml),
    Sxcu(SxcuToml),
    Shortener(ShortenerToml),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImgurToml {
    client_id: Option<String>,
    access_token: Option<String>,
    title: Option<String>,
    description: Option<String>,
    album: Option<String>,
    /// `small`, `thumb`, `medium`, `large` or `huge`.
    thumbnail_size: Option<String>,
    api_base: Option<String>,
    image_base: Option<String>,
    site_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct S3Toml {
    /// `aws`, `r2`, `b2`, `wasabi`, `minio` or `custom` (default).
    preset: Option<String>,
    bucket: String,
    region: Option<String>,
    endpoint: Option<String>,
    account_id: Option<String>,
    /// `auto` (default), `path` or `virtual`.
    addressing: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    key_prefix: Option<String>,
    key_template: Option<String>,
    acl: Option<String>,
    storage_class: Option<String>,
    cache_control: Option<String>,
    content_disposition: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    public_url_template: Option<String>,
    /// `auto` (default), `unsigned` or `hashed`.
    payload_signing: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpToml {
    url: String,
    /// `put` (default), `post` or `patch`.
    method: Option<String>,
    /// `raw` (default) or `multipart`.
    body: Option<String>,
    /// Multipart field that carries the file (default `file`).
    field: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// `none` (default), `bearer`, `basic` or `header`.
    auth: Option<String>,
    /// `keyring:<name>` holding the token / password / header value.
    auth_secret: Option<String>,
    /// User name for `basic`.
    auth_user: Option<String>,
    /// Header name for `header`.
    auth_header: Option<String>,
    /// `request_url` (default), `body`, `header:<Name>`, `json:<pointer>` or
    /// `template:<text>`.
    result: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalToml {
    dir: Option<PathBuf>,
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SxcuToml {
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShortenerToml {
    /// `is.gd`, `v.gd`, `tinyurl`; omit for a custom endpoint.
    service: Option<String>,
    endpoint: Option<String>,
    /// `get` (default) or `post`.
    method: Option<String>,
    /// Parameter carrying the long URL (default `url`).
    url_param: Option<String>,
    /// `text` (default) or `json:<pointer>`.
    response: Option<String>,
    #[serde(default)]
    params: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Reads a `keyring:<name>` reference and returns the secret's name.
fn secret_ref(key: &str, value: &str) -> Result<String, String> {
    match value.strip_prefix(KEYRING_PREFIX) {
        Some(name) if !name.trim().is_empty() => Ok(name.trim().to_owned()),
        Some(_) => Err(format!("`{key}` has an empty keyring reference; write \"keyring:<name>\"")),
        None => Err(format!(
            "`{key}` must be a keyring reference like \"keyring:my-{}\", never the secret itself \
             (save the value with `ssx uploaders secret set my-{}`)",
            key.replace('_', "-"),
            key.replace('_', "-"),
        )),
    }
}

fn pick<T: Copy>(
    key: &str,
    value: Option<&str>,
    options: &[(&str, T)],
) -> Result<Option<T>, String> {
    let Some(v) = value else { return Ok(None) };
    options.iter().find(|(n, _)| n.eq_ignore_ascii_case(v)).map(|(_, t)| Some(*t)).ok_or_else(
        || {
            format!(
                "`{key}` = {v:?} is not valid; use one of: {}",
                options.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            )
        },
    )
}

fn pairs(map: BTreeMap<String, String>) -> Vec<(String, String)> {
    map.into_iter().collect()
}

fn upload_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Builds the uploader described by `table`. `config_dir` resolves relative `.sxcu` paths.
/// The error is a complete, user-facing sentence.
pub fn build(
    name: &str,
    table: &toml::Table,
    config_dir: &std::path::Path,
) -> Result<Built, String> {
    if !table.get("type").is_some_and(toml::Value::is_str) {
        return Err(format!(
            "[uploaders.{name}] has no `type`; add one of: imgur, s3, http, local, sxcu, shortener"
        ));
    }
    let parsed: Table = table
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("[uploaders.{name}]: {}", e.message()))?;
    let wrap = |e: String| format!("[uploaders.{name}]: {e}");
    match parsed {
        Table::Imgur(t) => build_imgur(t).map_err(wrap),
        Table::S3(t) => build_s3(name, t).map_err(wrap),
        Table::Http(t) => build_http(name, t).map_err(wrap),
        Table::Local(t) => Ok(build_local(t)),
        Table::Sxcu(t) => build_sxcu(t, config_dir).map_err(wrap),
        Table::Shortener(t) => build_shortener(name, t).map_err(wrap),
    }
}

fn build_imgur(t: ImgurToml) -> Result<Built, String> {
    let mut cfg = match (&t.access_token, &t.client_id) {
        (Some(tok), _) => ImgurConfig::bearer_stored(secret_ref("access_token", tok)?),
        (None, Some(id)) if !id.trim().is_empty() => ImgurConfig::anonymous(id.trim()),
        _ => {
            return Err("needs `client_id` (anonymous uploads: register an application at \
                        https://api.imgur.com/oauth2/addclient) or `access_token = \"keyring:<name>\"`"
                .to_owned());
        }
    };
    cfg.title = t.title;
    cfg.description = t.description;
    cfg.album = t.album;
    if let Some(size) = pick(
        "thumbnail_size",
        t.thumbnail_size.as_deref(),
        &[
            ("small", ThumbnailSize::Small),
            ("thumb", ThumbnailSize::Thumb),
            ("medium", ThumbnailSize::Medium),
            ("large", ThumbnailSize::Large),
            ("huge", ThumbnailSize::Huge),
        ],
    )? {
        cfg.thumbnail_size = size;
    }
    if let Some(v) = t.api_base {
        cfg.api_base = v;
    }
    if let Some(v) = t.image_base {
        cfg.image_base = v;
    }
    if let Some(v) = t.site_base {
        cfg.site_base = v;
    }
    let up = ImgurUploader::new(cfg).map_err(upload_err)?;
    Ok(Built { kind: "imgur", uploader: Some(Arc::new(up)), shortener: None })
}

fn build_s3(name: &str, t: S3Toml) -> Result<Built, String> {
    let preset = t.preset.as_deref().unwrap_or("custom").to_ascii_lowercase();
    let need = |what: &str, v: &Option<String>| {
        v.clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("preset {preset:?} needs `{what}`"))
    };
    let mut cfg = match preset.as_str() {
        "aws" => S3Config::aws(&t.bucket, t.region.as_deref().unwrap_or("us-east-1")),
        "r2" => S3Config::cloudflare_r2(&need("account_id", &t.account_id)?, &t.bucket),
        "b2" => S3Config::backblaze_b2(&t.bucket, &need("region", &t.region)?),
        "wasabi" => S3Config::wasabi(&t.bucket, t.region.as_deref().unwrap_or("us-east-1")),
        "minio" | "custom" => S3Config::minio(&need("endpoint", &t.endpoint)?, &t.bucket),
        other => {
            return Err(format!(
                "`preset` = {other:?} is not valid; use one of: aws, r2, b2, wasabi, minio, custom"
            ));
        }
    };
    name.clone_into(&mut cfg.name);
    if let Some(r) = t.region.filter(|r| !r.is_empty()) {
        cfg.region = r;
    }
    if let Some(e) = t.endpoint.filter(|e| !e.is_empty()) {
        cfg.endpoint = Some(e);
    }
    if let Some(a) = pick(
        "addressing",
        t.addressing.as_deref(),
        &[
            ("auto", Addressing::Auto),
            ("path", Addressing::PathStyle),
            ("virtual", Addressing::VirtualHosted),
        ],
    )? {
        cfg.addressing = a;
    }
    if let Some(p) = pick(
        "payload_signing",
        t.payload_signing.as_deref(),
        &[
            ("auto", PayloadSigning::Auto),
            ("unsigned", PayloadSigning::Unsigned),
            ("hashed", PayloadSigning::Hashed),
        ],
    )? {
        cfg.payload_signing = p;
    }
    match (&t.access_key_id, &t.secret_access_key) {
        (Some(id), Some(secret)) => {
            cfg.credentials = Some(S3Credentials::Stored {
                access_key_id_key: secret_ref("access_key_id", id)?,
                secret_access_key_key: secret_ref("secret_access_key", secret)?,
                session_token_key: t
                    .session_token
                    .as_deref()
                    .map(|s| secret_ref("session_token", s))
                    .transpose()?,
            });
        }
        (None, None) => {}
        _ => {
            return Err("`access_key_id` and `secret_access_key` must be set together \
                        (both as keyring references)"
                .to_owned());
        }
    }
    if let Some(v) = t.key_prefix {
        cfg.key_prefix = v;
    }
    if let Some(v) = t.key_template {
        cfg.key_template = v;
    }
    cfg.acl = t.acl.or(cfg.acl);
    cfg.storage_class = t.storage_class.or(cfg.storage_class);
    cfg.cache_control = t.cache_control.or(cfg.cache_control);
    cfg.content_disposition = t.content_disposition.or(cfg.content_disposition);
    cfg.extra_headers = pairs(t.headers);
    cfg.public_url_template = t.public_url_template.or(cfg.public_url_template);
    let up = S3Uploader::new(cfg).map_err(upload_err)?;
    Ok(Built { kind: "s3", uploader: Some(Arc::new(up)), shortener: None })
}

fn build_http(name: &str, t: HttpToml) -> Result<Built, String> {
    let mut cfg = HttpUploaderConfig::put(name, &t.url);
    if let Some(m) = pick(
        "method",
        t.method.as_deref(),
        &[("put", UploadMethod::Put), ("post", UploadMethod::Post), ("patch", UploadMethod::Patch)],
    )? {
        cfg.method = m;
    }
    cfg.headers = pairs(t.headers);
    let multipart =
        pick("body", t.body.as_deref(), &[("raw", false), ("multipart", true)])?.unwrap_or(false);
    if multipart {
        cfg.body = HttpBody::Multipart {
            field: t.field.unwrap_or_else(|| "file".to_owned()),
            fields: pairs(t.fields),
        };
        // A multipart POST answering with the URL in its body is by far the common shape.
        if t.method.is_none() {
            cfg.method = UploadMethod::Post;
        }
        cfg.result = ResultUrl::Body;
    } else if t.field.is_some() || !t.fields.is_empty() {
        return Err("`field` / `fields` only apply with body = \"multipart\"".to_owned());
    }
    let auth = pick(
        "auth",
        t.auth.as_deref(),
        &[("none", 0), ("bearer", 1), ("basic", 2), ("header", 3)],
    )?
    .unwrap_or(0);
    let secret = || {
        t.auth_secret
            .as_deref()
            .ok_or_else(|| "this `auth` needs `auth_secret = \"keyring:<name>\"`".to_owned())
            .and_then(|s| secret_ref("auth_secret", s))
    };
    cfg.auth = match auth {
        1 => HttpAuth::Bearer { secret_key: secret()? },
        2 => HttpAuth::Basic {
            username: t.auth_user.clone().ok_or("auth = \"basic\" needs `auth_user`")?,
            secret_key: secret()?,
        },
        3 => HttpAuth::Header {
            name: t.auth_header.clone().ok_or("auth = \"header\" needs `auth_header`")?,
            secret_key: secret()?,
        },
        _ => HttpAuth::None,
    };
    if let Some(r) = t.result.as_deref() {
        cfg.result = parse_result(r)?;
    }
    let up = HttpUploader::new(cfg).map_err(upload_err)?;
    Ok(Built { kind: "http", uploader: Some(Arc::new(up)), shortener: None })
}

fn parse_result(r: &str) -> Result<ResultUrl, String> {
    let (head, tail) = r.split_once(':').map_or((r, ""), |(h, t)| (h, t));
    match (head.to_ascii_lowercase().as_str(), tail) {
        ("request_url", "") => Ok(ResultUrl::RequestUrl),
        ("body", "") => Ok(ResultUrl::Body),
        ("header", h) if !h.is_empty() => Ok(ResultUrl::Header(h.to_owned())),
        ("json", p) if !p.is_empty() => Ok(ResultUrl::JsonPointer(p.to_owned())),
        ("template", t) if !t.is_empty() => Ok(ResultUrl::Template(t.to_owned())),
        _ => Err(format!(
            "`result` = {r:?} is not valid; use request_url, body, header:<Name>, json:</pointer> or template:<text>"
        )),
    }
}

fn build_local(t: LocalToml) -> Built {
    let mut up = match t.dir {
        Some(d) => LocalUploader::to_directory(d),
        None => LocalUploader::noop(),
    };
    if let Some(b) = t.base_url {
        up = up.with_base_url(b);
    }
    Built { kind: "local", uploader: Some(Arc::new(up)), shortener: None }
}

fn build_sxcu(t: SxcuToml, config_dir: &std::path::Path) -> Result<Built, String> {
    let path = if t.file.is_absolute() { t.file } else { config_dir.join(t.file) };
    load_sxcu_file(&path)
}

/// Loads and validates one `.sxcu` file.
pub fn load_sxcu_file(path: &std::path::Path) -> Result<Built, String> {
    let (def, _warnings) = read_sxcu(path)?;
    let up = Arc::new(SxcuUploader::new(def).map_err(upload_err)?);
    Ok(Built { kind: "sxcu", uploader: Some(up.clone()), shortener: Some(up) })
}

/// Largest `.sxcu` file accepted (real ones are a few hundred bytes).
pub const MAX_SXCU_BYTES: u64 = 1024 * 1024;

/// Reads, parses and checks a `.sxcu` file, returning the definition and any warnings.
pub fn read_sxcu(path: &std::path::Path) -> Result<(CustomUploader, Vec<String>), String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_SXCU_BYTES {
        return Err(format!(
            "{} is {} bytes; a .sxcu file larger than {MAX_SXCU_BYTES} bytes is not accepted",
            path.display(),
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let def =
        CustomUploader::from_json_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let report = def.check();
    if !report.errors.is_empty() {
        return Err(format!("{}: {}", path.display(), report.errors.join("; ")));
    }
    Ok((def, report.warnings))
}

fn build_shortener(name: &str, t: ShortenerToml) -> Result<Built, String> {
    let short: HttpShortener = match (t.service.as_deref(), &t.endpoint) {
        (Some(s), ep) => {
            let ep = ep.as_deref();
            match s.to_ascii_lowercase().as_str() {
                "is.gd" => HttpShortener::is_gd(ep),
                "v.gd" => HttpShortener::v_gd(ep),
                "tinyurl" => HttpShortener::tiny_url(ep),
                other => {
                    return Err(format!(
                        "`service` = {other:?} is not built in; use is.gd, v.gd or tinyurl, or omit \
                         `service` and give `endpoint`"
                    ));
                }
            }
        }
        (None, Some(endpoint)) => {
            let method = pick(
                "method",
                t.method.as_deref(),
                &[("get", ShortenMethod::Get), ("post", ShortenMethod::Post)],
            )?
            .unwrap_or(ShortenMethod::Get);
            let response = match t.response.as_deref() {
                None | Some("text") => ShortenResponse::PlainText,
                Some(r) => match r.strip_prefix("json:") {
                    Some(p) if !p.is_empty() => ShortenResponse::JsonPointer(p.to_owned()),
                    _ => {
                        return Err(format!(
                            "`response` = {r:?} is not valid; use text or json:</pointer>"
                        ));
                    }
                },
            };
            HttpShortener::new(HttpShortenerConfig {
                name: name.to_owned(),
                endpoint: endpoint.clone(),
                method,
                url_param: t.url_param.clone().unwrap_or_else(|| "url".to_owned()),
                params: pairs(t.params),
                headers: pairs(t.headers),
                response,
            })
            .map_err(upload_err)?
        }
        (None, None) => return Err("needs `service` or `endpoint`".to_owned()),
    };
    Ok(Built { kind: "shortener", uploader: None, shortener: Some(Arc::new(short)) })
}

#[cfg(test)]
mod tests {
    use ssx_upload::UploadKind;

    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    fn built(text: &str) -> Result<Built, String> {
        build("t", &table(text), std::path::Path::new("/nonexistent"))
    }

    #[test]
    fn imgur_anonymous_and_bearer() {
        let b = built("type = \"imgur\"\nclient_id = \"abc\"").unwrap();
        assert_eq!(b.kind, "imgur");
        let up = b.uploader.unwrap();
        assert!(up.supports(UploadKind::Image) && up.supports(UploadKind::Video));
        assert!(!up.supports(UploadKind::Text));
        assert!(built("type = \"imgur\"\naccess_token = \"keyring:imgur\"").is_ok());
        let e = built("type = \"imgur\"").unwrap_err();
        assert!(e.contains("client_id") && e.contains("[uploaders.t]"), "{e}");
        let e = built("type = \"imgur\"\naccess_token = \"plaintext\"").unwrap_err();
        assert!(e.contains("keyring reference"), "{e}");
        assert!(
            built("type = \"imgur\"\nclient_id = \"a\"\nthumbnail_size = \"gigantic\"").is_err()
        );
    }

    #[test]
    fn s3_presets_and_credentials() {
        let b = built(
            r#"type = "s3"
preset = "aws"
bucket = "shots"
region = "eu-west-1"
access_key_id = "keyring:aws-id"
secret_access_key = "keyring:aws-secret"
public_url_template = "https://cdn.example.com/{key}"
headers = { "x-amz-meta-app" = "ssx" }"#,
        )
        .unwrap();
        assert_eq!(b.kind, "s3");
        assert!(b.uploader.unwrap().supports(UploadKind::File));

        assert!(
            built("type = \"s3\"\npreset = \"r2\"\nbucket = \"b\"\naccount_id = \"acc\"").is_ok()
        );
        let e = built("type = \"s3\"\npreset = \"r2\"\nbucket = \"b\"").unwrap_err();
        assert!(e.contains("account_id"), "{e}");
        let e = built("type = \"s3\"\nbucket = \"b\"").unwrap_err();
        assert!(e.contains("endpoint"), "custom needs an endpoint: {e}");
        assert!(
            built("type = \"s3\"\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:9000\"").is_ok()
        );
        let e = built(
            "type = \"s3\"\nbucket = \"b\"\nendpoint = \"http://h\"\naccess_key_id = \"keyring:x\"",
        )
        .unwrap_err();
        assert!(e.contains("together"), "{e}");
        let e = built(
            "type = \"s3\"\nbucket = \"b\"\nendpoint = \"http://h\"\naccess_key_id = \"AKIA\"\nsecret_access_key = \"keyring:s\"",
        )
        .unwrap_err();
        assert!(e.contains("keyring reference"), "plain-text key id rejected: {e}");
        assert!(built("type = \"s3\"\nbucket = \"a/b\"\nendpoint = \"http://h\"").is_err());
    }

    #[test]
    fn http_uploader_shapes() {
        let b = built(
            r#"type = "http"
url = "https://files.example.com/upload/{filename}"
auth = "bearer"
auth_secret = "keyring:files-token"
result = "template:https://cdn.example.com/{filename}""#,
        )
        .unwrap();
        assert_eq!(b.kind, "http");
        assert!(built(
            "type = \"http\"\nurl = \"https://h/u\"\nbody = \"multipart\"\nfield = \"f\"\nresult = \"json:/data/link\""
        )
        .is_ok());
        for bad in [
            "type = \"http\"\nurl = \"ftp://h/u\"",
            "type = \"http\"\nurl = \"https://h/u\"\nauth = \"bearer\"",
            "type = \"http\"\nurl = \"https://h/u\"\nauth = \"basic\"\nauth_secret = \"keyring:x\"",
            "type = \"http\"\nurl = \"https://h/u\"\nresult = \"nonsense\"",
            "type = \"http\"\nurl = \"https://h/u\"\nfield = \"f\"",
            "type = \"http\"\nurl = \"https://h/u\"\nmethod = \"delete\"",
        ] {
            assert!(built(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn local_and_shorteners() {
        let b = built("type = \"local\"").unwrap();
        assert!(b.uploader.unwrap().supports(UploadKind::Image));
        for svc in ["is.gd", "v.gd", "tinyurl"] {
            let b = built(&format!("type = \"shortener\"\nservice = \"{svc}\"")).unwrap();
            assert!(b.uploader.is_none() && b.shortener.is_some(), "{svc}");
        }
        let b = built(
            "type = \"shortener\"\nendpoint = \"https://s.example.com/api\"\nmethod = \"post\"\nresponse = \"json:/short\"",
        )
        .unwrap();
        assert_eq!(b.shortener.unwrap().name(), "t");
        assert!(built("type = \"shortener\"").is_err());
        assert!(built("type = \"shortener\"\nservice = \"bit.ly\"").is_err());
    }

    #[test]
    fn unknown_keys_and_types_are_reported_helpfully() {
        let e = built("type = \"s3\"\nbukcet = \"typo\"").unwrap_err();
        assert!(e.contains("bukcet") && e.contains("bucket"), "lists the valid keys: {e}");
        let e = built("client_id = \"x\"").unwrap_err();
        assert!(e.contains("`type`"), "{e}");
        let e = built("type = \"ftp\"").unwrap_err();
        assert!(e.contains("ftp"), "{e}");
        let e = built("type = 5").unwrap_err();
        assert!(e.contains("`type`"), "{e}");
    }

    #[test]
    fn sxcu_files_resolve_against_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("my.sxcu"),
            r#"{"Version":"14.0.0","Name":"mine","DestinationType":"ImageUploader, FileUploader",
                "RequestMethod":"POST","RequestURL":"http://127.0.0.1:9/up","Body":"MultipartFormData",
                "FileFormName":"file","URL":"{json:url}"}"#,
        )
        .unwrap();
        let t = table("type = \"sxcu\"\nfile = \"my.sxcu\"");
        let b = build("x", &t, dir.path()).unwrap();
        assert_eq!(b.kind, "sxcu");
        assert!(b.uploader.as_ref().unwrap().supports(UploadKind::Image));
        assert!(!b.uploader.unwrap().supports(UploadKind::Text));
        let e =
            build("x", &table("type = \"sxcu\"\nfile = \"missing.sxcu\""), dir.path()).unwrap_err();
        assert!(e.contains("missing.sxcu"), "{e}");
    }

    #[test]
    fn broken_sxcu_files_are_rejected_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.sxcu");
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_sxcu_file(&p).unwrap_err().contains("bad.sxcu"));
        std::fs::write(&p, r#"{"Version":"14.0.0","Name":"no url"}"#).unwrap();
        assert!(load_sxcu_file(&p).is_err(), "missing RequestURL");
        std::fs::write(&p, vec![b' '; (MAX_SXCU_BYTES + 1) as usize]).unwrap();
        assert!(load_sxcu_file(&p).unwrap_err().contains("larger"));
    }
}
