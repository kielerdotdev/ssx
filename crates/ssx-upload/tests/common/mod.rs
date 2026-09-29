//! Shared helpers for the integration tests: a proxy-free context, a multipart parser and a
//! couple of wiremock conveniences.
#![allow(dead_code, reason = "each test binary uses a different subset of the helpers")]

use std::sync::{Arc, Mutex};

use ssx_upload::{ProgressSink, UploadContext};
use wiremock::{MockServer, Request};

/// A context whose client ignores `HTTP(S)_PROXY` (the sandbox and CI set them; the mock
/// servers live on loopback).
pub fn ctx() -> UploadContext {
    let http = ssx_upload::http::client_builder()
        .expect("client builder")
        .no_proxy()
        .build()
        .expect("client");
    UploadContext::new(http)
}

/// Progress sink that records every update.
#[derive(Default)]
pub struct Recorder(pub Mutex<Vec<(u64, Option<u64>)>>);

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<(u64, Option<u64>)> {
        self.0.lock().expect("lock").clone()
    }
}

impl ProgressSink for Recorder {
    fn report(&self, sent: u64, total: Option<u64>) {
        self.0.lock().expect("lock").push((sent, total));
    }
}

/// One part of a parsed multipart body.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl Part {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn param(header: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = header.find(&pat)? + pat.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

/// Strict multipart/form-data parser: panics (fails the test) on malformed framing.
pub fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<Part> {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .expect("multipart content type has a boundary")
        .trim_matches('"')
        .to_owned();
    let delim = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut pos = find(body, delim.as_bytes(), 0).expect("opening boundary");
    assert_eq!(pos, 0, "body must start with the boundary");
    loop {
        pos += delim.len();
        if body[pos..].starts_with(b"--") {
            assert_eq!(&body[pos..], b"--\r\n", "closing boundary must end the body");
            break;
        }
        assert!(body[pos..].starts_with(b"\r\n"), "boundary line ends with CRLF");
        pos += 2;
        let header_end = find(body, b"\r\n\r\n", pos).expect("part headers end");
        let headers = std::str::from_utf8(&body[pos..header_end]).expect("utf8 headers");
        let data_start = header_end + 4;
        let next = find(body, format!("\r\n{delim}").as_bytes(), data_start).expect("next boundary");
        let mut part = Part { name: String::new(), filename: None, content_type: None, data: body[data_start..next].to_vec() };
        for line in headers.split("\r\n") {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                part.name = param(line, "name").expect("part has a name");
                part.filename = param(line, "filename");
            } else if lower.starts_with("content-type:") {
                part.content_type = Some(line["content-type:".len()..].trim().to_owned());
            }
        }
        parts.push(part);
        pos = next + 2;
    }
    parts
}

/// Header value of a recorded request.
pub fn header(req: &Request, name: &str) -> Option<String> {
    req.headers.get(name).map(|v| v.to_str().expect("ascii header").to_owned())
}

/// The only request the server received.
pub async fn only_request(server: &MockServer) -> Request {
    let mut all = server.received_requests().await.expect("recording enabled");
    assert_eq!(all.len(), 1, "expected exactly one request, got {}", all.len());
    all.remove(0)
}

/// Multipart parts of a recorded request.
pub fn multipart_of(req: &Request) -> Vec<Part> {
    parse_multipart(&header(req, "content-type").expect("content-type"), &req.body)
}

/// Fixture text by file name.
pub fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Counters kept by [`SinkServer`].
#[derive(Default)]
pub struct SinkStats {
    /// Body bytes received (all requests).
    pub received: std::sync::atomic::AtomicU64,
    /// Largest single `read` that returned data.
    pub max_read: std::sync::atomic::AtomicUsize,
    /// Number of `read` calls that returned data.
    pub reads: std::sync::atomic::AtomicU64,
    /// Requests whose head advertised `Transfer-Encoding: chunked`.
    pub chunked: std::sync::atomic::AtomicU64,
    /// Requests seen.
    pub requests: std::sync::atomic::AtomicU64,
    /// Content-Length values seen.
    pub lengths: Mutex<Vec<u64>>,
}

/// How the sink behaves once the body starts arriving.
#[derive(Clone, Copy)]
pub enum SinkMode {
    /// Read everything, discard it, answer 200 `ok`.
    Discard,
    /// Read this many bytes then never read or answer again (to test cancellation).
    StallAfter(u64),
}

/// A minimal raw-TCP HTTP/1.1 server that never buffers request bodies, so tests can push
/// hundreds of megabytes at it and observe the read pattern.
pub struct SinkServer {
    pub addr: std::net::SocketAddr,
    pub stats: Arc<SinkStats>,
}

impl SinkServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

pub async fn spawn_sink(mode: SinkMode) -> SinkServer {
    use std::sync::atomic::Ordering::Relaxed;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stats = Arc::new(SinkStats::default());
    let st = stats.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let st = st.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                let head_end = loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if let Some(i) = find(&head, b"\r\n\r\n", 0) {
                        break i + 4;
                    }
                };
                let head_text = String::from_utf8_lossy(&head[..head_end]).to_ascii_lowercase();
                st.requests.fetch_add(1, Relaxed);
                if head_text.contains("transfer-encoding: chunked") {
                    st.chunked.fetch_add(1, Relaxed);
                }
                let len: u64 = head_text
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0);
                st.lengths.lock().expect("lock").push(len);
                let mut remaining = len.saturating_sub((head.len() - head_end) as u64);
                let mut got = (head.len() - head_end) as u64;
                st.received.fetch_add(got, Relaxed);
                while remaining > 0 {
                    if let SinkMode::StallAfter(limit) = mode {
                        if got >= limit {
                            std::future::pending::<()>().await;
                        }
                    }
                    let want = usize::try_from(remaining).unwrap_or(usize::MAX).min(buf.len());
                    let n = sock.read(&mut buf[..want]).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    st.reads.fetch_add(1, Relaxed);
                    st.max_read.fetch_max(n, Relaxed);
                    st.received.fetch_add(n as u64, Relaxed);
                    got += n as u64;
                    remaining -= n as u64;
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
                let _ = sock.shutdown().await;
            });
        }
    });
    SinkServer { addr, stats }
}

/// Create a sparse file of `len` bytes (reads back as zeros, uses no disk).
pub fn sparse_file(dir: &std::path::Path, name: &str, len: u64) -> std::path::PathBuf {
    let path = dir.join(name);
    let f = std::fs::File::create(&path).expect("create");
    f.set_len(len).expect("set_len");
    path
}
