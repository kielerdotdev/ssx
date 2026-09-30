//! The `ssx` command-line tool.
//!
//! The binary (`src/main.rs`) is a few lines: parse, [`run`], exit. Everything else is here so
//! it can be unit-tested: the clap grammar ([`cli`]), one module per command family
//! ([`commands`]) that returns data and renders it with pure functions, and the plumbing
//! ([`app`], [`report`], [`progress`], [`flows`], [`settings_edit`], [`forward`]). Real work is
//! done by `ssx-services` and `ssx-core`; this crate only wires arguments to them.
//!
//! # Exit codes
//!
//! | code | meaning |
//! |---|---|
//! | 0 | success |
//! | 1 | error (also a workflow that only partly succeeded, e.g. saved but not uploaded) |
//! | 2 | usage error |
//! | 3 | cancelled (Ctrl-C, closed the editor) |
//!
//! Errors print as `error: <what> (hint: <what to do>)` on stderr.
//!
//! # Logging
//!
//! `-v` / `-vv` / `-vvv` raise the level of ssx's own logs (info, debug, trace); `RUST_LOG`
//! overrides. Logs go to stderr, results to stdout.

#![forbid(unsafe_code)]

pub mod app;
pub mod autostart;
pub mod cli;
pub mod commands;
pub mod error;
pub mod flows;
pub mod forward;
pub mod output;
pub mod progress;
pub mod report;
pub mod settings_edit;

use ssx_core::workflow::CancelToken;
use tracing_subscriber::EnvFilter;

use crate::{
    app::App,
    cli::{Cli, GlobalArgs},
    error::{CliError, ExitCode},
    output::{Style, err_line},
};

/// The `EnvFilter` directive for the `-v` / `-q` flags: ssx's own crates at the chosen level,
/// everything else one notch quieter.
pub fn log_directive(quiet: bool, verbose: u8) -> String {
    let (ours, deps) = match (quiet, verbose) {
        (true, _) => ("error", "error"),
        (false, 0) => ("warn", "error"),
        (false, 1) => ("info", "warn"),
        (false, 2) => ("debug", "info"),
        (false, _) => ("trace", "debug"),
    };
    format!(
        "{deps},ssx={ours},ssx_cli={ours},ssx_core={ours},ssx_services={ours},ssx_upload={ours},\
         ssx_platform={ours},ssx_capture={ours},ssx_capture_x11={ours},ssx_capture_wayland={ours},\
         ssx_capture_portal={ours},ssx_hdr={ours},ssx_shell={ours},ssx_hotkeys={ours},ssx_ipc={ours},\
         ssx_overlay={ours},ssx_record={ours}"
    )
}

fn init_logging(global: &GlobalArgs) {
    // RUST_LOG wins when set and non-empty, like every other Rust CLI.
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(log_directive(global.quiet, global.verbose)));
    let ansi = Style::for_stream(global.color, true) == Style::colored();
    // A second initialisation (tests calling `run` twice) is harmless: ignore the error.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(false)
        .try_init();
}

/// Runs a parsed command line and returns the process exit code.
pub fn run(cli: Cli) -> i32 {
    init_logging(&cli.global);
    let cancel = CancelToken::new();
    let first_interrupt: std::sync::Arc<std::sync::Mutex<Option<CancelToken>>> =
        std::sync::Arc::default();
    {
        let c = cancel.clone();
        let first = std::sync::Arc::clone(&first_interrupt);
        // The first Ctrl-C cancels the command (or, while a recording runs, stops it and keeps
        // the file: the command registers that token in `first_interrupt`). A further Ctrl-C
        // while cancelling forces the exit (some steps cannot be interrupted).
        let hits = std::sync::atomic::AtomicU32::new(0);
        let _ = ctrlc::set_handler(move || {
            let n = hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let stop = first.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            match (n, stop) {
                (0, Some(t)) => t.cancel(),
                (0, None) | (1, Some(_)) => c.cancel(),
                _ => std::process::exit(ExitCode::Cancelled.code()),
            }
        });
    }
    let style = Style::for_stream(cli.global.color, true);
    let result = App::new(cli.global, cancel).and_then(|mut app| {
        app.first_interrupt = first_interrupt;
        commands::dispatch(&app, cli.command)
    });
    match result {
        Ok(()) => ExitCode::Ok.code(),
        Err(e) => {
            report_error(&e, style);
            e.code.code()
        }
    }
}

/// Prints `error: ... (hint: ...)` on stderr. A cancelled run prints nothing extra.
pub fn report_error(e: &CliError, style: Style) {
    if e.code == ExitCode::Cancelled {
        err_line(&style.yellow("cancelled"));
        return;
    }
    let mut line = format!("{} {}", style.red("error:"), e.message);
    if let Some(h) = &e.hint {
        line.push(' ');
        line.push_str(&style.dim(&format!("(hint: {h})")));
    }
    err_line(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_follow_the_flags() {
        assert!(log_directive(false, 0).contains("ssx_services=warn"));
        assert!(log_directive(false, 1).contains("ssx_services=info"));
        assert!(log_directive(false, 2).starts_with("info,"));
        assert!(log_directive(false, 3).contains("ssx_core=trace"));
        assert!(log_directive(true, 3).contains("ssx_cli=error"));
        for v in 0..5 {
            EnvFilter::try_new(log_directive(false, v)).expect("valid directive");
        }
    }
}
