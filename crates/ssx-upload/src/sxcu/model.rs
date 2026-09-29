//! The `.sxcu` document model: tolerant parsing, migration of old files, validation and
//! round-trip serialisation.
//!
//! Field names and semantics come from ShareX's `CustomUploaderItem`. Tolerance rules:
//! keys match case-insensitively; enum values may be names (any case), comma separated
//! flag lists, arrays of names, or the numeric value; scalar JSON values in string maps
//! (`"Arguments": {"n": 5}`) are stringified; `null` means "unset"; unknown keys are kept
//! verbatim in [`CustomUploader::extra`] and written back on serialisation.
//!
//! Like ShareX, loading moves any query string in `RequestURL` into `Parameters` and
//! migrates the pre-13.7.2 `$function$` syntax to `{function}`.

use std::cmp::Ordering;
use std::fmt;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::template::{self, Template};
use crate::types::UploadKind;

/// Version written into migrated files.
pub const CURRENT_VERSION: &str = "16.1.0";

/// Last version that used the legacy `$function$` template syntax.
const LEGACY_SYNTAX_MAX: &str = "13.7.1";
/// Versions up to and including this one are rejected by ShareX itself.
const UNSUPPORTED_MAX: &str = "12.3.1";

/// Problems reading a `.sxcu` file.
#[derive(Debug, thiserror::Error)]
pub enum SxcuError {
    /// Not JSON.
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// JSON but not an object.
    #[error("a .sxcu file must contain a JSON object")]
    NotAnObject,
    /// A field has the wrong JSON type.
    #[error("field '{field}' must be {expected}")]
    BadField {
        /// Field name.
        field: String,
        /// What was expected, e.g. "a string".
        expected: &'static str,
    },
    /// An enum field holds an unknown value.
    #[error("field '{field}' has unknown value '{value}' (expected one of: {expected})")]
    BadEnum {
        /// Field name.
        field: &'static str,
        /// Offending value.
        value: String,
        /// Accepted names.
        expected: &'static str,
    },
    /// The file predates the format ShareX still reads.
    #[error("unsupported custom uploader version '{0}' (ShareX 12.3.1 and older files are not readable)")]
    UnsupportedVersion(String),
}

/// `DestinationType`: a set of ShareX destination categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct DestinationType(u8);

impl DestinationType {
    /// No destination selected.
    pub const NONE: Self = Self(0);
    /// `ImageUploader`.
    pub const IMAGE_UPLOADER: Self = Self(1);
    /// `TextUploader`.
    pub const TEXT_UPLOADER: Self = Self(1 << 1);
    /// `FileUploader`.
    pub const FILE_UPLOADER: Self = Self(1 << 2);
    /// `URLShortener`.
    pub const URL_SHORTENER: Self = Self(1 << 3);
    /// `URLSharingService`.
    pub const URL_SHARING_SERVICE: Self = Self(1 << 4);

    const ALL_BITS: u8 = 0b1_1111;
    const NAMES: [(&'static str, Self); 5] = [
        ("ImageUploader", Self::IMAGE_UPLOADER),
        ("TextUploader", Self::TEXT_UPLOADER),
        ("FileUploader", Self::FILE_UPLOADER),
        ("URLShortener", Self::URL_SHORTENER),
        ("URLSharingService", Self::URL_SHARING_SERVICE),
    ];

    /// Union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    /// Whether any flag of `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// No flags.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Canonical flag names, in ShareX order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES.iter().filter(|(_, f)| self.contains(*f)).map(|(n, _)| *n).collect()
    }

    fn parse_name(name: &str) -> Option<Self> {
        let n = name.trim();
        if n.eq_ignore_ascii_case("none") || n.is_empty() {
            return Some(Self::NONE);
        }
        Self::NAMES.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, f)| *f)
    }
}

impl fmt::Display for DestinationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() { f.write_str("None") } else { f.write_str(&self.names().join(", ")) }
    }
}

/// HTTP method (`RequestMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST (the ShareX default)
    #[default]
    Post,
    /// PUT
    Put,
    /// PATCH
    Patch,
    /// DELETE
    Delete,
}

impl HttpMethod {
    const NAMES: [(&'static str, Self); 5] = [
        ("GET", Self::Get),
        ("POST", Self::Post),
        ("PUT", Self::Put),
        ("PATCH", Self::Patch),
        ("DELETE", Self::Delete),
    ];

    /// Upper-case name as written in `.sxcu` files.
    pub fn as_str(self) -> &'static str {
        Self::NAMES.iter().find(|(_, m)| *m == self).map_or("POST", |(n, _)| n)
    }

