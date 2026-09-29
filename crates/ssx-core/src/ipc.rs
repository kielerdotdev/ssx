//! IPC message types shared by the tray app, the CLI and the file-manager shell shims, plus
//! a JSON-lines codec. The *transport* (named pipe on Windows, Unix socket elsewhere,
//! single-instance handling) lives in `ssx-ipc`; this module is only the vocabulary and the
//! framing, so all three sides agree byte for byte.
//!
//! # Wire format
//!
//! One JSON object per line, UTF-8, `\n` terminated (`\r\n` accepted). Every object carries
//! the protocol version `v`. Requests also carry a caller-chosen correlation `id` that the
//! response echoes:
//!
//! ```text
//! {"v":1,"id":7,"type":"post_files","paths":["/home/u/a.png"],"action":{"kind":"upload"}}
//! {"v":1,"id":7,"type":"accepted","run_id":3}
//! ```
//!
//! # Compatibility rules
//!
//! * Unknown *fields* are ignored (a newer peer may add optional fields).
//! * An unknown message `type` is an error the receiver answers with
//!   [`ErrorCode::InvalidRequest`]; a `v` newer than [`PROTOCOL_VERSION`] is refused up front
//!   with [`ErrorCode::VersionMismatch`], so old and new binaries fail loudly, not weirdly.
//! * Lines longer than [`MAX_LINE_BYTES`] are rejected without buffering them, so a
//!   misbehaving peer cannot make the tray app allocate unbounded memory.
//! * Paths travel as JSON strings and therefore must be valid UTF-8; encoding a request
//!   containing a non-UTF-8 path fails with [`CodecError::Encode`] (shims should report the
//!   file to the user instead of silently mangling it).

use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::workflow::Outcome;

/// Version of this protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest accepted line, in bytes (1 MiB: room for thousands of paths).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// What to capture for [`Request::Capture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// Interactive region.
    Region,
    /// Whole desktop.
    Fullscreen,
    /// Monitor under the cursor.
    Monitor,
    /// Active window.
    Window,
    /// Previous region.
    LastRegion,
}

/// What to do with files handed over from a file manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PostAction {
    /// Upload with the default file workflow.
    Upload,
    /// Open images in the editor first, then upload.
    Edit,
    /// Run a specific workflow (id, CLI name or display name) on the files.
    Workflow {
        /// The workflow reference.
        workflow: String,
    },
}

/// A request to the running ssx instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Liveness / version probe.
    Ping,
    /// Run a workflow. Give exactly one of `id` and `name` (`name` matches CLI name or display
    /// name). With `wait`, the response arrives when the run has finished.
    RunWorkflow {
        /// Workflow id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Workflow CLI name or display name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Reply only when the run has finished (default: reply with `accepted` at once).
        #[serde(default)]
        wait: bool,
    },
    /// Post files or folders (shell menu, `ssx upload`).
    PostFiles {
        /// Absolute paths (see the module docs on UTF-8).
        paths: Vec<PathBuf>,
        /// What to do with them.
        action: PostAction,
        /// Reply only when finished.
        #[serde(default)]
        wait: bool,
    },
    /// Take a screenshot.
    Capture {
        /// What to capture.
        target: CaptureKind,
        /// Workflow to run on the result (default: the built-in one for `target`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        /// Delay before capturing, overriding the setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u32>,
        /// Reply only when finished.
        #[serde(default)]
        wait: bool,
    },
    /// List configured workflows.
    ListWorkflows,
    /// Cancel a run started earlier.
    CancelRun {
        /// The id from [`Response::Accepted`].
        run_id: u64,
    },
    /// Stop recording of a running recording workflow.
    StopRecording,
    /// Ask the instance to exit.
    Quit,
}

