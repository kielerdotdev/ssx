//! Every fixture in `tests/fixtures/*.sxcu` (modelled on real hosts) is loaded, retargeted at
//! a local mock server and exercised end to end, asserting both what the mock received and
//! what the uploader produced.
#![cfg(feature = "sxcu")]

mod common;

use common::{ctx, fixture, header, multipart_of, only_request};
use ssx_upload::sxcu::{CustomUploader, SxcuUploader};
use ssx_upload::{UploadError, UploadKind, UploadRequest, Uploader, UrlShortener};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bytes that look like an image and contain CRLFs and NULs to stress framing.
fn png_bytes() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(b"\r\n--not-a-boundary\r\n\x00\x01\xff\xfe");
    v.extend((0..=255u8).cycle().take(5000));
    v
}

fn uploader(fixture_name: &str, server: &MockServer) -> SxcuUploader {
    let mut def = CustomUploader::from_json_str(&fixture(fixture_name)).expect("fixture parses");
    let (_, rest) = def.request_url.split_once("://").expect("fixture URL has a scheme");
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    def.request_url = format!("{}{path}", server.uri());
    SxcuUploader::new(def).expect("fixture validates")
}

fn image() -> UploadRequest {
    UploadRequest::from_bytes(png_bytes(), "shot.png", UploadKind::Image)
}

async fn run(
    u: &SxcuUploader,
    req: &UploadRequest,
) -> Result<ssx_upload::UploadResult, UploadError> {
    u.upload(req, &ctx()).await
}

#[test]
fn corpus_is_large_and_every_file_is_valid_and_round_trips() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).expect("fixtures dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "sxcu") {
            continue;
        }
        count += 1;
        let text = std::fs::read_to_string(&path).expect("read");
        let def = CustomUploader::from_json_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let report = def.check();
        assert!(report.errors.is_empty(), "{}: {:?}", path.display(), report.errors);
        let again = CustomUploader::from_json_str(&def.to_json_string()).expect("re-parse");
        assert_eq!(again, def, "{} does not round-trip", path.display());
        SxcuUploader::new(def).expect("constructs");
    }
    assert!(count >= 12, "need at least 12 fixtures, found {count}");
}

#[tokio::test]
async fn imgur_style_success_and_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/image"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"id":"AbCdEfG","link":"https://i.imgur.com/AbCdEfG.png","deletehash":"DEL123"},"success":true,"status":200}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("imgur.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://i.imgur.com/AbCdEfG.png");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://i.imgur.com/AbCdEfGm.jpg"));
    assert_eq!(res.deletion_url.as_deref(), Some("https://imgur.com/delete/DEL123"));
    assert_eq!(res.uploader_name, "Imgur (anonymous)");
    assert!(res.raw_response.contains("AbCdEfG"));

    let req = only_request(&server).await;
    assert_eq!(header(&req, "authorization").as_deref(), Some("Client-ID 1a2b3c4d5e6f7a8"));
    let parts = multipart_of(&req);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name, "image");
    assert_eq!(parts[0].filename.as_deref(), Some("shot.png"));
    assert_eq!(parts[0].content_type.as_deref(), Some("image/png"));
    assert_eq!(parts[0].data, png_bytes(), "file bytes must survive framing intact");
    assert_eq!(
        header(&req, "content-length").map(|v| v.parse::<usize>().unwrap()),
        Some(req.body.len())
    );
    assert!(header(&req, "transfer-encoding").is_none(), "known length: no chunked encoding");

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                r#"{"data":{"error":"Invalid image"},"success":false,"status":400}"#,
            ),
        )
        .mount(&server)
        .await;
    match run(&u, &image()).await.expect_err("must fail") {
        UploadError::Http { status: 400, message: Some(m), .. } => assert_eq!(m, "Invalid image"),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn catbox_style_plain_text_url_and_empty_argument() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/user/api.php"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("https://files.catbox.moe/abc123.png\n"),
        )
        .mount(&server)
        .await;
    let res = run(&uploader("catbox.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://files.catbox.moe/abc123.png", "trailing newline trimmed");
    let parts = multipart_of(&only_request(&server).await);
    let by_name =
        |n: &str| parts.iter().find(|p| p.name == n).unwrap_or_else(|| panic!("missing part {n}"));
    assert_eq!(by_name("reqtype").text(), "fileupload");
    assert_eq!(by_name("userhash").text(), "", "empty arguments are still sent");
    assert_eq!(by_name("fileToUpload").filename.as_deref(), Some("shot.png"));
    assert_eq!(parts.last().map(|p| p.name.as_str()), Some("fileToUpload"), "file part comes last");
}