    /// The reqwest equivalent.
    pub fn to_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Patch => reqwest::Method::PATCH,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

/// Request body type (`Body`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyType {
    /// No body.
    #[default]
    None,
    /// `multipart/form-data` (files and arguments).
    MultipartFormData,
    /// `application/x-www-form-urlencoded` (arguments).
    FormUrlEncoded,
    /// `application/json` (`Data`).
    Json,
    /// `application/xml` (`Data`).
    Xml,
    /// The raw file bytes.
    Binary,
}

impl BodyType {
    const NAMES: [(&'static str, Self); 6] = [
        ("None", Self::None),
        ("MultipartFormData", Self::MultipartFormData),
        ("FormURLEncoded", Self::FormUrlEncoded),
        ("JSON", Self::Json),
        ("XML", Self::Xml),
        ("Binary", Self::Binary),
    ];

    /// Name as written in `.sxcu` files.
    pub fn as_str(self) -> &'static str {
        Self::NAMES.iter().find(|(_, b)| *b == self).map_or("None", |(n, _)| n)
    }

    /// The `Content-Type` ShareX sends for this body (multipart adds its boundary).
    pub fn content_type(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::MultipartFormData => Some("multipart/form-data"),
            Self::FormUrlEncoded => Some("application/x-www-form-urlencoded"),
            Self::Json => Some("application/json"),
            Self::Xml => Some("application/xml"),
            Self::Binary => Some("application/octet-stream"),
        }
    }
}

/// A parsed `.sxcu` custom uploader definition.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CustomUploader {
    /// `Version`: the ShareX version that wrote the file (drives migration).
    pub version: String,
    /// `Name`.
    pub name: String,
    /// `DestinationType`.
    pub destination_type: DestinationType,
    /// `RequestMethod`.
    pub request_method: HttpMethod,
    /// `RequestURL` (a template; `{filename}`/`{input}` are URL-encoded here).
    pub request_url: String,
    /// `Parameters`: query string parameters (name-parser and template expanded).
    pub parameters: IndexMap<String, String>,
    /// `Headers`.
    pub headers: IndexMap<String, String>,
    /// `Body`.
    pub body: BodyType,
    /// `Arguments`: form fields for multipart / URL-encoded bodies.
    pub arguments: IndexMap<String, String>,
    /// `FileFormName`: multipart field carrying the file.
    pub file_form_name: String,
    /// `Data`: JSON/XML body text (`{input}`/`{filename}` and `%` codes only).
    pub data: String,
    /// `URL`: response template producing the public URL (empty: whole response).
    pub url: String,
    /// `ThumbnailURL`.
    pub thumbnail_url: String,
    /// `DeletionURL`.
    pub deletion_url: String,
    /// `ErrorMessage`: response template evaluated on failure.
    pub error_message: String,
    /// Unknown top-level keys, preserved verbatim.
    pub extra: Map<String, Value>,
}

const KNOWN_KEYS: [&str; 15] = [
    "Version", "Name", "DestinationType", "RequestMethod", "RequestURL", "Parameters", "Headers",
    "Body", "Arguments", "FileFormName", "Data", "URL", "ThumbnailURL", "DeletionURL",
    "ErrorMessage",
];

fn bad(field: &str, expected: &'static str) -> SxcuError {
    SxcuError::BadField { field: field.to_owned(), expected }
}

fn scalar_to_string(field: &str, v: &Value) -> Result<String, SxcuError> {
    match v {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b { "True" } else { "False" }.to_owned()),
        _ => Err(bad(field, "a string")),
    }
}

fn map_field(field: &str, v: &Value) -> Result<IndexMap<String, String>, SxcuError> {
    match v {
        Value::Null => Ok(IndexMap::new()),
        Value::Object(o) => {
            let mut out = IndexMap::new();
            for (k, val) in o {
                out.insert(k.clone(), scalar_to_string(&format!("{field}.{k}"), val)?);
            }
            Ok(out)
        }
        _ => Err(bad(field, "an object of string values")),
    }
}

fn parse_enum<T: Copy>(
    field: &'static str,
    v: &Value,
    names: &[(&str, T)],
    expected: &'static str,
) -> Result<Option<T>, SxcuError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => names
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(s.trim()))
            .map(|(_, t)| Some(*t))
            .ok_or_else(|| SxcuError::BadEnum { field, value: s.clone(), expected }),
        Value::Number(n) => n
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| names.get(i))
            .map(|(_, t)| Some(*t))
            .ok_or_else(|| SxcuError::BadEnum { field, value: n.to_string(), expected }),
        _ => Err(bad(field, "a string")),
    }
}

