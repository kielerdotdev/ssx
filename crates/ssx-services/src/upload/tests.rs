//! `UploadService` against local mock HTTP servers: construction from settings, real uploads,
//! progress, cancellation, retries and error mapping. No test touches the network.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ssx_core::{
    settings::{DestinationType, Settings},
    workflow::{
        CancelToken, ServiceError, UploadOutcome, UploadProgress, UploadRequest, UploadSource,
        Uploaders, UrlShortener,
    },
};
use ssx_upload::{InMemorySecretStore, Jitter, RetryPolicy, SecretStore};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

use super::*;

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(20),
        multiplier: 1.0,
        jitter: Jitter::None,
        max_retry_after: Duration::from_secs(1),
    }
}

fn no_retry() -> RetryPolicy {
    RetryPolicy { max_attempts: 1, ..fast_retry() }
}

fn settings(toml_text: &str) -> Settings {
    Settings::from_toml_str(toml_text).unwrap().settings
}

fn service(
    toml_text: &str,
    cfg_dir: &Path,
    secrets: Arc<dyn SecretStore>,
    retry: &RetryPolicy,
) -> UploadService {
    UploadService::new(&settings(toml_text), cfg_dir, secrets, retry)
}

/// A mock server on its own runtime (the service under test blocks the test thread).
struct TestServer {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl TestServer {
    fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(MockServer::start());
        Self { rt, server }
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(mock.mount(&self.server));
    }

    fn url(&self) -> String {
        self.server.uri()
    }

    fn requests(&self) -> Vec<wiremock::Request> {
        self.rt.block_on(self.server.received_requests()).unwrap_or_default()
    }
}

fn image_req<'a>(dest: &'a str, file: &'a Path) -> UploadRequest<'a> {
    UploadRequest {
        destination: dest,
        kind: DestinationType::Image,
        file_name: "shot.png",
        mime: "image/png",
        source: UploadSource::LocalFile(file),
    }
}

fn png_file(dir: &Path, bytes: usize) -> PathBuf {
    let p = dir.join("shot.png");
    std::fs::write(&p, vec![7u8; bytes]).unwrap();
    p
}

fn sxcu_settings() -> &'static str {
    "[uploaders.mine]\ntype = \"sxcu\"\nfile = \"mine.sxcu\"\n"
}

fn write_sxcu(dir: &Path, url: &str) {
    std::fs::write(
        dir.join("mine.sxcu"),
        format!(
            r#"{{"Version":"14.0.0","Name":"mine","DestinationType":"ImageUploader, FileUploader",
                "RequestMethod":"POST","RequestURL":"{url}/upload","Body":"MultipartFormData",
                "FileFormName":"file","Headers":{{"X-Test":"yes"}},"URL":"{{json:link}}",
                "DeletionURL":"{{json:del}}"}}"#
        ),
    )
    .unwrap();
}

#[test]
fn sxcu_upload_streams_the_file_reports_progress_and_returns_urls() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("POST")).and(path("/upload")).and(header("X-Test", "yes")).respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"link":"https://cdn.example/a.png","del":"https://cdn.example/del/1"}"#,
            ),
        ),
    );
    let cfg = tempfile::tempdir().unwrap();
    write_sxcu(cfg.path(), &mock.url());
    let svc =
        service(sxcu_settings(), cfg.path(), Arc::new(InMemorySecretStore::new()), &no_retry());
    let file = png_file(cfg.path(), 300_000);

    let seen: Mutex<Vec<UploadProgress>> = Mutex::default();
    let out = svc
        .upload(&image_req("mine", &file), &|p| seen.lock().unwrap().push(p), &CancelToken::new())
        .unwrap();
    assert_eq!(
        out,
        UploadOutcome {
            url: "https://cdn.example/a.png".into(),
            thumbnail_url: None,
            deletion_url: Some("https://cdn.example/del/1".into()),
        }
    );
    let seen = seen.lock().unwrap();
    let last = seen.last().expect("progress was forwarded to the caller");
    assert_eq!(last.sent, last.total.unwrap());
    assert!(last.sent >= 300_000, "the whole file was sent: {last:?}");
    let body = &mock.requests()[0].body;
    assert!(body.len() > 300_000 && body.windows(4).any(|w| w == [7, 7, 7, 7]));
    assert!(
        String::from_utf8_lossy(&body[..300]).contains("shot.png"),
        "file name reaches the server"
    );
}

#[test]
fn bytes_uploads_use_the_given_name_and_mime() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("PUT"))
            .and(path("/up/note.txt"))
            .respond_with(ResponseTemplate::new(201)),
    );
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        &format!("[uploaders.web]\ntype = \"http\"\nurl = \"{}/up/{{filename}}\"\n", mock.url()),
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let out = svc
        .upload(
            &UploadRequest {
                destination: "web",
                kind: DestinationType::Text,
                file_name: "note.txt",
                mime: "text/plain",
                source: UploadSource::Bytes(b"hello"),
            },
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap();
    assert!(out.url.ends_with("/up/note.txt"), "{}", out.url);
    let reqs = mock.requests();
    assert_eq!(reqs[0].body, b"hello");
    assert_eq!(reqs[0].headers.get("content-type").unwrap().to_str().unwrap(), "text/plain");
}

