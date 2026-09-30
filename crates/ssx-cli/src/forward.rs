//! Handing work to a running ssx instance over `ssx-ipc`: `post-file --coalesce`, `run`,
//! interactive `capture region`, `record start|stop|toggle|status`, `daemon status|stop`.
//!
//! File managers may start one `ssx post-file` process *per selected file* (Explorer does for
//! classic verbs). Each of those forwards its path here and exits at once; the running app
//! merges requests that arrive within a short window (about 400 ms) into one batch, so the
//! user sees one upload, one notification and one combined URL list. Merging is the daemon's
//! job; this side only forwards.
//!
//! When no instance is listening (or forwarding fails for any reason) the caller runs the
//! request in-process instead, so a right-click never silently does nothing.

use std::{path::PathBuf, time::Duration};

use ssx_core::{
    ipc::{
        ErrorCode, PostAction, Request, RequestEnvelope, Response, ResponseEnvelope, RunSummary,
        decode_line, encode_line,
    },
    workflow::CancelToken,
};
use ssx_ipc::{Client, ClientConfig};

use crate::error::{CliError, CliResult};

/// The application id of the tray/daemon instance.
pub const APP_ID: &str = "ssx";

/// What happened to a forwarding attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    /// The running instance accepted the request.
    Sent,
    /// Nothing is listening: run the request here.
    NotRunning,
    /// Forwarding was not possible or was refused; run the request here. The text says why.
    Failed(String),
}

/// The absolute UTF-8 form of `paths`, as the wire protocol requires.
pub fn wire_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|p| {
            let abs = std::path::absolute(p)
                .map_err(|e| format!("cannot resolve {}: {e}", p.display()))?;
            if abs.to_str().is_none() {
                return Err(format!(
                    "{} is not valid UTF-8, which the instance protocol needs",
                    abs.display()
                ));
            }
            Ok(abs)
        })
        .collect()
}

/// The request line for `paths` and `action` (no waiting for the run to finish).
pub fn request_line(paths: &[PathBuf], action: PostAction) -> Result<String, String> {
    let paths = wire_paths(paths)?;
    let env = RequestEnvelope::new(1, Request::PostFiles { paths, action, wait: false });
    // `encode_line` ends the line with `\n`; the transport adds its own terminator and refuses
    // a line that already contains one.
    encode_line(&env)
        .map(|l| l.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|e| e.to_string())
}

/// Interprets the instance's reply line.
pub fn interpret_reply(line: &str) -> Forward {
    match decode_line::<ResponseEnvelope>(line) {
        Ok(env) => match env.response {
            Response::Accepted { .. } | Response::Ok | Response::Finished(_) => Forward::Sent,
            Response::Error { code, message } => Forward::Failed(format!(
                "the running instance refused the request ({code:?}): {message}"
            )),
            other => {
                Forward::Failed(format!("unexpected reply from the running instance: {other:?}"))
            }
        },
        Err(e) => Forward::Failed(format!("unreadable reply from the running instance: {e}")),
    }
}

/// Sends `paths` to the running instance, if there is one.
pub fn forward_post_files(paths: &[PathBuf], action: PostAction) -> Forward {
    if !enabled() {
        return Forward::NotRunning;
    }
    let line = match request_line(paths, action) {
        Ok(l) => l,
        Err(e) => return Forward::Failed(e),
    };
    let client = match Client::for_app(APP_ID) {
        Ok(c) => c,
        Err(e) => return Forward::Failed(format!("cannot reach the instance socket: {e}")),
    };
    match client.is_listening() {
        Ok(false) => return Forward::NotRunning,
        Ok(true) => {}
        Err(e) => return Forward::Failed(format!("cannot reach the instance: {e}")),
    }
    match client.request(&line) {
        Ok(reply) => interpret_reply(&reply),
        Err(ssx_ipc::Error::NotRunning) => Forward::NotRunning,
        Err(e) => Forward::Failed(format!("the running instance did not answer: {e}")),
    }
}

/// Handing work to the running instance can be switched off with `SSX_NO_DAEMON=1`
/// (debugging, and tests that must exercise the in-process path).
pub fn enabled() -> bool {
    std::env::var_os("SSX_NO_DAEMON").is_none_or(|v| v.is_empty() || v == "0")
}

/// A running ssx-app instance.
#[derive(Debug, Clone)]
pub struct Daemon {
    client: Client,
}

/// How long a request that waits for a whole workflow run may take (a recording can be as
/// long as the user likes, an editor session open for hours).
const LONG_REQUEST: Duration = Duration::from_secs(24 * 60 * 60);

fn line_of(request: Request) -> Result<String, String> {
    encode_line(&RequestEnvelope::new(1, request))
        .map(|l| l.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|e| e.to_string())
}

impl Daemon {
    /// The running instance, if one is listening (and hand-off is not disabled).
    pub fn connect() -> Option<Self> {
        if !enabled() {
            return None;
        }
        let client = Client::for_app(APP_ID).ok()?;
        matches!(client.is_listening(), Ok(true)).then_some(Self { client })
    }

    /// A handle for `client` without checking that anything listens (tests).
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Like [`connect`](Self::connect) but ignores `SSX_NO_DAEMON` (`ssx daemon status`).
    pub fn connect_always() -> Option<Self> {
        let client = Client::for_app(APP_ID).ok()?;
        matches!(client.is_listening(), Ok(true)).then_some(Self { client })
    }

    fn exchange(client: &Client, request: Request) -> Result<Response, String> {
        let line = line_of(request)?;
        let reply =
            client.request(&line).map_err(|e| format!("the ssx app did not answer: {e}"))?;
        decode_line::<ResponseEnvelope>(&reply)
            .map(|env| env.response)
            .map_err(|e| format!("unreadable reply from the ssx app: {e}"))
    }

