//! Error mapping, retry behaviour against a real (mock) server, and engine edge cases.
#![cfg(feature = "sxcu")]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ctx, header, only_request};
use ssx_upload::sxcu::{Interaction, SxcuUploader};
use ssx_upload::{
    Jitter, RetryPolicy, RetryingUploader, UploadError, UploadKind, UploadRequest, Uploader,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn text_uploader(url: &str, extra: &str) -> SxcuUploader {
    let json = format!(
        r#"{{"Version":"14.1.0","Name":"t","DestinationType":"TextUploader","RequestMethod":"POST",
            "RequestURL":"{url}","Body":"FormURLEncoded","Arguments":{{"t":"{{input}}"}}{extra}}}"#
    );
    SxcuUploader::from_json_str(&json).expect("uploader")
}

fn def(json: &str) -> Result<SxcuUploader, UploadError> {
    SxcuUploader::from_json_str(json)
}

async fn status_error(status: u16, headers: &[(&str, &str)], body: &str) -> UploadError {
    let server = MockServer::start().await;
    let mut resp = ResponseTemplate::new(status).set_body_string(body);
    for (k, v) in headers {
        resp = resp.insert_header(*k, *v);
    }
    Mock::given(method("POST")).respond_with(resp).mount(&server).await;
    text_uploader(&server.uri(), "").upload(&UploadRequest::text("x"), &ctx()).await.expect_err("must fail")
}

#[tokio::test]
async fn http_statuses_map_to_actionable_errors() {
    assert!(matches!(status_error(401, &[], "bad token").await, UploadError::Auth { message } if message.contains("bad token")));
    assert!(matches!(status_error(403, &[], "forbidden").await, UploadError::Auth { .. }));
    match status_error(429, &[("Retry-After", "7")], "slow down").await {
        UploadError::RateLimited { retry_after } => assert_eq!(retry_after, Some(Duration::from_secs(7))),
        other => panic!("{other:?}"),
    }
    match status_error(503, &[("Retry-After", "3")], "maintenance").await {
        UploadError::Http { status: 503, retry_after, body_snippet, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(3)));
            assert_eq!(body_snippet, "maintenance");
        }
        other => panic!("{other:?}"),
    }
    let e = status_error(500, &[], &"x".repeat(5000)).await;
    match &e {
        UploadError::Http { status: 500, body_snippet, .. } => assert!(body_snippet.chars().count() <= 301, "snippet is bounded"),
        other => panic!("{other:?}"),
    }
    assert!(e.is_retryable());
    let e = status_error(404, &[], "nope").await;
    assert!(!e.is_retryable());
    assert!(e.to_string().contains("404"));
}

#[tokio::test]
async fn connection_failure_is_a_retryable_network_error() {
    // Bind then drop to obtain a port nothing listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let u = text_uploader(&format!("http://127.0.0.1:{port}/x"), "");
    let err = u.upload(&UploadRequest::text("x"), &ctx()).await.expect_err("refused");
    assert!(matches!(err, UploadError::Network { .. }), "{err:?}");
    assert!(err.is_retryable());
    assert!(!err.to_string().contains(&port.to_string()) || !err.to_string().contains("127.0.0.1:"), "URLs are stripped from network errors: {err}");
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 4,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
        jitter: Jitter::None,
        ..RetryPolicy::default()
    }
}

#[tokio::test]
async fn retries_5xx_and_resends_the_file_from_disk() {
    let server = MockServer::start().await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(503)).up_to_n_times(2).mount(&server).await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200).set_body_string("https://ok/1")).mount(&server).await;
    let json = format!(
        r#"{{"Version":"14.1.0","Name":"bin","DestinationType":"FileUploader","RequestMethod":"PUT","RequestURL":"{}/up","Body":"Binary","URL":"{{response}}"}}"#,
        server.uri()
    );
    let inner: Arc<dyn Uploader> = Arc::new(def(&json).expect("uploader"));
    let up = RetryingUploader::new(inner, fast_policy());
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    let payload: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    std::fs::write(&file, &payload).expect("write");
    let res = up.upload(&UploadRequest::from_path(&file, UploadKind::File), &ctx()).await.expect("eventually succeeds");
    assert_eq!(res.url, "https://ok/1");
    let all = server.received_requests().await.expect("recorded");
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|r| r.body == payload), "every attempt sends the complete body");
}

#[tokio::test]
async fn honours_retry_after_header_in_real_time() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1")).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("done")).mount(&server).await;
    let inner: Arc<dyn Uploader> = Arc::new(text_uploader(&server.uri(), r#","URL":"{response}""#));
    let up = RetryingUploader::new(inner, fast_policy());
    let started = Instant::now();
    let res = up.upload(&UploadRequest::text("x"), &ctx()).await.expect("succeeds after waiting");
    assert_eq!(res.url, "done");
    assert!(started.elapsed() >= Duration::from_millis(950), "waited only {:?}", started.elapsed());
}

#[tokio::test]
async fn does_not_retry_client_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_string("bad")).mount(&server).await;
    let inner: Arc<dyn Uploader> = Arc::new(text_uploader(&server.uri(), ""));
    let up = RetryingUploader::new(inner, fast_policy());
    assert!(up.upload(&UploadRequest::text("x"), &ctx()).await.is_err());
    assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
}

