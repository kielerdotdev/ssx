//! Semantic validation of [`Settings`].
//!
//! Parsing catches type errors; this catches values that parse but cannot work, with a
//! message that says what to change. No I/O happens here, so it is cheap enough to run on
//! every keystroke of a settings UI.
//!
//! Errors block saving. Warnings are things that will work but probably not as intended.

use std::{collections::BTreeMap, fmt};

use toml::Value;

use super::{
    AfterCapture, AfterUpload, Hotkey, InputKind, Settings, Workflow, destinations::DestinationType,
};
use crate::pattern::{Pattern, SanitizeOptions, sanitize_file_name, suspicious_tokens};

/// How serious an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Works, but is probably a mistake.
    Warning,
    /// Cannot work / must not be saved.
    Error,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// Severity.
    pub severity: Severity,
    /// Location, e.g. `workflows[2].trigger.hotkey` or `general.image_quality`.
    pub path: String,
    /// What is wrong and how to fix it.
    pub message: String,
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{sev}: {}: {}", self.path, self.message)
    }
}

struct Sink(Vec<ValidationIssue>);

impl Sink {
    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(ValidationIssue {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        });
    }
    fn warn(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(ValidationIssue {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        });
    }
}

/// Key names that suggest a credential.
const SECRET_KEY_HINTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
    "private_key",
    "auth_key",
    "credential",
];
/// Key suffixes that mark a *reference or a non-secret* even if the name has a hint.
const NON_SECRET_SUFFIXES: &[&str] = &["_url", "_uri", "_endpoint", "_keyring", "_name", "_header"];
/// Prefix of a keyring reference value.
pub const KEYRING_PREFIX: &str = "keyring:";

/// Runs every check.
pub fn validate(s: &Settings) -> Vec<ValidationIssue> {
    let mut out = Sink(Vec::new());
    validate_general(s, &mut out);
    validate_capture(s, &mut out);
    validate_history_and_post(s, &mut out);
    validate_destinations(s, &mut out);
    validate_uploaders(s, &mut out);
    validate_workflows_and_hotkeys(s, &mut out);
    out.0
}

fn check_pattern(out: &mut Sink, path: &str, src: &str) {
    let p = Pattern::parse(src);
    for t in p.unknown_tokens() {
        out.warn(
            path,
            format!("unknown token {t}; it will appear literally in the result (write %% for a literal percent sign)"),
        );
    }
    for (tok, hint) in suspicious_tokens(src) {
        out.warn(path, format!("{tok} is not a supported token: {hint}"));
    }
}

fn check_folder_name(out: &mut Sink, path: &str, name: &str) {
    if name.trim().is_empty() {
        out.error(path, "must not be empty; choose a folder name such as \"Screenshots\"");
        return;
    }
    let cleaned = sanitize_file_name(name, &SanitizeOptions::default());
    if cleaned != name {
        out.error(
            path,
            format!(
                "{name:?} is not a valid folder name on all platforms (illegal character, reserved name or trailing dot/space); use {cleaned:?}"
            ),
        );
    }
}

fn validate_general(s: &Settings, out: &mut Sink) {
    let g = &s.general;
    if g.file_name_pattern.trim().is_empty() {
        out.error(
            "general.file_name_pattern",
            "must not be empty, otherwise every file is called \"file\"; the default is \"Screenshot_%y-%mo-%d_%h-%mi-%s\"",
        );
    }
    check_pattern(out, "general.file_name_pattern", &g.file_name_pattern);
    check_pattern(out, "general.folder_pattern", &g.folder_pattern);
    if !(1..=100).contains(&g.image_quality) {
        out.error(
            "general.image_quality",
            format!("{} is out of range; use a value from 1 to 100", g.image_quality),
        );
    }
    if g.max_file_name_len > 255 {
        out.error(
            "general.max_file_name_len",
            format!("{} exceeds the 255-character limit of common file systems; use 0 to only apply the OS limit", g.max_file_name_len),
        );
    }
    if let Some(dir) = &g.save_dir
        && dir.as_os_str().is_empty()
    {
        out.error(
            "general.save_dir",
            "is empty; remove the key to use the default Pictures/ssx folder",
        );
    }
    if g.use_type_subfolders {
        check_folder_name(out, "general.subfolders.image", &g.subfolders.image);
        check_folder_name(out, "general.subfolders.video", &g.subfolders.video);
        check_folder_name(out, "general.subfolders.text", &g.subfolders.text);
        check_folder_name(out, "general.subfolders.file", &g.subfolders.file);
    }
}

