//! What destinations exist, which can take what, importing `.sxcu` files, and test uploads.
//!
//! The list comes straight from `ssx_services::UploadService::new` (built-ins, the
//! `uploaders/*.sxcu` files and the `[uploaders.*]` tables of the settings being edited), so
//! what this page shows is what the running app will resolve, including *broken* entries with
//! the reason. Building the registry does no network I/O.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ssx_core::{
    settings::{DestinationType, Settings},
    workflow::{
        CancelToken, ServiceError, UploadOutcome, UploadProgress, UploadRequest, UploadSource,
        Uploaders, UrlShortener,
    },
};
use ssx_services::{
    UploadService, UploaderInfo,
    upload::{
        ImportError, Imported, config::read_sxcu, import_sxcu, remove_sxcu, sxcu_dir,
        sxcu_files::sanitize_name,
    },
};
use ssx_upload::{RetryPolicy, SecretStore};

/// A destination as listed.
pub type Info = UploaderInfo;

/// The destinations known for one state of the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    /// Sorted by name.
    pub entries: Vec<Info>,
}

/// Where a destination came from, as a coarse category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Compiled in.
    Builtin,
    /// A table in settings.toml.
    Table,
    /// An imported `.sxcu` file.
    File,
}

impl Registry {
    /// Builds the registry the app would build from `settings` and the files in `config_dir`.
    pub fn build(settings: &Settings, config_dir: &Path, secrets: Arc<dyn SecretStore>) -> Self {
        let svc = UploadService::new(settings, config_dir, secrets, &RetryPolicy::default());
        Self { entries: svc.list() }
    }

    /// The entry called `name`.
    pub fn get(&self, name: &str) -> Option<&Info> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// All names.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// The category of an entry's origin.
    pub fn source(info: &Info) -> Source {
        match info.origin.as_str() {
            "built-in" => Source::Builtin,
            "settings.toml" => Source::Table,
            _ => Source::File,
        }
    }

    /// The destinations that can serve `ty`, working ones first. Broken entries are included
    /// (with `error` set) so that a workflow already pointing at one still shows it.
    pub fn choices_for(&self, ty: DestinationType) -> Vec<&Info> {
        let accepts = |e: &Info| match ty {
            DestinationType::Image => e.uploads.contains(&"image"),
            DestinationType::Text => e.uploads.contains(&"text"),
            DestinationType::File => e.uploads.contains(&"file"),
            DestinationType::Video => e.uploads.contains(&"video"),
            DestinationType::UrlShortener => e.shortens,
            DestinationType::UrlSharing => false,
        };
        let mut v: Vec<&Info> = self.entries.iter().filter(|e| e.error.is_none() && accepts(e)).collect();
        v.extend(self.entries.iter().filter(|e| e.error.is_some()));
        v
    }

    /// Whether `name` exists and works.
    pub fn works(&self, name: &str) -> bool {
        self.get(name).is_some_and(|e| e.error.is_none())
    }

    /// A problem with using `name` for `ty`, if any (for a picker's hint line).
    pub fn problem_for(&self, ty: DestinationType, name: &str) -> Option<String> {
        let Some(e) = self.get(name) else {
            return Some(format!(
                "there is no destination called {name:?}; built-in ones need no setup, others are added on the Uploaders page"
            ));
        };
        if let Some(err) = &e.error {
            return Some(format!("{name:?} cannot be used: {err}"));
        }
        let ok = match ty {
            DestinationType::Image => e.uploads.contains(&"image"),
            DestinationType::Text => e.uploads.contains(&"text"),
            DestinationType::File => e.uploads.contains(&"file"),
            DestinationType::Video => e.uploads.contains(&"video"),
            DestinationType::UrlShortener => e.shortens,
            DestinationType::UrlSharing => true,
        };
        (!ok).then(|| format!("{name:?} cannot handle this kind of content"))
    }

    /// The settings paths that refer to `name` (defaults, extension overrides, workflows),
    /// so that removing it can warn.
    pub fn references_to(settings: &Settings, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ty in DestinationType::ALL {
            if settings.destinations.default_for(ty) == Some(name) {
                out.push(format!("default {} destination", type_word(ty)));
            }
        }
        for (ext, n) in &settings.destinations.extension_overrides {
            if n == name {
                out.push(format!("the .{ext} override"));
            }
        }
        for w in &settings.workflows {
            for (ty, n) in w.destination.entries() {
                if n == name {
                    out.push(format!("workflow {:?} ({} destination)", w.name, type_word(ty)));
                }
            }
        }
        out
    }
}

