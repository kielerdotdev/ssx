//! Imgur, S3, generic HTTP, shorteners and the local uploader against mock servers.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Recorder, ctx, header, multipart_of, only_request};
use ssx_upload::http_uploader::{HttpAuth, HttpBody, HttpUploader, HttpUploaderConfig, ResultUrl};
use ssx_upload::imgur::{ImgurConfig, ImgurUploader};
use ssx_upload::local::LocalUploader;
use ssx_upload::shorten::{HttpShortener, HttpShortenerConfig, ShortenMethod, ShortenResponse};
use ssx_upload::{
    InMemorySecretStore, SecretStore, ShorteningUploader, UploadError, UploadKind, UploadRequest,
    Uploader, UrlShortener,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn png() -> UploadRequest {
    UploadRequest::from_bytes((0..=255u8).collect::<Vec<_>>(), "shot.png", UploadKind::Image)
}

// ---------------------------------------------------------------- Imgur

fn imgur(server: &MockServer, cfg: ImgurConfig) -> ImgurUploader {
    let mut cfg = cfg;
    cfg.api_base = server.uri();
    ImgurUploader::new(cfg).expect("imgur")
}

const IMGUR_OK: &str = r#"{"data":{"id":"AbC123x","deletehash":"DELhash","link":"http://i.imgur.com/AbC123x.png"},"success":true,"status":200}"#;

#[tokio::test]
async fn imgur_anonymous_image_upload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/image"))
        .respond_with(ResponseTemplate::new(200).set_body_string(IMGUR_OK))
        .mount(&server)
        .await;
    let mut cfg = ImgurConfig::anonymous("client123");
    cfg.title = Some("My shot".into());
    cfg.album = Some("albumhash".into());
    let res = imgur(&server, cfg).upload(&png(), &ctx()).await.expect("upload");
    assert_eq!(res.url, "https://i.imgur.com/AbC123x.png", "http links are upgraded");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://i.imgur.com/AbC123xm.jpg"));
    assert_eq!(res.deletion_url.as_deref(), Some("https://imgur.com/delete/DELhash"));
    assert_eq!(res.extra["id"], "AbC123x");
    assert_eq!(res.extra["deletehash"], "DELhash");
    let req = only_request(&server).await;
    assert_eq!(header(&req, "authorization").as_deref(), Some("Client-ID client123"));
    let parts = multipart_of(&req);
    let text = |n: &str| parts.iter().find(|p| p.name == n).map(common::Part::text);
    assert_eq!(text("title").as_deref(), Some("My shot"));
    assert_eq!(text("album").as_deref(), Some("albumhash"));
    let img = parts.iter().find(|p| p.name == "image").expect("image part");
    assert_eq!(img.data, (0..=255u8).collect::<Vec<_>>());
    assert_eq!(img.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn imgur_video_uses_the_upload_endpoint_and_bearer_tokens_come_from_the_store() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/3/upload")).respond_with(ResponseTemplate::new(200).set_body_string(
        r#"{"data":{"id":"vid1","deletehash":null,"link":"https://i.imgur.com/vid1.mp4"},"success":true,"status":200}"#,
    )).mount(&server).await;
    let store = Arc::new(InMemorySecretStore::new());
    let up = imgur(&server, ImgurConfig::bearer_stored("imgur.token"));
    let video = UploadRequest::from_bytes(vec![0u8; 100], "clip.mp4", UploadKind::Video);

    let ctx_no_secret = ctx().with_secrets(store.clone());
    assert!(
        matches!(up.upload(&video, &ctx_no_secret).await, Err(UploadError::Auth { .. })),
        "missing token is an Auth error"
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    store.set("imgur.token", "tok-1").unwrap();
    let res = up.upload(&video, &ctx_no_secret).await.expect("upload");
    assert_eq!(res.url, "https://i.imgur.com/vid1.mp4");
    assert_eq!(res.deletion_url, None, "account uploads have no deletehash");
    let req = only_request(&server).await;
    assert_eq!(header(&req, "authorization").as_deref(), Some("Bearer tok-1"));
    assert_eq!(multipart_of(&req).last().map(|p| p.name.clone()).as_deref(), Some("video"));
    assert!(!up.supports(UploadKind::File) && !up.supports(UploadKind::Text));
}

