//! Streaming request bodies with progress and cancellation.
//!
//! A [`BodyPlan`] is `prefix + payload + suffix`, where the payload is either in-memory
//! bytes or a file. For multipart uploads the prefix is the text fields plus the file part
//! header and the suffix is the closing boundary; for raw uploads both are empty. The plan
//! knows its exact length up front, so we send a real `Content-Length` instead of chunked
//! encoding (many hosts, including S3, reject chunked uploads).
//!
//! Files are read in [`CHUNK`]-sized pieces as the HTTP client pulls them, so peak memory is
//! independent of file size. The stream checks the cancellation token on every poll and
//! reports progress (throttled) as chunks are handed over.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use tokio_util::io::ReaderStream;

use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http::BodyFault;
use crate::progress::{ProgressSink, ThrottledProgress};
use crate::types::{UploadRequest, UploadSource};

/// Read granularity for files and slice size for in-memory payloads.
pub const CHUNK: usize = 64 * 1024;

/// Minimum spacing between progress callbacks.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// The payload part of a body.
#[derive(Debug, Clone)]
pub enum Payload {
    /// No payload.
    Empty,
    /// In-memory bytes.
    Memory(Bytes),
    /// A file, streamed.
    File {
        /// Path to read.
        path: PathBuf,
        /// Length at planning time; the stream fails if the file ends early.
        len: u64,
    },
}

impl Payload {
    /// Payload for an upload request (stats the file to learn its length).
    pub async fn from_request(req: &UploadRequest) -> Result<Self, UploadError> {
        match &req.source {
            UploadSource::Bytes { data, .. } => Ok(Self::Memory(data.clone())),
            UploadSource::Path(p) => {
                let len = req.payload_len().await?;
                Ok(Self::File { path: p.clone(), len })
            }
        }
    }

    /// Byte length.
    pub fn len(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::Memory(b) => b.len() as u64,
            Self::File { len, .. } => *len,
        }
    }

    /// Zero length.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `prefix + payload + suffix`.
#[derive(Debug, Clone)]
pub struct BodyPlan {
    prefix: Bytes,
    payload: Payload,
    suffix: Bytes,
}

impl BodyPlan {
    /// Just the payload.
    pub fn raw(payload: Payload) -> Self {
        Self { prefix: Bytes::new(), payload, suffix: Bytes::new() }
    }

    /// Payload framed by `prefix` and `suffix`.
    pub fn framed(prefix: impl Into<Bytes>, payload: Payload, suffix: impl Into<Bytes>) -> Self {
        Self { prefix: prefix.into(), payload, suffix: suffix.into() }
    }

    /// Total body length.
    pub fn content_length(&self) -> u64 {
        self.prefix.len() as u64 + self.payload.len() + self.suffix.len() as u64
    }

    /// Open the payload and produce the reqwest body plus a fault slot for local I/O errors.
    pub async fn into_body(
        self,
        ctx: &UploadContext,
    ) -> Result<(reqwest::Body, BodyFault), UploadError> {
        let total = self.content_length();
        let fault = BodyFault::default();
        let source = match self.payload {
            Payload::Empty => Source::Memory(Bytes::new()),
            Payload::Memory(b) => Source::Memory(b),
            Payload::File { path, len } => {
                let file = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| UploadError::io(format!("opening {}", path.display()), e))?;
                Source::File { reader: ReaderStream::with_capacity(file, CHUNK), expected: len, read: 0 }
            }
        };
        let progress: Arc<dyn ProgressSink> =
            Arc::new(ThrottledProgress::new(ctx.progress.clone(), PROGRESS_INTERVAL));
        let state = Streamer {
            stage: Stage::Prefix,
            prefix: self.prefix,
            source,
            suffix: self.suffix,
            sent: 0,
            total,
            progress,
            cancel: ctx.cancel.clone(),
            fault: fault.clone(),
        };
        let stream = futures_util::stream::unfold(state, |mut st| async move {
            let item = st.next_chunk().await?;
            Some((item, st))
        });
        Ok((reqwest::Body::wrap_stream(stream.boxed()), fault))
    }
}

enum Source {
    Memory(Bytes),
    File { reader: ReaderStream<tokio::fs::File>, expected: u64, read: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Prefix,
    Payload,
    Suffix,
    Done,
}

struct Streamer {
    stage: Stage,
    prefix: Bytes,
    source: Source,
    suffix: Bytes,
    sent: u64,
    total: u64,
    progress: Arc<dyn ProgressSink>,
    cancel: tokio_util::sync::CancellationToken,
    fault: BodyFault,
}

impl Streamer {
    fn emit(&mut self, chunk: Bytes) -> Option<std::io::Result<Bytes>> {
        self.sent += chunk.len() as u64;
        self.progress.report(self.sent, Some(self.total));
        Some(Ok(chunk))
    }

    fn fail(&mut self, e: std::io::Error, record: bool) -> Option<std::io::Result<Bytes>> {
        self.stage = Stage::Done;
        let out = std::io::Error::new(e.kind(), e.to_string());
        if record {
            self.fault.set(e);
        }
        Some(Err(out))
    }

