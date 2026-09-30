//! Form descriptions and edits for the `[uploaders.<name>]` tables.
//!
//! `ssx-core` keeps these tables opaque and `ssx-services` owns their schema
//! (`upload::config`). The window therefore edits the raw `toml::Table` through a small list of
//! [`FieldSpec`]s per uploader type, which has three advantages: unknown keys the user typed
//! by hand survive an edit, nothing here can drift from the schema without the "every field
//! is accepted by the real builder" test failing, and secrets have a type of their own
//! ([`FieldKind::Secret`]) so a page cannot accidentally render or store one as text.
//!
//! Whether a table *works* is not decided here: [`check`] runs the real builder
//! (`ssx_services::upload::config::build`) and returns its message.

use std::path::Path;

use ssx_core::settings::KEYRING_PREFIX;
use toml::{Table, Value};

/// The kinds of destination the window can create and edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploaderKind {
    /// Imgur API v3.
    Imgur,
    /// S3-compatible object storage.
    S3,
    /// A generic HTTP PUT/POST endpoint.
    Http,
    /// Copy into a folder (or do nothing and report `file://` URLs).
    Local,
    /// A URL shortener.
    Shortener,
}

impl UploaderKind {
    /// The kinds a new destination can be.
    pub const ALL: [UploaderKind; 5] = [
        UploaderKind::Imgur,
        UploaderKind::S3,
        UploaderKind::Http,
        UploaderKind::Local,
        UploaderKind::Shortener,
    ];

    /// The `type` value in the table.
    pub const fn type_name(self) -> &'static str {
        match self {
            UploaderKind::Imgur => "imgur",
            UploaderKind::S3 => "s3",
            UploaderKind::Http => "http",
            UploaderKind::Local => "local",
            UploaderKind::Shortener => "shortener",
        }
    }

    /// The name shown in menus.
    pub const fn label(self) -> &'static str {
        match self {
            UploaderKind::Imgur => "Imgur",
            UploaderKind::S3 => "S3-compatible storage",
            UploaderKind::Http => "HTTP endpoint",
            UploaderKind::Local => "Local folder",
            UploaderKind::Shortener => "URL shortener",
        }
    }

    /// One line about it.
    pub const fn blurb(self) -> &'static str {
        match self {
            UploaderKind::Imgur => "Anonymous or account uploads to imgur.com.",
            UploaderKind::S3 => "AWS S3, Cloudflare R2, Backblaze B2, Wasabi, MinIO and others.",
            UploaderKind::Http => "PUT or POST the file to any URL, with optional authentication.",
            UploaderKind::Local => "Copy files into a folder and report a file:// or web URL.",
            UploaderKind::Shortener => {
                "Turns long URLs into short ones (is.gd, v.gd, TinyURL or your own)."
            }
        }
    }

    /// Reads the kind from a table's `type` key.
    pub fn of(table: &Table) -> Option<UploaderKind> {
        let t = table.get("type")?.as_str()?;
        Self::ALL.into_iter().find(|k| k.type_name() == t)
    }
}

/// How a field is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text.
    Text,
    /// One of a fixed list (an empty value means "the default").
    Choice(&'static [&'static str]),
    /// A credential: the table holds `keyring:<name>`, the value lives in the secret store.
    Secret,
    /// A `name = "value"` table (headers, extra fields).
    Map,
    /// A folder path.
    Folder,
}

/// One editable key of an uploader table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// The TOML key.
    pub key: &'static str,
    /// The label.
    pub label: &'static str,
    /// Help shown under the field.
    pub help: &'static str,
    /// How it is edited.
    pub kind: FieldKind,
    /// The uploader cannot work without it (shown with an asterisk).
    pub required: bool,
    /// Shown only under "Advanced".
    pub advanced: bool,
}

const fn f(
    key: &'static str,
    label: &'static str,
    help: &'static str,
    kind: FieldKind,
) -> FieldSpec {
    FieldSpec { key, label, help, kind, required: false, advanced: false }
}

const fn req(mut s: FieldSpec) -> FieldSpec {
    s.required = true;
    s
}

const fn adv(mut s: FieldSpec) -> FieldSpec {
    s.advanced = true;
    s
}

use FieldKind::{Choice, Folder, Map, Secret, Text};