/// A workflow reference resolved from [`Request::RunWorkflow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRef<'a> {
    /// By id.
    Id(&'a str),
    /// By CLI or display name.
    Name(&'a str),
}

impl Request {
    /// For [`Request::RunWorkflow`], the validated reference (exactly one of id / name).
    pub fn workflow_ref(&self) -> Result<WorkflowRef<'_>, String> {
        let Request::RunWorkflow { id, name, .. } = self else {
            return Err("not a run_workflow request".to_owned());
        };
        match (id.as_deref(), name.as_deref()) {
            (Some(id), None) if !id.trim().is_empty() => Ok(WorkflowRef::Id(id)),
            (None, Some(name)) if !name.trim().is_empty() => Ok(WorkflowRef::Name(name)),
            (Some(_), Some(_)) => Err("give either `id` or `name`, not both".to_owned()),
            _ => Err("run_workflow needs a non-empty `id` or `name`".to_owned()),
        }
    }
}

/// Machine-readable error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed or unknown request.
    InvalidRequest,
    /// The peer speaks a newer protocol.
    VersionMismatch,
    /// No such workflow.
    UnknownWorkflow,
    /// Another run is in progress and this one cannot start (single-recording limit, …).
    Busy,
    /// Nothing to cancel / stop.
    NotRunning,
    /// Something went wrong inside ssx.
    Internal,
}

/// Compact description of a workflow for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInfo {
    /// Workflow id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `ssx run <cli_name>` name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_name: Option<String>,
    /// Hotkey.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
}

/// One item of a finished run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemSummary {
    /// The local file, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The URL, if uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What failed, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of a finished run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Run id.
    pub run_id: u64,
    /// Overall outcome.
    pub outcome: Outcome,
    /// One-paragraph summary suitable for a terminal.
    pub message: String,
    /// Per-item details.
    #[serde(default)]
    pub items: Vec<ItemSummary>,
}

/// The instance's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Answer to [`Request::Ping`].
    Pong {
        /// ssx application version.
        app_version: String,
    },
    /// The request was accepted and is running in the background.
    Accepted {
        /// Id to use with [`Request::CancelRun`].
        run_id: u64,
    },
    /// A run finished (reply to a `wait` request).
    Finished(RunSummary),
    /// Answer to [`Request::ListWorkflows`].
    Workflows {
        /// The workflows.
        workflows: Vec<WorkflowInfo>,
    },
    /// Generic success (cancel, quit, stop).
    Ok,
    /// The request failed.
    Error {
        /// Category.
        code: ErrorCode,
        /// Human-readable explanation.
        message: String,
    },
}

impl Response {
    /// An error response.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error { code, message: message.into() }
    }
}

/// A request with its framing fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Protocol version.
    pub v: u32,
    /// Correlation id chosen by the caller, echoed in the response.
    #[serde(default)]
    pub id: u64,
    /// The request (its `type` and fields are flattened next to `v` and `id`).
    #[serde(flatten)]
    pub request: Request,
}

/// A response with its framing fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// Protocol version.
    pub v: u32,
    /// The request's correlation id.
    #[serde(default)]
    pub id: u64,
    /// The response.
    #[serde(flatten)]
    pub response: Response,
}

impl RequestEnvelope {
    /// Wraps `request` with the current version.
    pub fn new(id: u64, request: Request) -> Self {
        Self { v: PROTOCOL_VERSION, id, request }
    }
}

impl ResponseEnvelope {
    /// Wraps `response` with the current version.
    pub fn new(id: u64, response: Response) -> Self {
        Self { v: PROTOCOL_VERSION, id, response }
    }
}

/// Why encoding or decoding a line failed.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The line is not a valid message (bad JSON, unknown `type`, wrong field types).
    #[error("malformed message: {0}")]
    Malformed(String),
    /// The message has no `v` field.
    #[error("message has no protocol version (`v`)")]
    MissingVersion,
    /// The peer speaks a newer protocol.
    #[error("peer uses protocol version {found}, this build supports up to {supported}; upgrade ssx")]
    UnsupportedVersion {
        /// Version in the message.
        found: u32,
        /// Highest supported.
        supported: u32,
    },
    /// The line exceeded [`MAX_LINE_BYTES`] and was discarded.
    #[error("message is longer than {MAX_LINE_BYTES} bytes and was discarded")]
    LineTooLong,
    /// The message cannot be represented as JSON (for example a non-UTF-8 path).
    #[error("cannot encode message: {0}")]
    Encode(String),
    /// Reading or writing failed.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

