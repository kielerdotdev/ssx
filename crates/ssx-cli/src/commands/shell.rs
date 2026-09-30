//! `ssx shell`: file-manager right-click entries (`ssx-shell`).

use std::path::PathBuf;

use serde::Serialize;
use ssx_shell::{Context, Integrations};

use crate::{
    app::{App, exe_path},
    cli::ShellCmd,
    error::{CliError, CliResult},
    output::{Style, Table, err_line, out_line, out_text},
};

/// One integration's state, for `status` and JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationState {
    /// Stable id (`nautilus`, `dolphin`, ...).
    pub id: String,
    /// Display name.
    pub name: String,
    /// The file manager looks present on this machine.
    pub detected: bool,
    /// Why (evidence or what is missing).
    pub detail: String,
    /// Everything is installed and current.
    pub installed: bool,
}

/// Collects the state of every integration that applies to this OS.
pub fn states(integrations: &Integrations, ctx: &Context) -> Vec<IntegrationState> {
    integrations
        .iter()
        .filter(|i| i.platform() == ctx.platform)
        .map(|i| {
            let d = i.detect(ctx);
            IntegrationState {
                id: i.id().to_owned(),
                name: i.name().to_owned(),
                detected: d.available,
                detail: d.reason,
                installed: i.is_installed(ctx).unwrap_or(false),
            }
        })
        .collect()
}

/// The status table.
pub fn render_states(states: &[IntegrationState], style: Style) -> String {
    let mut t = Table::new(["FILE MANAGER", "FOUND", "INSTALLED", "DETAILS"]);
    for s in states {
        t.row([
            s.name.clone(),
            if s.detected { "yes" } else { "no" }.to_owned(),
            if s.installed { style.green("yes") } else { "no".to_owned() },
            s.detail.clone(),
        ]);
    }
    t.render(style)
}

fn context(exe: Option<PathBuf>) -> CliResult<Context> {
    let exe = match exe {
        Some(e) => std::path::absolute(e)
            .map_err(|e| CliError::new(format!("cannot resolve the --exe path: {e}")))?,
        None => exe_path()?,
    };
    Context::from_env(exe).map_err(|e| {
        CliError::new(e.to_string())
            .hint("the ssx executable must have an absolute path without control characters")
    })
}

/// Dispatches `ssx shell ...`.
pub fn run(app: &App, cmd: ShellCmd) -> CliResult<()> {
    let integrations = Integrations::native();
    match cmd {
        ShellCmd::Status { json } => {
            let ctx = context(None)?;
            let st = states(&integrations, &ctx);
            if json {
                out_line(&serde_json::to_string_pretty(&st)?);
            } else {
                out_text(&render_states(&st, app.out));
            }
            Ok(())
        }
        ShellCmd::Install { dry_run, force, exe } => {
            let ctx = context(exe)?;
            if dry_run {
                let mut any = false;
                for i in integrations.iter().filter(|i| i.platform() == ctx.platform) {
                    let d = i.detect(&ctx);
                    if !d.available && !force {
                        err_line(&format!("skip {}: {}", i.name(), d.reason));
                        continue;
                    }
                    any = true;
                    out_line(&format!("would install for {}:", i.name()));
                    out_text(&i.describe(&ctx).to_string());
                }
                if !any {
                    err_line("no supported file manager was found (use --force to install anyway)");
                }
                err_line("dry run: nothing was written");
                return Ok(());
            }
            let report = integrations.install_all_with(&ctx, force);
            out_text(&report.to_string());
            if report.has_failures() {
                return Err(CliError::new("some file-manager entries could not be installed"));
            }
            Ok(())
        }
        ShellCmd::Uninstall { dry_run, exe } => {
            let ctx = context(exe)?;
            if dry_run {
                for s in states(&integrations, &ctx).iter().filter(|s| s.installed) {
                    out_line(&format!("would remove the entries for {}", s.name));
                }
                err_line("dry run: nothing was removed");
                return Ok(());
            }
            let report = integrations.uninstall_all(&ctx);
            out_text(&report.to_string());
            if report.has_failures() {
                return Err(CliError::new("some file-manager entries could not be removed"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_table_lists_every_file_manager_of_this_os() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context::sandboxed(dir.path(), dir.path().join("bin/ssx"));
        let integrations = Integrations::native();
        let st = states(&integrations, &ctx);
        assert!(!st.is_empty());
        assert!(st.iter().all(|s| !s.installed), "a fresh sandbox has nothing installed");
        let table = render_states(&st, Style::plain());
        assert!(table.starts_with("FILE MANAGER"));
        for s in &st {
            assert!(table.contains(&s.name), "{} missing", s.name);
        }
        let json = serde_json::to_value(&st).unwrap();
        assert!(json[0]["id"].is_string() && json[0]["installed"].is_boolean());
    }
}