const IMGUR: &[FieldSpec] = &[
    f(
        "client_id",
        "Client ID",
        "For anonymous uploads: register an application at api.imgur.com/oauth2/addclient.",
        Text,
    ),
    f(
        "access_token",
        "Access token",
        "For uploads to your account. Used instead of the client ID when set.",
        Secret,
    ),
    f("title", "Title", "Title given to every upload.", Text),
    f("description", "Description", "Description given to every upload.", Text),
    f("album", "Album", "Album (id or delete hash) to add uploads to.", Text),
    f(
        "thumbnail_size",
        "Thumbnail size",
        "Size of the thumbnail URL.",
        Choice(&["small", "thumb", "medium", "large", "huge"]),
    ),
    adv(f("api_base", "API base URL", "For self-hosted or test servers.", Text)),
    adv(f("image_base", "Image base URL", "Where image links point.", Text)),
    adv(f("site_base", "Site base URL", "Where page links point.", Text)),
];

const S3: &[FieldSpec] = &[
    f(
        "preset",
        "Provider",
        "Fills in the endpoint style. `custom` needs an endpoint.",
        Choice(&["aws", "r2", "b2", "wasabi", "minio", "custom"]),
    ),
    req(f("bucket", "Bucket", "The bucket to upload into.", Text)),
    f("region", "Region", "For example us-east-1; Backblaze needs it.", Text),
    f("endpoint", "Endpoint", "Required for `custom` and `minio`: https://host:port.", Text),
    f("account_id", "Account ID", "Cloudflare R2 only.", Text),
    f("access_key_id", "Access key ID", "Stored in the keyring, never in settings.toml.", Secret),
    f(
        "secret_access_key",
        "Secret access key",
        "Stored in the keyring, never in settings.toml.",
        Secret,
    ),
    f("key_prefix", "Key prefix", "Folder inside the bucket, e.g. screenshots/.", Text),
    f(
        "public_url_template",
        "Public URL",
        "How the link is built, e.g. https://cdn.example.com/{key}.",
        Text,
    ),
    adv(f("session_token", "Session token", "Temporary credentials only.", Secret)),
    adv(f(
        "addressing",
        "Addressing",
        "How the bucket appears in the URL.",
        Choice(&["auto", "path", "virtual"]),
    )),
    adv(f("key_template", "Key template", "Object name pattern.", Text)),
    adv(f("acl", "ACL", "For example public-read.", Text)),
    adv(f("storage_class", "Storage class", "For example STANDARD_IA.", Text)),
    adv(f("cache_control", "Cache-Control", "Header stored with the object.", Text)),
    adv(f("content_disposition", "Content-Disposition", "Header stored with the object.", Text)),
    adv(f(
        "payload_signing",
        "Payload signing",
        "Whether request bodies are hashed.",
        Choice(&["auto", "unsigned", "hashed"]),
    )),
    adv(f("headers", "Extra headers", "Sent with every request.", Map)),
];

const HTTP: &[FieldSpec] = &[
    req(f(
        "url",
        "URL",
        "Where to send the file. The file name is appended for raw PUT uploads.",
        Text,
    )),
    f("method", "Method", "Default is put.", Choice(&["put", "post", "patch"])),
    f(
        "body",
        "Body",
        "raw sends the file as the body; multipart sends a form.",
        Choice(&["raw", "multipart"]),
    ),
    f(
        "field",
        "Form field",
        "Multipart only: the field that carries the file (default file).",
        Text,
    ),
    f(
        "auth",
        "Authentication",
        "How to prove who you are.",
        Choice(&["none", "bearer", "basic", "header"]),
    ),
    f("auth_secret", "Token or password", "Stored in the keyring, never in settings.toml.", Secret),
    f("auth_user", "User name", "For basic authentication.", Text),
    f("auth_header", "Header name", "For header authentication, e.g. X-Api-Key.", Text),
    f(
        "result",
        "URL of the result",
        "request_url, body, header:<Name>, json:</pointer> or template:<text>.",
        Text,
    ),
    adv(f("fields", "Extra form fields", "Multipart only.", Map)),
    adv(f("headers", "Extra headers", "Sent with every request.", Map)),
];

const LOCAL: &[FieldSpec] = &[
    f("dir", "Folder", "Copy files here. Empty: do nothing and report file:// URLs.", Folder),
    f("base_url", "Base URL", "If the folder is served by a web server, the link prefix.", Text),
];

