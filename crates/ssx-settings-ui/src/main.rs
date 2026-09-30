//! `ssx-settings-ui [--page NAME] [--config-dir DIR] [--json]`
//!
//! Exit codes: `0` the window closed normally, `2` it could not start (bad arguments, a
//! settings file from a newer ssx, no display).

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use ssx_settings_ui::{RunOptions, nav::Page, run};

/// The ssx settings and history window.
#[derive(Debug, Parser)]
#[command(name = "ssx-settings-ui", version, about)]
struct Args {
    /// The page to open: general, capture, workflows, hotkeys, uploaders, history,
    /// integration or about
    #[arg(long, value_name = "PAGE", default_value = "general")]
    page: Page,
    /// Use this folder for settings and data instead of the default (like SSX_CONFIG_DIR)
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    /// Print how the window ended as one line of JSON on stdout
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let args = Args::parse();
    match run(RunOptions { page: args.page, config_dir: args.config_dir }) {
        Ok((outcome, file)) => {
            if args.json {
                println!("{}", outcome.to_json(&file));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
