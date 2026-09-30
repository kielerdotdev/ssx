//! `ssx run WORKFLOW`: executes a workflow from `settings.toml` with the same engine the
//! tray app uses, printing engine events as progress lines.

use ssx_core::settings::{InputKind, Settings, Workflow};

use crate::{
    app::{App, Session},
    cli::RunArgs,
    error::{CliError, CliResult},
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
        InputKind::RecordScreen | InputKind::RecordGif => Some(
            CliError::new(format!(
                "workflow {:?} records the screen, which is not available yet",
                wf.id
            ))
            .hint("recording arrives with the ssx-record crate"),
        ),
        _ => None,
    }
}

/// `ssx run`.
pub fn run(app: &App, args: RunArgs) -> CliResult<()> {
    let mut settings = app.load_settings()?;
    let wf = find_workflow(&settings, &args.workflow)?.clone();
    if let Some(blocker) = run_blocker(&wf) {
        return Err(blocker);
    }
    if let Some(delay) = args.delay {
        settings.capture.delay_ms = delay;
    }
    let session = Session::new(app, settings)?;
    let sink =
        ProgressLines::new(Verbosity::from_flags(app.global.quiet, app.global.verbose), app.err, 1);
    let report = session.engine.post_screenshot(&wf, &session.bundle(), &sink, &app.cancel);
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
        let e = run_blocker(s.workflow_by_id("record-screen").unwrap()).unwrap();
        assert!(e.message.contains("not available yet"));
        assert!(run_blocker(s.workflow_by_id("capture-region").unwrap()).is_none());
        assert!(run_blocker(s.workflow_by_id("upload-clipboard").unwrap()).is_none());
    }
}