const SHORTENER: &[FieldSpec] = &[
    f(
        "service",
        "Service",
        "A built-in service, or leave empty and give an endpoint.",
        Choice(&["is.gd", "v.gd", "tinyurl"]),
    ),
    f("endpoint", "Endpoint", "Your own shortening service.", Text),
    adv(f("method", "Method", "Default is get.", Choice(&["get", "post"]))),
    adv(f(
        "url_param",
        "URL parameter",
        "Name of the parameter that carries the long URL (default url).",
        Text,
    )),
    adv(f("response", "Response", "text, or json:</pointer>.", Text)),
    adv(f("params", "Extra parameters", "Sent with every request.", Map)),
    adv(f("headers", "Extra headers", "Sent with every request.", Map)),
];

/// The fields of `kind`.
pub const fn fields(kind: UploaderKind) -> &'static [FieldSpec] {
    match kind {
        UploaderKind::Imgur => IMGUR,
        UploaderKind::S3 => S3,
        UploaderKind::Http => HTTP,
        UploaderKind::Local => LOCAL,
        UploaderKind::Shortener => SHORTENER,
    }
}

/// A new table of `kind` with sensible starting values.
pub fn new_table(kind: UploaderKind) -> Table {
    let mut t = Table::new();
    t.insert("type".into(), Value::String(kind.type_name().into()));
    match kind {
        UploaderKind::S3 => {
            t.insert("preset".into(), Value::String("aws".into()));
            t.insert("bucket".into(), Value::String(String::new()));
        }
        UploaderKind::Http => {
            t.insert("url".into(), Value::String(String::new()));
        }
        UploaderKind::Shortener => {
            t.insert("service".into(), Value::String("is.gd".into()));
        }
        UploaderKind::Imgur | UploaderKind::Local => {}
    }
    t
}

/// The string value at `key` (empty if unset or not a string).
pub fn get_text<'a>(t: &'a Table, key: &str) -> &'a str {
    t.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Sets a string key; an empty value removes the key (a missing key means "default").
/// Required keys are kept as empty strings by [`set_required_text`].
pub fn set_text(t: &mut Table, key: &str, value: &str) {
    if value.is_empty() {
        t.remove(key);
    } else {
        t.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

/// Sets a required key: kept (empty) so the form still shows it.
pub fn set_required_text(t: &mut Table, key: &str, value: &str) {
    t.insert(key.to_owned(), Value::String(value.to_owned()));
}

/// The entries of a map-valued key, in key order.
pub fn get_map(t: &Table, key: &str) -> Vec<(String, String)> {
    t.get(key)
        .and_then(Value::as_table)
        .map(|m| {
            m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect()
        })
        .unwrap_or_default()
}

/// Replaces a map-valued key; an empty list removes it. Entries with an empty name are
/// dropped.
pub fn set_map(t: &mut Table, key: &str, entries: &[(String, String)]) {
    let mut m = Table::new();
    for (k, v) in entries {
        if !k.trim().is_empty() {
            m.insert(k.trim().to_owned(), Value::String(v.clone()));
        }
    }
    if m.is_empty() {
        t.remove(key);
    } else {
        t.insert(key.to_owned(), Value::Table(m));
    }
}

/// The secret fields of a table that currently hold a reference: `(key, secret name)`.
pub fn secret_refs(kind: UploaderKind, t: &Table) -> Vec<(&'static str, String)> {
    fields(kind)
        .iter()
        .filter(|s| s.kind == Secret)
        .filter_map(|s| {
            let name = crate::secrets::reference_name(get_text(t, s.key))?;
            Some((s.key, name.to_owned()))
        })
        .collect()
}

/// `true` if a secret field holds something that is not a keyring reference (a plain-text
/// secret typed by hand into the file). The window never displays it.
pub fn has_plaintext_secret(kind: UploaderKind, t: &Table) -> Vec<&'static str> {
    fields(kind)
        .iter()
        .filter(|s| s.kind == Secret)
        .filter(|s| {
            let v = get_text(t, s.key);
            !v.is_empty() && !v.starts_with(KEYRING_PREFIX)
        })
        .map(|s| s.key)
        .collect()
}

/// Asks the real builder whether the table works; the message is a complete sentence.
pub fn check(name: &str, table: &Table, config_dir: &Path) -> Result<(), String> {
    ssx_services::upload::config::build(name, table, config_dir).map(|_| ())
}

/// The names in a table, for "is this name taken" checks.
pub fn name_problem(name: &str, taken: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some("give the destination a name".to_owned());
    }
    if !ssx_services::upload::sxcu_files::valid_uploader_name(name) {
        return Some(
            "use letters, digits, '.', '_' or '-' (at most 64 characters), no spaces".to_owned(),
        );
    }
    if taken.iter().any(|t| t == name) {
        return Some(format!("there is already a destination called {name:?}"));
    }
    None
}

