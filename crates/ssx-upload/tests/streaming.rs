//! Progress reporting, cancellation and multipart framing against raw TCP sinks.
#![cfg(feature = "sxcu")]

mod common;

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use common::{
    Recorder, SinkMode, ctx, header, multipart_of, only_request, sparse_file, spawn_sink,
};
use ssx_upload::sxcu::SxcuUploader;
use ssx_upload::{UploadError, UploadKind, UploadRequest, Uploader};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn multipart_uploader(url: &str) -> SxcuUploader {
    let json = format!(
        r#"{{"Version":"14.1.0","Name":"mp","DestinationType":"ImageUploader, FileUploader, TextUploader","RequestMethod":"POST",
            "RequestURL":"{url}","Body":"MultipartFormData","Arguments":{{"a":"1"}},"FileFormName":"file","URL":"{{response}}"}}"#
    );
    SxcuUploader::from_json_str(&json).expect("uploader")
}

#[tokio::test]
async fn progress_is_monotonic_and_ends_at_the_bytes_the_server_received() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("data.bin");
    std::fs::write(&path, (0..=255u8).cycle().take(3 * 1024 * 1024 + 17).collect::<Vec<_>>())
        .expect("write");
    let sink = spawn_sink(SinkMode::Discard).await;
    let rec = Recorder::new();
    let ctx = ctx().with_progress(rec.clone());
    let u = multipart_uploader(&sink.url("/up"));
    u.upload(&UploadRequest::from_path(&path, UploadKind::File), &ctx).await.expect("upload");

    let received = sink.stats.received.load(Relaxed);
    let events = rec.events();
    assert!(events.len() >= 2, "expected several progress events, got {events:?}");
    assert!(events.windows(2).all(|w| w[0].0 <= w[1].0), "monotonic: {events:?}");
    assert!(events.iter().all(|e| e.1 == Some(received)), "total is the full body length");
    assert_eq!(events.last().map(|e| e.0), Some(received), "ends at total");
    assert_eq!(sink.stats.lengths.lock().unwrap().as_slice(), &[received]);
    assert_eq!(sink.stats.chunked.load(Relaxed), 0);
}

#[tokio::test]
async fn cancelling_mid_upload_returns_promptly_and_stops_sending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = sparse_file(dir.path(), "big.bin", 256 * 1024 * 1024);
    // The server stops reading after 1 MiB, so the client blocks on a full socket buffer:
    // only cancellation can end this upload.
    let sink = spawn_sink(SinkMode::StallAfter(1024 * 1024)).await;
    let rec = Recorder::new();
    let ctx = ctx().with_progress(rec.clone());
    let token = ctx.cancel.clone();
    let u = multipart_uploader(&sink.url("/up"));

    let canceller = tokio::spawn({
        let rec = rec.clone();
        async move {
            while rec.events().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            token.cancel();
        }
    });
    let req = UploadRequest::from_path(&path, UploadKind::File);
    let started = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(10), u.upload(&req, &ctx))
        .await
        .expect("cancellation must not hang");
    canceller.await.expect("canceller");
    assert!(matches!(res, Err(UploadError::Cancelled)), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let sent = rec.events().last().map_or(0, |e| e.0);
    assert!(sent < 64 * 1024 * 1024, "upload kept going after cancel: {sent} bytes reported");
}

#[tokio::test]
async fn cancelling_before_start_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&server)
        .await;
    let ctx = ctx();
    ctx.cancel.cancel();
    let u = multipart_uploader(&format!("{}/up", server.uri()));
    let err = u.upload(&UploadRequest::text("hi"), &ctx).await.expect_err("cancelled");
    assert!(err.is_cancelled());
    assert!(server.received_requests().await.expect("recorded").is_empty());
}

#[tokio::test]
async fn multipart_escapes_awkward_file_names_and_keeps_utf8() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://x/1"))
        .mount(&server)
        .await;
    let u = multipart_uploader(&format!("{}/up", server.uri()));
    let name = "we\"ird\r\nname ☃ файл.png";
    let data = vec![b'-'; 100];
    let req = UploadRequest::from_bytes(data.clone(), name, UploadKind::Image);
    u.upload(&req, &ctx()).await.expect("upload");
    let got = only_request(&server).await;
    let parts = multipart_of(&got);
    let file = parts.iter().find(|p| p.name == "file").expect("file part");
    assert_eq!(file.filename.as_deref(), Some("we%22ird%0D%0Aname ☃ файл.png"));
    assert_eq!(file.data, data);
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
    assert_eq!(
        header(&got, "content-length").map(|v| v.parse::<usize>().unwrap()),
        Some(got.body.len())
    );
}

#[tokio::test]
async fn text_can_be_uploaded_as_a_file_part_and_from_disk() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("https://x/t"))
        .mount(&server)
        .await;
    let u = multipart_uploader(&format!("{}/up", server.uri()));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "héllo\nwörld").expect("write");
    u.upload(&UploadRequest::from_path(&path, UploadKind::Text), &ctx()).await.expect("upload");
    let parts = multipart_of(&only_request(&server).await);
    let file = parts.iter().find(|p| p.name == "file").expect("file part");
    assert_eq!(file.text(), "héllo\nwörld");
    assert_eq!(file.filename.as_deref(), Some("note.txt"));
    assert_eq!(parts[0].name, "a");
}

#[tokio::test]
async fn missing_source_file_is_an_io_error_not_a_panic() {
    let server = MockServer::start().await;
    let u = multipart_uploader(&format!("{}/up", server.uri()));
    let err = u
        .upload(&UploadRequest::from_path("/definitely/not/here.png", UploadKind::Image), &ctx())
        .await
        .expect_err("io");
    assert!(matches!(err, UploadError::Io { .. }), "{err:?}");
    assert!(server.received_requests().await.expect("recorded").is_empty());
}