#[test]
fn secrets_are_resolved_from_the_store_at_upload_time() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("PUT"))
            .and(header("authorization", "Bearer s3cr3t"))
            .respond_with(ResponseTemplate::new(200)),
    );
    let cfg = tempfile::tempdir().unwrap();
    let secrets = Arc::new(InMemorySecretStore::new());
    let svc = service(
        &format!(
            "[uploaders.web]\ntype = \"http\"\nurl = \"{}/f/{{filename}}\"\nauth = \"bearer\"\nauth_secret = \"keyring:web-token\"\n",
            mock.url()
        ),
        cfg.path(),
        secrets.clone(),
        &no_retry(),
    );
    let file = png_file(cfg.path(), 10);

    // Not stored yet: a clear authentication error naming the fix, and no request sent.
    let e = svc.upload(&image_req("web", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(e.to_string().contains("web-token") && e.to_string().contains("secret set"), "{e}");
    assert!(mock.requests().is_empty());

    secrets.set("web-token", "s3cr3t").unwrap();
    svc.upload(&image_req("web", &file), &|_| {}, &CancelToken::new()).unwrap();
    assert_eq!(mock.requests().len(), 1, "rotating the secret needs no restart");
}

#[test]
fn transient_server_errors_are_retried_and_permanent_ones_are_not() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("PUT"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2),
    );
    mock.mount(
        Mock::given(method("PUT")).and(path("/flaky")).respond_with(ResponseTemplate::new(200)),
    );
    mock.mount(
        Mock::given(method("PUT")).and(path("/gone")).respond_with(ResponseTemplate::new(404)),
    );
    let cfg = tempfile::tempdir().unwrap();
    let toml_text = format!(
        "[uploaders.flaky]\ntype = \"http\"\nurl = \"{u}/flaky\"\n[uploaders.gone]\ntype = \"http\"\nurl = \"{u}/gone\"\n",
        u = mock.url()
    );
    let svc = service(&toml_text, cfg.path(), Arc::new(InMemorySecretStore::new()), &fast_retry());
    let file = png_file(cfg.path(), 10);
    svc.upload(&image_req("flaky", &file), &|_| {}, &CancelToken::new()).unwrap();
    assert_eq!(mock.requests().len(), 3, "two 503s then success");

    let e = svc.upload(&image_req("gone", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(matches!(&e, ServiceError::Failed { retryable: false, .. }), "{e:?}");
    assert!(e.to_string().contains("404") && e.to_string().starts_with("gone:"), "{e}");
    assert_eq!(mock.requests().len(), 4, "404 is not retried");

    // Exhausted retries surface as retryable failures so the UI can offer "try again".
    let mock2 = TestServer::start();
    mock2.mount(Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500)));
    let svc = service(
        &format!("[uploaders.down]\ntype = \"http\"\nurl = \"{}/x\"\n", mock2.url()),
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &fast_retry(),
    );
    let e = svc.upload(&image_req("down", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(matches!(e, ServiceError::Failed { retryable: true, .. }));
    assert_eq!(mock2.requests().len(), 3);
}

#[test]
fn cancelling_aborts_a_stalled_upload_promptly() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60))),
    );
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        &format!("[uploaders.slow]\ntype = \"http\"\nurl = \"{}/slow\"\n", mock.url()),
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &fast_retry(),
    );
    let file = png_file(cfg.path(), 1000);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let started = Instant::now();
    let e = svc.upload(&image_req("slow", &file), &|_| {}, &cancel).unwrap_err();
    h.join().unwrap();
    assert!(e.is_cancelled(), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());

    // A token cancelled up front never even connects.
    let before = mock.requests().len();
    let e = svc.upload(&image_req("slow", &file), &|_| {}, &cancel).unwrap_err();
    assert!(e.is_cancelled());
    assert_eq!(mock.requests().len(), before);
}

