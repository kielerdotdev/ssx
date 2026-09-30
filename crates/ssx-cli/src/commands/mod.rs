//! One module per command family. Each exposes plain functions that take the parsed arguments
//! and return data or a [`CliResult`](crate::error::CliResult); printing happens at the edge
//! (`render_*` functions are pure and unit-tested).

pub mod capture;
pub mod config;
pub mod doctor;
pub mod files;
pub mod history;
pub mod hotkeys;
pub mod list;
pub mod run;
pub mod shell;
pub mod uploaders;

use clap::CommandFactory;

use crate::{
    app::App,
    cli::{Cli, Command},
    error::{CliError, CliResult},
};

/// Runs the parsed command.
pub fn dispatch(app: &App, command: Command) -> CliResult<()> {
    match command {
        Command::Capture(a) => capture::run(app, a),
        Command::Monitors(a) => list::monitors(app, &a),
        Command::Windows(a) => list::windows(app, &a),
        Command::PostFile(a) => files::post_file(app, a),
        Command::PostVideo(a) => files::post_video(app, a),
        Command::Edit(a) => files::edit(app, a),
        Command::Upload(a) => files::upload(app, a),
        Command::Uploaders { cmd } => uploaders::run(app, cmd),
        Command::Run(a) => run::run(app, a),
        Command::History { cmd } => history::run(app, cmd),
        Command::Config { cmd } => config::run(app, cmd),
        Command::Hotkeys { cmd } => hotkeys::run(app, cmd),
        Command::Shell { cmd } => shell::run(app, cmd),
        Command::Doctor(a) => doctor::run(app, &a),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "ssx", &mut std::io::stdout());
            Ok(())
        }
        Command::Man => {
            clap_mangen::Man::new(Cli::command())
                .render(&mut std::io::stdout())
                .map_err(|e| CliError::new(format!("cannot write the man page: {e}")))
        }
    }
}