/// A free name derived from `base`.
pub fn free_name(base: &str, taken: &[String]) -> String {
    let base = ssx_services::upload::sxcu_files::sanitize_name(base);
    if !taken.contains(&base) {
        return base;
    }
    (2..=usize::MAX).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(c)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent-config-dir")
    }

    fn table(src: &str) -> Table {
        src.parse().unwrap()
    }

    #[test]
    fn kinds_are_read_from_the_type_key() {
        assert_eq!(UploaderKind::of(&table("type = 's3'")), Some(UploaderKind::S3));
        assert_eq!(UploaderKind::of(&table("type = 'sxcu'\nfile='a'")), None);
        assert_eq!(UploaderKind::of(&table("x = 1")), None);
        for k in UploaderKind::ALL {
            assert_eq!(UploaderKind::of(&new_table(k)), Some(k));
            assert!(!k.label().is_empty() && !k.blurb().is_empty());
        }
    }

    #[test]
    fn every_field_the_form_offers_is_accepted_by_the_real_builder() {
        // A value for every field, so `deny_unknown_fields` in the builder would reject a spec
        // that names a key the schema does not have.
        let sample = |kind: UploaderKind, spec: &FieldSpec| -> Value {
            match spec.kind {
                Choice(options) => Value::String(options[0].to_owned()),
                Secret => Value::String("keyring:some-secret".to_owned()),
                Map => Value::Table(
                    [("X-A".to_owned(), Value::String("b".into()))].into_iter().collect(),
                ),
                Folder => Value::String("/tmp/ssx-out".into()),
                Text => Value::String(match (kind, spec.key) {
                    (UploaderKind::S3, "endpoint") => "https://s3.example.com".to_owned(),
                    (UploaderKind::Http, "url") => "https://example.com/up".to_owned(),
                    (UploaderKind::Shortener, "endpoint") => "https://s.example.com/api".to_owned(),
                    (_, "result") => "body".to_owned(),
                    (_, "response") => "text".to_owned(),
                    (UploaderKind::Imgur, "client_id") => "abc123".to_owned(),
                    (_, k) if k.ends_with("_base") || k == "base_url" => {
                        "https://example.com".to_owned()
                    }
                    (_, "public_url_template") => "https://cdn.example.com/{key}".to_owned(),
                    _ => "x".to_owned(),
                }),
            }
        };
        for kind in UploaderKind::ALL {
            let mut t = new_table(kind);
            for spec in fields(kind) {
                t.insert(spec.key.to_owned(), sample(kind, spec));
            }
            match kind {
                // choose consistent combinations for the fields that exclude each other
                UploaderKind::Http => {
                    t.insert("body".into(), Value::String("multipart".into()));
                    t.insert("auth".into(), Value::String("basic".into()));
                }
                UploaderKind::S3 => {
                    t.insert("preset".into(), Value::String("custom".into()));
                    t.insert("session_token".into(), Value::String("keyring:tok".into()));
                }
                UploaderKind::Shortener => {
                    t.remove("service");
                    t.insert("method".into(), Value::String("get".into()));
                }
                _ => {}
            }
            if let Err(e) = check("sample", &t, &dir()) {
                panic!("{kind:?}: {e}\n{t}");
            }
        }
    }

    #[test]
    fn required_fields_are_marked_and_missing_ones_are_reported_by_the_builder() {
        for kind in [UploaderKind::S3, UploaderKind::Http] {
            let required: Vec<_> = fields(kind).iter().filter(|s| s.required).collect();
            assert_eq!(required.len(), 1, "{kind:?}");
        }
        let e = check("s", &new_table(UploaderKind::Imgur), &dir()).unwrap_err();
        assert!(e.contains("client_id"), "{e}");
    }

    #[test]
    fn choices_are_all_valid_values() {
        for kind in UploaderKind::ALL {
            for spec in fields(kind) {
                if let Choice(options) = spec.kind {
                    assert!(options.len() >= 2, "{}", spec.key);
                    for o in options {
                        let mut t = new_table(kind);
                        t.insert(spec.key.into(), Value::String((*o).into()));
                        // satisfy the requirements that are unrelated to the choice
                        match kind {
                            UploaderKind::Imgur => {
                                t.insert("client_id".into(), Value::String("id".into()));
                            }
                            UploaderKind::S3 => {
                                t.insert("bucket".into(), Value::String("b".into()));
                                t.insert(
                                    "endpoint".into(),
                                    Value::String("https://e.example.com".into()),
                                );
                                t.insert("account_id".into(), Value::String("acc".into()));
                                t.insert("region".into(), Value::String("eu-1".into()));
                            }
                            UploaderKind::Http => {
                                t.insert(
                                    "url".into(),
                                    Value::String("https://e.example.com".into()),
                                );
                                if spec.key == "auth" && *o != "none" {
                                    t.insert(
                                        "auth_secret".into(),
                                        Value::String("keyring:s".into()),
                                    );
                                    t.insert("auth_user".into(), Value::String("u".into()));
                                    t.insert("auth_header".into(), Value::String("X-K".into()));
                                }
                            }
                            UploaderKind::Shortener => {
                                if spec.key != "service" {
                                    t.remove("service");
                                    t.insert(
                                        "endpoint".into(),
                                        Value::String("https://e.example.com/api".into()),
                                    );
                                }
                            }
                            UploaderKind::Local => {}
                        }
                        if let Err(e) = check("c", &t, &dir()) {
                            panic!("{kind:?}.{} = {o}: {e}", spec.key);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_helpers_keep_unknown_keys_and_drop_empty_values() {
        let mut t = table("type='http'\nurl='https://a'\ncustom_future_key = 5\n");
        set_text(&mut t, "method", "post");
        set_text(&mut t, "field", "");
        set_required_text(&mut t, "url", "");
        assert_eq!(get_text(&t, "method"), "post");
        assert!(t.get("field").is_none());
        assert_eq!(t.get("url").and_then(Value::as_str), Some(""), "required keys stay visible");
        assert_eq!(t.get("custom_future_key").and_then(Value::as_integer), Some(5));
        set_text(&mut t, "method", "");
        assert!(t.get("method").is_none());
    }

    #[test]
    fn maps_round_trip() {
        let mut t = Table::new();
        set_map(
            &mut t,
            "headers",
            &[
                ("X-A".into(), "1".into()),
                ("  ".into(), "dropped".into()),
                ("X-B".into(), "2".into()),
            ],
        );
        assert_eq!(
            get_map(&t, "headers"),
            [("X-A".to_owned(), "1".to_owned()), ("X-B".into(), "2".into())]
        );
        set_map(&mut t, "headers", &[]);
        assert!(t.get("headers").is_none());
        assert!(get_map(&t, "nothing").is_empty());
    }

    #[test]
    fn secret_references_and_plain_text_are_told_apart() {
        let t = table(
            "type='s3'\nbucket='b'\naccess_key_id='keyring:my-key'\nsecret_access_key='oops-plain'\n",
        );
        assert_eq!(secret_refs(UploaderKind::S3, &t), [("access_key_id", "my-key".to_owned())]);
        assert_eq!(has_plaintext_secret(UploaderKind::S3, &t), ["secret_access_key"]);
        assert!(has_plaintext_secret(UploaderKind::Local, &Table::new()).is_empty());
    }

    #[test]
    fn names_are_checked_like_the_validator_does() {
        let taken = vec!["imgur".to_owned()];
        assert!(name_problem("", &taken).is_some());
        assert!(name_problem("has space", &taken).unwrap().contains("no spaces"));
        assert!(name_problem("imgur", &taken).unwrap().contains("already"));
        assert!(name_problem("my-s3.2", &taken).is_none());
        assert_eq!(free_name("imgur", &taken), "imgur-2");
        assert_eq!(free_name("New Thing!", &taken), "New-Thing");
        assert_eq!(free_name("s3", &[]), "s3");
    }

    #[test]
    fn plaintext_secrets_are_refused_by_the_core_validator_for_every_secret_field() {
        for kind in UploaderKind::ALL {
            for spec in fields(kind).iter().filter(|s| s.kind == Secret) {
                let mut t = new_table(kind);
                t.insert(spec.key.into(), Value::String("plain-text-value".into()));
                let mut s = ssx_core::settings::Settings::default();
                s.uploaders.insert("u".into(), t);
                let bad = s.validate().into_iter().any(|i| {
                    i.severity == ssx_core::settings::Severity::Error && i.path.ends_with(spec.key)
                });
                assert!(bad, "{kind:?}.{} would be written in plain text", spec.key);
            }
        }
    }
}