    /// Sends one request and returns the answer (10 s limit).
    pub fn call(&self, request: Request) -> Result<Response, String> {
        Self::exchange(&self.client, request)
    }

    /// Like [`call`](Self::call) for requests that wait for a run to finish.
    pub fn call_long(&self, request: Request) -> Result<Response, String> {
        let config = ClientConfig { request_timeout: LONG_REQUEST, ..self.client.config().clone() };
        Self::exchange(&self.client.clone().with_config(config), request)
    }

    /// The application version, if it answers a ping.
    pub fn ping(&self) -> Option<String> {
        match self.call(Request::Ping) {
            Ok(Response::Pong { app_version }) => Some(app_version),
            _ => None,
        }
    }
}

/// Maps an error response to a CLI error with a hint that says what to do.
pub fn error_from_response(code: ErrorCode, message: &str) -> CliError {
    let e = CliError::new(message.to_owned());
    match code {
        ErrorCode::Busy => e.hint(
            "finish the capture or recording that is open first, or cancel it from the tray menu",
        ),
        ErrorCode::UnknownWorkflow => e.hint("`ssx config show` lists the workflows"),
        ErrorCode::NotRunning => e,
        ErrorCode::VersionMismatch | ErrorCode::InvalidRequest => e.hint(
            "the running ssx-app may be older than this `ssx`: restart it with `ssx daemon restart`",
        ),
        ErrorCode::Internal => e.hint("the ssx-app log file has the details"),
    }
}

/// Runs a request that starts a run in the app and waits for the result. The request must
/// have `wait: false`; this function then follows the run with `WaitRun`, and forwards a
/// Ctrl-C (`cancel`) to the app as `CancelRun`, so cancelling the CLI cancels the overlay or
/// upload in the app.
pub fn run_remote(
    daemon: &Daemon,
    request: Request,
    cancel: &CancelToken,
) -> CliResult<RunSummary> {
    let run_id = match daemon.call(request).map_err(CliError::new)? {
        Response::Accepted { run_id } => run_id,
        Response::Recording(status) => status
            .run_id
            .ok_or_else(|| CliError::new("the ssx app accepted the request but started no run"))?,
        Response::Error { code, message } => return Err(error_from_response(code, &message)),
        other => {
            return Err(CliError::new(format!("unexpected answer from the ssx app: {other:?}")));
        }
    };
    let done = CancelToken::new();
    let canceller = {
        let (cancel, done) = (cancel.clone(), done.clone());
        let daemon = daemon.clone();
        std::thread::Builder::new().name("ssx-forward-cancel".into()).spawn(move || {
            loop {
                if cancel.wait_timeout(Duration::from_millis(100)) {
                    let _ = daemon.call(Request::CancelRun { run_id });
                    return;
                }
                if done.is_cancelled() {
                    return;
                }
            }
        })
    };
    let answer = daemon.call_long(Request::WaitRun { run_id });
    done.cancel();
    if let Ok(t) = canceller {
        let _ = t.join();
    }
    match answer.map_err(CliError::new)? {
        Response::Finished(summary) => Ok(summary),
        Response::Error { code, message } => Err(error_from_response(code, &message)),
        other => Err(CliError::new(format!("unexpected answer from the ssx app: {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_carry_absolute_paths_verbatim() {
        let hostile = PathBuf::from("rel/a b \"quoted\" $(x) \n newline.png");
        let line =
            request_line(&[hostile.clone(), PathBuf::from("/abs/é.png")], PostAction::Upload)
                .unwrap();
        assert!(!line.contains('\n'), "no terminator: the transport adds it: {line:?}");
        let env: RequestEnvelope = decode_line(&line).unwrap();
        let Request::PostFiles { paths, action, wait } = env.request else {
            panic!("wrong request")
        };
        assert_eq!(action, PostAction::Upload);
        assert!(!wait);
        assert!(paths[0].is_absolute() && paths[0].ends_with(&hostile), "{paths:?}");
        assert_eq!(paths[1], PathBuf::from("/abs/é.png"));
    }

    #[test]
    fn workflow_actions_are_forwarded() {
        let line = request_line(
            &[PathBuf::from("/a")],
            PostAction::Workflow { workflow: "upload".into() },
        )
        .unwrap();
        assert!(line.contains("\"workflow\":\"upload\""), "{line}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_cannot_be_forwarded() {
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/bad\xff.png"));
        let e = request_line(&[bad], PostAction::Upload).unwrap_err();
        assert!(e.contains("UTF-8"), "{e}");
    }

    #[test]
    fn replies_are_interpreted() {
        let ok =
            encode_line(&ssx_core::ipc::ResponseEnvelope::new(1, Response::Accepted { run_id: 3 }))
                .unwrap();
        assert_eq!(interpret_reply(&ok), Forward::Sent);
        let busy = encode_line(&ssx_core::ipc::ResponseEnvelope::new(
            1,
            Response::error(ErrorCode::Busy, "another run is active"),
        ))
        .unwrap();
        assert!(
            matches!(interpret_reply(&busy), Forward::Failed(m) if m.contains("another run is active"))
        );
        assert!(matches!(interpret_reply("not json"), Forward::Failed(_)));
        let pong = encode_line(&ssx_core::ipc::ResponseEnvelope::new(
            1,
            Response::Pong { app_version: "1".into() },
        ))
        .unwrap();
        assert!(matches!(interpret_reply(&pong), Forward::Failed(m) if m.contains("unexpected")));
    }
}
