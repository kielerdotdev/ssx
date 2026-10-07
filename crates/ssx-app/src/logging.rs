//! Logging and the panic hook.
//!
//! * A **rotating log file** in `<data dir>/logs` (`ssx-app.log.YYYY-MM-DD`, the last seven
//!   days kept) at `info` for ssx's own crates, through a non-blocking writer so a slow disk
//!   never stalls a hotkey. `RUST_LOG` overrides the filter.
//! * The **terminal** gets output only with `--foreground` or `-v` (a tray app started from a
//!   launcher has no terminal to speak of); the level follows the number of `-v`.
//! * The **panic hook** writes the panic, location and a backtrace to the log and shows a
//!   notification, then lets the default behaviour continue: in a release build with
//!   `panic = "abort"` that ends the process (an in-flight recording is fragmented MP4 for
//!   exactly that reason); in other profiles the panic unwinds and the supervisor turns it into a
//!   failed run.

use std::{path::Path, sync::Arc};

use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{Builder, Rotation},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{cli::Args, notify::DaemonNotifier};

/// Keeps the non-blocking writer alive; dropping it flushes the log.
#[derive(Debug)]
pub struct LogGuard {
    #[allow(dead_code)] // held only to flush the log on drop
    file: Option<WorkerGuard>,
    /// Where the log file lives.
    pub dir: std::path::PathBuf,
}

/// The crates whose logs are ssx's own.
const OURS: &[&str] = &[
    "ssx",
    "ssx_app",
    "ssx_cli",
    "ssx_core",
    "ssx_services",
    "ssx_upload",
    "ssx_platform",
    "ssx_capture",
    "ssx_capture_x11",
    "ssx_capture_wayland",
    "ssx_capture_portal",
    "ssx_hdr",
    "ssx_hotkeys",
    "ssx_ipc",
    "ssx_overlay",
    "ssx_record",
];

/// The filter directive with ssx's crates at `ours` and everything else at `deps`.
pub fn directive(ours: &str, deps: &str) -> String {
    let mut d = deps.to_owned();
    for c in OURS {
        d.push(',');
        d.push_str(c);
        d.push('=');
        d.push_str(ours);
    }
    d
}

/// The terminal level for `-v` counts (0 = `--foreground` alone).
pub fn terminal_directive(verbose: u8) -> String {
    match verbose {
        0 => directive("info", "warn"),
        1 => directive("debug", "warn"),
        _ => directive("trace", "info"),
    }
}

fn file_filter() -> EnvFilter {
    std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(directive("info", "warn")))
}

/// Starts logging. Failing to create the log directory is not fatal: the daemon runs on with
/// terminal-only (or no) logging.
pub fn init(data_dir: &Path, args: &Args) -> LogGuard {
    let dir = data_dir.join("logs");
    let mut guard = None;
    let file_layer = match std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|()| {
        Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix("ssx-app")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&dir)
            .map_err(|e| e.to_string())
    }) {
        Ok(appender) => {
            let (writer, g) = tracing_appender::non_blocking(appender);
            guard = Some(g);
            Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_filter(file_filter()),
            )
        }
        Err(e) => {
            eprintln!("ssx-app: cannot write a log file in {}: {e}", dir.display());
            None
        }
    };
    let terminal_layer = args.log_to_terminal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_filter(EnvFilter::new(terminal_directive(args.verbose)))
    });
    // A second initialisation (tests calling `run` twice) is harmless: ignore the error.
    let _ = tracing_subscriber::registry().with(file_layer).with(terminal_layer).try_init();
    LogGuard { file: guard, dir }
}

/// Logs panics (with a backtrace) and tells the user. See the module docs.
pub fn install_panic_hook(notifier: Arc<DaemonNotifier>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("ssx-app panicked: {info}\n{backtrace}");
        let text = info.to_string();
        let n = Arc::clone(&notifier);
        // Never notify from the panicking thread: the notifier may itself be what broke.
        let _ = std::thread::Builder::new().name("ssx-panic-notice".into()).spawn(move || {
            n.say(
                ssx_core::workflow::NotificationLevel::Error,
                "ssx hit an internal error",
                &format!("{text}\nThe details are in the ssx log file."),
            );
        });
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_name_every_ssx_crate() {
        let d = directive("debug", "warn");
        assert!(d.starts_with("warn,"));
        for c in ["ssx_app=debug", "ssx_core=debug", "ssx_hotkeys=debug", "ssx_overlay=debug"] {
            assert!(d.contains(c), "{c} in {d}");
        }
        EnvFilter::try_new(&d).expect("valid filter");
        for v in 0..4 {
            EnvFilter::try_new(terminal_directive(v)).expect("valid terminal filter");
        }
        assert!(terminal_directive(0).contains("ssx_app=info"));
        assert!(terminal_directive(1).contains("ssx_app=debug"));
        assert!(terminal_directive(2).contains("ssx_app=trace"));
    }

    #[test]
    fn init_creates_the_log_directory_and_survives_a_second_call() {
        let d = tempfile::tempdir().unwrap();
        let args = <Args as clap::Parser>::try_parse_from(["ssx-app"]).unwrap();
        let g1 = init(d.path(), &args);
        assert!(g1.dir.is_dir());
        let _g2 = init(d.path(), &args);
        tracing::info!("hello from the test");
    }

    #[test]
    fn an_unwritable_data_dir_is_not_fatal() {
        let args = <Args as clap::Parser>::try_parse_from(["ssx-app"]).unwrap();
        // A data "directory" that is a regular file cannot hold a `logs` directory, on any OS.
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("data");
        std::fs::write(&not_a_dir, b"a file, not a directory").unwrap();
        let g = init(&not_a_dir, &args);
        assert!(g.file.is_none());
    }
}
