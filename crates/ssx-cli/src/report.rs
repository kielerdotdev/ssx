//! Turns a workflow [`RunReport`] into printable, serialisable data and an exit code.
//!
//! Conventions shared by `capture`, `edit`, `upload`, `post-file` and `run`:
//!
//! * **stdout carries the result**: the URLs when anything was uploaded (one per line, in
//!   input order), otherwise the saved file paths. That makes `ssx capture ... --upload | xclip`
//!   and `url=$(ssx upload a.png)` work. Everything else (which file was saved, warnings,
//!   errors) goes to stderr.
//! * `--json` prints one JSON document with everything instead.
//! * The exit code is 0 only when everything asked for worked; a workflow that saved but could
//!   not upload is a failure (1), and a cancelled one is 3.

use std::path::PathBuf;

use serde::Serialize;
use ssx_core::workflow::{Outcome, RunReport, StepKind, StepStatus};

use crate::{
    error::{CliError, CliResult, ExitCode},
    output::{Style, err_line, out_line},
};

/// One item (file, screenshot) of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResult {
    /// The file the caller passed in (`post-file`, `upload`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<PathBuf>,
    /// The file that exists locally for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Public URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Shortened URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_url: Option<String>,
    /// Thumbnail URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    /// Deletion URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_url: Option<String>,
    /// Uploader that handled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uploader: Option<String>,
    /// History entry id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_id: Option<i64>,
    /// `success`, `partial_success`, `failed` or `cancelled`.
    pub outcome: Outcome,
    /// What went wrong for this item, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunResult {
    /// Workflow id.
    pub workflow: String,
    /// Overall outcome.
    pub outcome: Outcome,
    /// One-paragraph summary.
    pub message: String,
    /// The items, in input order.
    pub items: Vec<ItemResult>,
    /// Run-level errors (input could not be acquired, ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Failures of optional steps (clipboard, notification, browser).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn describe(step: &ssx_core::workflow::StepReport) -> Option<String> {
    match &step.status {
        StepStatus::Failed(f) => Some(format!("{}: {}", step.kind, f.message)),
        _ => None,
    }
}

impl RunResult {
    /// Builds the result from the engine's report.
    ///
    /// `explicit` lists optional steps the user asked for on the command line (`--copy`):
    /// when one of them fails it is an error, not just a warning.
    pub fn from_report(report: &RunReport, explicit: &[StepKind]) -> Self {
        let mut errors: Vec<String> = report.steps.iter().filter_map(describe).collect();
        let mut warnings = Vec::new();
        let items: Vec<ItemResult> = report
            .items
            .iter()
            .map(|i| {
                let mut error: Option<String> = None;
                for step in &i.steps {
                    let Some(text) = describe(step) else { continue };
                    let optional = step.kind.importance() == ssx_core::workflow::Importance::Optional;
                    if optional && !explicit.contains(&step.kind) {
                        warnings.push(text);
                    } else {
                        match &mut error {
                            Some(e) => {
                                e.push_str("; ");
                                e.push_str(&text);
                            }
                            None => error = Some(text),
                        }
                    }
                }
                ItemResult {
                    input: i.input_path.clone(),
                    path: i.local_path.clone(),
                    url: i.url.clone(),
                    short_url: i.short_url.clone(),
                    thumbnail_url: i.thumbnail_url.clone(),
                    deletion_url: i.deletion_url.clone(),
                    uploader: i.uploader.clone(),
                    history_id: i.history_id,
                    outcome: i.outcome,
                    error,
                }
            })
            .collect();
        // Run-level optional failures (the joined "copy all URLs" step) are warnings too.
        for step in &report.steps {
            if step.kind.importance() == ssx_core::workflow::Importance::Optional
                && let Some(text) = describe(step)
            {
                errors.retain(|e| *e != text);
                if explicit.contains(&step.kind) {
                    errors.push(text);
                } else {
                    warnings.push(text);
                }
            }
        }
        let outcome = if outcome_downgraded(report.outcome, &items, &errors) {
            Outcome::PartialSuccess
        } else {
            report.outcome
        };
        Self {
            workflow: report.workflow_id.clone(),
            outcome,
            message: report.summary(),
            items,
            errors,
            warnings,
        }
    }

