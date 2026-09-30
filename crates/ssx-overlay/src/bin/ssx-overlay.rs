//! `ssx-overlay`: the helper process.
//!
//! * No arguments: reads a request on stdin, shows the overlay, writes one JSON line with the
//!   outcome on stdout (see `ssx_overlay::protocol`). This is how the library's
//!   `select_via_helper` uses it.
//! * `--demo [WxH] [--mode rect|ellipse|freeform|monitor|window]`: shows the overlay over a
//!   synthetic desktop in this process and prints the outcome, for manual verification on a
//!   real compositor (see the README checklists).

use std::{process::ExitCode, time::Instant};

use ssx_overlay::{
    OverlayInput, OverlayOptions, SelectMode, demo::demo_frame, runner::run_helper, select,
};
use ssx_types::Point;
use tracing_subscriber::EnvFilter;

fn demo(args: &[String]) -> ExitCode {
    let mut size = (1920u32, 1080u32);
    let mut mode = SelectMode::Rect;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                mode = match it.next().map(String::as_str) {
                    Some("ellipse") => SelectMode::Ellipse,
                    Some("freeform") => SelectMode::Freeform,
                    Some("monitor") => SelectMode::Monitor,
                    Some("window") => SelectMode::Window,
                    Some("rect") => SelectMode::Rect,
                    other => {
                        eprintln!("unknown --mode {other:?}");
                        return ExitCode::from(64);
                    }
                };
            }
            wh => {
                let parsed =
                    wh.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                let Some(s) = parsed else {
                    eprintln!("unrecognised argument {wh:?}; expected WxH or --mode <mode>");
                    return ExitCode::from(64);
                };
                size = s;
            }
        }
    }
    let input = OverlayInput {
        desktop: demo_frame(size.0, size.1, Point::new(0, 0)),
        monitors: vec![],
        windows: vec![],
        options: OverlayOptions { mode, timeout_ms: Some(120_000), ..OverlayOptions::default() },
    };
    let t = Instant::now();
    match select(input) {
        Ok(outcome) => {
            println!("{}", serde_json::to_string_pretty(&outcome).unwrap_or_default());
            eprintln!("finished after {:?}", t.elapsed());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("overlay failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let t0 = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SSX_OVERLAY_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--demo") {
        return demo(&args[1..]);
    }
    if let Some(a) = args.first() {
        eprintln!("ssx-overlay is a helper process; unexpected argument {a:?} (try --demo)");
        return ExitCode::from(64);
    }
    let code = run_helper(t0, std::io::stdin().lock(), std::io::stdout().lock());
    ExitCode::from(u8::try_from(code).unwrap_or(2))
}
