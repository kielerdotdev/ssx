//! The IPC request handler: `ssx_core::ipc::Request` in, `Response` out.
//!
//! [`IpcHandler::handle_line`] is what `ssx_ipc::Server::serve` calls for every request line on
//! its per-connection threads. Requests that wait for a run (`wait: true`, `WaitRun`, a
//! `PostFiles` that waits for its batch) block *their own connection thread* only.
//!
//! Which request becomes which job is decided by [`crate::requests`]; whether the job may run
//! now, must queue or is refused is the [`Supervisor`]'s decision. This module only glues them
//! together and turns the answers into protocol responses, so a bug here cannot change what
//! runs. Everything the handler needs from the rest of the app (settings, quit, show a window,
//! status) is the small [`AppControl`] trait, which the tests implement with a fake.

use std::{
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

use ssx_core::{
    ipc::{
        DaemonStatus, ErrorCode, PostAction, Request, RequestEnvelope, Response, ResponseEnvelope,
        RunSummary, ShowTarget, decode_line, encode_line,
    },
    settings::Settings,
    workflow::{NotificationLevel, Outcome},
};

use crate::{
    clock::Clock,
    coalesce::{Batch, CoalesceConfig, Coalescer},
    daemon::{Rejection, Submitted, Supervisor, Waited, plain_summary},
    events::{UiEvent, UiSink},
    ids::RunIds,
    requests::{job_for_files, job_for_request, list_workflows},
};

/// What the handler needs from the application.
pub trait AppControl: Send + Sync + 'static {
    /// The settings in force.
    fn settings(&self) -> Arc<Settings>;
    /// Exit the daemon.
    fn quit(&self);
    /// Bring a window up.
    fn show(&self, target: ShowTarget);
    /// Re-read the settings file now.
    fn reload_settings(&self);
    /// Everything `Status` reports except the load (the handler adds runs and recording).
    fn status(&self) -> DaemonStatus;
}

/// A batch's waiters hear either the summary or why the batch could not start.
pub type BatchResult = Result<RunSummary, Rejection>;

/// The coalescer for `PostFiles`: closed batches become one run each.
pub type FilesCoalescer = Coalescer<mpsc::Sender<BatchResult>>;

/// Builds the coalescer whose sink submits batches to `sup`.
pub fn files_coalescer(
    cfg: CoalesceConfig,
    ids: RunIds,
    clock: Arc<dyn Clock>,
    sup: Arc<Supervisor>,
    control: Arc<dyn AppControl>,
    ui: Arc<dyn UiSink>,
) -> FilesCoalescer {
    Coalescer::new(cfg, ids, clock, move |batch: Batch<mpsc::Sender<BatchResult>>| {
        let Batch { id, action, paths, waiters, requests } = batch;
        tracing::info!(run = id, files = paths.len(), requests, "uploading a batch of files");
        let settings = control.settings();
        let submitted = job_for_files(&settings, &action, paths, id)
            .and_then(|job| sup.submit(job).map(|s| (s, job_id_of(id))));
        match submitted {
            Ok((sub, _)) => {
                if waiters.is_empty() {
                    return;
                }
                // Tell the waiting connections when the run ends, without holding the
                // coalescer's thread.
                let sup = Arc::clone(&sup);
                let run_id = sub.run_id();
                let spawned =
                    std::thread::Builder::new().name("ssx-batch-wait".into()).spawn(move || {
                        let result = match sup.wait(run_id, None) {
                            Waited::Finished(s) => Ok(s),
                            Waited::TimedOut | Waited::Unknown => Err(Rejection::NotRunning(
                                "the run ended without a result".to_owned(),
                            )),
                        };
                        for w in waiters {
                            let _ = w.send(result.clone());
                        }
                    });
                if let Err(e) = spawned {
                    tracing::error!("cannot start a waiter thread: {e}");
                }
            }
            Err(rej) => {
                ui.emit(UiEvent::Notice {
                    level: NotificationLevel::Warning,
                    title: "Could not upload the files".to_owned(),
                    body: rej.to_string(),
                });
                for w in waiters {
                    let _ = w.send(Err(rej.clone()));
                }
            }
        }
    })
}