#[tokio::test]
async fn zerox0_style_headers_random_secret_and_response_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("https://0x0.st/abcd.png\n")
                .insert_header("X-Token", "tok-42"),
        )
        .mount(&server)
        .await;
    let res = run(&uploader("0x0.st.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://0x0.st/abcd.png");
    assert_eq!(res.deletion_url.as_deref(), Some("tok-42"));
    let req = only_request(&server).await;
    assert_eq!(header(&req, "user-agent").as_deref(), Some("ShareX/16.0"));
    let secret =
        multipart_of(&req).into_iter().find(|p| p.name == "secret").expect("secret part").text();
    assert_eq!(secret.len(), 12);
    assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()), "{secret}");
}

#[tokio::test]
async fn pomf_style_files_array_field_and_query_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"success":true,"files":[{"hash":"h","name":"shot.png","url":"https://a.uguu.se/xYz.png","size":5}]}"#,
        ))
        .mount(&server)
        .await;
    let res = run(&uploader("uguu-pomf.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://a.uguu.se/xYz.png");
    let req = only_request(&server).await;
    assert_eq!(req.url.query(), Some("output=json"));
    assert_eq!(multipart_of(&req)[0].name, "files[]");
}

#[tokio::test]
async fn zipline_style_multiple_custom_headers_and_array_of_strings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"files":["https://zipline.example.com/u/AbC.png"]}"#),
        )
        .mount(&server)
        .await;
    let res = run(&uploader("zipline.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://zipline.example.com/u/AbC.png");
    let req = only_request(&server).await;
    assert_eq!(
        header(&req, "authorization").as_deref(),
        Some("MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0.tokenpart")
    );
    assert_eq!(header(&req, "format").as_deref(), Some("RANDOM"));
    assert_eq!(header(&req, "embed").as_deref(), Some("true"));
}

#[tokio::test]
async fn sxcu_net_style_thumbnail_and_deletion_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/files/create"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"url":"https://sxcu.net/AbCd","del_url":"https://sxcu.net/api/files/delete/AbCd/tok","thumb":"https://sxcu.net/t/AbCd.png"}"#,
        ))
        .mount(&server)
        .await;
    let res = run(&uploader("sxcu-net.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://sxcu.net/AbCd");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://sxcu.net/t/AbCd.png"));
    assert_eq!(res.deletion_url.as_deref(), Some("https://sxcu.net/api/files/delete/AbCd/tok"));
    let parts = multipart_of(&only_request(&server).await);
    assert_eq!(
        parts.iter().find(|p| p.name == "collection").map(common::Part::text).as_deref(),
        Some("0123abcd")
    );
}

#[tokio::test]
async fn lensdump_style_nested_json_and_service_error_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/1/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"status_code":200,"image":{"url":"https://i.lensdump.com/i/abc.png","thumb":{"url":"https://i.lensdump.com/t/abc.png"},"delete_url":"https://lensdump.com/i/abc/del"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("lensdump.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://i.lensdump.com/i/abc.png");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://i.lensdump.com/t/abc.png"));
    assert_eq!(res.deletion_url.as_deref(), Some("https://lensdump.com/i/abc/del"));
    assert_eq!(
        header(&only_request(&server).await, "x-api-key").as_deref(),
        Some("lensdump-api-key")
    );

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"status_code":400,"error":{"message":"Empty upload source.","code":400}}"#,
        ))
        .mount(&server)
        .await;
    let err = run(&u, &image()).await.expect_err("fails");
    assert!(err.to_string().contains("Empty upload source."), "{err}");
}

