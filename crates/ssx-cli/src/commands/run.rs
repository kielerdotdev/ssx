//! `ssx run WORKFLOW`: executes a workflow from `settings.toml`.
//!
//! When the ssx app is running the workflow is *handed to it* (`RunWorkflow` over IPC): that is
//! what a compositor keybinding running `ssx run region` wants, because the app owns the
//! "one interactive capture at a time" rule, the tray state and the notifications; Ctrl-C here
//! cancels the run there. Without the app (or with `SSX_NO_DAEMON=1`, or a `--delay`, which the
//! IPC request cannot carry) the same engine runs in this process and prints its events as
//! progress lines.
//!
//! Recording workflows run in-process too: Ctrl-C *stops* the recording (the file is kept and
//! the workflow carries on with the upload), a second Ctrl-C cancels it.

use ssx_core::{
    ipc::Request,
    settings::{InputKind, Settings, Workflow},
    workflow::{CancelToken, VideoSource},
};

use crate::{
    app::{App, Session},
    cli::RunArgs,
    error::{CliError, CliResult},
    forward::{Daemon, run_remote},
    progress::{ProgressLines, Verbosity},
    report::{RunResult, print_result},
};

/// Looks the workflow up (id, then CLI name, then name) with a helpful error.
pub fn find_workflow<'a>(settings: &'a Settings, name: &str) -> CliResult<&'a Workflow> {
    settings.find_workflow(name).ok_or_else(|| {
        let known: Vec<String> = settings
            .workflows
            .iter()
            .map(|w| match &w.trigger.cli_name {
                Some(c) if *c != w.id => format!("{c} ({})", w.id),
                _ => w.id.clone(),
            })
            .collect();
        CliError::new(format!("there is no workflow called {name:?}"))
            .hint(format!("available workflows: {}", known.join(", ")))
    })
}

/// Why a workflow cannot be started with `ssx run`, if it cannot.
pub fn run_blocker(wf: &Workflow) -> Option<CliError> {
    match wf.input {
        InputKind::Files => Some(
            CliError::usage(format!("workflow {:?} takes files", wf.id))
                .hint(format!("run it with `ssx post-file --workflow {} PATH...`", wf.id)),
        ),
        _ => None,
    }
}

/// Hands the workflow to the running ssx app and prints its result.
fn run_via_app(app: &App, daemon: &Daemon, wf: &Workflow, json: bool) -> CliResult<()> {
    let request = Request::RunWorkflow { id: Some(wf.id.clone()), name: None, wait: false };
    let summary = run_remote(daemon, request, &app.cancel)?;
    let result = RunResult::from_summary(&wf.id, &summary);
    print_result(&result, json, app.global.quiet, app.err)
}

/// `ssx run`.
pub fn run(app: &App, args: RunArgs) -> CliResult<()> {
    let mut settings = app.load_settings()?;
    let wf = find_workflow(&settings, &args.workflow)?.clone();
    if let Some(blocker) = run_blocker(&wf) {
        return Err(blocker);
    }
    if args.delay.is_none()
        && let Some(daemon) = Daemon::connect()
    {
        tracing::info!("handing the workflow to the running ssx app");
        return run_via_app(app, &daemon, &wf, args.json);
    }
    if let Some(delay) = args.delay {
        settings.capture.delay_ms = delay;
    }
    let session = Session::new(app, settings)?;
    let sink =
        ProgressLines::new(Verbosity::from_flags(app.global.quiet, app.global.verbose), app.err, 1);
    let report = if wf.input.is_recording() {
        // Ctrl-C ends the recording gracefully; the run then continues (upload, ...).
        let stop = CancelToken::new();
        app.stop_on_first_interrupt(stop.clone());
        if !app.global.quiet {
            crate::output::err_line("recording: press Ctrl-C to stop (a second Ctrl-C cancels)");
        }
        session.engine.post_video(
            &wf,
            VideoSource::Record { stop },
            &session.bundle(),
            &sink,
            &app.cancel,
        )
    } else {
        session.engine.post_screenshot(&wf, &session.bundle(), &sink, &app.cancel)
    };
    print_result(&RunResult::from_report(&report, &[]), args.json, app.global.quiet, app.err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflows_are_found_by_id_cli_name_or_name() {
        let s = Settings::default();
        assert_eq!(find_workflow(&s, "capture-region").unwrap().id, "capture-region");
        assert_eq!(find_workflow(&s, "region").unwrap().id, "capture-region");
        assert_eq!(
            find_workflow(&s, "CAPTURE ACTIVE WINDOW, SAVE AND UPLOAD").unwrap().id,
            "capture-window"
        );
        let e = find_workflow(&s, "nope").unwrap_err();
        let hint = e.hint.unwrap();
        assert!(hint.contains("region (capture-region)") && hint.contains("screen"), "{hint}");
    }

    #[test]
    fn workflows_that_cannot_run_here_say_what_to_do() {
        let s = Settings::default();
        let e = run_blocker(s.workflow_by_id("upload-files").unwrap()).unwrap();
        assert_eq!(e.code, crate::error::ExitCode::Usage);
        assert!(e.hint.unwrap().contains("ssx post-file --workflow upload-files"));
        assert!(
            run_blocker(s.workflow_by_id("record-screen").unwrap()).is_none(),
            "recording workflows run now"
        );
        assert!(run_blocker(s.workflow_by_id("capture-region").unwrap()).is_none());
        assert!(run_blocker(s.workflow_by_id("upload-clipboard").unwrap()).is_none());
    }
}
