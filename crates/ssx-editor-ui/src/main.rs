//! `ssx-editor-ui <image-or-.ssxe> [--output PATH] [--json]`
//!
//! Opens the editor window. With `--output` the editor runs in *workflow mode*: Save and Done
//! write the PNG to that path. With `--json` the outcome is printed on stdout as
//! `{"action":"save"|"copy"|"upload"|"cancel","path":...}`. Exit code 0 means the user kept
//! something, 3 means they cancelled, 2 that the editor could not start.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, ValueEnum};
use ssx_editor_ui::{BenchMode, DevOptions, EditorInput, EditorOutcome, EditorRequest, run};

#[derive(Clone, Copy, ValueEnum)]
enum BenchArg {
    PanZoom,
    Objects,
}

/// The ssx image editor.
#[derive(Parser)]
#[command(name = "ssx-editor-ui", version, about)]
struct Cli {
    /// Image (png, jpg, webp, bmp, gif) or `.ssxe` project to edit. Omit it to edit the
    /// clipboard image.
    input: Option<PathBuf>,

    /// Where "Save" / "Done" write the edited PNG (enables workflow mode).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Print the outcome as JSON on stdout.
    #[arg(long)]
    json: bool,

    /// Start from the clipboard image even if an input is given.
    #[arg(long, hide = true)]
    clipboard: bool,

    /// Close after this many frames (smoke tests).
    #[arg(long, hide = true, value_name = "N")]
    exit_after_frames: Option<u32>,

    /// Run a scripted workload and print frame statistics to stderr.
    #[arg(long, hide = true, value_enum)]
    bench: Option<BenchArg>,

    /// Do not read or write the persistent state file.
    #[arg(long, hide = true)]
    ephemeral: bool,
}

fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();
    let input = match (&cli.input, cli.clipboard) {
        (Some(p), false) => EditorInput::Path(p.clone()),
        _ => EditorInput::Clipboard,
    };
    let mut request = EditorRequest::new(input);
    request.output = cli.output;
    request.dev = DevOptions {
        exit_after_frames: cli.exit_after_frames,
        bench: cli.bench.map(|b| match b {
            BenchArg::PanZoom => BenchMode::PanZoom,
            BenchArg::Objects => BenchMode::Objects,
        }),
        ephemeral_state: cli.ephemeral,
    };
    match run(request) {
        Ok(outcome) => {
            if cli.json {
                println!("{}", outcome.to_json());
            }
            ExitCode::from(u8::try_from(outcome.exit_code()).unwrap_or(1))
        }
        Err(e) => {
            eprintln!("ssx-editor-ui: {e}");
            if cli.json {
                println!("{}", EditorOutcome::cancelled().to_json());
            }
            ExitCode::from(2)
        }
    }
}
