//! Handing work to a running ssx instance over `ssx-ipc` (`post-file --coalesce`).
//!
//! File managers may start one `ssx post-file` process *per selected file* (Explorer does for
//! classic verbs). Each of those forwards its path here and exits at once; the running app
//! merges requests that arrive within a short window (about 400 ms) into one batch, so the
//! user sees one upload, one notification and one combined URL list. Merging is the daemon's
//! job; this side only forwards.
//!
//! When no instance is listening (or forwarding fails for any reason) the caller runs the
//! request in-process instead, so a right-click never silently does nothing.

use std::path::PathBuf;

use ssx_core::ipc::{
    PostAction, Request, RequestEnvelope, Response, ResponseEnvelope, decode_line, encode_line,
};
use ssx_ipc::Client;

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

#[cfg(test)]
mod tests {
    use ssx_core::ipc::ErrorCode;

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