    /// The result of a plain "saved this file" command with no engine run.
    pub fn saved(workflow: &str, path: PathBuf) -> Self {
        Self {
            workflow: workflow.to_owned(),
            outcome: Outcome::Success,
            message: "Done".to_owned(),
            items: vec![ItemResult {
                input: None,
                path: Some(path),
                url: None,
                short_url: None,
                thumbnail_url: None,
                deletion_url: None,
                uploader: None,
                history_id: None,
                outcome: Outcome::Success,
                error: None,
            }],
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Records a failure that happened outside the engine (a requested clipboard copy).
    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
        if self.outcome == Outcome::Success {
            self.outcome = Outcome::PartialSuccess;
        }
        self.message = format!("{}; {}", self.message, self.errors.join("; "));
    }

    /// URLs to print: short URL if there is one, in input order.
    pub fn urls(&self) -> Vec<&str> {
        self.items.iter().filter_map(|i| i.short_url.as_deref().or(i.url.as_deref())).collect()
    }

    /// Local paths to print.
    pub fn paths(&self) -> Vec<&std::path::Path> {
        self.items.iter().filter_map(|i| i.path.as_deref()).collect()
    }

    /// Exit code for this result.
    pub fn exit_code(&self) -> ExitCode {
        match self.outcome {
            Outcome::Success => ExitCode::Ok,
            Outcome::Cancelled => ExitCode::Cancelled,
            Outcome::PartialSuccess | Outcome::Failed => ExitCode::Error,
        }
    }
}

/// A `Success` run whose explicitly requested optional steps failed is no longer a success.
fn outcome_downgraded(outcome: Outcome, items: &[ItemResult], errors: &[String]) -> bool {
    outcome == Outcome::Success && (!errors.is_empty() || items.iter().any(|i| i.error.is_some()))
}

/// Prints the result (`json` → JSON on stdout; otherwise URLs or paths on stdout and the
/// details on stderr) and returns the process result.
pub fn print_result(result: &RunResult, json: bool, quiet: bool, err_style: Style) -> CliResult<()> {
    if json {
        out_line(&serde_json::to_string_pretty(result)?);
    } else {
        let urls = result.urls();
        if urls.is_empty() {
            for p in result.paths() {
                out_line(&p.display().to_string());
            }
        } else {
            for u in urls {
                out_line(u);
            }
            if !quiet {
                for p in result.paths() {
                    err_line(&format!("saved: {}", p.display()));
                }
            }
        }
    }
    for w in &result.warnings {
        err_line(&format!("{} {w}", err_style.yellow("warning:")));
    }
    let failed = result.items.iter().filter_map(|i| i.error.as_deref());
    for e in result.errors.iter().map(String::as_str).chain(failed) {
        err_line(&format!("{} {e}", err_style.red("error:")));
    }
    match result.exit_code() {
        ExitCode::Ok => Ok(()),
        ExitCode::Cancelled => Err(CliError::cancelled()),
        _ => Err(CliError::new(result.message.clone())
            .hint("the details are above; `ssx history list` shows what was recorded")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ssx_core::{
        history::EntryKind,
        workflow::{
            FailureKind, ItemReport, SkipReason, StepFailure, StepReport, StepStatus,
        },
    };

    use super::*;

    fn step(kind: StepKind, status: StepStatus) -> StepReport {
        StepReport { kind, item: Some(0), status, detail: None, duration: Duration::ZERO }
    }

    fn failed(kind: StepKind, msg: &str) -> StepReport {
        step(
            kind,
            StepStatus::Failed(StepFailure {
                kind: FailureKind::Service,
                message: msg.into(),
                retryable: false,
            }),
        )
    }

    fn item(url: Option<&str>, steps: Vec<StepReport>, outcome: Outcome) -> ItemReport {
        ItemReport {
            index: 0,
            kind: EntryKind::Image,
            input_path: None,
            local_path: Some(PathBuf::from("/tmp/a.png")),
            created_by_workflow: true,
            url: url.map(str::to_owned),
            short_url: None,
            thumbnail_url: None,
            deletion_url: None,
            uploader: Some("local".into()),
            history_id: Some(7),
            steps,
            outcome,
        }
    }

    fn report(outcome: Outcome, items: Vec<ItemReport>, steps: Vec<StepReport>) -> RunReport {
        RunReport { workflow_id: "wf".into(), outcome, items, steps }
    }

    #[test]
    fn a_successful_upload_prints_urls_and_exits_zero() {
        let r = report(
            Outcome::Success,
            vec![item(Some("https://x/1"), vec![step(StepKind::Upload, StepStatus::Succeeded)], Outcome::Success)],
            vec![],
        );
        let res = RunResult::from_report(&r, &[]);
        assert_eq!(res.urls(), ["https://x/1"]);
        assert_eq!(res.exit_code(), ExitCode::Ok);
        assert!(res.errors.is_empty() && res.warnings.is_empty());
        let json = serde_json::to_value(&res).unwrap();
        assert_eq!(json["outcome"], "success");
        assert_eq!(json["items"][0]["url"], "https://x/1");
        assert_eq!(json["items"][0]["history_id"], 7);
        assert!(json.get("errors").is_none(), "empty lists are omitted");
    }

    #[test]
    fn optional_failures_are_warnings_unless_the_user_asked_for_them() {
        let steps = vec![failed(StepKind::CopyImage, "no clipboard")];
        let r = report(Outcome::Success, vec![item(None, steps, Outcome::Success)], vec![]);

        let lenient = RunResult::from_report(&r, &[]);
        assert_eq!(lenient.warnings, ["copy_image_to_clipboard: no clipboard"]);
        assert_eq!(lenient.exit_code(), ExitCode::Ok);

        let strict = RunResult::from_report(&r, &[StepKind::CopyImage]);
        assert!(strict.warnings.is_empty());
        assert_eq!(strict.items[0].error.as_deref(), Some("copy_image_to_clipboard: no clipboard"));
        assert_eq!(strict.outcome, Outcome::PartialSuccess, "an explicit --copy that failed is not a success");
        assert_eq!(strict.exit_code(), ExitCode::Error);
    }

    #[test]
    fn failed_uploads_and_run_level_errors_fail_the_run() {
        let steps = vec![failed(StepKind::Upload, "HTTP 500"), step(StepKind::CopyUrl, StepStatus::Skipped(SkipReason::NoUrl))];
        let r = report(
            Outcome::PartialSuccess,
            vec![item(None, steps, Outcome::PartialSuccess)],
            vec![failed(StepKind::LoadFile, "no files were given")],
        );
        let res = RunResult::from_report(&r, &[]);
        assert_eq!(res.exit_code(), ExitCode::Error);
        assert_eq!(res.items[0].error.as_deref(), Some("upload: HTTP 500"));
        assert_eq!(res.errors, ["load_file: no files were given"]);
        assert!(res.urls().is_empty());
        assert_eq!(res.paths(), [std::path::Path::new("/tmp/a.png")]);
    }

    #[test]
    fn cancelled_runs_exit_three() {
        let res = RunResult::from_report(&report(Outcome::Cancelled, vec![], vec![]), &[]);
        assert_eq!(res.exit_code(), ExitCode::Cancelled);
        let e = print_result(&res, true, true, Style::plain()).unwrap_err();
        assert_eq!(e.code, ExitCode::Cancelled);
    }

    #[test]
    fn short_urls_win_and_order_is_preserved() {
        let mut a = item(Some("https://long/1"), vec![], Outcome::Success);
        a.short_url = Some("https://s/1".into());
        let b = item(Some("https://long/2"), vec![], Outcome::Success);
        let res = RunResult::from_report(&report(Outcome::Success, vec![a, b], vec![]), &[]);
        assert_eq!(res.urls(), ["https://s/1", "https://long/2"]);
    }
}