fn validate_capture(s: &Settings, out: &mut Sink) {
    let c = &s.capture;
    if c.delay_ms > 60_000 {
        out.error(
            "capture.delay_ms",
            format!("{} ms is more than one minute; use at most 60000", c.delay_ms),
        );
    }
    let h = &c.hdr;
    if !h.peak.is_finite() || h.peak < 1.0 || h.peak > 100.0 {
        out.error(
            "capture.hdr.peak",
            format!(
                "{} is out of range; peak is a multiple of SDR white between 1 and 100 (default 4)",
                h.peak
            ),
        );
    }
    if !h.knee.is_finite() || !(0.0..=1.0).contains(&h.knee) {
        out.error(
            "capture.hdr.knee",
            format!("{} is out of range; knee must be between 0 and 1 (default 0.75)", h.knee),
        );
    }
    if !h.exposure.is_finite() || !(-10.0..=10.0).contains(&h.exposure) {
        out.error(
            "capture.hdr.exposure",
            format!("{} is out of range; exposure is in stops between -10 and 10", h.exposure),
        );
    }
}

fn validate_history_and_post(s: &Settings, out: &mut Sink) {
    if !(16..=1024).contains(&s.history.thumbnail_max_edge) {
        out.error(
            "history.thumbnail_max_edge",
            format!("{} is out of range; use 16 to 1024 pixels", s.history.thumbnail_max_edge),
        );
    }
    if !(1..=16).contains(&s.post_file.max_parallel_uploads) {
        out.error(
            "post_file.max_parallel_uploads",
            format!(
                "{} is out of range; use 1 to 16 (1 uploads files one after another)",
                s.post_file.max_parallel_uploads
            ),
        );
    }
}

fn valid_uploader_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn check_uploader_ref(s: &Settings, out: &mut Sink, path: &str, name: &str) {
    if name.trim().is_empty() {
        out.error(path, "uploader name is empty; remove the key to use the default");
    } else if !valid_uploader_name(name) {
        out.error(
            path,
            format!("{name:?} is not a valid uploader name; use letters, digits, '.', '_' or '-'"),
        );
    } else if !s.uploaders.contains_key(name) {
        // Built-in uploaders need no table, so this is only a hint.
        out.warn(
            path,
            format!("uploader {name:?} has no [uploaders.{name}] section; that is fine for built-in uploaders that need no settings"),
        );
    }
}

fn validate_destinations(s: &Settings, out: &mut Sink) {
    for ty in DestinationType::ALL {
        if let Some(n) = s.destinations.default_for(ty) {
            check_uploader_ref(s, out, &format!("destinations.{}", type_key(ty)), n);
        }
    }
    for (ext, name) in &s.destinations.extension_overrides {
        let path = format!("destinations.extension_overrides.{ext}");
        if ext.is_empty()
            || ext.starts_with('.')
            || ext != &ext.to_ascii_lowercase()
            || ext.contains(['/', '\\', ' '])
        {
            out.error(
                &path,
                format!("{ext:?} must be a lower-case extension without the dot, e.g. \"zip\""),
            );
        }
        check_uploader_ref(s, out, &path, name);
    }
}

fn type_key(ty: DestinationType) -> &'static str {
    match ty {
        DestinationType::Image => "image",
        DestinationType::Text => "text",
        DestinationType::File => "file",
        DestinationType::Video => "video",
        DestinationType::UrlShortener => "url_shortener",
        DestinationType::UrlSharing => "url_sharing",
    }
}

fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEY_HINTS.iter().any(|h| k.contains(h))
        && !NON_SECRET_SUFFIXES.iter().any(|suf| k.ends_with(suf))
}

