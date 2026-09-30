//! `ssx-app`: the ssx background daemon. All the logic is in the library.

// A tray app must not open a console window on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use clap::Parser;

fn main() -> std::process::ExitCode {
    let args = ssx_app::cli::Args::parse();
    ssx_app::run(args)
}