fn parse_destination(v: &Value) -> Result<DestinationType, SxcuError> {
    const EXPECTED: &str = "None, ImageUploader, TextUploader, FileUploader, URLShortener, URLSharingService";
    let err = |value: String| SxcuError::BadEnum { field: "DestinationType", value, expected: EXPECTED };
    match v {
        Value::Null => Ok(DestinationType::NONE),
        Value::Number(n) => {
            let bits = n.as_u64().ok_or_else(|| err(n.to_string()))?;
            if bits > u64::from(DestinationType::ALL_BITS) {
                return Err(err(n.to_string()));
            }
            Ok(DestinationType(u8::try_from(bits).map_err(|_| err(n.to_string()))?))
        }
        Value::String(s) => s.split(',').try_fold(DestinationType::NONE, |acc, part| {
            DestinationType::parse_name(part).map(|f| acc.union(f)).ok_or_else(|| err(part.trim().to_owned()))
        }),
        Value::Array(items) => items.iter().try_fold(DestinationType::NONE, |acc, item| {
            let name = item.as_str().ok_or_else(|| bad("DestinationType", "a string, number or array of strings"))?;
            DestinationType::parse_name(name).map(|f| acc.union(f)).ok_or_else(|| err(name.to_owned()))
        }),
        _ => Err(bad("DestinationType", "a string, number or array of strings")),
    }
}

/// Compare dotted version strings numerically; missing/non-numeric parts count as 0.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| -> Vec<u64> { s.trim().split('.').map(|p| p.trim().parse().unwrap_or(0)).collect() };
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let o = pa.get(i).unwrap_or(&0).cmp(pb.get(i).unwrap_or(&0));
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Convert the legacy `$function$` syntax to `{function}`, escaping literal braces.
///
/// Mirrors ShareX's `MigrateOldSyntax`, including its quirk of dropping a backslash *and
/// the character after it*.
fn migrate_old_syntax(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut start = true;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '$' => {
                out.push(if start { '{' } else { '}' });
                start = !start;
            }
            '\\' => {
                chars.next();
            }
            '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

pub(super) fn replace_ci(haystack: &str, needle: &str, with: &str) -> String {
    let lower = haystack.to_lowercase();
    let needle_l = needle.to_lowercase();
    // Lowercasing can change byte lengths for exotic characters; only use the fast path
    // when it did not.
    if lower.len() != haystack.len() {
        return haystack.replace(needle, with);
    }
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(&needle_l) {
        out.push_str(&haystack[last..i]);
        out.push_str(with);
        last = i + needle.len();
    }
    out.push_str(&haystack[last..]);
    out
}

/// Split a `?query` (outside `{}` calls and not escaped) off `url`.
fn split_query(url: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (i, c) in url.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '?' if depth == 0 => return Some((&url[..i], &url[i + 1..])),
            _ => {}
        }
    }
    None
}

impl CustomUploader {
    /// Parse and normalise a `.sxcu` document.
    pub fn from_json_str(text: &str) -> Result<Self, SxcuError> {
        let text = text.trim_start_matches('\u{feff}');
        let value: Value = serde_json::from_str(text)?;
        Self::from_json_value(&value)
    }

    /// Parse and normalise an already parsed document.
    pub fn from_json_value(value: &Value) -> Result<Self, SxcuError> {
        let Value::Object(obj) = value else { return Err(SxcuError::NotAnObject) };
        let mut u = Self::default();
        for (key, v) in obj {
            let canonical = KNOWN_KEYS.iter().find(|k| k.eq_ignore_ascii_case(key));
            match canonical.copied() {
                Some("Version") => u.version = scalar_to_string(key, v)?,
                Some("Name") => u.name = scalar_to_string(key, v)?,
                Some("DestinationType") => u.destination_type = parse_destination(v)?,
                Some("RequestMethod") => {
                    if let Some(m) = parse_enum("RequestMethod", v, &HttpMethod::NAMES, "GET, POST, PUT, PATCH, DELETE")? {
                        u.request_method = m;
                    }
                }
                Some("RequestURL") => u.request_url = scalar_to_string(key, v)?,
                Some("Parameters") => u.parameters = map_field(key, v)?,
                Some("Headers") => u.headers = map_field(key, v)?,
                Some("Body") => {
                    u.body = parse_enum(
                        "Body",
                        v,
                        &BodyType::NAMES,
                        "None, MultipartFormData, FormURLEncoded, JSON, XML, Binary",
                    )?
                    .unwrap_or_default();
                }
                Some("Arguments") => u.arguments = map_field(key, v)?,
                Some("FileFormName") => u.file_form_name = scalar_to_string(key, v)?,
                Some("Data") => u.data = scalar_to_string(key, v)?,
                Some("URL") => u.url = scalar_to_string(key, v)?,
                Some("ThumbnailURL") => u.thumbnail_url = scalar_to_string(key, v)?,
                Some("DeletionURL") => u.deletion_url = scalar_to_string(key, v)?,
                Some("ErrorMessage") => u.error_message = scalar_to_string(key, v)?,
                _ => {
                    u.extra.insert(key.clone(), v.clone());
                }
            }
        }
        u.normalize()?;
        Ok(u)
    }