fn job_id_of(id: u64) -> u64 {
    id
}

/// The handler. Cheap to share behind an `Arc`.
pub struct IpcHandler {
    sup: Arc<Supervisor>,
    files: Arc<FilesCoalescer>,
    control: Arc<dyn AppControl>,
}

impl std::fmt::Debug for IpcHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcHandler").finish_non_exhaustive()
    }
}

/// How long a connection waits for a batch it asked to wait for (12 hours: effectively "until
/// it is done", but bounded so a bug cannot leak a thread forever).
const BATCH_WAIT: Duration = Duration::from_secs(12 * 60 * 60);

fn error(rej: &Rejection) -> Response {
    Response::error(rej.code(), rej.to_string())
}

impl IpcHandler {
    /// A handler over these parts.
    pub fn new(
        sup: Arc<Supervisor>,
        files: Arc<FilesCoalescer>,
        control: Arc<dyn AppControl>,
    ) -> Self {
        Self { sup, files, control }
    }

    /// Handles one request line and returns one response line (no trailing newline).
    pub fn handle_line(&self, line: &str) -> String {
        let (seq, response) = match decode_line::<RequestEnvelope>(line) {
            Ok(env) => {
                let seq = env.seq;
                let request = env.request;
                let r = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle(&request)))
                    .unwrap_or_else(|_| {
                        tracing::error!("the IPC handler panicked on {request:?}");
                        Response::error(ErrorCode::Internal, "ssx hit an internal error")
                    });
                (seq, r)
            }
            Err(e) => (
                0,
                Response::error(e.error_code().unwrap_or(ErrorCode::InvalidRequest), e.to_string()),
            ),
        };
        let line = encode_line(&ResponseEnvelope::new(seq, response)).unwrap_or_else(|e| {
            let fallback = ResponseEnvelope::new(
                seq,
                Response::error(ErrorCode::Internal, format!("cannot encode the response: {e}")),
            );
            encode_line(&fallback).unwrap_or_default()
        });
        line.trim_end_matches(['\r', '\n']).to_owned()
    }

    fn finished(&self, run_id: u64) -> Response {
        match self.sup.wait(run_id, None) {
            Waited::Finished(s) => Response::Finished(s),
            Waited::Unknown | Waited::TimedOut => {
                Response::error(ErrorCode::NotRunning, format!("run {run_id} is not known"))
            }
        }
    }

    /// Handles one decoded request.
    pub fn handle(&self, request: &Request) -> Response {
        match request {
            Request::Ping => Response::Pong { app_version: env!("CARGO_PKG_VERSION").to_owned() },
            Request::Quit => {
                self.control.quit();
                Response::Ok
            }
            Request::ListWorkflows => {
                Response::Workflows { workflows: list_workflows(&self.control.settings()) }
            }
            Request::Status => {
                let mut st = self.control.status();
                let load = self.sup.load();
                st.active_runs = load.active;
                st.queued_runs = load.queued;
                st.recording = self.sup.recording().status();
                Response::Status(st)
            }
            Request::Show { target } => {
                self.control.show(*target);
                Response::Ok
            }
            Request::ReloadSettings => {
                self.control.reload_settings();
                Response::Ok
            }
            Request::StopRecording => match self.sup.stop_recording() {
                Ok(()) => Response::Ok,
                Err(r) => error(&r),
            },
            Request::RecordingStatus => Response::Recording(self.sup.recording().status()),
            Request::CancelRun { run_id } => {
                if self.sup.cancel(*run_id) {
                    Response::Ok
                } else {
                    Response::error(
                        ErrorCode::NotRunning,
                        format!("run {run_id} is not running (it may have finished already)"),
                    )
                }
            }
            Request::WaitRun { run_id } => self.finished(*run_id),
            Request::PostFiles { paths, action, wait } => self.post_files(paths, action, *wait),
            Request::RunWorkflow { .. }
            | Request::Capture { .. }
            | Request::StartRecording(_)
            | Request::ToggleRecording(_) => self.run_job(request),
        }
    }

    fn post_files(&self, paths: &[PathBuf], action: &PostAction, wait: bool) -> Response {
        if paths.is_empty() {
            return Response::error(
                ErrorCode::InvalidRequest,
                "post_files needs at least one path",
            );
        }
        if let Some(p) = paths.iter().find(|p| !p.is_absolute()) {
            return Response::error(
                ErrorCode::InvalidRequest,
                format!("{} is not an absolute path; the sender must resolve it", p.display()),
            );
        }
        // Validate the action against the settings now, so a typo in a workflow name is an
        // error for the caller instead of a notification 400 ms later.
        if let Err(r) = crate::requests::workflow_for_files(&self.control.settings(), action) {
            return error(&r);
        }
        if !wait {
            let id = self.files.add(action, paths.to_vec(), None);
            return Response::Accepted { run_id: id };
        }
        let (tx, rx) = mpsc::channel();
        let _ = self.files.add(action, paths.to_vec(), Some(tx));
        match rx.recv_timeout(BATCH_WAIT) {
            Ok(Ok(summary)) => Response::Finished(summary),
            Ok(Err(r)) => error(&r),
            Err(_) => Response::error(ErrorCode::Internal, "the batch did not finish"),
        }
    }

    fn run_job(&self, request: &Request) -> Response {
        let settings = self.control.settings();
        let (job, wait) = match job_for_request(&settings, request) {
            Ok(Some(x)) => x,
            Ok(None) => return Response::error(ErrorCode::Internal, "not a run request"),
            Err(r) => return error(&r),
        };
        let recording_request =
            matches!(request, Request::StartRecording(_) | Request::ToggleRecording(_));
        match self.sup.submit(job) {
            Ok(sub) => {
                let run_id = sub.run_id();
                if wait {
                    return self.finished(run_id);
                }
                match sub {
                    Submitted::StoppedRecording { .. } => {
                        Response::Recording(self.sup.recording().status())
                    }
                    _ if recording_request => Response::Recording(self.sup.recording().status()),
                    _ => Response::Accepted { run_id },
                }
            }
            Err(r) => error(&r),
        }
    }
}

