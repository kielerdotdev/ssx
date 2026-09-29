//! Peak memory must not scale with file size.
//!
//! A sparse 200 MB file is streamed to a server that discards the body. This is the *only*
//! test in this binary so that the process high-water mark (`VmHWM`) reflects nothing but
//! this upload. Two independent signals are checked: the server observes reads no larger
//! than the transport's chunking (the body is produced piecewise), and on Linux the
//! process RSS high-water mark grows by far less than the file size.
#![cfg(feature = "sxcu")]

mod common;

use std::sync::atomic::Ordering::Relaxed;

use common::{SinkMode, ctx, spawn_sink, sparse_file};
use ssx_upload::sxcu::SxcuUploader;
use ssx_upload::{UploadKind, UploadRequest, Uploader};

const FILE_LEN: u64 = 200 * 1024 * 1024;

fn vm_hwm_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploading_a_200_mb_file_streams_in_small_chunks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = sparse_file(dir.path(), "huge.bin", FILE_LEN);
    let sink = spawn_sink(SinkMode::Discard).await;

    let json = format!(
        r#"{{"Version":"14.1.0","Name":"sink","DestinationType":"FileUploader","RequestMethod":"PUT",
            "RequestURL":"{}","Body":"Binary","URL":"{{response}}"}}"#,
        sink.url("/up")
    );
    let uploader = SxcuUploader::from_json_str(&json).expect("uploader");
    let ctx = ctx();

    // Warm up (TLS config, allocator, tokio) so the baseline includes fixed costs.
    let warm = UploadRequest::from_bytes(vec![0u8; 1024], "w.bin", UploadKind::File);
    uploader.upload(&warm, &ctx).await.expect("warm-up");
    let before = vm_hwm_kib();

    let recorder = common::Recorder::new();
    let ctx = ctx.with_progress(recorder.clone());
    let req = UploadRequest::from_path(&path, UploadKind::File);
    let started = std::time::Instant::now();
    let res = uploader.upload(&req, &ctx).await.expect("upload");
    let elapsed = started.elapsed();
    assert_eq!(res.url, "ok");

    let s = &sink.stats;
    assert_eq!(s.chunked.load(Relaxed), 0, "must use Content-Length, not chunked encoding");
    assert_eq!(s.received.load(Relaxed), FILE_LEN + 1024, "every byte arrives");
    assert!(s.reads.load(Relaxed) > 200, "the body arrives in many pieces, got {} reads", s.reads.load(Relaxed));
    let events = recorder.events();
    assert_eq!(events.last().copied(), Some((FILE_LEN, Some(FILE_LEN))));
    assert!(events.windows(2).all(|w| w[0].0 <= w[1].0), "progress is monotonic");
    assert!(events.len() < 20_000, "progress is throttled ({} events in {elapsed:?})", events.len());

    match (before, vm_hwm_kib()) {
        (Some(before), Some(after)) => {
            let grown_mib = after.saturating_sub(before) / 1024;
            eprintln!("VmHWM grew by {grown_mib} MiB while uploading {} MiB in {elapsed:?}", FILE_LEN / 1024 / 1024);
            assert!(grown_mib < 40, "peak memory grew by {grown_mib} MiB: the file is being buffered");
            // Control: prove the probe can see a 200 MB buffer, i.e. the assertion above is
            // not vacuous.
            let buffered = std::fs::read(&path).expect("read whole file");
            let after_read = vm_hwm_kib().unwrap_or(0);
            assert!(after_read.saturating_sub(after) / 1024 > 150, "probe failed to notice a 200 MB buffer");
            drop(buffered);
        }
        _ => eprintln!("skipping the RSS assertion: /proc/self/status is unavailable on this OS"),
    }
}