/// A lower-case word for a destination type.
pub const fn type_word(ty: DestinationType) -> &'static str {
    match ty {
        DestinationType::Image => "image",
        DestinationType::Text => "text",
        DestinationType::File => "file",
        DestinationType::Video => "video",
        DestinationType::UrlShortener => "URL shortener",
        DestinationType::UrlSharing => "URL sharing",
    }
}

// ---- importing .sxcu ----------------------------------------------------------------------

/// What checking a `.sxcu` file found, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPreview {
    /// The file that was checked.
    pub source: PathBuf,
    /// The uploader's own name (from the file).
    pub display_name: String,
    /// A valid destination name derived from it.
    pub suggested_name: String,
    /// Unsupported features and other non-fatal findings.
    pub warnings: Vec<String>,
    /// Why the file cannot be imported at all.
    pub error: Option<String>,
}

impl ImportPreview {
    /// Whether the file can be imported.
    pub fn importable(&self) -> bool {
        self.error.is_none()
    }
}

/// Parses and validates `path` the way an import would.
pub fn preview_import(path: &Path) -> ImportPreview {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match read_sxcu(path) {
        Ok((def, warnings)) => {
            let display_name = def.display_name();
            let base = if def.name.trim().is_empty() { stem } else { def.name.clone() };
            ImportPreview {
                source: path.to_path_buf(),
                display_name,
                suggested_name: sanitize_name(&base),
                warnings,
                error: None,
            }
        }
        Err(e) => ImportPreview {
            source: path.to_path_buf(),
            display_name: String::new(),
            suggested_name: sanitize_name(&stem),
            warnings: Vec::new(),
            error: Some(e),
        },
    }
}

/// How an import under `name` would collide with what exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collision {
    /// Nothing is in the way.
    None,
    /// An imported file with this name exists and would be replaced.
    ReplacesFile,
    /// A built-in or a `[uploaders.<name>]` table has this name and would win over the file.
    Shadowed(String),
}

/// Checks `name` against the registry and the uploaders folder.
pub fn import_collision(registry: &Registry, config_dir: &Path, name: &str) -> Collision {
    if sxcu_dir(config_dir).join(format!("{name}.sxcu")).exists() {
        return Collision::ReplacesFile;
    }
    match registry.get(name).map(|e| (Registry::source(e), e)) {
        Some((Source::Builtin, _)) => Collision::Shadowed("a built-in destination".to_owned()),
        Some((Source::Table, _)) => {
            Collision::Shadowed("a [uploaders.*] table in settings.toml, which takes precedence".to_owned())
        }
        _ => Collision::None,
    }
}

/// Imports `source` as `name` (replacing an existing imported file only with `overwrite`).
pub fn import(
    config_dir: &Path,
    source: &Path,
    name: &str,
    overwrite: bool,
) -> Result<Imported, ImportError> {
    import_sxcu(config_dir, source, Some(name), overwrite)
}

/// Deletes an imported `.sxcu` file.
pub fn remove_file(config_dir: &Path, name: &str) -> std::io::Result<bool> {
    remove_sxcu(config_dir, name)
}

// ---- test uploads -------------------------------------------------------------------------

/// A small PNG made on the spot: a 48x32 gradient, a few hundred bytes.
pub fn test_png() -> Vec<u8> {
    let (w, h) = (48u32, 32u32);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[
                (30 + x * 4) as u8,
                (90 + y * 4) as u8,
                (220 - x * 2) as u8,
                255,
            ]);
        }
    }
    ssx_types::Frame::from_rgba8(w, h, data)
        .ok()
        .and_then(|f| f.encode(ssx_types::EncodeOptions::default()).ok())
        .unwrap_or_default()
}

/// Everything a test upload needs; owned so it can move to a worker thread.
#[derive(Clone)]
pub struct TestJob {
    /// The settings being edited (so unsaved changes can be tried).
    pub settings: Settings,
    /// Where `uploaders/*.sxcu` live.
    pub config_dir: PathBuf,
    /// The destination to test.
    pub name: String,
    /// Where secrets are read from.
    pub secrets: Arc<dyn SecretStore>,
}

impl std::fmt::Debug for TestJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestJob").field("name", &self.name).finish_non_exhaustive()
    }
}