#[tokio::test]
async fn chevereto_style_query_string_in_request_url_becomes_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/1/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"image":{"url":"https://c.example.com/1.png","thumb":{"url":"https://c.example.com/1t.png"},"delete_url":"https://c.example.com/d"}}"#,
        ))
        .mount(&server)
        .await;
    let res = run(&uploader("chevereto.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, "https://c.example.com/1.png");
    let req = only_request(&server).await;
    let q: Vec<(String, String)> =
        req.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert_eq!(q, vec![("key".into(), "chv_abcdef".into()), ("format".into(), "json".into())]);
}

#[tokio::test]
async fn x0_style_minimal_uploader() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://x0.at/aB1.png"))
        .mount(&server)
        .await;
    let file = UploadRequest::from_bytes(b"hello".to_vec(), "notes.bin", UploadKind::File);
    let res = run(&uploader("x0.sxcu", &server), &file).await.expect("upload");
    assert_eq!(res.url, "https://x0.at/aB1.png");
    let p = &multipart_of(&only_request(&server).await)[0];
    assert_eq!(p.content_type.as_deref(), Some("application/octet-stream"));
}

#[tokio::test]
async fn generic_json_api_bearer_json_escaping_and_error_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/pastes"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string(r#"{"data":{"slug":"xY7","delete_token":"tok/1"}}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("generic-json-api.sxcu", &server);
    let text = "line one\n\"quoted\" \\ back\tтекст ✓ {not a template}";
    let res = run(&u, &UploadRequest::text(text).with_filename("note.txt")).await.expect("upload");
    assert_eq!(res.url, "https://paste.example.com/xY7");
    assert_eq!(
        res.deletion_url.as_deref(),
        Some("https://paste.example.com/api/v1/pastes/xY7?token=tok/1"),
        "json values in URL templates stay raw; only input/filename are encoded"
    );
    let req = only_request(&server).await;
    assert_eq!(header(&req, "authorization").as_deref(), Some("Bearer sk_live_9f8e7d6c"));
    assert_eq!(header(&req, "content-type").as_deref(), Some("application/json"));
    let sent: serde_json::Value =
        serde_json::from_slice(&req.body).expect("body is valid JSON despite hostile input");
    assert_eq!(sent["title"], "note.txt");
    assert_eq!(sent["content"], text);
    assert_eq!(sent["expires"], "1d");

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_string(r#"{"errors":[{"detail":"content too long"}]}"#),
        )
        .mount(&server)
        .await;
    match run(&u, &UploadRequest::text("x")).await.expect_err("fails") {
        UploadError::Http { status: 422, message, .. } => {
            assert_eq!(message.as_deref(), Some("content too long"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn xml_response_success_and_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<?xml version=\"1.0\"?><response><status>ok</status><file><url>https://xml.example.com/f/1.png</url><thumb>https://xml.example.com/t/1.png</thumb></file></response>",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("xml-response.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://xml.example.com/f/1.png");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://xml.example.com/t/1.png"));
    let parts = multipart_of(&only_request(&server).await);
    assert_eq!(parts[0].name, "apikey");
    assert_eq!(parts[1].name, "userfile");

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                "<response><error><message>File too big</message></error></response>",
            ),
        )
        .mount(&server)
        .await;
    let err = run(&u, &image()).await.expect_err("fails");
    assert!(err.to_string().contains("File too big"), "{err}");
}

#[tokio::test]
async fn html_scrape_with_regex_groups_and_named_groups() {
    let server = MockServer::start().await;
    let html = r#"<html><body><img src="https://files.example.com/t/9.jpg"><a class="dl" href="https://files.example.com/d/9xyz">Download</a></body></html>"#;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("html-scrape.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://files.example.com/d/9xyz");
    assert_eq!(res.thumbnail_url.as_deref(), Some("https://files.example.com/t/9.jpg"));

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(413).set_body_string(r#"<p class="error">File is too large</p>"#),
        )
        .mount(&server)
        .await;
    assert!(run(&u, &image()).await.expect_err("fails").to_string().contains("File is too large"));
}

#[tokio::test]
async fn url_shortener_with_input_in_the_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/create.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://is.gd/AbC123"))
        .mount(&server)
        .await;
    let u = uploader("is-gd-shortener.sxcu", &server);
    assert!(u.supports(UploadKind::Url) && !u.supports(UploadKind::Image));
    let long = "https://example.com/a path?x=1&y=2#frag";
    let short = u.shorten(long, &ctx()).await.expect("shorten");
    assert_eq!(short, "https://is.gd/AbC123");
    let req = only_request(&server).await;
    let q: Vec<(String, String)> =
        req.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert_eq!(q, vec![("format".into(), "simple".into()), ("url".into(), long.into())]);
    assert!(req.body.is_empty());
}