    /// ShareX's load-time fix-ups (`CheckBackwardCompatibility`).
    fn normalize(&mut self) -> Result<(), SxcuError> {
        if !self.version.is_empty() && compare_versions(&self.version, UNSUPPORTED_MAX) != Ordering::Greater {
            return Err(SxcuError::UnsupportedVersion(self.version.clone()));
        }
        self.move_request_url_query();
        if !self.version.is_empty() && compare_versions(&self.version, LEGACY_SYNTAX_MAX) != Ordering::Greater {
            self.migrate_legacy_syntax();
        }
        Ok(())
    }

    fn move_request_url_query(&mut self) {
        let Some((base, query)) = split_query(&self.request_url) else { return };
        let (base, query) = (base.to_owned(), query.to_owned());
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if v.is_empty() && !query.contains(&format!("{k}=")) {
                // A key-less `?flag`: ShareX stores it as a parameter with empty value.
                self.parameters.entry(k.into_owned()).or_default();
            } else {
                self.parameters.entry(k.into_owned()).or_insert_with(|| v.into_owned());
            }
        }
        self.request_url = base;
    }

    fn migrate_legacy_syntax(&mut self) {
        self.request_url = migrate_old_syntax(&self.request_url);
        for map in [&mut self.parameters, &mut self.headers, &mut self.arguments] {
            for v in map.values_mut() {
                *v = migrate_old_syntax(v);
            }
        }
        self.data = replace_ci(&replace_ci(&self.data, "$input$", "{input}"), "$filename$", "{filename}");
        self.url = migrate_old_syntax(&self.url);
        self.thumbnail_url = migrate_old_syntax(&self.thumbnail_url);
        self.deletion_url = migrate_old_syntax(&self.deletion_url);
        self.error_message = migrate_old_syntax(&self.error_message);
        self.version = CURRENT_VERSION.to_owned();
    }

    /// Serialise to the JSON value ShareX would write (empty/default fields omitted).
    pub fn to_json_value(&self) -> Value {
        fn put_str(m: &mut Map<String, Value>, k: &str, v: &str) {
            if !v.is_empty() {
                m.insert(k.to_owned(), Value::String(v.to_owned()));
            }
        }
        fn put_map(m: &mut Map<String, Value>, k: &str, v: &IndexMap<String, String>) {
            if !v.is_empty() {
                let o: Map<String, Value> = v.iter().map(|(a, b)| (a.clone(), Value::String(b.clone()))).collect();
                m.insert(k.to_owned(), Value::Object(o));
            }
        }
        let mut m = Map::new();
        put_str(&mut m, "Version", &self.version);
        put_str(&mut m, "Name", &self.name);
        if !self.destination_type.is_none() {
            m.insert("DestinationType".into(), Value::String(self.destination_type.to_string()));
        }
        m.insert("RequestMethod".into(), Value::String(self.request_method.as_str().to_owned()));
        put_str(&mut m, "RequestURL", &self.request_url);
        put_map(&mut m, "Parameters", &self.parameters);
        put_map(&mut m, "Headers", &self.headers);
        if self.body != BodyType::None {
            m.insert("Body".into(), Value::String(self.body.as_str().to_owned()));
        }
        put_map(&mut m, "Arguments", &self.arguments);
        put_str(&mut m, "FileFormName", &self.file_form_name);
        put_str(&mut m, "Data", &self.data);
        put_str(&mut m, "URL", &self.url);
        put_str(&mut m, "ThumbnailURL", &self.thumbnail_url);
        put_str(&mut m, "DeletionURL", &self.deletion_url);
        put_str(&mut m, "ErrorMessage", &self.error_message);
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }

    /// Serialise to pretty printed `.sxcu` text.
    pub fn to_json_string(&self) -> String {
        // Serialising a `Value` built from strings cannot fail.
        serde_json::to_string_pretty(&self.to_json_value()).unwrap_or_default()
    }

    /// Display name: `Name`, else the request host, else "Custom uploader".
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        host_of(&self.request_url).unwrap_or_else(|| "Custom uploader".to_owned())
    }

    /// Whether this definition can serve `kind` (destination flags *and* body type).
    pub fn supports(&self, kind: UploadKind) -> bool {
        let dest_ok = if self.destination_type.is_none() {
            true
        } else {
            match kind {
                UploadKind::Image => self.destination_type.contains(DestinationType::IMAGE_UPLOADER),
                UploadKind::Text => self.destination_type.contains(DestinationType::TEXT_UPLOADER),
                UploadKind::File | UploadKind::Video => {
                    self.destination_type.contains(DestinationType::FILE_UPLOADER)
                }
                UploadKind::Url => self
                    .destination_type
                    .intersects(DestinationType::URL_SHORTENER.union(DestinationType::URL_SHARING_SERVICE)),
            }
        };
        dest_ok && self.body_can_carry(kind)
    }

    /// Whether the body type is able to carry `kind` (ShareX throws "Unsupported request
    /// format" otherwise).
    pub fn body_can_carry(&self, kind: UploadKind) -> bool {
        match (self.body, kind) {
            (BodyType::MultipartFormData, UploadKind::Text | UploadKind::Url) => true,
            (BodyType::MultipartFormData, _) => !self.file_form_name.is_empty(),
            (BodyType::Binary, UploadKind::Url) => false,
            (BodyType::Binary, _) => true,
            (_, UploadKind::Text | UploadKind::Url) => true,
            _ => false,
        }
    }

    /// Statically check the definition; see [`ValidationReport`].
    pub fn check(&self) -> ValidationReport {
        let mut r = ValidationReport::default();
        self.check_version(&mut r);
        self.check_destination_and_body(&mut r);
        self.check_request_side(&mut r);
        for (field, text) in [
            ("URL", &self.url),
            ("ThumbnailURL", &self.thumbnail_url),
            ("DeletionURL", &self.deletion_url),
            ("ErrorMessage", &self.error_message),
        ] {
            check_template(&mut r, field, text, true);
        }
        r
    }

    /// `Ok` when there are no errors (warnings are available through [`Self::check`]).
    pub fn validate(&self) -> Result<(), ValidationError> {
        let report = self.check();
        if report.errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { uploader: self.display_name(), errors: report.errors })
        }
    }

    fn check_version(&self, r: &mut ValidationReport) {
        if self.version.is_empty() {
            r.warn("Version is missing; assuming a current-syntax file (ShareX would refuse it)");
        }
    }

    fn check_destination_and_body(&self, r: &mut ValidationReport) {
        let file_dest = DestinationType::IMAGE_UPLOADER.union(DestinationType::FILE_UPLOADER);
        if self.destination_type.is_none() {
            r.warn("DestinationType is empty; pick where this uploader may be used");
        }
        if self.destination_type.intersects(file_dest) {
            match self.body {
                BodyType::MultipartFormData if self.file_form_name.is_empty() => r.error(
                    "FileFormName is required: image/file uploads with a MultipartFormData body need the form field name that carries the file",
                ),
                BodyType::MultipartFormData | BodyType::Binary => {}
                other => r.error(format!(
                    "Body '{}' cannot upload files; image and file uploaders need MultipartFormData or Binary",
                    other.as_str()
                )),
            }
        }
        if self.body == BodyType::None && self.destination_type.contains(DestinationType::TEXT_UPLOADER) {
            r.warn("Body is None, so uploaded text can only be sent through RequestURL or Parameters");
        }
        if self.request_method == HttpMethod::Get && self.body != BodyType::None {
            r.warn("RequestMethod is GET but a Body is configured; many servers ignore GET bodies");
        }
        if !self.data.is_empty() && !matches!(self.body, BodyType::Json | BodyType::Xml) {
            r.warn("Data is ignored unless Body is JSON or XML");
        }
        if !self.arguments.is_empty() && !matches!(self.body, BodyType::MultipartFormData | BodyType::FormUrlEncoded) {
            r.warn("Arguments are ignored unless Body is MultipartFormData or FormURLEncoded");
        }
        if !self.file_form_name.is_empty() && self.body != BodyType::MultipartFormData {
            r.warn("FileFormName is ignored unless Body is MultipartFormData");
        }
        if matches!(self.body, BodyType::Json | BodyType::Xml) && self.data.is_empty() {
            r.warn("Body is JSON/XML but Data is empty");
        }
        if self.body == BodyType::Json && !self.data.is_empty() {
            let sample = replace_ci(&replace_ci(&self.data, "{input}", "x"), "{filename}", "x");
            if serde_json::from_str::<Value>(&crate::nameparser::NameParser::text().parse(&sample)).is_err() {
                r.warn("Data is not valid JSON after substituting {input}/{filename}");
            }
        }
    }

    fn check_request_side(&self, r: &mut ValidationReport) {
        if self.request_url.trim().is_empty() {
            r.error("RequestURL must be configured");
        } else {
            check_template(r, "RequestURL", &self.request_url, false);
            if let Some(scheme) = literal_scheme(&self.request_url) {
                if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                    r.error(format!("RequestURL scheme '{scheme}' is not supported (use http or https)"));
                }
            }
        }
        for (name, value) in &self.parameters {
            if name.is_empty() {
                r.error("Parameters contains an empty name");
            }
            check_template(r, &format!("Parameters.{name}"), value, false);
        }
        for (name, value) in &self.arguments {
            if name.is_empty() {
                r.error("Arguments contains an empty name");
            }
            check_template(r, &format!("Arguments.{name}"), value, false);
        }
        for (name, value) in &self.headers {
            if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                r.error(format!("Headers: '{name}' is not a valid HTTP header name"));
            }
            check_template(r, &format!("Headers.{name}"), value, false);
        }
    }
}