fn scan_secrets(v: &Value, path: &str, out: &mut Sink) {
    match v {
        Value::Table(t) => {
            for (k, child) in t {
                let p = format!("{path}.{k}");
                match child {
                    Value::String(s) if looks_secret(k) => {
                        if !s.is_empty() && !s.starts_with(KEYRING_PREFIX) {
                            out.error(
                                &p,
                                format!("looks like a plain-text secret. ssx never stores secrets in the settings file: save it in the OS keyring and put \"{KEYRING_PREFIX}<name>\" here instead"),
                            );
                        }
                    }
                    other => scan_secrets(other, &p, out),
                }
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                scan_secrets(child, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn validate_uploaders(s: &Settings, out: &mut Sink) {
    for (name, table) in &s.uploaders {
        let path = format!("uploaders.{name}");
        if !valid_uploader_name(name) {
            out.error(
                &path,
                "invalid uploader name; use letters, digits, '.', '_' or '-' (at most 64 characters)",
            );
        }
        scan_secrets(&Value::Table(table.clone()), &path, out);
    }
}

fn valid_workflow_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn valid_cli_name(n: &str) -> bool {
    let mut chars = n.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.len() <= 32
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn validate_workflows_and_hotkeys(s: &Settings, out: &mut Sink) {
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    let mut clis: BTreeMap<&str, usize> = BTreeMap::new();
    let mut hotkeys: BTreeMap<Hotkey, String> = BTreeMap::new();

    let mut register_hotkey = |out: &mut Sink, path: String, text: &str| match text
        .parse::<Hotkey>()
    {
        Ok(hk) => {
            if let Some(prev) = hotkeys.get(&hk) {
                out.error(
                    &path,
                    format!("hotkey {hk} is already used by {prev}; each hotkey can trigger only one thing"),
                );
            } else {
                hotkeys.insert(hk, path);
            }
        }
        Err(e) => out.error(&path, e.to_string()),
    };

    for (i, w) in s.workflows.iter().enumerate() {
        let base = format!("workflows[{i}]");
        if !valid_workflow_id(&w.id) {
            out.error(
                format!("{base}.id"),
                format!("{:?} is not a valid workflow id; use 1-64 characters from a-z, 0-9, '.', '_' and '-'", w.id),
            );
        } else if let Some(prev) = ids.insert(w.id.as_str(), i) {
            out.error(
                format!("{base}.id"),
                format!("duplicate workflow id {:?} (also used by workflows[{prev}]); ids must be unique", w.id),
            );
        }
        if w.name.trim().is_empty() {
            out.error(
                format!("{base}.name"),
                "give the workflow a name so it can be found in menus",
            );
        }
        if let Some(cli) = &w.trigger.cli_name {
            if !valid_cli_name(cli) {
                out.error(
                    format!("{base}.trigger.cli_name"),
                    format!("{cli:?} is not a valid command name; use lower-case letters, digits and '-', at most 32 characters"),
                );
            } else if let Some(prev) = clis.insert(cli.as_str(), i) {
                out.error(
                    format!("{base}.trigger.cli_name"),
                    format!("duplicate command name {cli:?} (also used by workflows[{prev}])"),
                );
            }
        }
        if let Some(hk) = &w.trigger.hotkey {
            register_hotkey(out, format!("{base}.trigger.hotkey"), hk);
        }
        validate_workflow_steps(s, w, &base, out);
    }
    for (name, hk) in s.hotkeys.entries() {
        register_hotkey(out, name.to_owned(), hk);
    }
}

fn validate_workflow_steps(s: &Settings, w: &Workflow, base: &str, out: &mut Sink) {
    use AfterCapture as C;
    let ac = &w.after_capture;
    let pos = |t: AfterCapture| ac.iter().position(|x| *x == t);

    for (i, step) in ac.iter().enumerate() {
        if ac[..i].contains(step) {
            out.warn(
                format!("{base}.after_capture[{i}]"),
                format!("{step:?} appears more than once; the later copy is redundant"),
            );
        }
    }
    if w.input.is_recording() {
        for t in [C::OpenEditor, C::CopyImageToClipboard, C::PinToScreen, C::Ocr] {
            if let Some(i) = pos(t) {
                out.error(
                    format!("{base}.after_capture[{i}]"),
                    format!("{t:?} only works on images, but this workflow's input is a recording; remove it"),
                );
            }
        }
    }
    if let (Some(edit), Some(save)) = (pos(C::OpenEditor), pos(C::SaveToFile))
        && edit > save
    {
        out.warn(
                format!("{base}.after_capture[{edit}]"),
                "open_editor runs after save_to_file, so the saved file will not contain your edits; put open_editor first",
            );
    }
    if let (Some(edit), Some(up)) = (pos(C::OpenEditor), pos(C::Upload))
        && edit > up
    {
        out.warn(
                format!("{base}.after_capture[{edit}]"),
                "open_editor runs after upload, so the upload will not contain your edits; put open_editor first",
            );
    }
    if let Some(del) = pos(C::DeleteLocalFile) {
        match pos(C::Upload) {
            None => out.warn(
                format!("{base}.after_capture[{del}]"),
                "delete_local_file only runs after a confirmed upload, but this workflow has no upload step, so it will never delete anything",
            ),
            Some(up) if up > del => out.warn(
                format!("{base}.after_capture[{del}]"),
                "delete_local_file is listed before upload, so it can never run; move it after upload",
            ),
            Some(_) => {}
        }
        if w.input == InputKind::Files {
            out.warn(
                format!("{base}.after_capture[{del}]"),
                "delete_local_file never deletes files you passed in (ssx only deletes files it created), so it has no effect for this input",
            );
        }
        if pos(C::SaveToFile).is_none() && !w.input.is_recording() && w.input != InputKind::Files {
            out.warn(
                format!("{base}.after_capture[{del}]"),
                "there is no save_to_file step, so no local file exists to delete",
            );
        }
    }
    if pos(C::SaveAsDialog).is_some() && pos(C::SaveToFile).is_some() {
        out.warn(
            format!("{base}.after_capture"),
            "both save_to_file and save_as_dialog are set; the image will be saved twice",
        );
    }

    let needs_url = |a: &AfterUpload| {
        !matches!(a, AfterUpload::ShowNotification | AfterUpload::RunCommand { .. })
    };
    if !w.uploads()
        && let Some(i) = w.after_upload.iter().position(needs_url)
    {
        out.warn(
                format!("{base}.after_upload[{i}]"),
                "this step needs a URL but the workflow has no upload step, so it will be skipped; add upload to after_capture",
            );
    }
    for (i, step) in w.after_upload.iter().enumerate() {
        let p = format!("{base}.after_upload[{i}]");
        if w.after_upload[..i].contains(step) {
            out.warn(&p, format!("{step:?} appears more than once; the later copy is redundant"));
        }
        if let AfterUpload::RunCommand { program, .. } = step
            && program.trim().is_empty()
        {
            out.error(&p, "run_command needs a program, e.g. program = \"/usr/bin/notify-send\"");
        }
    }
    for (ty, name) in w.destination.entries() {
        check_uploader_ref(s, out, &format!("{base}.destination.{}", type_key(ty)), name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Trigger, Workflow, workflow::builtin_workflows};

    fn errors(s: &Settings) -> Vec<ValidationIssue> {
        validate(s).into_iter().filter(|i| i.severity == Severity::Error).collect()
    }
    fn find<'a>(issues: &'a [ValidationIssue], path: &str) -> Option<&'a ValidationIssue> {
        issues.iter().find(|i| i.path == path)
    }

    #[test]
    fn defaults_have_no_issues_at_all() {
        let issues = validate(&Settings::default());
        assert!(issues.is_empty(), "{issues:#?}");
        let _ = builtin_workflows();
    }

    #[test]
    fn general_errors_are_actionable() {
        let mut s = Settings::default();
        s.general.image_quality = 0;
        s.general.file_name_pattern = "  ".into();
        s.general.max_file_name_len = 1000;
        s.general.subfolders.image = "a/b".into();
        s.general.subfolders.video = "CON".into();
        s.general.save_dir = Some(std::path::PathBuf::new());
        let e = errors(&s);
        for p in [
            "general.image_quality",
            "general.file_name_pattern",
            "general.max_file_name_len",
            "general.subfolders.image",
            "general.subfolders.video",
            "general.save_dir",
        ] {
            let issue = find(&e, p).unwrap_or_else(|| panic!("missing error for {p}: {e:#?}"));
            assert!(issue.message.len() > 15, "{issue}");
        }
        assert!(find(&e, "general.subfolders.video").unwrap().message.contains("_CON"));
    }

    #[test]
    fn subfolder_names_only_checked_when_used() {
        let mut s = Settings::default();
        s.general.use_type_subfolders = false;
        s.general.subfolders.image = "a/b".into();
        assert!(errors(&s).is_empty());
    }

    #[test]
    fn pattern_warnings() {
        let mut s = Settings::default();
        s.general.file_name_pattern = "%y-%foo-%hh".into();
        s.general.folder_pattern = "%bar".into();
        let w = validate(&s);
        assert!(w.iter().all(|i| i.severity == Severity::Warning));
        assert!(w.iter().any(|i| i.message.contains("%foo")));
        assert!(w.iter().any(|i| i.message.contains("%hh")));
        assert!(w.iter().any(|i| i.path == "general.folder_pattern" && i.message.contains("%bar")));
    }

    #[test]
    fn capture_ranges() {
        let mut s = Settings::default();
        s.capture.delay_ms = 61_000;
        s.capture.hdr.peak = 0.5;
        s.capture.hdr.knee = 1.5;
        s.capture.hdr.exposure = f32::NAN;
        let e = errors(&s);
        for p in
            ["capture.delay_ms", "capture.hdr.peak", "capture.hdr.knee", "capture.hdr.exposure"]
        {
            assert!(find(&e, p).is_some(), "{p}");
        }
        s.capture.hdr.peak = f32::INFINITY;
        assert!(find(&errors(&s), "capture.hdr.peak").is_some());
    }

    #[test]
    fn history_and_post_file_ranges() {
        let mut s = Settings::default();
        s.history.thumbnail_max_edge = 4;
        s.post_file.max_parallel_uploads = 0;
        let e = errors(&s);
        assert!(find(&e, "history.thumbnail_max_edge").is_some());
        assert!(find(&e, "post_file.max_parallel_uploads").is_some());
    }

    #[test]
    fn destination_checks() {
        let mut s = Settings::default();
        s.destinations.image = Some("bad name!".into());
        s.destinations.file = Some(String::new());
        s.destinations.video = Some("known".into());
        s.uploaders.insert("known".into(), toml::Table::new());
        s.destinations.extension_overrides.insert(".ZIP".into(), "known".into());
        let all = validate(&s);
        assert!(find(&all, "destinations.image").is_some_and(|i| i.severity == Severity::Error));
        assert!(find(&all, "destinations.file").is_some_and(|i| i.severity == Severity::Error));
        assert!(find(&all, "destinations.video").is_none(), "configured uploader is fine");
        assert!(find(&all, "destinations.extension_overrides..ZIP").is_some());
    }

    #[test]
    fn unconfigured_uploader_is_only_a_warning() {
        let mut s = Settings::default();
        s.destinations.image = Some("imgur".into());
        let all = validate(&s);
        let i = find(&all, "destinations.image").unwrap();
        assert_eq!(i.severity, Severity::Warning);
        assert!(i.message.contains("[uploaders.imgur]"));
    }

    #[test]
    fn plaintext_secrets_are_rejected() {
        let mut s = Settings::default();
        let t: toml::Table = toml::from_str(
            r#"
            endpoint = "https://x.example"
            api_key = "hunter2"
            token_url = "https://x.example/token"
            client_secret = "keyring:x-secret"
            access_token_keyring = "x-token"
            [nested]
            password = "pw"
            headers = [ { auth_token = "abc" } ]
            "#,
        )
        .unwrap();
        s.uploaders.insert("x".into(), t);
        let e = errors(&s);
        let paths: Vec<_> = e.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"uploaders.x.api_key"), "{paths:?}");
        assert!(paths.contains(&"uploaders.x.nested.password"), "{paths:?}");
        assert!(paths.contains(&"uploaders.x.nested.headers[0].auth_token"), "{paths:?}");
        assert!(!paths.contains(&"uploaders.x.client_secret"), "keyring reference is fine");
        assert!(!paths.contains(&"uploaders.x.token_url"));
        assert!(!paths.contains(&"uploaders.x.access_token_keyring"));
        assert!(!paths.contains(&"uploaders.x.endpoint"));
        assert!(e[0].message.contains("keyring"), "{}", e[0]);
    }

    #[test]
    fn empty_secret_value_is_allowed() {
        let mut s = Settings::default();
        s.uploaders.insert("x".into(), toml::from_str("api_key = \"\"").unwrap());
        assert!(errors(&s).is_empty());
    }

    fn wf(id: &str) -> Workflow {
        Workflow { id: id.into(), name: id.into(), ..Workflow::default() }
    }

    #[test]
    fn workflow_identity_errors() {
        let mut s = Settings {
            workflows: vec![
                Workflow { id: "Bad Id".into(), name: String::new(), ..Workflow::default() },
                wf("dup"),
                wf("dup"),
            ],
            ..Settings::default()
        };
        s.workflows[1].trigger.cli_name = Some("go".into());
        s.workflows[2].trigger.cli_name = Some("go".into());
        s.workflows[0].trigger.cli_name = Some("UPPER".into());
        let e = errors(&s);
        assert!(find(&e, "workflows[0].id").is_some());
        assert!(find(&e, "workflows[0].name").is_some());
        assert!(find(&e, "workflows[2].id").is_some_and(|i| i.message.contains("workflows[1]")));
        assert!(find(&e, "workflows[0].trigger.cli_name").is_some());
        assert!(find(&e, "workflows[2].trigger.cli_name").is_some());
    }

    #[test]
    fn hotkey_errors_and_duplicates() {
        let mut s = Settings { workflows: vec![wf("a"), wf("b"), wf("c")], ..Settings::default() };
        s.workflows[0].trigger.hotkey = Some("ctrl+shift+printscreen".into());
        s.workflows[1].trigger.hotkey = Some("Shift+Ctrl+PrtSc".into());
        s.workflows[2].trigger.hotkey = Some("ctrl+".into());
        s.hotkeys.open_history = Some("Ctrl+Shift+PrintScreen".into());
        s.hotkeys.open_settings = Some("F13".into());
        let e = errors(&s);
        let dup = find(&e, "workflows[1].trigger.hotkey").expect("duplicate flagged");
        assert!(dup.message.contains("workflows[0].trigger.hotkey"), "{dup}");
        assert!(find(&e, "workflows[2].trigger.hotkey").is_some());
        assert!(find(&e, "hotkeys.open_history").is_some());
        assert!(find(&e, "hotkeys.open_settings").is_none());
    }

    #[test]
    fn workflow_step_ordering_warnings() {
        use AfterCapture as C;
        let mut s = Settings::default();
        let mut w = wf("x");
        w.after_capture =
            vec![C::SaveToFile, C::OpenEditor, C::DeleteLocalFile, C::Upload, C::Upload];
        s.workflows = vec![w];
        let all = validate(&s);
        assert!(errors(&s).is_empty());
        let msgs: Vec<_> = all.iter().map(|i| i.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("saved file will not contain your edits")),
            "{msgs:?}"
        );
        assert!(msgs.iter().any(|m| m.contains("can never run")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("more than once")), "{msgs:?}");
    }

    #[test]
    fn image_steps_on_recordings_are_errors() {
        let mut s = Settings::default();
        let mut w = wf("v");
        w.input = InputKind::RecordScreen;
        w.after_capture = vec![AfterCapture::OpenEditor, AfterCapture::Upload];
        s.workflows = vec![w];
        assert!(find(&errors(&s), "workflows[0].after_capture[0]").is_some());
    }

    #[test]
    fn delete_without_upload_and_for_files_warns() {
        let mut s = Settings::default();
        let mut a = wf("a");
        a.after_capture = vec![AfterCapture::SaveToFile, AfterCapture::DeleteLocalFile];
        let mut b = wf("b");
        b.input = InputKind::Files;
        b.after_capture = vec![AfterCapture::Upload, AfterCapture::DeleteLocalFile];
        s.workflows = vec![a, b];
        let all = validate(&s);
        assert!(all.iter().any(|i| i.path.starts_with("workflows[0]") && i.message.contains("no upload step")));
        assert!(all.iter().any(|i| i.path.starts_with("workflows[1]")
            && i.message.contains("never deletes files you passed")));
    }

    #[test]
    fn after_upload_checks() {
        let mut s = Settings::default();
        let mut w = wf("u");
        w.after_upload = vec![
            AfterUpload::CopyUrl,
            AfterUpload::RunCommand { program: " ".into(), args: vec![] },
        ];
        s.workflows = vec![w];
        let all = validate(&s);
        assert!(
            all.iter().any(
                |i| i.path == "workflows[0].after_upload[0]" && i.severity == Severity::Warning
            )
        );
        assert!(find(&errors(&s), "workflows[0].after_upload[1]").is_some());
    }

    #[test]
    fn workflow_destination_overrides_are_checked() {
        let mut s = Settings::default();
        let mut w = wf("d");
        w.destination.video = Some("!!".into());
        s.workflows = vec![w];
        assert!(find(&errors(&s), "workflows[0].destination.video").is_some());
        let _ = Trigger::default();
    }

    #[test]
    fn issue_display() {
        let i = ValidationIssue {
            severity: Severity::Error,
            path: "a.b".into(),
            message: "bad".into(),
        };
        assert_eq!(i.to_string(), "error: a.b: bad");
    }
}