#[tokio::test]
async fn imgur_errors_are_mapped() {
    let server = MockServer::start().await;
    let up = imgur(&server, ImgurConfig::anonymous("c"));
    let fail =
        |status: u16, body: &str| ResponseTemplate::new(status).set_body_string(body.to_owned());

    Mock::given(method("POST")).respond_with(fail(400, r#"{"data":{"error":{"message":"Invalid URL","code":1003}},"success":false,"status":400}"#)).up_to_n_times(1).mount(&server).await;
    match up.upload(&png(), &ctx()).await.expect_err("400") {
        UploadError::Http { status: 400, message, .. } => {
            assert_eq!(message.as_deref(), Some("Invalid URL"));
        }
        other => panic!("{other:?}"),
    }
    Mock::given(method("POST"))
        .respond_with(fail(429, "{}").insert_header("Retry-After", "42"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(
        matches!(up.upload(&png(), &ctx()).await, Err(UploadError::RateLimited { retry_after: Some(d) }) if d == Duration::from_secs(42))
    );
    Mock::given(method("POST"))
        .respond_with(fail(429, "{}").insert_header("X-RateLimit-UserReset", "90"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(
        matches!(up.upload(&png(), &ctx()).await, Err(UploadError::RateLimited { retry_after: Some(d) }) if d == Duration::from_secs(90))
    );
    Mock::given(method("POST"))
        .respond_with(fail(
            403,
            r#"{"data":{"error":"Invalid client_id"},"success":false,"status":403}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(
        matches!(up.upload(&png(), &ctx()).await, Err(UploadError::Auth { message }) if message.contains("Invalid client_id"))
    );
    Mock::given(method("POST"))
        .respond_with(fail(
            200,
            r#"{"data":{"error":"soft failure"},"success":false,"status":400}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(up.upload(&png(), &ctx()).await, Err(UploadError::Http { status: 400, .. })));
    Mock::given(method("POST"))
        .respond_with(fail(200, r#"{"data":{},"success":true,"status":200}"#))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(up.upload(&png(), &ctx()).await, Err(UploadError::InvalidResponse { .. })));
    Mock::given(method("POST"))
        .respond_with(fail(200, "<html>"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(matches!(up.upload(&png(), &ctx()).await, Err(UploadError::InvalidResponse { .. })));
}

// ------------------------------------------------------------------- S3

#[cfg(feature = "s3")]
mod s3 {
    use super::*;
    use chrono::{TimeZone as _, Utc};
    use ssx_upload::s3::{PayloadSigning, S3Config, S3Uploader};
    use ssx_upload::sigv4::{self, Credentials, SigningInput};

    fn fixed_clock() -> Arc<dyn Fn() -> chrono::DateTime<Utc> + Send + Sync> {
        Arc::new(|| Utc.with_ymd_and_hms(2024, 5, 17, 10, 30, 0).unwrap())
    }

    /// Recompute the signature from the request the server actually received, using only
    /// the SigV4 primitives (which are verified against AWS's vectors), and compare.
    fn assert_signature_valid(
        req: &wiremock::Request,
        secret: &str,
        access_key: &str,
        region: &str,
    ) {
        let auth = header(req, "authorization").expect("authorization header");
        let signed_names = auth.split("SignedHeaders=").nth(1).unwrap().split(',').next().unwrap();
        let headers: Vec<(String, String)> = signed_names
            .split(';')
            .map(|n| {
                let v = header(req, n)
                    .unwrap_or_else(|| panic!("signed header {n} missing from request"));
                (n.to_owned(), v)
            })
            .collect();
        let time = chrono::NaiveDateTime::parse_from_str(
            &header(req, "x-amz-date").unwrap(),
            "%Y%m%dT%H%M%SZ",
        )
        .unwrap()
        .and_utc();
        let expected = sigv4::sign(
            &Credentials {
                access_key_id: access_key.into(),
                secret_access_key: secret.into(),
                session_token: None,
            },
            &SigningInput {
                method: req.method.as_str(),
                path: req.url.path(),
                query: req.url.query().unwrap_or(""),
                headers: &headers,
                payload_hash: &header(req, "x-amz-content-sha256").unwrap(),
                region,
                service: "s3",
                time,
                s3: true,
            },
        );
        assert_eq!(auth, expected.authorization);
    }

    fn uploader(server: &MockServer, tweak: impl FnOnce(&mut S3Config)) -> S3Uploader {
        let mut cfg = S3Config::minio(&server.uri(), "pics")
            .with_static_credentials("AKIATEST", "s3cr3t/KEY");
        tweak(&mut cfg);
        S3Uploader::new(cfg).expect("s3").with_clock(fixed_clock())
    }

    #[tokio::test]
    async fn put_object_is_signed_and_carries_the_configured_headers() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"abc123\""))
            .mount(&server)
            .await;
        let up = uploader(&server, |c| {
            c.key_prefix = "shots/".into();
            c.acl = Some("public-read".into());
            c.storage_class = Some("STANDARD_IA".into());
            c.cache_control = Some("max-age=31536000".into());
            c.extra_headers.push(("X-Amz-Meta-Origin".into(), "ssx".into()));
        });
        let data: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
        let req = UploadRequest::from_bytes(data.clone(), "my shot ü.png", UploadKind::Image);
        let res = up.upload(&req, &ctx()).await.expect("upload");

        let got = only_request(&server).await;
        assert_eq!(got.method.as_str(), "PUT");
        assert_eq!(got.url.path(), "/pics/shots/my%20shot%20%C3%BC.png");
        assert_eq!(got.body, data);
        assert_eq!(header(&got, "content-type").as_deref(), Some("image/png"));
        assert_eq!(header(&got, "x-amz-acl").as_deref(), Some("public-read"));
        assert_eq!(header(&got, "x-amz-storage-class").as_deref(), Some("STANDARD_IA"));
        assert_eq!(header(&got, "x-amz-meta-origin").as_deref(), Some("ssx"));
        assert_eq!(header(&got, "x-amz-date").as_deref(), Some("20240517T103000Z"));
        // Over plain HTTP the payload hash is signed.
        assert_eq!(header(&got, "x-amz-content-sha256"), Some(sigv4::sha256_hex(&data)));
        let auth = header(&got, "authorization").unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIATEST/20240517/us-east-1/s3/aws4_request, SignedHeaders="), "{auth}");
        for h in [
            "host",
            "x-amz-acl",
            "x-amz-content-sha256",
            "x-amz-date",
            "x-amz-storage-class",
            "cache-control",
            "content-type",
            "x-amz-meta-origin",
        ] {
            assert!(auth.contains(h), "{h} must be signed: {auth}");
        }
        assert_signature_valid(&got, "s3cr3t/KEY", "AKIATEST", "us-east-1");
        assert_eq!(
            header(&got, "content-length").map(|v| v.parse::<usize>().unwrap()),
            Some(data.len())
        );

        assert!(res.url.ends_with("/pics/shots/my%20shot%20%C3%BC.png"), "{}", res.url);
        assert_eq!(res.extra["etag"], "abc123");
        assert_eq!(res.extra["key"], "shots/my shot ü.png");
    }

    #[tokio::test]
    async fn unsigned_payload_and_public_url_template_and_file_streaming() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let up = uploader(&server, |c| {
            c.payload_signing = PayloadSigning::Unsigned;
            c.public_url_template = Some("https://cdn.example.com/{key}".into());
            c.key_template = "%y/{stem}.{ext}".into();
        });
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("clip.mp4");
        std::fs::write(&file, vec![9u8; 200_000]).unwrap();
        let rec = Recorder::new();
        let res = up
            .upload(
                &UploadRequest::from_path(&file, UploadKind::Video),
                &ctx().with_progress(rec.clone()),
            )
            .await
            .expect("upload");
        assert!(
            res.url.starts_with("https://cdn.example.com/20") && res.url.ends_with("/clip.mp4"),
            "{}",
            res.url
        );
        let got = only_request(&server).await;
        assert_eq!(header(&got, "x-amz-content-sha256").as_deref(), Some("UNSIGNED-PAYLOAD"));
        assert_signature_valid(&got, "s3cr3t/KEY", "AKIATEST", "us-east-1");
        assert_eq!(got.body.len(), 200_000);
        assert_eq!(rec.events().last().map(|e| e.0), Some(200_000));
    }

    #[tokio::test]
    async fn hashed_payload_streams_files_and_credentials_come_from_the_store() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let store = Arc::new(InMemorySecretStore::new());
        let mut cfg =
            S3Config::minio(&server.uri(), "pics").with_stored_credentials("s3.id", "s3.secret");
        cfg.payload_signing = PayloadSigning::Hashed;
        let up = S3Uploader::new(cfg).unwrap().with_clock(fixed_clock());
        let c = ctx().with_secrets(store.clone());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        std::fs::write(&file, &data).unwrap();
        let req = UploadRequest::from_path(&file, UploadKind::File);

        assert!(
            matches!(up.upload(&req, &c).await, Err(UploadError::Auth { message }) if message.contains("s3.id"))
        );
        store.set("s3.id", "AKIASTORED").unwrap();
        store.set("s3.secret", "stored/secret").unwrap();
        up.upload(&req, &c).await.expect("upload");
        let got = only_request(&server).await;
        assert_eq!(header(&got, "x-amz-content-sha256"), Some(sigv4::sha256_hex(&data)));
        assert_signature_valid(&got, "stored/secret", "AKIASTORED", "us-east-1");
    }

    #[tokio::test]
    async fn s3_errors_are_actionable() {
        let server = MockServer::start().await;
        let up = uploader(&server, |_| {});
        let req = UploadRequest::from_bytes(vec![1], "a.png", UploadKind::Image);
        let xml = |code: &str, msg: &str| {
            format!(
                "<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{msg}</Message></Error>"
            )
        };
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(403).set_body_string(xml("SignatureDoesNotMatch", "bad sig")),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        assert!(
            matches!(up.upload(&req, &ctx()).await, Err(UploadError::Auth { message }) if message.contains("SignatureDoesNotMatch"))
        );
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_string(xml("NoSuchBucket", "The specified bucket does not exist")),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        assert!(
            matches!(up.upload(&req, &ctx()).await, Err(UploadError::Config { message }) if message.contains("bucket"))
        );
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string(xml("SlowDown", "Please reduce your request rate."))
                    .insert_header("Retry-After", "2"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        assert!(
            matches!(up.upload(&req, &ctx()).await, Err(UploadError::RateLimited { retry_after: Some(d) }) if d == Duration::from_secs(2))
        );
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500).set_body_string(xml("InternalError", "oops")))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let e = up.upload(&req, &ctx()).await.unwrap_err();
        assert!(e.is_retryable() && e.to_string().contains("InternalError"), "{e}");
        assert!(matches!(
            up.upload(&UploadRequest::url("http://x"), &ctx()).await,
            Err(UploadError::Unsupported { .. })
        ));
    }
}

// ---------------------------------------------------------- generic HTTP

#[tokio::test]
async fn http_put_with_basic_auth_from_the_secret_store() {
    let server = MockServer::start().await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let mut cfg =
        HttpUploaderConfig::put("WebDAV", &format!("{}/dav/{{stem}}-x.{{ext}}", server.uri()));
    cfg.auth = HttpAuth::Basic { username: "alice".into(), secret_key: "dav.pw".into() };
    cfg.headers.push(("X-Custom".into(), "v".into()));
    let up = HttpUploader::new(cfg).expect("uploader");
    let store = Arc::new(InMemorySecretStore::new());
    let c = ctx().with_secrets(store.clone());
    let req = UploadRequest::from_bytes(b"data".to_vec(), "my file.txt", UploadKind::File);
    assert!(matches!(up.upload(&req, &c).await, Err(UploadError::Auth { .. })));
    store.set("dav.pw", "pässword").unwrap();
    let res = up.upload(&req, &c).await.expect("upload");
    assert!(res.url.ends_with("/dav/my%20file-x.txt"), "{}", res.url);
    let got = only_request(&server).await;
    assert_eq!(got.url.path(), "/dav/my%20file-x.txt");
    assert_eq!(got.body, b"data");
    assert_eq!(header(&got, "x-custom").as_deref(), Some("v"));
    // base64("alice:pässword")
    assert_eq!(header(&got, "authorization").as_deref(), Some("Basic YWxpY2U6cMOkc3N3b3Jk"));
    assert_eq!(header(&got, "content-type").as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn http_post_multipart_with_result_extraction_variants() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"data":{"link":"https://h/1"}}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/hdr"))
        .respond_with(ResponseTemplate::new(201).insert_header("Location", "https://h/2"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/body"))
        .respond_with(ResponseTemplate::new(200).set_body_string("  https://h/3\n"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/empty"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let mk = |p: &str, result: ResultUrl| {
        let mut cfg =
            HttpUploaderConfig::post_multipart("m", &format!("{}{p}", server.uri()), "upload");
        cfg.body = HttpBody::Multipart {
            field: "upload".into(),
            fields: vec![("name".into(), "{filename}".into())],
        };
        cfg.result = result;
        HttpUploader::new(cfg).unwrap()
    };
    let req = UploadRequest::from_bytes(b"x".to_vec(), "a.txt", UploadKind::File);
    assert_eq!(
        mk("/json", ResultUrl::JsonPointer("/data/link".into()))
            .upload(&req, &ctx())
            .await
            .unwrap()
            .url,
        "https://h/1"
    );
    assert_eq!(
        mk("/hdr", ResultUrl::Header("location".into())).upload(&req, &ctx()).await.unwrap().url,
        "https://h/2"
    );
    assert_eq!(mk("/body", ResultUrl::Body).upload(&req, &ctx()).await.unwrap().url, "https://h/3");
    assert_eq!(
        mk("/json", ResultUrl::Template("https://cdn/{filename}".into()))
            .upload(&req, &ctx())
            .await
            .unwrap()
            .url,
        "https://cdn/a.txt"
    );
    assert!(matches!(
        mk("/empty", ResultUrl::JsonPointer("/data/link".into())).upload(&req, &ctx()).await,
        Err(UploadError::InvalidResponse { .. })
    ));
    let all = server.received_requests().await.unwrap();
    let parts = multipart_of(&all[0]);
    assert_eq!(parts[0].name, "name");
    assert_eq!(parts[0].text(), "a.txt");
    assert_eq!(parts[1].name, "upload");
}

// ------------------------------------------------------------ shorteners

#[tokio::test]
async fn presets_and_configurable_shorteners() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/create.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://is.gd/xyz\n"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api-create.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://tinyurl.com/abc"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/yourls"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"shorturl":"https://sho.rt/q"}"#),
        )
        .mount(&server)
        .await;

    let long = "https://example.com/some path?a=1&b=2";
    let isgd = HttpShortener::is_gd(Some(&format!("{}/create.php", server.uri())));
    assert_eq!(isgd.shorten(long, &ctx()).await.unwrap(), "https://is.gd/xyz");
    let tiny = HttpShortener::tiny_url(Some(&format!("{}/api-create.php", server.uri())));
    assert_eq!(tiny.shorten(long, &ctx()).await.unwrap(), "https://tinyurl.com/abc");
    let yourls = HttpShortener::new(HttpShortenerConfig {
        name: "yourls".into(),
        endpoint: format!("{}/yourls", server.uri()),
        method: ShortenMethod::Post,
        url_param: "url".into(),
        params: vec![("action".into(), "shorturl".into()), ("format".into(), "json".into())],
        headers: vec![],
        response: ShortenResponse::JsonPointer("/shorturl".into()),
    })
    .unwrap();
    assert_eq!(yourls.shorten(long, &ctx()).await.unwrap(), "https://sho.rt/q");

    let all = server.received_requests().await.unwrap();
    let q: Vec<(String, String)> =
        all[0].url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert_eq!(q, vec![("format".into(), "simple".into()), ("url".into(), long.into())]);
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&all[2].body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(
        form.contains(&("url".into(), long.into()))
            && form.contains(&("action".into(), "shorturl".into()))
    );

    assert!(matches!(isgd.shorten("not a url", &ctx()).await, Err(UploadError::Config { .. })));
    assert!(
        HttpShortener::new(HttpShortenerConfig {
            endpoint: "ftp://x".into(),
            ..HttpShortenerConfig {
                name: "n".into(),
                endpoint: String::new(),
                method: ShortenMethod::Get,
                url_param: "url".into(),
                params: vec![],
                headers: vec![],
                response: ShortenResponse::PlainText
            }
        })
        .is_err()
    );
}

#[tokio::test]
async fn shortener_errors_and_fail_open_chain() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("Error: Please enter a valid URL to shorten"),
        )
        .mount(&server)
        .await;
    let bad = Arc::new(HttpShortener::is_gd(Some(&format!("{}/create.php", server.uri()))));
    let e = bad.shorten("https://example.com", &ctx()).await.unwrap_err();
    assert!(e.to_string().contains("Please enter a valid URL"), "{e}");

    let dir = tempfile::tempdir().unwrap();
    let inner: Arc<dyn Uploader> = Arc::new(
        LocalUploader::to_directory(dir.path()).with_base_url("https://files.example.com/u"),
    );
    let open = ShorteningUploader::new(inner.clone(), bad.clone());
    let res = open.upload(&png(), &ctx()).await.expect("fails open");
    assert_eq!(res.url, "https://files.example.com/u/shot.png");
    assert!(res.extra["shorten_error"].contains("Please enter a valid URL"));
    let closed = ShorteningUploader::new(inner.clone(), bad).fail_closed();
    assert!(closed.upload(&png(), &ctx()).await.is_err());

    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://is.gd/ok"))
        .mount(&server)
        .await;
    let good = Arc::new(HttpShortener::is_gd(Some(&format!("{}/create.php", server.uri()))));
    let res = ShorteningUploader::new(inner, good).upload(&png(), &ctx()).await.unwrap();
    assert_eq!(res.url, "https://is.gd/ok");
    assert_eq!(
        res.extra["original_url"], "https://files.example.com/u/shot-2.png",
        "earlier uploads in this folder took shot.png and shot-1.png"
    );
    let q = only_request(&server).await;
    assert!(
        q.url.query().unwrap().contains("url=https%3A%2F%2Ffiles.example.com%2Fu%2Fshot-2.png")
    );
}

// ----------------------------------------------------------------- local

#[tokio::test]
async fn local_uploader_noop_and_directory_copy() {
    let noop = LocalUploader::noop();
    assert!(noop.supports(UploadKind::Url) && noop.supports(UploadKind::Video));
    let r = noop.upload(&png(), &ctx()).await.unwrap();
    assert_eq!(r.url, "local://shot.png");

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    std::fs::write(&src, vec![5u8; 300_000]).unwrap();
    let r = noop.upload(&UploadRequest::from_path(&src, UploadKind::File), &ctx()).await.unwrap();
    assert!(r.url.starts_with("file://") && r.url.ends_with("/src.bin"), "{}", r.url);

    let dest = dir.path().join("out/nested");
    let up = LocalUploader::to_directory(&dest);
    let rec = Recorder::new();
    let c = ctx().with_progress(rec.clone());
    let a = up.upload(&UploadRequest::from_path(&src, UploadKind::File), &c).await.unwrap();
    let b = up.upload(&UploadRequest::from_path(&src, UploadKind::File), &c).await.unwrap();
    assert_eq!(std::fs::read(dest.join("src.bin")).unwrap().len(), 300_000);
    assert_eq!(std::fs::read(dest.join("src-1.bin")).unwrap().len(), 300_000, "no overwrite");
    assert!(a.url.ends_with("/src.bin") && b.url.ends_with("/src-1.bin"));
    assert_eq!(rec.events().last().map(|e| e.0), Some(300_000));

    let evil = UploadRequest::from_bytes(b"x".to_vec(), "../../escape.txt", UploadKind::File);
    up.upload(&evil, &c).await.unwrap();
    assert!(dest.join("escape.txt").exists(), "traversal is neutralised");
    assert!(!dir.path().join("escape.txt").exists());

    let cancelled = ctx();
    cancelled.cancel.cancel();
    assert!(up.upload(&png(), &cancelled).await.unwrap_err().is_cancelled());
}