/// `scheme` when the URL literally starts with `something://`.
fn literal_scheme(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once("://")?;
    (!scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))).then_some(scheme)
}

fn host_of(url: &str) -> Option<String> {
    let fixed = if url.contains("://") { url.to_owned() } else { format!("https://{url}") };
    let host = url::Url::parse(&fixed).ok()?.host_str()?.to_owned();
    Some(host.strip_prefix("www.").map_or(host.clone(), str::to_owned))
}

fn check_template(r: &mut ValidationReport, field: &str, text: &str, response_side: bool) {
    let tpl = match Template::parse(text) {
        Ok(t) => t,
        Err(e) => {
            r.error(format!("{field}: {e}"));
            return;
        }
    };
    for call in tpl.calls() {
        let Some(name) = call.literal_name() else { continue };
        if name.is_empty() {
            r.error(format!("{field}: empty function name (a literal '{{' must be written '\\{{')"));
            continue;
        }
        let Some(spec) = template::lookup(&name) else {
            r.error(format!("{field}: unknown function '{name}' (escape a literal '{{' as '\\{{')"));
            continue;
        };
        let n_args = call.args.as_ref().map_or(0, Vec::len);
        if n_args < spec.min_params {
            r.error(format!("{field}: '{}' needs at least {} parameter(s), found {n_args}", spec.name, spec.min_params));
        }
        if !response_side && (spec.needs_response)(n_args) {
            r.error(format!(
                "{field}: '{}' reads the server response and is only available in URL, ThumbnailURL, DeletionURL and ErrorMessage",
                spec.name
            ));
        }
        if spec.interactive {
            r.warn(format!("{field}: '{}' is interactive; headless runs use its default", spec.name));
        }
    }
}

