//! `ssx-app` command line.

use std::path::PathBuf;

use clap::Parser;

/// The ssx background app: tray icon, global hotkeys and the service `ssx` talks to.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "ssx-app",
    version,
    about = "The ssx background app (tray icon, global hotkeys, IPC server)",
    long_about = "The ssx background app.\n\n\
                  It stays running per user: it owns the tray icon and the global hotkeys, runs \
                  workflows for hotkeys, the tray menu and the `ssx` command line, and merges \
                  the files a file manager hands over into one upload. Starting it a second \
                  time just tells the running one to show its settings.\n\n\
                  Normally started with `ssx daemon start` or at login (`ssx daemon autostart \
                  enable`).",
    after_help = "ENVIRONMENT:\n  SSX_CONFIG_DIR    relocate settings and data (config in DIR, data in DIR/data)\n  SSX_BACKEND       force the capture backend: windows, wayland, portal or x11\n  SSX_OVERLAY       path of the ssx-overlay helper, or `none`\n  SSX_EDITOR_UI     path of the ssx-editor-ui helper, or `none`\n  SSX_SETTINGS_UI   path of the ssx-settings-ui helper, or `none`\n  RUST_LOG          log filter for the log file (default: ssx at info)"
)]
pub struct Args {
    /// Use this directory for settings and data instead of the default (like `SSX_CONFIG_DIR`)
    #[arg(long, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// Do not show a tray icon (the app still serves hotkeys and the command line)
    #[arg(long)]
    pub no_tray: bool,
    /// Do not register global hotkeys
    #[arg(long)]
    pub no_hotkeys: bool,
    /// Also log to the terminal (at info level; add -v for more)
    #[arg(long)]
    pub foreground: bool,
    /// More log detail on the terminal (-v debug, -vv trace); implies --foreground
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Force a capture backend: windows, wayland, portal or x11 (like `SSX_BACKEND`)
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
}

impl Args {
    /// `true` when the terminal should get log output.
    pub fn log_to_terminal(&self) -> bool {
        self.foreground || self.verbose > 0
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_grammar_is_consistent() {
        Args::command().debug_assert();
    }

    #[test]
    fn flags_parse() {
        let a = Args::try_parse_from([
            "ssx-app",
            "--config-dir",
            "/tmp/x",
            "--no-tray",
            "--no-hotkeys",
            "-vv",
            "--backend",
            "x11",
        ])
        .unwrap();
        assert_eq!(a.config_dir.as_deref(), Some(std::path::Path::new("/tmp/x")));
        assert!(a.no_tray && a.no_hotkeys && a.log_to_terminal());
        assert_eq!((a.verbose, a.backend.as_deref()), (2, Some("x11")));
        let plain = Args::try_parse_from(["ssx-app"]).unwrap();
        assert!(!plain.log_to_terminal());
        assert!(Args::try_parse_from(["ssx-app", "--nonsense"]).is_err());
    }

    #[test]
    fn version_is_reported() {
        let e = Args::try_parse_from(["ssx-app", "--version"]).unwrap_err();
        assert_eq!(e.exit_code(), 0);
        assert!(e.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}