impl CodecError {
    /// The [`ErrorCode`] a server should answer with for a *decode* error, or `None` if the
    /// connection itself is broken.
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Malformed(_) | Self::MissingVersion | Self::LineTooLong => Some(ErrorCode::InvalidRequest),
            Self::UnsupportedVersion { .. } => Some(ErrorCode::VersionMismatch),
            Self::Encode(_) | Self::Io(_) => None,
        }
    }
}

/// Serialises `msg` as one line (compact JSON plus `\n`).
pub fn encode_line<T: Serialize>(msg: &T) -> Result<String, CodecError> {
    let mut s = serde_json::to_string(msg).map_err(|e| CodecError::Encode(e.to_string()))?;
    debug_assert!(!s.contains('\n'), "compact JSON never contains raw newlines");
    if s.len() >= MAX_LINE_BYTES {
        return Err(CodecError::Encode(format!(
            "message is {} bytes; the limit is {MAX_LINE_BYTES}",
            s.len()
        )));
    }
    s.push('\n');
    Ok(s)
}

#[derive(Deserialize)]
struct VersionProbe {
    v: Option<serde_json::Value>,
}

/// Parses one line, checking the protocol version *before* interpreting the body so that a
/// newer peer's unknown message types are reported as a version problem.
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, CodecError> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::LineTooLong);
    }
    let probe: VersionProbe =
        serde_json::from_str(line).map_err(|e| CodecError::Malformed(e.to_string()))?;
    match probe.v {
        None => return Err(CodecError::MissingVersion),
        Some(serde_json::Value::Number(n)) => match n.as_u64().and_then(|v| u32::try_from(v).ok()) {
            Some(v) if v > PROTOCOL_VERSION => {
                return Err(CodecError::UnsupportedVersion { found: v, supported: PROTOCOL_VERSION });
            }
            Some(_) => {}
            None => return Err(CodecError::Malformed("`v` must be a small whole number".to_owned())),
        },
        Some(_) => return Err(CodecError::Malformed("`v` must be a number".to_owned())),
    }
    serde_json::from_str(line).map_err(|e| CodecError::Malformed(e.to_string()))
}

/// Reads messages from a byte stream, one per line, with bounded memory.
#[derive(Debug)]
pub struct LineReader<R> {
    inner: R,
}

impl<R: BufRead> LineReader<R> {
    /// Wraps a buffered reader.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Reads the next non-empty line. `Ok(None)` at end of stream (a final line without a
    /// trailing newline is still delivered). Over-long lines are skipped to the next newline
    /// and reported as [`CodecError::LineTooLong`], after which reading can continue.
    pub fn read_line(&mut self) -> Result<Option<String>, CodecError> {
        loop {
            let mut buf = Vec::new();
            let n = (&mut self.inner).take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE_BYTES {
                self.discard_rest_of_line()?;
                return Err(CodecError::LineTooLong);
            }
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return String::from_utf8(buf)
                .map(Some)
                .map_err(|_| CodecError::Malformed("line is not valid UTF-8".to_owned()));
        }
    }

    fn discard_rest_of_line(&mut self) -> io::Result<()> {
        loop {
            let (consumed, found) = {
                let chunk = self.inner.fill_buf()?;
                if chunk.is_empty() {
                    return Ok(());
                }
                match chunk.iter().position(|b| *b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (chunk.len(), false),
                }
            };
            self.inner.consume(consumed);
            if found {
                return Ok(());
            }
        }
    }

    /// Reads and decodes the next message.
    pub fn read_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, CodecError> {
        match self.read_line()? {
            Some(line) => decode_line(&line).map(Some),
            None => Ok(None),
        }
    }
}