/// A summary for callers when the daemon refuses at the last moment (used in tests and by
/// the shutdown path).
pub fn shutting_down_summary(run_id: u64) -> RunSummary {
    plain_summary(run_id, Outcome::Cancelled, "ssx is shutting down")
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use ssx_core::ipc::{RecordSpec, RecordingStatus};

    use super::*;
    use crate::{
        clock::{FakeClock, SystemClock},
        daemon::{Job, JobRunner, JobSpec, Limits, RunOutput},
        events::CollectingUi,
        recording::RecordingController,
    };

    /// Finishes every run at once with a summary that names what was asked.
    struct Instant {
        seen: Mutex<Vec<String>>,
    }
    impl JobRunner for Instant {
        fn run(&self, job: &Job, _: &dyn UiSink) -> RunOutput {
            let msg = match &job.spec {
                JobSpec::Workflow { workflow, delay_ms, mode } => {
                    format!("workflow {} delay={delay_ms:?} mode={mode:?}", workflow.id)
                }
                JobSpec::Files { workflow, paths, edit_first } => {
                    format!("files {} n={} edit={edit_first}", workflow.id, paths.len())
                }
                JobSpec::Record { workflow, toggle, .. } => {
                    format!("record {} toggle={toggle}", workflow.id)
                }
            };
            self.seen.lock().unwrap().push(msg.clone());
            RunOutput { summary: plain_summary(job.run_id, Outcome::Success, msg), notified: false }
        }
    }

    #[derive(Default)]
    struct FakeControl {
        settings: Mutex<Settings>,
        quits: AtomicUsize,
        shown: Mutex<Vec<ShowTarget>>,
        reloads: AtomicUsize,
    }
    impl AppControl for FakeControl {
        fn settings(&self) -> Arc<Settings> {
            Arc::new(self.settings.lock().unwrap().clone())
        }
        fn quit(&self) {
            self.quits.fetch_add(1, Ordering::SeqCst);
        }
        fn show(&self, t: ShowTarget) {
            self.shown.lock().unwrap().push(t);
        }
        fn reload_settings(&self) {
            self.reloads.fetch_add(1, Ordering::SeqCst);
        }
        fn status(&self) -> DaemonStatus {
            DaemonStatus {
                app_version: "test".into(),
                pid: 1,
                uptime_secs: 2,
                tray: false,
                hotkey_backend: "none".into(),
                hotkeys_registered: 0,
                hotkey_problems: vec![],
                active_runs: vec![],
                queued_runs: 0,
                recording: RecordingStatus::default(),
                config_dir: "/c".into(),
                settings_problem: None,
            }
        }
    }

    struct Rig {
        h: IpcHandler,
        runner: Arc<Instant>,
        control: Arc<FakeControl>,
        ui: Arc<CollectingUi>,
    }

    fn rig(window_ms: u64) -> Rig {
        let clock = Arc::new(SystemClock::new());
        let ui = Arc::new(CollectingUi::new());
        let controller = Arc::new(RecordingController::new(Arc::new(FakeClock::new()), ui.clone()));
        let runner = Arc::new(Instant { seen: Mutex::default() });
        let ids = RunIds::new();
        let sup = Supervisor::new(
            runner.clone(),
            ui.clone(),
            clock.clone(),
            ids.clone(),
            controller,
            Limits::default(),
        );
        let control = Arc::new(FakeControl::default());
        let files = Arc::new(files_coalescer(
            CoalesceConfig {
                window: Duration::from_millis(window_ms),
                max_wait: Duration::from_millis(window_ms * 5),
                max_paths: 1000,
            },
            ids,
            clock,
            sup.clone(),
            control.clone(),
            ui.clone(),
        ));
        Rig { h: IpcHandler::new(sup, files, control.clone()), runner, control, ui }
    }

    fn req(seq: u64, r: &Request) -> String {
        encode_line(&RequestEnvelope::new(seq, r.clone())).unwrap().trim_end().to_owned()
    }

    fn ask(rig: &Rig, r: &Request) -> Response {
        let line = rig.h.handle_line(&req(9, r));
        let env: ResponseEnvelope = decode_line(&line).unwrap();
        assert_eq!(env.seq, 9, "the correlation number is echoed");
        env.response
    }

    #[test]
    fn ping_status_list_show_reload_quit() {
        let r = rig(50);
        assert!(matches!(ask(&r, &Request::Ping), Response::Pong { .. }));
        let Response::Workflows { workflows } = ask(&r, &Request::ListWorkflows) else { panic!() };
        assert!(workflows.iter().any(|w| w.id == "capture-region"));
        let Response::Status(st) = ask(&r, &Request::Status) else { panic!() };
        assert_eq!((st.pid, st.config_dir.as_str()), (1, "/c"));
        assert!(!st.recording.active && st.active_runs.is_empty());
        assert_eq!(ask(&r, &Request::Show { target: ShowTarget::History }), Response::Ok);
        assert_eq!(*r.control.shown.lock().unwrap(), [ShowTarget::History]);
        assert_eq!(ask(&r, &Request::ReloadSettings), Response::Ok);
        assert_eq!(r.control.reloads.load(Ordering::SeqCst), 1);
        assert_eq!(ask(&r, &Request::Quit), Response::Ok);
        assert_eq!(r.control.quits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn garbage_gets_an_error_response_never_a_hang_or_a_crash() {
        let r = rig(50);
        for bad in ["", "not json", r#"{"v":1,"type":"hologram"}"#, r#"{"type":"ping"}"#] {
            let line = r.h.handle_line(bad);
            let env: ResponseEnvelope = decode_line(&line).unwrap();
            assert!(
                matches!(env.response, Response::Error { code: ErrorCode::InvalidRequest, .. }),
                "{bad:?} -> {line}"
            );
        }
        let newer = r.h.handle_line(r#"{"v":99,"type":"ping"}"#);
        assert!(newer.contains("version_mismatch"), "{newer}");
    }

    #[test]
    fn run_workflow_and_capture_accept_or_wait() {
        let r = rig(50);
        let by_name = Request::RunWorkflow { id: None, name: Some("screen".into()), wait: false };
        let Response::Accepted { run_id } = ask(&r, &by_name) else { panic!() };
        let Response::Finished(s) = ask(&r, &Request::WaitRun { run_id }) else { panic!() };
        assert!(s.message.contains("capture-fullscreen"), "{}", s.message);

        let cap = Request::Capture {
            target: ssx_core::ipc::CaptureKind::Region,
            workflow: None,
            delay_ms: Some(10),
            wait: true,
            mode: Some(ssx_core::ipc::RegionMode::Window),
        };
        let Response::Finished(s) = ask(&r, &cap) else { panic!() };
        assert!(
            s.message.contains("delay=Some(10)") && s.message.contains("Window"),
            "{}",
            s.message
        );
        assert_eq!(s.outcome, Outcome::Success);
    }

    #[test]
    fn errors_carry_the_right_codes() {
        let r = rig(50);
        let unknown = Request::RunWorkflow { id: None, name: Some("zzz".into()), wait: false };
        assert!(matches!(
            ask(&r, &unknown),
            Response::Error { code: ErrorCode::UnknownWorkflow, .. }
        ));
        let files_wf =
            Request::RunWorkflow { id: Some("upload-files".into()), name: None, wait: false };
        assert!(matches!(
            ask(&r, &files_wf),
            Response::Error { code: ErrorCode::InvalidRequest, .. }
        ));
        assert!(matches!(
            ask(&r, &Request::CancelRun { run_id: 999 }),
            Response::Error { code: ErrorCode::NotRunning, .. }
        ));
        assert!(matches!(
            ask(&r, &Request::WaitRun { run_id: 999 }),
            Response::Error { code: ErrorCode::NotRunning, .. }
        ));
        assert!(matches!(
            ask(&r, &Request::StopRecording),
            Response::Error { code: ErrorCode::NotRunning, .. }
        ));
        assert_eq!(
            ask(&r, &Request::RecordingStatus),
            Response::Recording(RecordingStatus::default())
        );
    }

    #[test]
    fn post_files_within_the_window_run_as_one_batch_and_every_waiter_hears_the_result() {
        let r = Arc::new(rig(150));
        let paths = |n: &str| Request::PostFiles {
            paths: vec![PathBuf::from(format!("/tmp/{n}"))],
            action: PostAction::Upload,
            wait: true,
        };
        let handles: Vec<_> = ["a.png", "b.png", "c.png"]
            .into_iter()
            .map(|n| {
                let r = Arc::clone(&r);
                let line = req(1, &paths(n));
                std::thread::spawn(move || {
                    let env: ResponseEnvelope = decode_line(&r.h.handle_line(&line)).unwrap();
                    env.response
                })
            })
            .collect();
        let answers: Vec<Response> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut ids = Vec::new();
        for a in &answers {
            let Response::Finished(s) = a else { panic!("{a:?}") };
            assert!(s.message.contains("n=3"), "one run over three files: {}", s.message);
            ids.push(s.run_id);
        }
        ids.dedup();
        assert_eq!(ids.len(), 1, "all three callers got the same run");
        assert_eq!(r.runner.seen.lock().unwrap().len(), 1, "exactly one run happened");
    }

    #[test]
    fn a_non_waiting_post_files_answers_at_once_with_the_id_the_run_will_have() {
        let r = rig(80);
        let post = |n: &str| Request::PostFiles {
            paths: vec![PathBuf::from(format!("/tmp/{n}"))],
            action: PostAction::Edit,
            wait: false,
        };
        let Response::Accepted { run_id: a } = ask(&r, &post("a")) else { panic!() };
        let Response::Accepted { run_id: b } = ask(&r, &post("b")) else { panic!() };
        assert_eq!(a, b, "merged");
        let Response::Finished(s) = ask(&r, &Request::WaitRun { run_id: a }) else {
            // The batch may not have closed yet: wait for it.
            std::thread::sleep(Duration::from_millis(300));
            let Response::Finished(s) = ask(&r, &Request::WaitRun { run_id: a }) else { panic!() };
            assert!(s.message.contains("edit=true"));
            return;
        };
        assert!(s.message.contains("edit=true") && s.message.contains("n=2"), "{}", s.message);
    }

    #[test]
    fn post_files_validates_before_it_accepts() {
        let r = rig(50);
        let post = |paths: Vec<&str>, action: PostAction| Request::PostFiles {
            paths: paths.into_iter().map(PathBuf::from).collect(),
            action,
            wait: false,
        };
        assert!(matches!(
            ask(&r, &post(vec![], PostAction::Upload)),
            Response::Error { code: ErrorCode::InvalidRequest, .. }
        ));
        let rel = ask(&r, &post(vec!["relative.png"], PostAction::Upload));
        assert!(
            matches!(&rel, Response::Error { message, .. } if message.contains("absolute")),
            "{rel:?}"
        );
        assert!(matches!(
            ask(&r, &post(vec!["/a"], PostAction::Workflow { workflow: "zzz".into() })),
            Response::Error { code: ErrorCode::UnknownWorkflow, .. }
        ));
        assert!(r.runner.seen.lock().unwrap().is_empty(), "nothing was queued");
    }

    #[test]
    fn a_refused_batch_tells_the_person_and_the_waiters() {
        let r = rig(30);
        // Shut the supervisor down so every submission is refused.
        r.h.sup.shutdown(crate::daemon::ShutdownGrace::default());
        let post =
            Request::PostFiles { paths: vec!["/a".into()], action: PostAction::Upload, wait: true };
        let resp = ask(&r, &post);
        assert!(matches!(resp, Response::Error { code: ErrorCode::Busy, .. }), "{resp:?}");
        assert!(r.ui.events().iter().any(|e| matches!(e, UiEvent::Notice { .. })));
    }

    #[test]
    fn recording_requests_answer_with_the_recording_status() {
        let r = rig(50);
        // The instant runner never marks the recording started, so it stays "selecting" only
        // until the run ends; what matters here is the response type and the wait variant.
        let start = Request::StartRecording(RecordSpec { wait: true, ..RecordSpec::default() });
        let resp = ask(&r, &start);
        let Response::Finished(s) = resp else { panic!("{resp:?}") };
        assert!(s.message.contains("record record-screen toggle=false"), "{}", s.message);
        let toggle = Request::ToggleRecording(RecordSpec { gif: true, ..RecordSpec::default() });
        assert!(matches!(ask(&r, &toggle), Response::Recording(_)));
    }

    #[test]
    fn responses_are_single_lines() {
        let r = rig(50);
        let line = r.h.handle_line(&req(3, &Request::ListWorkflows));
        assert!(!line.contains('\n') && !line.is_empty());
        assert!(shutting_down_summary(4).message.contains("shutting down"));
    }
}