/// Result of [`CustomUploader::check`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Problems that make the uploader unusable.
    pub errors: Vec<String>,
    /// Suspicious but tolerated settings.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    fn error(&mut self, m: impl Into<String>) {
        self.errors.push(m.into());
    }

    fn warn(&mut self, m: impl Into<String>) {
        self.warnings.push(m.into());
    }
}

/// [`CustomUploader::validate`] failure listing every problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Display name of the uploader.
    pub uploader: String,
    /// All errors found.
    pub errors: Vec<String>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "custom uploader '{}' is invalid: {}", self.uploader, self.errors.join("; "))
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> CustomUploader {
        CustomUploader::from_json_str(s).unwrap()
    }

    #[test]
    fn minimal_file_gets_sharex_defaults() {
        let u = parse(r#"{"Version":"13.7.2","RequestURL":"https://a.example/up"}"#);
        assert_eq!(u.request_method, HttpMethod::Post);
        assert_eq!(u.body, BodyType::None);
        assert!(u.destination_type.is_none());
        assert_eq!(u.display_name(), "a.example");
    }

    #[test]
    fn keys_and_enums_are_case_insensitive_and_flexible() {
        let u = parse(
            r#"{"version":"14.0.0","NAME":"x","destinationtype":"imageuploader, FileUploader",
                "requestmethod":"put","requesturl":"http://h/u","body":"multipartformdata","fileformname":"f",
                "Arguments":{"n":5,"b":true,"z":null}}"#,
        );
        assert_eq!(u.destination_type.names(), vec!["ImageUploader", "FileUploader"]);
        assert_eq!(u.request_method, HttpMethod::Put);
        assert_eq!(u.body, BodyType::MultipartFormData);
        assert_eq!(u.arguments["n"], "5");
        assert_eq!(u.arguments["b"], "True");
        assert_eq!(u.arguments["z"], "");
        let numeric = parse(r#"{"Version":"14.0.0","DestinationType":5,"RequestMethod":2,"Body":5,"RequestURL":"h"}"#);
        assert_eq!(numeric.destination_type.names(), vec!["ImageUploader", "FileUploader"]);
        assert_eq!(numeric.request_method, HttpMethod::Put);
        assert_eq!(numeric.body, BodyType::Binary);
        let arr = parse(r#"{"Version":"14.0.0","DestinationType":["TextUploader","URLShortener"],"RequestURL":"h"}"#);
        assert_eq!(arr.destination_type.names(), vec!["TextUploader", "URLShortener"]);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let src = r#"{"Version":"14.0.0","RequestURL":"https://h/u","Body":"Binary","FutureField":{"a":[1,2]},"x-note":"hi"}"#;
        let u = parse(src);
        assert_eq!(u.extra.len(), 2);
        let again = parse(&u.to_json_string());
        assert_eq!(again, u);
        assert_eq!(again.extra["FutureField"], serde_json::json!({"a":[1,2]}));
    }

    #[test]
    fn bad_input_gives_clear_errors() {
        assert!(matches!(CustomUploader::from_json_str("[]"), Err(SxcuError::NotAnObject)));
        assert!(matches!(CustomUploader::from_json_str("{"), Err(SxcuError::Json(_))));
        let e = CustomUploader::from_json_str(r#"{"Body":"Zip"}"#).unwrap_err();
        assert!(e.to_string().contains("Body") && e.to_string().contains("Zip"), "{e}");
        let e = CustomUploader::from_json_str(r#"{"Headers":[1]}"#).unwrap_err();
        assert!(e.to_string().contains("Headers"), "{e}");
        let e = CustomUploader::from_json_str(r#"{"DestinationType":"Nope"}"#).unwrap_err();
        assert!(e.to_string().contains("Nope"), "{e}");
        let e = CustomUploader::from_json_str(r#"{"Version":"12.0.0","RequestURL":"h"}"#).unwrap_err();
        assert!(matches!(e, SxcuError::UnsupportedVersion(_)));
    }

    #[test]
    fn bom_is_tolerated() {
        parse("\u{feff}{\"Version\":\"14.0.0\",\"RequestURL\":\"h\"}");
    }

    #[test]
    fn query_string_in_request_url_moves_to_parameters() {
        let u = parse(r#"{"Version":"14.0.0","RequestURL":"https://h/u?key=abc&x={input}&flag","Parameters":{"key":"keep"}}"#);
        assert_eq!(u.request_url, "https://h/u");
        assert_eq!(u.parameters["key"], "keep", "explicit Parameters win");
        assert_eq!(u.parameters["x"], "{input}");
        assert!(u.parameters.contains_key("flag"));
    }

    #[test]
    fn question_mark_inside_a_call_is_not_a_query() {
        let u = parse(r#"{"Version":"14.0.0","RequestURL":"https://h/{random:a?|b}/u"}"#);
        assert_eq!(u.request_url, "https://h/{random:a?|b}/u");
        assert!(u.parameters.is_empty());
    }

    #[test]
    fn legacy_dollar_syntax_is_migrated() {
        let u = parse(
            r#"{"Version":"13.0.0","RequestURL":"https://h/u","Arguments":{"a":"$input$","b":"{x}"},
                "Body":"JSON","Data":"{\"t\":\"$input$\",\"f\":\"$FILENAME$\"}","URL":"$json:data.url$","ErrorMessage":"$response$"}"#,
        );
        assert_eq!(u.arguments["a"], "{input}");
        assert_eq!(u.arguments["b"], "\\{x\\}", "literal braces are escaped");
        assert_eq!(u.data, "{\"t\":\"{input}\",\"f\":\"{filename}\"}");
        assert_eq!(u.url, "{json:data.url}");
        assert_eq!(u.error_message, "{response}");
        assert_eq!(u.version, CURRENT_VERSION);
    }

    #[test]
    fn modern_files_are_not_migrated() {
        let u = parse(r#"{"Version":"14.1.0","RequestURL":"https://h/u","URL":"$json:a$"}"#);
        assert_eq!(u.url, "$json:a$");
        assert_eq!(u.version, "14.1.0");
    }

    #[test]
    fn version_comparison() {
        assert_eq!(compare_versions("13.7.1", "13.7.1"), Ordering::Equal);
        assert_eq!(compare_versions("13.7.2", "13.7.1"), Ordering::Greater);
        assert_eq!(compare_versions("13.10.0", "13.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("14", "14.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("12.3.1", "13.0"), Ordering::Less);
    }

    #[test]
    fn serialisation_omits_defaults_and_keeps_order() {
        let u = parse(
            r#"{"Version":"14.0.0","Name":"n","DestinationType":"ImageUploader","RequestMethod":"POST",
                "RequestURL":"https://h/u","Body":"MultipartFormData","FileFormName":"file","URL":"{response}"}"#,
        );
        let s = u.to_json_string();
        let keys: Vec<&str> = KNOWN_KEYS.iter().copied().filter(|k| s.contains(&format!("\"{k}\""))).collect();
        assert_eq!(keys, ["Version", "Name", "DestinationType", "RequestMethod", "RequestURL", "Body", "FileFormName", "URL"]);
        let positions: Vec<usize> = keys.iter().map(|k| s.find(&format!("\"{k}\"")).unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{s}");
        assert!(!s.contains("\"Parameters\"") && !s.contains("\"Headers\"") && !s.contains("\"Data\""));
        assert_eq!(parse(&s), u);
    }

    #[test]
    fn support_matrix() {
        let img = parse(
            r#"{"Version":"14.0.0","DestinationType":"ImageUploader","RequestURL":"h","Body":"MultipartFormData","FileFormName":"f"}"#,
        );
        assert!(img.supports(UploadKind::Image));
        assert!(!img.supports(UploadKind::File));
        assert!(!img.supports(UploadKind::Url));
        let text = parse(r#"{"Version":"14.0.0","DestinationType":"TextUploader","RequestURL":"h","Body":"JSON","Data":"{}"}"#);
        assert!(text.supports(UploadKind::Text));
        assert!(!text.supports(UploadKind::Image));
        let short = parse(r#"{"Version":"14.0.0","DestinationType":"URLShortener","RequestURL":"h"}"#);
        assert!(short.supports(UploadKind::Url));
        let none = parse(r#"{"Version":"14.0.0","RequestURL":"h","Body":"Binary"}"#);
        assert!(none.supports(UploadKind::Video) && none.supports(UploadKind::Text) && !none.supports(UploadKind::Url));
    }

    fn errors(json: &str) -> Vec<String> {
        parse(json).check().errors
    }

    #[test]
    fn validation_catches_common_mistakes() {
        assert!(errors(r#"{"Version":"14.0.0"}"#).iter().any(|e| e.contains("RequestURL")));
        assert!(errors(r#"{"Version":"14.0.0","RequestURL":"ftp://h/x"}"#).iter().any(|e| e.contains("scheme")));
        assert!(
            errors(r#"{"Version":"14.0.0","RequestURL":"h","DestinationType":"ImageUploader","Body":"MultipartFormData"}"#)
                .iter()
                .any(|e| e.contains("FileFormName"))
        );
        assert!(
            errors(r#"{"Version":"14.0.0","RequestURL":"h","DestinationType":"FileUploader","Body":"JSON"}"#)
                .iter()
                .any(|e| e.contains("cannot upload files"))
        );
        assert!(errors(r#"{"Version":"14.0.0","RequestURL":"h/{nope}"}"#).iter().any(|e| e.contains("unknown function")));
        assert!(errors(r#"{"Version":"14.0.0","RequestURL":"h/{random:a}"}"#).iter().any(|e| e.contains("at least 2")));
        assert!(errors(r#"{"Version":"14.0.0","RequestURL":"h/{json:a}"}"#).iter().any(|e| e.contains("server response")));
        assert!(errors(r#"{"Version":"14.0.0","RequestURL":"h","Headers":{"bad name":"x"}}"#).iter().any(|e| e.contains("header name")));
    }

    #[test]
    fn validation_accepts_response_functions_in_response_fields() {
        let u = parse(
            r#"{"Version":"14.0.0","RequestURL":"https://h/u","DestinationType":"ImageUploader","Body":"MultipartFormData","FileFormName":"f",
                "URL":"{json:a}","ThumbnailURL":"{regex:x|1}","DeletionURL":"{header:X}","ErrorMessage":"{response}"}"#,
        );
        assert_eq!(u.check().errors, Vec::<String>::new());
        assert!(u.validate().is_ok());
    }

    #[test]
    fn validation_warnings() {
        let u = parse(r#"{"RequestURL":"https://h/u","RequestMethod":"GET","Body":"JSON","Data":"{oops"}"#);
        let w = u.check().warnings;
        assert!(w.iter().any(|m| m.contains("Version")));
        assert!(w.iter().any(|m| m.contains("DestinationType")));
        assert!(w.iter().any(|m| m.contains("GET")));
        assert!(w.iter().any(|m| m.contains("not valid JSON")));
    }

    #[test]
    fn validation_error_lists_everything() {
        let u = parse(r#"{"Version":"14.0.0","Name":"Broken","RequestURL":"ftp://h/{nope}"}"#);
        let e = u.validate().unwrap_err();
        let s = e.to_string();
        assert!(s.contains("Broken") && s.contains("scheme") && s.contains("nope"), "{s}");
    }

    #[test]
    fn escaped_literal_braces_validate() {
        let u = parse(r#"{"Version":"14.0.0","RequestURL":"https://h/u","Arguments":{"x":"\\{not a call\\}"},"Body":"FormURLEncoded"}"#);
        assert!(u.check().errors.is_empty(), "{:?}", u.check().errors);
    }
}