/// Writes messages as lines and flushes after each.
#[derive(Debug)]
pub struct LineWriter<W> {
    inner: W,
}

impl<W: Write> LineWriter<W> {
    /// Wraps a writer.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Encodes and writes one message, then flushes.
    pub fn write_message<T: Serialize>(&mut self, msg: &T) -> Result<(), CodecError> {
        let line = encode_line(msg)?;
        self.inner.write_all(line.as_bytes())?;
        self.inner.flush()?;
        Ok(())
    }

    /// Gives the writer back.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use proptest::prelude::*;

    use super::*;

    fn all_requests() -> Vec<Request> {
        vec![
            Request::Ping,
            Request::RunWorkflow { id: Some("capture-region".into()), name: None, wait: false },
            Request::RunWorkflow { id: None, name: Some("Region".into()), wait: true },
            Request::PostFiles { paths: vec!["/a b/c.png".into(), "/日本/ファイル.txt".into()], action: PostAction::Upload, wait: false },
            Request::PostFiles { paths: vec![], action: PostAction::Edit, wait: true },
            Request::PostFiles { paths: vec!["/x".into()], action: PostAction::Workflow { workflow: "w".into() }, wait: false },
            Request::Capture { target: CaptureKind::Region, workflow: None, delay_ms: None, wait: false },
            Request::Capture { target: CaptureKind::LastRegion, workflow: Some("w".into()), delay_ms: Some(2500), wait: true },
            Request::ListWorkflows,
            Request::CancelRun { run_id: u64::MAX },
            Request::StopRecording,
            Request::Quit,
        ]
    }

    fn all_responses() -> Vec<Response> {
        vec![
            Response::Pong { app_version: "0.1.0".into() },
            Response::Accepted { run_id: 3 },
            Response::Finished(RunSummary {
                run_id: 3,
                outcome: Outcome::PartialSuccess,
                message: "1 of 2 uploaded".into(),
                items: vec![
                    ItemSummary { path: Some("/a.png".into()), url: Some("https://x/a".into()), error: None },
                    ItemSummary { path: None, url: None, error: Some("upload: offline".into()) },
                ],
            }),
            Response::Workflows { workflows: vec![WorkflowInfo { id: "a".into(), name: "A".into(), cli_name: Some("a".into()), hotkey: None }] },
            Response::Ok,
            Response::error(ErrorCode::UnknownWorkflow, "no workflow named \"x\""),
        ]
    }