/// Runs uploads for the window; blocks until done. Implemented by [`RealTester`] and by fakes.
pub trait UploadTester: Send + Sync + std::fmt::Debug {
    /// Uploads a tiny generated PNG (or shortens a URL) and reports progress.
    fn test(
        &self,
        job: &TestJob,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String>;

    /// Uploads the file at `path` (history "re-upload") to `job.name`.
    fn upload_file(
        &self,
        job: &TestJob,
        path: &Path,
        kind: DestinationType,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String>;
}

/// The real tester: builds the same `UploadService` the app uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealTester;

impl RealTester {
    fn service(job: &TestJob) -> UploadService {
        UploadService::new(&job.settings, &job.config_dir, job.secrets.clone(), &RetryPolicy::default())
    }
}

impl UploadTester for RealTester {
    fn upload_file(
        &self,
        job: &TestJob,
        path: &Path,
        kind: DestinationType,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String> {
        let svc = Self::service(job);
        let file_name =
            path.file_name().map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let req = UploadRequest {
            destination: &job.name,
            kind,
            file_name: &file_name,
            mime: mime_for(path),
            source: UploadSource::LocalFile(path),
        };
        svc.upload(&req, progress, cancel).map_err(service_error_text)
    }

    fn test(
        &self,
        job: &TestJob,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String> {
        let svc = Self::service(job);
        let info = svc
            .list()
            .into_iter()
            .find(|i| i.name == job.name)
            .ok_or_else(|| format!("there is no destination called {:?}", job.name))?;
        if let Some(e) = info.error {
            return Err(e);
        }
        let png = test_png();
        let (kind, file_name, mime, data): (DestinationType, &str, &str, &[u8]) =
            if info.uploads.contains(&"image") {
                (DestinationType::Image, "ssx-test.png", "image/png", &png)
            } else if info.uploads.contains(&"text") {
                (DestinationType::Text, "ssx-test.txt", "text/plain", b"ssx test upload\n")
            } else if info.uploads.contains(&"file") {
                (DestinationType::File, "ssx-test.bin", "application/octet-stream", b"ssx test upload\n")
            } else if info.shortens {
                return svc
                    .shorten(&job.name, "https://example.com/", cancel)
                    .map(UploadOutcome::url)
                    .map_err(service_error_text);
            } else {
                return Err(format!("{:?} does not accept any kind of upload", job.name));
            };
        let req = UploadRequest {
            destination: &job.name,
            kind,
            file_name,
            mime,
            source: UploadSource::Bytes(data),
        };
        svc.upload(&req, progress, cancel).map_err(service_error_text)
    }
}

/// A MIME type from the extension (only what a screenshot tool produces matters).
pub fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// A [`ServiceError`] as the text shown to the user.
pub fn service_error_text(e: ServiceError) -> String {
    e.to_string()
}

/// A scripted tester for tests and screenshots: reports `steps` of progress, then succeeds or
/// fails; honours cancellation between steps.
#[derive(Debug, Clone)]
pub struct FakeTester {
    /// Progress reports `(sent, total)`.
    pub steps: Vec<(u64, u64)>,
    /// Sleep between steps.
    pub delay: std::time::Duration,
    /// The result after the last step.
    pub result: Result<UploadOutcome, String>,
}

impl Default for FakeTester {
    fn default() -> Self {
        Self {
            steps: vec![(0, 1000), (500, 1000), (1000, 1000)],
            delay: std::time::Duration::ZERO,
            result: Ok(UploadOutcome {
                url: "https://i.example.com/ssx-test.png".to_owned(),
                thumbnail_url: None,
                deletion_url: Some("https://i.example.com/delete/abc".to_owned()),
            }),
        }
    }
}

impl UploadTester for FakeTester {
    fn upload_file(
        &self,
        job: &TestJob,
        _path: &Path,
        _kind: DestinationType,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String> {
        self.test(job, cancel, progress)
    }

    fn test(
        &self,
        _job: &TestJob,
        cancel: &CancelToken,
        progress: &dyn Fn(UploadProgress),
    ) -> Result<UploadOutcome, String> {
        for (sent, total) in &self.steps {
            if cancel.is_cancelled() {
                return Err("cancelled".to_owned());
            }
            progress(UploadProgress { sent: *sent, total: Some(*total) });
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
        }
        if cancel.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ssx_upload::InMemorySecretStore;

    use super::*;

    fn secrets() -> Arc<dyn SecretStore> {
        Arc::new(InMemorySecretStore::new())
    }

    fn registry_of(settings: &Settings, dir: &Path) -> Registry {
        Registry::build(settings, dir, secrets())
    }

    const SXCU: &str = r#"{
        "Version": "13.7.0",
        "Name": "My Server",
        "DestinationType": "ImageUploader",
        "RequestMethod": "POST",
        "RequestURL": "https://example.com/upload",
        "Body": "MultipartFormData",
        "FileFormName": "file",
        "URL": "{json:url}"
    }"#;

    #[test]
    fn the_registry_lists_builtins_tables_and_files_with_broken_ones_marked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("uploaders")).unwrap();
        std::fs::write(dir.path().join("uploaders/my-server.sxcu"), SXCU).unwrap();
        let mut s = Settings::default();
        s.uploaders.insert("imgur".into(), r#"type='imgur'
client_id='abc'"#.parse().unwrap());
        s.uploaders.insert("broken".into(), "type='http'".parse().unwrap());
        let r = registry_of(&s, dir.path());
        let names = r.names();
        for n in ["local", "is.gd", "v.gd", "tinyurl", "my-server", "imgur", "broken"] {
            assert!(names.contains(&n.to_owned()), "{n} in {names:?}");
        }
        assert_eq!(Registry::source(r.get("local").unwrap()), Source::Builtin);
        assert_eq!(Registry::source(r.get("imgur").unwrap()), Source::Table);
        assert_eq!(Registry::source(r.get("my-server").unwrap()), Source::File);
        assert!(r.get("broken").unwrap().error.as_deref().unwrap().contains("url"));
        assert!(r.works("imgur") && !r.works("broken") && !r.works("missing"));
    }

    #[test]
    fn pickers_offer_only_destinations_that_can_take_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        s.uploaders.insert("imgur".into(), "type='imgur'\nclient_id='abc'".parse().unwrap());
        s.uploaders.insert("oops".into(), "type='http'".parse().unwrap());
        let r = registry_of(&s, dir.path());
        let names = |ty| r.choices_for(ty).iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert!(names(DestinationType::Image).contains(&"imgur".to_owned()));
        assert!(names(DestinationType::Image).contains(&"local".to_owned()));
        assert!(!names(DestinationType::UrlShortener).contains(&"imgur".to_owned()));
        let short = names(DestinationType::UrlShortener);
        assert!(short.contains(&"is.gd".to_owned()) && short.contains(&"tinyurl".to_owned()));
        assert!(!short.contains(&"local".to_owned()));
        assert!(names(DestinationType::Video).contains(&"local".to_owned()));
        assert!(!names(DestinationType::Video).contains(&"is.gd".to_owned()), "shorteners take no uploads");
        assert!(names(DestinationType::Image).last() == Some(&"oops".to_owned()), "broken ones last");
        assert!(r.choices_for(DestinationType::UrlSharing).iter().all(|e| e.error.is_some()));
    }

    #[test]
    fn problems_explain_unknown_broken_and_unsuitable_choices() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        s.uploaders.insert("oops".into(), "type='http'".parse().unwrap());
        let r = registry_of(&s, dir.path());
        assert!(r.problem_for(DestinationType::Image, "nope").unwrap().contains("no destination"));
        assert!(r.problem_for(DestinationType::Image, "oops").unwrap().contains("cannot be used"));
        assert!(r.problem_for(DestinationType::Image, "is.gd").unwrap().contains("cannot handle"));
        assert!(r.problem_for(DestinationType::UrlShortener, "is.gd").is_none());
        assert!(r.problem_for(DestinationType::Image, "local").is_none());
    }

    #[test]
    fn references_are_found_in_defaults_extensions_and_workflows() {
        let mut s = Settings::default();
        s.destinations.image = Some("imgur".into());
        s.destinations.extension_overrides.insert("zip".into(), "imgur".into());
        s.workflows[0].destination.video = Some("imgur".into());
        let refs = Registry::references_to(&s, "imgur");
        assert_eq!(refs.len(), 3, "{refs:?}");
        assert!(refs.iter().any(|r| r.contains("default image")));
        assert!(refs.iter().any(|r| r.contains(".zip")));
        assert!(refs.iter().any(|r| r.contains("video destination")));
        assert!(Registry::references_to(&s, "other").is_empty());
    }

    #[test]
    fn previewing_a_valid_sxcu_suggests_a_name_and_shows_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("whatever.sxcu");
        std::fs::write(&p, SXCU).unwrap();
        let pv = preview_import(&p);
        assert!(pv.importable(), "{:?}", pv.error);
        assert_eq!(pv.display_name, "My Server");
        assert_eq!(pv.suggested_name, "My-Server");
        assert_eq!(pv.source, p);
    }

    #[test]
    fn previewing_a_broken_sxcu_explains_why() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.sxcu");
        std::fs::write(&p, "{ not json").unwrap();
        let pv = preview_import(&p);
        assert!(!pv.importable());
        assert!(pv.error.as_deref().unwrap().contains("bad.sxcu"));
        assert_eq!(pv.suggested_name, "bad");
        let missing = preview_import(&dir.path().join("missing.sxcu"));
        assert!(missing.error.unwrap().contains("cannot read"));
        let p = dir.path().join("empty.sxcu");
        std::fs::write(&p, r#"{"Version":"13.7.0"}"#).unwrap();
        assert!(!preview_import(&p).importable(), "no request URL");
    }

    #[test]
    fn importing_copies_the_file_and_the_registry_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.sxcu");
        std::fs::write(&src, SXCU).unwrap();
        let cfg = dir.path().join("cfg");
        let done = import(&cfg, &src, "my-server", false).unwrap();
        assert_eq!(done.name, "my-server");
        assert!(done.path.is_file());
        let r = registry_of(&Settings::default(), &cfg);
        assert!(r.works("my-server"));
        // a second import needs overwrite
        assert!(matches!(import(&cfg, &src, "my-server", false), Err(ImportError::Exists(_))));
        assert_eq!(import_collision(&r, &cfg, "my-server"), Collision::ReplacesFile);
        import(&cfg, &src, "my-server", true).unwrap();
        assert!(matches!(import(&cfg, &src, "bad name", false), Err(ImportError::BadName(_))));
        // removal
        assert!(remove_file(&cfg, "my-server").unwrap());
        assert!(!remove_file(&cfg, "my-server").unwrap());
        assert!(!registry_of(&Settings::default(), &cfg).works("my-server"));
    }

