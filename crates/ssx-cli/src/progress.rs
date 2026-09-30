//! Renders workflow engine events as progress lines on **stderr**.
//!
//! The engine may call the sink from several threads (parallel uploads), so lines are
//! written whole under a lock and never interleave. Byte progress is throttled to every 25 %
//! so a log file or CI output stays readable; there is no cursor trickery, the output is the
//! same on a terminal and in a pipe.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use ssx_core::workflow::{Event, EventSink, Outcome, StepKind, StepStatus};

use crate::output::{Style, err_line, human_bytes};

/// How chatty the progress output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    /// Nothing.
    Quiet,
    /// Slow steps (capture, edit, upload) and failures.
    Normal,
    /// Every step, including skipped ones.
    Verbose,
}

impl Verbosity {
    /// From the `-q` / `-v` flags.
    pub fn from_flags(quiet: bool, verbose: u8) -> Self {
        if quiet {
            Self::Quiet
        } else if verbose > 0 {
            Self::Verbose
        } else {
            Self::Normal
        }
    }
}

/// The sink.
#[derive(Debug)]
pub struct ProgressLines {
    verbosity: Verbosity,
    style: Style,
    last_pct: Mutex<HashMap<(Option<usize>, StepKind), u64>>,
}

/// Steps worth announcing while they run.
fn is_slow(step: StepKind) -> bool {
    matches!(
        step,
        StepKind::Capture
            | StepKind::Record
            | StepKind::OpenEditor
            | StepKind::Zip
            | StepKind::Upload
            | StepKind::ShortenUrl
            | StepKind::RunCommand
    )
}

fn label(item: Option<usize>, step: StepKind) -> String {
    match item {
        Some(i) => format!("[{}] {}", i + 1, step.name()),
        None => step.name().to_owned(),
    }
}

/// The line for a finished step, or `None` if it is not worth a line.
pub fn finished_line(
    item: Option<usize>,
    step: StepKind,
    status: &StepStatus,
    duration: Duration,
    verbosity: Verbosity,
    style: Style,
) -> Option<String> {
    let what = label(item, step);
    let secs = duration.as_secs_f64();
    match status {
        StepStatus::Succeeded if is_slow(step) || verbosity == Verbosity::Verbose => {
            Some(format!("  {} {what} ({secs:.2}s)", style.green("ok")))
        }
        StepStatus::Succeeded => None,
        StepStatus::Failed(f) => Some(format!("  {} {what}: {}", style.red("failed"), f.message)),
        StepStatus::Skipped(reason) if verbosity == Verbosity::Verbose => {
            Some(format!("  {} {what}: {reason}", style.dim("skipped")))
        }
        StepStatus::Skipped(_) => None,
        StepStatus::Cancelled => Some(format!("  {} {what}", style.yellow("cancelled"))),
    }
}

impl ProgressLines {
    /// A sink printing at `verbosity` with `style` (stderr styling).
    pub fn new(verbosity: Verbosity, style: Style) -> Self {
        Self { verbosity, style, last_pct: Mutex::default() }
    }
}

impl EventSink for ProgressLines {
    fn event(&self, event: Event) {
        if self.verbosity == Verbosity::Quiet {
            return;
        }
        match event {
            Event::RunStarted { workflow_name, .. } if self.verbosity == Verbosity::Verbose => {
                err_line(&format!("running workflow {workflow_name:?}"));
            }
            Event::StepStarted { item, step } if is_slow(step) => {
                err_line(&format!("  {}...", label(item, step)));
            }
            Event::StepProgress { item, step, done, total } => {
                let Some(total) = total.filter(|t| *t > 0) else { return };
                let pct = (done.min(total) * 100 / total) / 25 * 25;
                let mut seen = self.last_pct.lock().unwrap_or_else(PoisonError::into_inner);
                let last = seen.entry((item, step)).or_insert(0);
                if pct > *last {
                    *last = pct;
                    err_line(&format!(
                        "  {} {pct}% ({} of {})",
                        label(item, step),
                        human_bytes(done),
                        human_bytes(total)
                    ));
                }
            }
            Event::StepFinished { item, step, status, duration } => {
                if let Some(line) =
                    finished_line(item, step, &status, duration, self.verbosity, self.style)
                {
                    err_line(&line);
                }
            }
            Event::RunFinished { outcome } if self.verbosity == Verbosity::Verbose => {
                let text = match outcome {
                    Outcome::Success => "finished",
                    Outcome::PartialSuccess => "finished with problems",
                    Outcome::Failed => "failed",
                    Outcome::Cancelled => "cancelled",
                };
                err_line(text);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::workflow::{FailureKind, SkipReason, StepFailure};

    use super::*;

    const S: Style = Style::plain();
    const D: Duration = Duration::from_millis(310);

    #[test]
    fn slow_steps_and_failures_get_lines_quick_ones_do_not() {
        let ok = StepStatus::Succeeded;
        assert_eq!(
            finished_line(None, StepKind::Capture, &ok, D, Verbosity::Normal, S).unwrap(),
            "  ok capture (0.31s)"
        );
        assert_eq!(
            finished_line(Some(1), StepKind::Upload, &ok, D, Verbosity::Normal, S).unwrap(),
            "  ok [2] upload (0.31s)"
        );
        assert!(finished_line(None, StepKind::CopyImage, &ok, D, Verbosity::Normal, S).is_none());
        assert!(finished_line(None, StepKind::CopyImage, &ok, D, Verbosity::Verbose, S).is_some());

        let failed = StepStatus::Failed(StepFailure {
            kind: FailureKind::Service,
            message: "HTTP 500".into(),
            retryable: true,
        });
        assert_eq!(
            finished_line(None, StepKind::CopyImage, &failed, D, Verbosity::Normal, S).unwrap(),
            "  failed copy_image_to_clipboard: HTTP 500",
            "failures are always shown"
        );
    }

    #[test]
    fn skipped_steps_only_in_verbose() {
        let skipped = StepStatus::Skipped(SkipReason::NoUrl);
        assert!(finished_line(None, StepKind::CopyUrl, &skipped, D, Verbosity::Normal, S).is_none());
        let l = finished_line(None, StepKind::CopyUrl, &skipped, D, Verbosity::Verbose, S).unwrap();
        assert!(l.contains("skipped") && l.contains("copy_url"), "{l}");
    }

    #[test]
    fn verbosity_from_flags() {
        assert_eq!(Verbosity::from_flags(true, 3), Verbosity::Quiet);
        assert_eq!(Verbosity::from_flags(false, 0), Verbosity::Normal);
        assert_eq!(Verbosity::from_flags(false, 1), Verbosity::Verbose);
    }
}