#[tokio::test]
async fn transfer_sh_style_binary_put_with_encoded_path_and_response_header() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("https://transfer.sh/abc/my%20file.txt")
                .insert_header("X-Url-Delete", "https://transfer.sh/abc/my%20file.txt/tok"),
        )
        .mount(&server)
        .await;
    let u = uploader("transfer-sh-put.sxcu", &server);
    let req = UploadRequest::from_bytes(png_bytes(), "my file.txt", UploadKind::File);
    let res = run(&u, &req).await.expect("upload");
    assert_eq!(res.url, "https://transfer.sh/abc/my%20file.txt");
    assert_eq!(res.deletion_url.as_deref(), Some("https://transfer.sh/abc/my%20file.txt/tok"));
    let got = only_request(&server).await;
    assert_eq!(got.method.as_str(), "PUT");
    assert_eq!(got.url.path(), "/my%20file.txt");
    assert_eq!(got.body, png_bytes(), "raw body");
    assert_eq!(header(&got, "content-type").as_deref(), Some("text/plain"));
    assert_eq!(header(&got, "max-downloads").as_deref(), Some("5"));
}

#[tokio::test]
async fn dpaste_style_form_urlencoded_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/"))
        .respond_with(ResponseTemplate::new(201).set_body_string("https://dpaste.com/ABCDEF\n"))
        .mount(&server)
        .await;
    let u = uploader("dpaste-form.sxcu", &server);
    let text = "a=b&c d\nnew line ✓";
    let res = run(&u, &UploadRequest::text(text).with_filename("paste.txt")).await.expect("upload");
    assert_eq!(res.url, "https://dpaste.com/ABCDEF");
    let req = only_request(&server).await;
    assert_eq!(header(&req, "content-type").as_deref(), Some("application/x-www-form-urlencoded"));
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&req.body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("content").as_deref(), Some(text));
    assert_eq!(get("title").as_deref(), Some("paste.txt"));
    assert_eq!(get("syntax").as_deref(), Some("text"));
    assert!(!u.supports(UploadKind::Image), "form bodies cannot carry files");
}

#[tokio::test]
async fn legacy_dollar_syntax_file_still_works() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"data":{"url":"https://legacy.example.com/1.png"}}"#),
        )
        .mount(&server)
        .await;
    let u = uploader("legacy-dollar.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://legacy.example.com/1.png");
    let req = only_request(&server).await;
    assert_eq!(header(&req, "x-client").as_deref(), Some("shot.png"));
    let note = multipart_of(&req).into_iter().find(|p| p.name == "note").expect("note part").text();
    assert_eq!(note, "a {literal} brace");
}

#[tokio::test]
async fn responseurl_follows_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/final/image.png"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/final/image.png"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>view</html>"))
        .mount(&server)
        .await;
    let res = run(&uploader("redirect-responseurl.sxcu", &server), &image()).await.expect("upload");
    assert_eq!(res.url, format!("{}/final/image.png", server.uri()));
}

#[tokio::test]
async fn pixeldrain_style_basic_auth_via_base64_with_leading_colon() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/file/shot.png"))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id":"abc123"}"#))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let u = uploader("pixeldrain.sxcu", &server);
    let res = run(&u, &image()).await.expect("upload");
    assert_eq!(res.url, "https://pixeldrain.com/u/abc123");
    assert_eq!(
        res.thumbnail_url.as_deref(),
        Some("https://pixeldrain.com/api/file/abc123/thumbnail")
    );
    let req = only_request(&server).await;
    // base64("my-api-key") because the argument is "{base64::my-api-key}" => ":my-api-key"
    assert_eq!(header(&req, "authorization").as_deref(), Some("Basic Om15LWFwaS1rZXk="));

    server.reset().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(422).set_body_string(
            r#"{"success":false,"value":"file_too_large","message":"The file is too large"}"#,
        ))
        .mount(&server)
        .await;
    assert!(
        run(&u, &image()).await.expect_err("fails").to_string().contains("The file is too large")
    );
}
