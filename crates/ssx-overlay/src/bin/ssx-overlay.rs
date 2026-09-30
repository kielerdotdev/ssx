//! `ssx-overlay`: the helper process. Reads a request on stdin, shows the overlay, writes
//! one JSON line with the outcome on stdout.

use std::{process::ExitCode, time::Instant};

use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let t0 = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SSX_OVERLAY_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let code =
        ssx_overlay::runner::run_helper(t0, std::io::stdin().lock(), std::io::stdout().lock());
    ExitCode::from(u8::try_from(code).unwrap_or(2))
}