    async fn next_chunk(&mut self) -> Option<std::io::Result<Bytes>> {
        loop {
            if self.stage != Stage::Done && self.cancel.is_cancelled() {
                return self.fail(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"), false);
            }
            match self.stage {
                Stage::Prefix => {
                    self.stage = Stage::Payload;
                    if !self.prefix.is_empty() {
                        let p = std::mem::take(&mut self.prefix);
                        return self.emit(p);
                    }
                }
                Stage::Payload => match &mut self.source {
                    Source::Memory(b) => {
                        if b.is_empty() {
                            self.stage = Stage::Suffix;
                        } else {
                            let n = b.len().min(CHUNK);
                            let chunk = b.split_to(n);
                            return self.emit(chunk);
                        }
                    }
                    Source::File { reader, expected, read } => match reader.next().await {
                        Some(Ok(chunk)) => {
                            *read += chunk.len() as u64;
                            if *read > *expected {
                                let e = std::io::Error::other("file grew while uploading");
                                return self.fail(e, true);
                            }
                            return self.emit(chunk);
                        }
                        Some(Err(e)) => return self.fail(e, true),
                        None => {
                            if *read != *expected {
                                let e = std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "file shrank while uploading",
                                );
                                return self.fail(e, true);
                            }
                            self.stage = Stage::Suffix;
                        }
                    },
                },
                Stage::Suffix => {
                    self.stage = Stage::Done;
                    if !self.suffix.is_empty() {
                        let s = std::mem::take(&mut self.suffix);
                        let out = self.emit(s);
                        self.progress.report(self.sent, Some(self.total));
                        return out;
                    }
                    self.progress.report(self.sent, Some(self.total));
                }
                Stage::Done => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt as _;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec(Mutex<Vec<(u64, Option<u64>)>>);
    impl ProgressSink for Rec {
        fn report(&self, sent: u64, total: Option<u64>) {
            self.0.lock().unwrap().push((sent, total));
        }
    }

    async fn collect(plan: BodyPlan, ctx: &UploadContext) -> Vec<Bytes> {
        // Re-create the stream the same way `into_body` does, but read it directly.
        let (body, _fault) = plan.into_body(ctx).await.unwrap();
        let mut s = http_body_util_stream(body);
        let mut out = Vec::new();
        while let Some(c) = s.try_next().await.unwrap() {
            out.push(c);
        }
        out
    }

    fn http_body_util_stream(
        body: reqwest::Body,
    ) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
        use http_body::Body as _;
        futures_util::stream::unfold(Box::pin(body), |mut b| async move {
            let frame = std::future::poll_fn(|cx| b.as_mut().poll_frame(cx)).await?;
            match frame {
                Ok(f) => match f.into_data() {
                    Ok(d) => Some((Ok(d), b)),
                    Err(_) => None,
                },
                Err(e) => Some((Err(std::io::Error::other(e.to_string())), b)),
            }
        })
        .boxed()
    }

    fn ctx(rec: Arc<Rec>) -> UploadContext {
        UploadContext::new(crate::http::build_http_client().unwrap()).with_progress(rec)
    }

    #[tokio::test]
    async fn memory_payload_is_chunked_and_framed() {
        let data = Bytes::from(vec![7u8; CHUNK * 2 + 10]);
        let plan = BodyPlan::framed("PRE", Payload::Memory(data.clone()), "SUF");
        assert_eq!(plan.content_length(), (data.len() + 6) as u64);
        let rec = Arc::new(Rec::default());
        let chunks = collect(plan, &ctx(rec.clone())).await;
        assert!(chunks.iter().all(|c| c.len() <= CHUNK));
        let joined: Vec<u8> = chunks.concat();
        assert_eq!(&joined[..3], b"PRE");
        assert_eq!(&joined[joined.len() - 3..], b"SUF");
        assert_eq!(joined.len(), data.len() + 6);
        let v = rec.0.lock().unwrap();
        assert_eq!(v.last().copied(), Some((joined.len() as u64, Some(joined.len() as u64))));
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[tokio::test]
    async fn file_payload_streams_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![1u8; CHUNK * 3 + 5]).unwrap();
        let plan = BodyPlan::raw(Payload::File { path, len: (CHUNK * 3 + 5) as u64 });
        let rec = Arc::new(Rec::default());
        let chunks = collect(plan, &ctx(rec.clone())).await;
        assert!(chunks.iter().all(|c| c.len() <= CHUNK));
        assert_eq!(chunks.iter().map(Bytes::len).sum::<usize>(), CHUNK * 3 + 5);
        let v = rec.0.lock().unwrap();
        assert_eq!(v.last().unwrap().0, (CHUNK * 3 + 5) as u64);
    }

    #[tokio::test]
    async fn shrinking_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"abc").unwrap();
        let plan = BodyPlan::raw(Payload::File { path, len: 10 });
        let rec = Arc::new(Rec::default());
        let (body, fault) = plan.into_body(&ctx(rec)).await.unwrap();
        let mut s = http_body_util_stream(body);
        let mut err = None;
        while let Some(item) = s.next().await {
            if let Err(e) = item {
                err = Some(e);
            }
        }
        assert!(err.is_some());
        assert!(fault.take().is_some());
    }

    #[tokio::test]
    async fn missing_file_reports_io_error() {
        let plan = BodyPlan::raw(Payload::File { path: "/nonexistent/x".into(), len: 1 });
        let rec = Arc::new(Rec::default());
        let err = plan.into_body(&ctx(rec)).await.unwrap_err();
        assert!(matches!(err, UploadError::Io { .. }));
    }

    #[tokio::test]
    async fn cancellation_stops_the_stream() {
        let plan = BodyPlan::raw(Payload::Memory(Bytes::from(vec![0u8; CHUNK * 4])));
        let rec = Arc::new(Rec::default());
        let c = ctx(rec);
        let (body, _f) = plan.into_body(&c).await.unwrap();
        c.cancel.cancel();
        let mut s = http_body_util_stream(body);
        assert!(s.next().await.unwrap().is_err());
        assert!(s.next().await.is_none());
    }
}