    #[test]
    fn collisions_with_builtins_and_tables_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        s.uploaders.insert("mine".into(), "type='local'".parse().unwrap());
        let r = registry_of(&s, dir.path());
        assert!(matches!(import_collision(&r, dir.path(), "local"), Collision::Shadowed(w) if w.contains("built-in")));
        assert!(matches!(import_collision(&r, dir.path(), "mine"), Collision::Shadowed(w) if w.contains("precedence")));
        assert_eq!(import_collision(&r, dir.path(), "fresh"), Collision::None);
    }

    #[test]
    fn the_test_image_is_a_small_valid_png() {
        let png = test_png();
        assert!(png.starts_with(b"\x89PNG"), "{:?}", &png[..8.min(png.len())]);
        assert!(png.len() < 8 * 1024, "{} bytes", png.len());
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (48, 32));
    }

    fn job(settings: Settings, name: &str, dir: &Path) -> TestJob {
        TestJob { settings, config_dir: dir.to_path_buf(), name: name.into(), secrets: secrets() }
    }

    #[test]
    fn the_real_tester_uploads_to_the_builtin_local_destination() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Mutex::new(Vec::new());
        let out = RealTester
            .test(&job(Settings::default(), "local", dir.path()), &CancelToken::new(), &|p| {
                seen.lock().unwrap().push(p);
            })
            .unwrap();
        assert!(out.url.starts_with("file://") || out.url.contains("ssx-test"), "{}", out.url);
    }

    #[test]
    fn the_real_tester_reports_unknown_and_broken_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let e = RealTester.test(&job(Settings::default(), "nope", dir.path()), &cancel, &|_| {}).unwrap_err();
        assert!(e.contains("no destination"), "{e}");
        let mut s = Settings::default();
        s.uploaders.insert("oops".into(), "type='http'".parse().unwrap());
        let e = RealTester.test(&job(s, "oops", dir.path()), &cancel, &|_| {}).unwrap_err();
        assert!(e.contains("url"), "{e}");
    }

    #[test]
    fn the_fake_tester_reports_progress_and_honours_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let f = FakeTester::default();
        let seen = Mutex::new(Vec::new());
        let out = f
            .test(&job(Settings::default(), "x", dir.path()), &CancelToken::new(), &|p| seen.lock().unwrap().push(p.sent))
            .unwrap();
        assert!(out.url.contains("ssx-test"));
        assert_eq!(*seen.lock().unwrap(), [0, 500, 1000]);
        let c = CancelToken::new();
        c.cancel();
        assert_eq!(f.test(&job(Settings::default(), "x", dir.path()), &c, &|_| {}).unwrap_err(), "cancelled");
    }
}