    #[test]
    fn every_request_round_trips_through_a_line() {
        for (i, r) in all_requests().into_iter().enumerate() {
            let env = RequestEnvelope::new(i as u64, r);
            let line = encode_line(&env).unwrap();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1, "{line:?}");
            let back: RequestEnvelope = decode_line(&line).unwrap();
            assert_eq!(back, env);
        }
    }

    #[test]
    fn every_response_round_trips_through_a_line() {
        for (i, r) in all_responses().into_iter().enumerate() {
            let env = ResponseEnvelope::new(i as u64, r);
            let back: ResponseEnvelope = decode_line(&encode_line(&env).unwrap()).unwrap();
            assert_eq!(back, env);
        }
    }

    #[test]
    fn wire_format_is_stable() {
        // These literals are the protocol; changing them is a breaking change.
        let cases: Vec<(RequestEnvelope, &str)> = vec![
            (RequestEnvelope::new(1, Request::Ping), r#"{"v":1,"id":1,"type":"ping"}"#),
            (
                RequestEnvelope::new(7, Request::PostFiles { paths: vec!["/a.png".into()], action: PostAction::Upload, wait: false }),
                r#"{"v":1,"id":7,"type":"post_files","paths":["/a.png"],"action":{"kind":"upload"},"wait":false}"#,
            ),
            (
                RequestEnvelope::new(2, Request::RunWorkflow { id: None, name: Some("region".into()), wait: true }),
                r#"{"v":1,"id":2,"type":"run_workflow","name":"region","wait":true}"#,
            ),
            (
                RequestEnvelope::new(3, Request::Capture { target: CaptureKind::LastRegion, workflow: None, delay_ms: Some(10), wait: false }),
                r#"{"v":1,"id":3,"type":"capture","target":"last_region","delay_ms":10,"wait":false}"#,
            ),
        ];
        for (env, expected) in cases {
            assert_eq!(encode_line(&env).unwrap().trim_end(), expected);
            let back: RequestEnvelope = decode_line(expected).unwrap();
            assert_eq!(back, env);
        }
        assert_eq!(
            encode_line(&ResponseEnvelope::new(7, Response::Accepted { run_id: 3 })).unwrap().trim_end(),
            r#"{"v":1,"id":7,"type":"accepted","run_id":3}"#
        );
        assert_eq!(
            encode_line(&ResponseEnvelope::new(0, Response::error(ErrorCode::Busy, "b"))).unwrap().trim_end(),
            r#"{"v":1,"id":0,"type":"error","code":"busy","message":"b"}"#
        );
        assert_eq!(
            encode_line(&ResponseEnvelope::new(0, Response::Ok)).unwrap().trim_end(),
            r#"{"v":1,"id":0,"type":"ok"}"#
        );
    }

    #[test]
    fn optional_fields_default_and_unknown_fields_are_ignored() {
        let r: RequestEnvelope = decode_line(r#"{"v":1,"type":"run_workflow","id":"x","future_field":{"a":[1,2]}}"#).unwrap();
        assert_eq!(r.id, 0, "missing correlation id defaults to 0");
        assert_eq!(r.request, Request::RunWorkflow { id: Some("x".into()), name: None, wait: false });
        let r: RequestEnvelope = decode_line(r#"{"v":1,"id":5,"type":"capture","target":"window"}"#).unwrap();
        assert_eq!(r.request, Request::Capture { target: CaptureKind::Window, workflow: None, delay_ms: None, wait: false });
    }

    #[test]
    fn newer_versions_are_refused_before_the_body_is_parsed() {
        let e = decode_line::<RequestEnvelope>(r#"{"v":2,"id":1,"type":"teleport","x":1}"#).unwrap_err();
        assert!(matches!(e, CodecError::UnsupportedVersion { found: 2, supported: 1 }), "{e}");
        assert_eq!(e.error_code(), Some(ErrorCode::VersionMismatch));
        assert!(e.to_string().contains("upgrade ssx"));
    }

    #[test]
    fn bad_versions_and_bodies() {
        let cases = [
            (r#"{"id":1,"type":"ping"}"#, "MissingVersion"),
            (r#"{"v":"1","type":"ping"}"#, "Malformed"),
            (r#"{"v":-1,"type":"ping"}"#, "Malformed"),
            (r#"{"v":1.5,"type":"ping"}"#, "Malformed"),
            (r#"{"v":99999999999,"type":"ping"}"#, "Malformed"),
            (r#"{"v":1,"type":"teleport"}"#, "Malformed"),
            (r#"{"v":1}"#, "Malformed"),
            (r#"{"v":1,"type":"post_files","paths":"nope","action":{"kind":"upload"}}"#, "Malformed"),
            (r#"{"v":1,"type":"post_files","paths":[],"action":{"kind":"explode"}}"#, "Malformed"),
            ("not json", "Malformed"),
            ("", "Malformed"),
            ("[]", "Malformed"),
            ("null", "Malformed"),
        ];
        for (line, want) in cases {
            let e = decode_line::<RequestEnvelope>(line).unwrap_err();
            assert!(format!("{e:?}").starts_with(want), "{line}: {e:?}");
            assert_eq!(e.error_code(), Some(ErrorCode::InvalidRequest), "{line}");
        }
    }

    #[test]
    fn workflow_ref_validation() {
        let r = |id: Option<&str>, name: Option<&str>| Request::RunWorkflow { id: id.map(Into::into), name: name.map(Into::into), wait: false };
        assert_eq!(r(Some("a"), None).workflow_ref().unwrap(), WorkflowRef::Id("a"));
        assert_eq!(r(None, Some("b")).workflow_ref().unwrap(), WorkflowRef::Name("b"));
        assert!(r(Some("a"), Some("b")).workflow_ref().unwrap_err().contains("not both"));
        assert!(r(None, None).workflow_ref().is_err());
        assert!(r(Some("  "), None).workflow_ref().is_err());
        assert!(Request::Ping.workflow_ref().is_err());
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let r: RequestEnvelope = decode_line("{\"v\":1,\"id\":1,\"type\":\"ping\"}\r\n").unwrap();
        assert_eq!(r.request, Request::Ping);
    }

    #[test]
    fn non_utf8_paths_cannot_be_encoded() {
        #[cfg(unix)]
        {
            use std::{ffi::OsString, os::unix::ffi::OsStringExt};
            let bad = PathBuf::from(OsString::from_vec(vec![b'/', 0xFF, 0xFE]));
            let env = RequestEnvelope::new(1, Request::PostFiles { paths: vec![bad], action: PostAction::Upload, wait: false });
            let e = encode_line(&env).unwrap_err();
            assert!(matches!(e, CodecError::Encode(_)), "{e}");
            assert!(e.to_string().contains("UTF-8"));
            assert_eq!(e.error_code(), None);
        }
    }

    #[test]
    fn oversized_messages_are_refused_on_encode() {
        let big = Request::PostFiles { paths: vec![PathBuf::from("x".repeat(MAX_LINE_BYTES))], action: PostAction::Upload, wait: false };
        assert!(matches!(encode_line(&RequestEnvelope::new(1, big)), Err(CodecError::Encode(_))));
    }

    // ---- framing ----

    fn reader(bytes: &[u8]) -> LineReader<Cursor<Vec<u8>>> {
        LineReader::new(Cursor::new(bytes.to_vec()))
    }

    #[test]
    fn stream_of_messages() {
        let mut out = LineWriter::new(Vec::new());
        for (i, r) in all_requests().into_iter().enumerate() {
            out.write_message(&RequestEnvelope::new(i as u64, r)).unwrap();
        }
        let bytes = out.into_inner();
        let mut rd = LineReader::new(Cursor::new(bytes));
        let mut n = 0;
        while let Some(m) = rd.read_message::<RequestEnvelope>().unwrap() {
            assert_eq!(m.id, n);
            n += 1;
        }
        assert_eq!(n as usize, all_requests().len());
        assert!(rd.read_message::<RequestEnvelope>().unwrap().is_none(), "EOF stays EOF");
    }

    #[test]
    fn last_line_without_newline_and_blank_lines() {
        let mut rd = reader(b"\n\r\n  \n{\"v\":1,\"id\":1,\"type\":\"ping\"}\n\n{\"v\":1,\"id\":2,\"type\":\"quit\"}");
        assert_eq!(rd.read_message::<RequestEnvelope>().unwrap().unwrap().request, Request::Ping);
        assert_eq!(rd.read_message::<RequestEnvelope>().unwrap().unwrap().request, Request::Quit);
        assert!(rd.read_message::<RequestEnvelope>().unwrap().is_none());
    }

    #[test]
    fn empty_stream() {
        assert!(reader(b"").read_line().unwrap().is_none());
        assert!(reader(b"\n\n").read_line().unwrap().is_none());
    }

    #[test]
    fn partial_message_at_eof_is_malformed_not_a_hang() {
        let mut rd = reader(b"{\"v\":1,\"id\":1,\"ty");
        let e = rd.read_message::<RequestEnvelope>().unwrap_err();
        assert!(matches!(e, CodecError::Malformed(_)));
    }

    #[test]
    fn oversized_line_is_skipped_and_the_stream_resynchronises() {
        let mut data = vec![b'x'; MAX_LINE_BYTES + 5000];
        data.push(b'\n');
        data.extend_from_slice(b"{\"v\":1,\"id\":9,\"type\":\"ping\"}\n");
        let mut rd = reader(&data);
        let e = rd.read_message::<RequestEnvelope>().unwrap_err();
        assert!(matches!(e, CodecError::LineTooLong));
        assert_eq!(e.error_code(), Some(ErrorCode::InvalidRequest));
        assert_eq!(rd.read_message::<RequestEnvelope>().unwrap().unwrap().id, 9);
    }

    #[test]
    fn oversized_line_without_any_newline_terminates() {
        let data = vec![b'y'; MAX_LINE_BYTES * 3];
        let mut rd = reader(&data);
        assert!(matches!(rd.read_line(), Err(CodecError::LineTooLong)));
        assert!(rd.read_line().unwrap().is_none());
    }

    #[test]
    fn line_of_exactly_the_limit_is_accepted() {
        let mut data = vec![b' '; MAX_LINE_BYTES - 1];
        data[0] = b'"';
        data[MAX_LINE_BYTES - 2] = b'"';
        data.push(b'\n');
        let line = reader(&data).read_line().unwrap().unwrap();
        assert_eq!(line.len(), MAX_LINE_BYTES - 1);
    }

    #[test]
    fn invalid_utf8_is_malformed() {
        let mut rd = reader(b"{\"v\":1,\"type\":\"\xFF\"}\n{\"v\":1,\"id\":1,\"type\":\"ping\"}\n");
        assert!(matches!(rd.read_message::<RequestEnvelope>(), Err(CodecError::Malformed(_))));
        assert!(rd.read_message::<RequestEnvelope>().unwrap().is_some(), "next line still readable");
    }

    #[test]
    fn a_slow_byte_at_a_time_stream_works() {
        struct Trickle(Vec<u8>, usize);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.1 >= self.0.len() {
                    return Ok(0);
                }
                buf[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut rd = LineReader::new(io::BufReader::with_capacity(1, Trickle(b"{\"v\":1,\"id\":4,\"type\":\"ping\"}\n".to_vec(), 0)));
        assert_eq!(rd.read_message::<RequestEnvelope>().unwrap().unwrap().id, 4);
    }

    #[test]
    fn io_errors_propagate() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
        }
        let mut rd = LineReader::new(io::BufReader::new(Broken));
        let e = rd.read_line().unwrap_err();
        assert!(matches!(e, CodecError::Io(_)));
        assert_eq!(e.error_code(), None);
        struct BrokenWrite;
        impl Write for BrokenWrite {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(matches!(LineWriter::new(BrokenWrite).write_message(&Request::Ping), Err(CodecError::Io(_))));
    }

    proptest! {
        #[test]
        fn arbitrary_paths_and_strings_round_trip(
            paths in proptest::collection::vec("\\PC{0,40}", 0..6),
            wf in "\\PC{0,30}",
            id in any::<u64>(),
            wait in any::<bool>(),
        ) {
            let env = RequestEnvelope::new(id, Request::PostFiles {
                paths: paths.into_iter().map(PathBuf::from).collect(),
                action: PostAction::Workflow { workflow: wf },
                wait,
            });
            let line = encode_line(&env).unwrap();
            prop_assert_eq!(line.matches('\n').count(), 1);
            prop_assert_eq!(decode_line::<RequestEnvelope>(&line).unwrap(), env);
        }

        #[test]
        fn decoding_arbitrary_text_never_panics(s in "\\PC{0,200}") {
            let _ = decode_line::<RequestEnvelope>(&s);
            let _ = decode_line::<ResponseEnvelope>(&s);
        }

        #[test]
        fn reader_handles_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let mut rd = LineReader::new(Cursor::new(bytes));
            for _ in 0..400 {
                match rd.read_message::<RequestEnvelope>() {
                    Ok(None) => break,
                    Ok(Some(_)) | Err(_) => {}
                }
            }
        }
    }
}