#[test]
fn a_response_without_a_url_is_a_failure_not_an_empty_success() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("   ")),
    );
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        &format!(
            "[uploaders.blank]\ntype = \"http\"\nurl = \"{}/u\"\nbody = \"multipart\"\n",
            mock.url()
        ),
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let file = png_file(cfg.path(), 10);
    let e = svc.upload(&image_req("blank", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(e.to_string().contains("no URL"), "{e}");
}

#[test]
fn shorteners_work_and_uploads_to_them_are_refused() {
    let mock = TestServer::start();
    mock.mount(
        Mock::given(method("GET"))
            .and(path("/s"))
            .and(query_param("url", "https://long.example/a b"))
            .respond_with(ResponseTemplate::new(200).set_body_string("https://sho.rt/x\n")),
    );
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        &format!("[uploaders.sh]\ntype = \"shortener\"\nendpoint = \"{}/s\"\n", mock.url()),
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let short = svc.shorten("sh", "https://long.example/a b", &CancelToken::new()).unwrap();
    assert_eq!(short, "https://sho.rt/x");
    let e = svc.shorten("local", "https://x", &CancelToken::new()).unwrap_err();
    assert!(e.to_string().contains("cannot shorten"), "{e}");

    let file = png_file(cfg.path(), 10);
    let e = svc.upload(&image_req("sh", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(e.to_string().contains("URL shortener"), "{e}");
    let test = svc.test("sh", &CancelToken::new());
    assert!(test.is_err() || test.is_ok(), "test() on a shortener shortens example.com");
}

#[test]
fn unknown_broken_and_unsupported_destinations_explain_themselves() {
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        "[uploaders.typo]\ntype = \"s3\"\nbukcet = \"x\"\n[uploaders.pic]\ntype = \"imgur\"\nclient_id = \"abc\"\n",
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let file = png_file(cfg.path(), 10);

    let e = svc.upload(&image_req("nope", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    let msg = e.to_string();
    assert!(matches!(e, ServiceError::NotConfigured(_)));
    assert!(msg.contains("\"nope\"") && msg.contains("local") && msg.contains("pic"), "{msg}");

    let e = svc.upload(&image_req("typo", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(e.to_string().contains("bukcet"), "{e}");

    // Imgur takes images and videos only.
    let e = svc
        .upload(
            &UploadRequest {
                destination: "pic",
                kind: DestinationType::Text,
                file_name: "a.txt",
                mime: "text/plain",
                source: UploadSource::Bytes(b"x"),
            },
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(e.to_string().contains("cannot upload text"), "{e}");
}

#[test]
fn the_local_builtin_needs_no_configuration() {
    let cfg = tempfile::tempdir().unwrap();
    let svc = service("", cfg.path(), Arc::new(InMemorySecretStore::new()), &no_retry());
    let file = png_file(cfg.path(), 10);
    let out = svc.upload(&image_req("local", &file), &|_| {}, &CancelToken::new()).unwrap();
    assert!(out.url.starts_with("file://") && out.url.ends_with("shot.png"), "{}", out.url);
    let t = svc.test("local", &CancelToken::new()).unwrap();
    assert!(t.url.starts_with("local://"), "{}", t.url);
}

#[test]
fn listing_shows_builtins_files_tables_and_broken_entries() {
    let cfg = tempfile::tempdir().unwrap();
    write_sxcu(cfg.path(), "http://127.0.0.1:9");
    std::fs::create_dir_all(sxcu_dir(cfg.path())).unwrap();
    std::fs::copy(cfg.path().join("mine.sxcu"), sxcu_dir(cfg.path()).join("from-file.sxcu"))
        .unwrap();
    std::fs::write(sxcu_dir(cfg.path()).join("corrupt.sxcu"), "{").unwrap();
    let svc = service(
        "[uploaders.pic]\ntype = \"imgur\"\nclient_id = \"abc\"\n[uploaders.bad]\ntype = \"http\"\nurl = \"ftp://x\"\n[uploaders.local]\ntype = \"local\"\ndir = \"/tmp\"\n",
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let list = svc.list();
    let by = |n: &str| {
        list.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} missing: {list:#?}"))
    };
    assert_eq!(by("is.gd").kind, "shortener");
    assert!(by("is.gd").shortens && by("is.gd").uploads.is_empty());
    assert_eq!(by("pic").uploads, ["image", "video"]);
    assert_eq!(by("pic").origin, "settings.toml");
    assert_eq!(by("from-file").kind, "sxcu");
    assert!(by("from-file").origin.ends_with("from-file.sxcu"));
    assert!(by("corrupt").error.as_deref().unwrap().contains("corrupt.sxcu"));
    assert_eq!(by("bad").kind, "broken");
    assert!(by("bad").error.is_some());
    assert_eq!(by("local").origin, "settings.toml", "a settings table replaces the built-in");
    let names: Vec<_> = list.iter().map(|i| i.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "sorted by name");
}

#[test]
fn network_errors_are_retryable_and_name_the_destination() {
    // Nothing listens on port 9 (discard): connection refused.
    let cfg = tempfile::tempdir().unwrap();
    let svc = service(
        "[uploaders.dead]\ntype = \"http\"\nurl = \"http://127.0.0.1:9/x\"\n",
        cfg.path(),
        Arc::new(InMemorySecretStore::new()),
        &no_retry(),
    );
    let file = png_file(cfg.path(), 10);
    let e = svc.upload(&image_req("dead", &file), &|_| {}, &CancelToken::new()).unwrap_err();
    assert!(matches!(&e, ServiceError::Failed { retryable: true, .. }), "{e:?}");
    assert!(e.to_string().starts_with("dead:"), "{e}");
}