#[tokio::test]
async fn invalid_or_empty_responses_are_invalid_response_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/notjson")).respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>")).mount(&server).await;
    Mock::given(method("POST")).and(path("/missing")).respond_with(ResponseTemplate::new(200).set_body_string(r#"{"other":1}"#)).mount(&server).await;
    Mock::given(method("POST")).and(path("/emptybody")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let u = text_uploader(&format!("{}/notjson", server.uri()), r#","URL":"{json:data.url}""#);
    match u.upload(&UploadRequest::text("x"), &ctx()).await.expect_err("fails") {
        UploadError::InvalidResponse { why } => assert!(why.contains("not valid JSON") && why.contains("<html>oops"), "{why}"),
        other => panic!("{other:?}"),
    }
    let u = text_uploader(&format!("{}/missing", server.uri()), r#","URL":"{json:data.url}""#);
    match u.upload(&UploadRequest::text("x"), &ctx()).await.expect_err("fails") {
        UploadError::InvalidResponse { why } => assert!(why.contains("no URL"), "{why}"),
        other => panic!("{other:?}"),
    }
    let u = text_uploader(&format!("{}/emptybody", server.uri()), "");
    assert!(matches!(u.upload(&UploadRequest::text("x"), &ctx()).await, Err(UploadError::InvalidResponse { .. })));
}

#[tokio::test]
async fn outputbox_urls_may_be_empty_and_interaction_is_pluggable() {
    struct Spy(std::sync::Mutex<Vec<String>>);
    impl Interaction for Spy {
        fn select(&self, options: &[String]) -> Option<String> {
            options.last().cloned()
        }
        fn input_box(&self, _t: &str, d: &str) -> Option<String> {
            Some(d.to_owned())
        }
        fn output_box(&self, title: &str, text: &str) {
            self.0.lock().unwrap().push(format!("{title}|{text}"));
        }
    }
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"q1"}"#)).mount(&server).await;
    let spy = Arc::new(Spy(std::sync::Mutex::new(vec![])));
    let u = text_uploader(&server.uri(), r#","URL":"{outputbox:Result|id={json:id}}""#).with_interaction(spy.clone());
    let res = u.upload(&UploadRequest::text("x"), &ctx()).await.expect("upload");
    assert_eq!(res.url, "");
    assert_eq!(spy.0.lock().unwrap().as_slice(), ["Result|id=q1"]);

    server.reset().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("ok")).mount(&server).await;
    let u = text_uploader(&server.uri(), r#","Parameters":{"host":"{select:a.example|b.example}"},"URL":"{response}""#).with_interaction(spy);
    u.upload(&UploadRequest::text("x"), &ctx()).await.expect("upload");
    let req = only_request(&server).await;
    assert_eq!(req.url.query(), Some("host=b.example"));
}

#[tokio::test]
async fn unsupported_kinds_and_bad_definitions_fail_early_with_clear_messages() {
    let server = MockServer::start().await;
    let u = text_uploader(&server.uri(), "");
    let err = u.upload(&UploadRequest::url("http://a"), &ctx()).await.expect_err("unsupported");
    assert!(matches!(err, UploadError::Unsupported { kind: UploadKind::Url, .. }), "{err:?}");
    assert!(err.to_string().contains("does not support URL"));

    let err = def(r#"{"Version":"14.1.0","RequestURL":"http://h/u","DestinationType":"ImageUploader","Body":"MultipartFormData"}"#).expect_err("invalid");
    assert!(matches!(&err, UploadError::Config { message } if message.contains("FileFormName")), "{err:?}");
    let err = def(r#"{"Version":"14.1.0","RequestURL":"http://h/{bogus}"}"#).expect_err("invalid");
    assert!(err.to_string().contains("bogus"));
    let err = def("not json").expect_err("invalid");
    assert!(matches!(err, UploadError::Config { .. }));
    assert!(server.received_requests().await.expect("recorded").is_empty());
}

#[tokio::test]
async fn files_cannot_go_through_a_body_less_definition() {
    let server = MockServer::start().await;
    let u = def(&format!(r#"{{"Version":"14.1.0","RequestURL":"{}","DestinationType":"ImageUploader, TextUploader","RequestMethod":"GET"}}"#, server.uri()));
    // Validation rejects ImageUploader + no body.
    assert!(u.is_err());
    let u = def(&format!(r#"{{"Version":"14.1.0","RequestURL":"{}","RequestMethod":"GET","URL":"{{response}}"}}"#, server.uri())).expect("valid without destination");
    assert!(!u.supports(UploadKind::Image));
    assert!(u.supports(UploadKind::Text));
}

#[tokio::test]
async fn name_parser_codes_in_headers_and_arguments_are_expanded_and_escapable() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("k")).mount(&server).await;
    let u = text_uploader(
        &server.uri(),
        r#","Headers":{"X-Stamp":"%y-%mo","X-Literal":"\\%y","X-Brace":"\\{x\\}"},"URL":"{response}""#,
    );
    u.upload(&UploadRequest::text("x"), &ctx()).await.expect("upload");
    let req = only_request(&server).await;
    let stamp = header(&req, "x-stamp").expect("stamp");
    assert!(stamp.len() == 7 && stamp.as_bytes()[4] == b'-', "{stamp}");
    assert_eq!(header(&req, "x-literal").as_deref(), Some("%y"));
    assert_eq!(header(&req, "x-brace").as_deref(), Some("{x}"));
}

#[tokio::test]
async fn request_url_encodes_input_and_filename() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string("k")).mount(&server).await;
    let u = def(&format!(
        r#"{{"Version":"14.1.0","RequestURL":"{}/s/{{filename}}/{{input}}","RequestMethod":"GET","DestinationType":"TextUploader","URL":"{{response}}"}}"#,
        server.uri()
    ))
    .expect("uploader");
    u.upload(&UploadRequest::text("a/b c&d").with_filename("n é.txt"), &ctx()).await.expect("upload");
    let req = only_request(&server).await;
    assert_eq!(req.url.path(), "/s/n%20%C3%A9.txt/a%2Fb%20c%26d");
}
