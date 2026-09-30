//! The command-line grammar (clap derive). Only parsing lives here; behaviour is in
//! [`crate::commands`].

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use ssx_types::Rect;

use crate::output::ColorChoice;

const AFTER_HELP: &str = "\
EXIT CODES:
  0  success
  1  error (also: a workflow that only partly succeeded, e.g. saved but not uploaded)
  2  usage error (bad command line)
  3  cancelled (Ctrl-C, closed the editor, dismissed the region overlay)

ENVIRONMENT:
  SSX_CONFIG_DIR   relocate settings and data (config in DIR, data in DIR/data)
  SSX_BACKEND      force the capture backend: windows, wayland, portal or x11
  SSX_EDITOR_UI    path of the ssx-editor-ui helper used by `edit` and --edit
  SSX_SECRET_<NAME> supply an uploader secret without a keyring (NAME upper-cased)
  RUST_LOG         log filter (overrides -v), e.g. RUST_LOG=ssx_services=debug
  NO_COLOR         disable coloured output

Run `ssx <command> --help` for the details of one command. `ssx doctor` explains what works
on this machine and why.";

/// ssx: capture, edit, upload and share screenshots.
#[derive(Debug, Parser)]
#[command(
    name = "ssx",
    version,
    about = "Capture, edit and upload screenshots (a cross-platform ShareX alternative)",
    long_about = "Capture, edit and upload screenshots and files.\n\n\
                  Examples:\n  \
                  ssx capture fullscreen -o shot.png\n  \
                  ssx capture region --rect 100,100,800,600 --upload\n  \
                  ssx upload --to my-host a.png b.png\n  \
                  ssx run capture-region-edit\n  \
                  ssx doctor",
    after_help = AFTER_HELP,
    arg_required_else_help = true,
    propagate_version = true,
    max_term_width = 100
)]
pub struct Cli {
    /// Options shared by every command.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Options accepted before or after any command.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// More log detail on stderr (-v info, -vv debug, -vvv trace); `RUST_LOG` overrides
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// Only print errors and the requested output
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Use this directory for settings and data instead of the default (like SSX_CONFIG_DIR)
    #[arg(long, value_name = "DIR", global = true)]
    pub config_dir: Option<PathBuf>,
    /// Force a capture backend: windows, wayland, portal or x11 (like SSX_BACKEND)
    #[arg(long, value_name = "NAME", global = true)]
    pub backend: Option<String>,
    /// When to colour output
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Take a screenshot
    Capture(CaptureArgs),
    /// List monitors (position, scale, HDR state)
    Monitors(ListArgs),
    /// List top-level windows
    Windows(ListArgs),
    /// Upload files through a workflow; what file-manager "Upload with ssx" entries run
    #[command(name = "post-file")]
    PostFile(PostFileArgs),
    /// Upload video files (uses the video destination, falling back to the file one)
    #[command(name = "post-video")]
    PostVideo(PostVideoArgs),
    /// Open an image in the editor
    Edit(EditArgs),
    /// Upload files and print their URLs (one per line), quietly, for scripts
    Upload(UploadArgs),
    /// Manage upload destinations (Imgur, S3, HTTP, ShareX .sxcu files)
    Uploaders {
        /// What to do.
        #[command(subcommand)]
        cmd: UploadersCmd,
    },
    /// Run a workflow from settings.toml
    Run(RunArgs),
    /// Browse and maintain the capture and upload history
    History {
        /// What to do.
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    /// Show, check and change settings.toml
    Config {
        /// What to do.
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Global hotkeys: detect what works, generate compositor bindings
    Hotkeys {
        /// What to do.
        #[command(subcommand)]
        cmd: HotkeysCmd,
    },
    /// File-manager right-click entries
    Shell {
        /// What to do.
        #[command(subcommand)]
        cmd: ShellCmd,
    },
    /// Explain what works on this machine and what does not
    Doctor(DoctorArgs),
    /// Print a shell completion script
    Completions {
        /// The shell to generate for.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Print the man page (roff) to stdout
    #[command(hide = true)]
    Man,
}

// ---- capture -----------------------------------------------------------------------------

/// Image container for `--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// Lossless PNG.
    Png,
    /// JPEG (quality from settings).
    Jpg,
    /// Lossless WebP.
    Webp,
}

/// `ssx capture`.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// What to capture.
    #[command(subcommand)]
    pub target: CaptureTarget,
    /// Options for every target.
    #[command(flatten)]
    pub opts: CaptureOpts,
}

/// What `ssx capture` captures.
#[derive(Debug, Subcommand)]
pub enum CaptureTarget {
    /// The whole desktop (all monitors)
    #[command(alias = "screen")]
    Fullscreen,
    /// One monitor: the one showing the focused window, or the one given with --id
    Monitor {
        /// Monitor id from `ssx monitors`
        #[arg(long)]
        id: Option<String>,
    },
    /// A window: the focused one, or the one given with --id
    Window {
        /// The focused window (default)
        #[arg(long, conflicts_with = "id")]
        active: bool,
        /// Window id from `ssx windows`
        #[arg(long)]
        id: Option<String>,
    },
    /// A region: an exact rectangle with --rect (interactive selection needs the overlay,
    /// which is not available yet)
    Region {
        /// Rectangle in desktop pixels: X,Y,WIDTH,HEIGHT (X and Y may be negative)
        #[arg(long, value_name = "X,Y,W,H", value_parser = parse_rect, allow_hyphen_values = true)]
        rect: Option<Rect>,
    },
    /// The region captured last time
    #[command(name = "last-region")]
    LastRegion,
}

/// Options of `ssx capture`. They are `global` so they may follow the target
/// (`ssx capture fullscreen -o x.png`).
#[derive(Debug, Args)]
pub struct CaptureOpts {
    /// Write the image to this file (overwriting); the format follows the extension unless
    /// --format is given. Without it the image is saved in the configured save folder
    #[arg(short, long, value_name = "PATH", global = true)]
    pub output: Option<PathBuf>,
    /// Image format (default: from --output's extension, else the setting)
    #[arg(long, value_enum, global = true)]
    pub format: Option<FormatArg>,
    /// Include the mouse cursor
    #[arg(long, global = true)]
    pub cursor: bool,
    /// Wait this many milliseconds first (overrides the setting)
    #[arg(long, value_name = "MS", global = true)]
    pub delay: Option<u32>,
    /// Copy the image to the clipboard
    #[arg(long, global = true)]
    pub copy: bool,
    /// Upload the image and print its URL
    #[arg(long, global = true)]
    pub upload: bool,
    /// Upload destination (implies --upload)
    #[arg(long, value_name = "NAME", global = true)]
    pub to: Option<String>,
    /// Copy the URL to the clipboard after uploading
    #[arg(long, global = true)]
    pub copy_url: bool,
    /// Edit the image before saving
    #[arg(long, global = true)]
    pub edit: bool,
    /// Print the result as JSON
    #[arg(long, global = true)]
    pub json: bool,
}

/// Parses `X,Y,W,H`.
pub fn parse_rect(s: &str) -> Result<Rect, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let [x, y, w, h] = parts.as_slice() else {
        return Err(format!("expected X,Y,WIDTH,HEIGHT (four numbers separated by commas), got {s:?}"));
    };
    let int = |name: &str, v: &str| {
        v.parse::<i32>().map_err(|_| format!("{name} must be a whole number, got {v:?}"))
    };
    let size = |name: &str, v: &str| {
        v.parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{name} must be a positive whole number, got {v:?}"))
    };
    Ok(Rect::new(int("X", x)?, int("Y", y)?, size("WIDTH", w)?, size("HEIGHT", h)?))
}

/// `--json` for listing commands.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

// ---- files -------------------------------------------------------------------------------

/// Content class for `post-file --kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    /// Use the image destination.
    Image,
    /// Use the file destination.
    File,
    /// Use the video destination.
    Video,
    /// Use the text destination.
    Text,
}

/// `ssx post-file`.
#[derive(Debug, Args)]
pub struct PostFileArgs {
    /// Hand the files to the running ssx instance if there is one (it merges requests that
    /// arrive within a few hundred milliseconds into one batch); otherwise run here
    #[arg(long)]
    pub coalesce: bool,
    /// Send everything through this kind of destination instead of choosing by extension
    #[arg(long, value_enum)]
    pub kind: Option<KindArg>,
    /// Workflow to run (id, CLI name or name); default: upload-files
    #[arg(long, value_name = "WORKFLOW")]
    pub workflow: Option<String>,
    /// Upload destination for all files
    #[arg(long, value_name = "NAME")]
    pub to: Option<String>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
    /// Files and folders (folders are zipped); put `--` before names that start with `-`
    #[arg(required = true, value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,
}

/// `ssx post-video`.
#[derive(Debug, Args)]
pub struct PostVideoArgs {
    /// Workflow to run (default: upload-files)
    #[arg(long, value_name = "WORKFLOW")]
    pub workflow: Option<String>,
    /// Upload destination
    #[arg(long, value_name = "NAME")]
    pub to: Option<String>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
    /// Video files
    #[arg(required = true, value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,
}

/// `ssx edit`.
#[derive(Debug, Args)]
pub struct EditArgs {
    /// Save the edited image here (default: `<name>-edited.<ext>` next to the original)
    #[arg(short, long, value_name = "PATH", conflicts_with = "in_place")]
    pub output: Option<PathBuf>,
    /// Replace the original file
    #[arg(long)]
    pub in_place: bool,
    /// Copy the edited image to the clipboard
    #[arg(long)]
    pub copy: bool,
    /// Upload the edited image and print its URL
    #[arg(long)]
    pub upload: bool,
    /// Upload destination (implies --upload)
    #[arg(long, value_name = "NAME")]
    pub to: Option<String>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
    /// The image to edit
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

/// `ssx upload`.
#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Upload destination (default: by content type from the settings)
    #[arg(long, value_name = "NAME")]
    pub to: Option<String>,
    /// Copy the URL(s) to the clipboard
    #[arg(long)]
    pub copy: bool,
    /// Print JSON (one object per file) instead of bare URLs
    #[arg(long)]
    pub json: bool,
    /// Files and folders (folders are zipped); put `--` before names that start with `-`
    #[arg(required = true, value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,
}

// ---- uploaders ---------------------------------------------------------------------------

/// `ssx uploaders`.
#[derive(Debug, Subcommand)]
pub enum UploadersCmd {
    /// List destinations and whether they load
    List(ListArgs),
    /// Import a ShareX custom uploader (.sxcu)
    Import {
        /// The .sxcu file.
        file: PathBuf,
        /// Name to register it under (default: from the file)
        #[arg(long)]
        name: Option<String>,
        /// Replace an existing import of the same name
        #[arg(long)]
        force: bool,
    },
    /// Remove an imported .sxcu and any [uploaders.NAME] table with that name
    Remove {
        /// Destination name.
        name: String,
    },
    /// Upload a tiny test file (or shorten example.com) to check a destination
    Test {
        /// Destination name.
        name: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage the secrets that "keyring:NAME" references in settings.toml point to
    Secret {
        /// What to do.
        #[command(subcommand)]
        cmd: SecretCmd,
    },
}

/// `ssx uploaders secret`.
#[derive(Debug, Subcommand)]
pub enum SecretCmd {
    /// Store a secret; the value is read from standard input (never from the command line)
    Set {
        /// Secret name (the part after "keyring:").
        name: String,
    },
    /// Delete a secret
    Delete {
        /// Secret name.
        name: String,
    },
    /// Show whether a secret exists (never prints it) and where secrets are stored
    Status {
        /// Secret names to check.
        names: Vec<String>,
    },
}

// ---- run ---------------------------------------------------------------------------------

/// `ssx run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Workflow id, CLI name or name (see `ssx config show`)
    pub workflow: String,
    /// Wait this many milliseconds before capturing (overrides the setting)
    #[arg(long, value_name = "MS")]
    pub delay: Option<u32>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

// ---- history -----------------------------------------------------------------------------

/// Entry kind filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HistoryKindArg {
    /// Screenshots.
    Image,
    /// Recordings.
    Video,
    /// Other files.
    File,
    /// Text.
    Text,
    /// URLs.
    Url,
}

/// Shared listing filters.
#[derive(Debug, Args)]
pub struct HistoryFilter {
    /// Show at most this many entries (newest first)
    #[arg(short = 'n', long, default_value_t = 20)]
    pub limit: usize,
    /// Only this kind of entry
    #[arg(long, value_enum)]
    pub kind: Option<HistoryKindArg>,
    /// Only entries that were uploaded
    #[arg(long)]
    pub uploaded: bool,
    /// Print JSON
    #[arg(long)]
    pub json: bool,
}

/// `ssx history`.
#[derive(Debug, Subcommand)]
pub enum HistoryCmd {
    /// List the newest entries
    List(HistoryFilter),
    /// Search paths, URLs, window titles, uploaders and notes
    Search {
        /// Words to look for (all must match).
        #[arg(required = true)]
        text: Vec<String>,
        /// Listing options.
        #[command(flatten)]
        filter: HistoryFilter,
    },
    /// Show one entry in full
    Show {
        /// Entry id.
        id: i64,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete entries from the history (files on disk are kept)
    Delete {
        /// Entry ids.
        #[arg(required = true)]
        ids: Vec<i64>,
    },
    /// Apply the retention limits now (default: those in settings.toml)
    Prune {
        /// Keep only the newest N entries
        #[arg(long)]
        max_entries: Option<u32>,
        /// Remove entries older than this many days
        #[arg(long)]
        max_age_days: Option<u32>,
        /// Also remove entries whose file no longer exists
        #[arg(long)]
        orphans: bool,
    },
    /// Open an entry's URL in the browser (or its file with --file)
    Open {
        /// Entry id.
        id: i64,
        /// Open the local file instead of the URL
        #[arg(long)]
        file: bool,
    },
}

// ---- config ------------------------------------------------------------------------------

/// `ssx config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print where settings and data live
    Path {
        /// Print every location with a label
        #[arg(long)]
        all: bool,
    },
    /// Print the effective settings
    Show {
        /// Print JSON instead of TOML
        #[arg(long)]
        json: bool,
        /// Print the built-in defaults instead of the current settings
        #[arg(long)]
        defaults: bool,
    },
    /// Check a settings file (default: the current one) and explain every problem
    Validate {
        /// File to check.
        file: Option<PathBuf>,
        /// Fail on warnings too
        #[arg(long)]
        strict: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open settings.toml in $VISUAL / $EDITOR (or the system default), then validate it
    Edit,
    /// Set one value, e.g. `ssx config set general.image_quality 80`
    Set {
        /// Dotted key: general.image_format, capture.hdr.exposure, workflows[0].name, ...
        key: String,
        /// New value: a TOML value (`true`, `80`, `[1,2]`) or plain text
        value: String,
    },
    /// Restore defaults (the old file is kept as settings.toml.bak-<time>)
    Reset {
        /// Only reset this key.
        key: Option<String>,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

// ---- hotkeys -----------------------------------------------------------------------------

/// Desktop to generate bindings for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HotkeyTarget {
    /// sway (`bindsym`).
    Sway,
    /// Hyprland (`bind =`).
    Hyprland,
    /// GNOME custom keybindings.
    Gnome,
    /// KDE Plasma command shortcuts.
    Kde,
}

/// `ssx hotkeys`.
#[derive(Debug, Subcommand)]
pub enum HotkeysCmd {
    /// Show which hotkey mechanism suits this session
    Detect {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the generated bindings for the workflows that have a hotkey (changes nothing)
    Print {
        /// Desktop to generate for (default: detected)
        #[arg(long, value_enum)]
        target: Option<HotkeyTarget>,
        /// Program the bindings run (default: ssx, found on PATH by the desktop)
        #[arg(long, value_name = "PATH", default_value = "ssx")]
        exe: String,
    },
    /// Install the bindings. sway/Hyprland: write ssx's own include file and tell you the line
    /// to add to your config; --apply adds it for you. GNOME/KDE change desktop settings, so
    /// they need --apply
    Install {
        /// Desktop to install for (default: detected)
        #[arg(long, value_enum)]
        target: Option<HotkeyTarget>,
        /// Also make the change to your own configuration
        #[arg(long)]
        apply: bool,
        /// Reload the compositor / shortcut daemon afterwards
        #[arg(long)]
        reload: bool,
        /// Program the bindings run (default: the absolute path of this executable)
        #[arg(long, value_name = "PATH")]
        exe: Option<String>,
    },
    /// Remove everything `install` added
    Uninstall {
        /// Desktop to uninstall from (default: detected)
        #[arg(long, value_enum)]
        target: Option<HotkeyTarget>,
    },
}

// ---- shell -------------------------------------------------------------------------------

/// `ssx shell`.
#[derive(Debug, Subcommand)]
pub enum ShellCmd {
    /// Show which file managers were found and what is installed
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add "Upload with ssx", "Edit image with ssx" and "Upload video with ssx"
    Install {
        /// Only show what would be written
        #[arg(long)]
        dry_run: bool,
        /// Install for file managers that were not detected too
        #[arg(long)]
        force: bool,
        /// Program the entries run (default: the absolute path of this executable)
        #[arg(long, value_name = "PATH")]
        exe: Option<PathBuf>,
    },
    /// Remove what `install` added (only files ssx created)
    Uninstall {
        /// Only show what would be removed
        #[arg(long)]
        dry_run: bool,
        /// Program path the entries were installed with (default: this executable)
        #[arg(long, value_name = "PATH")]
        exe: Option<PathBuf>,
    },
}

// ---- doctor ------------------------------------------------------------------------------

/// `ssx doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Print the report as JSON
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_grammar_is_consistent() {
        // Catches duplicate flags, bad conflicts, missing help, ... at test time.
        Cli::command().debug_assert();
    }

    #[test]
    fn rect_parsing() {
        assert_eq!(parse_rect("1,2,3,4"), Ok(Rect::new(1, 2, 3, 4)));
        assert_eq!(parse_rect(" -1920 , 0,1920,1080"), Ok(Rect::new(-1920, 0, 1920, 1080)));
        for bad in ["", "1,2,3", "1,2,3,4,5", "a,2,3,4", "1,2,0,4", "1,2,3,-4", "1,2,3.5,4", "1,2,3,99999999999"] {
            assert!(parse_rect(bad).is_err(), "{bad:?}");
        }
        assert!(parse_rect("1,2,3").unwrap_err().contains("four numbers"));
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ssx").chain(args.iter().copied()))
    }

    #[test]
    fn capture_options_may_follow_the_target() {
        let cli = parse(&["capture", "fullscreen", "-o", "x.png", "--cursor", "--delay", "500", "--json"]).unwrap();
        let Command::Capture(c) = cli.command else { panic!("not capture") };
        assert!(matches!(c.target, CaptureTarget::Fullscreen));
        assert_eq!(c.opts.output.as_deref(), Some(std::path::Path::new("x.png")));
        assert!(c.opts.cursor && c.opts.json);
        assert_eq!(c.opts.delay, Some(500));

        let cli = parse(&["capture", "-o", "y.png", "region", "--rect", "-10,5,20,30"]).unwrap();
        let Command::Capture(c) = cli.command else { panic!() };
        assert!(matches!(c.target, CaptureTarget::Region { rect: Some(r) } if r == Rect::new(-10, 5, 20, 30)));
        assert_eq!(c.opts.output.as_deref(), Some(std::path::Path::new("y.png")));
    }

    #[test]
    fn screen_is_an_alias_and_window_flags_conflict() {
        assert!(matches!(
            parse(&["capture", "screen"]).unwrap().command,
            Command::Capture(CaptureArgs { target: CaptureTarget::Fullscreen, .. })
        ));
        assert!(parse(&["capture", "window", "--active", "--id", "5"]).is_err());
    }

    #[test]
    fn paths_after_double_dash_may_look_like_flags() {
        let cli = parse(&["post-file", "--coalesce", "--", "-rf", "--weird name", "a b.png"]).unwrap();
        let Command::PostFile(a) = cli.command else { panic!() };
        assert!(a.coalesce);
        assert_eq!(a.paths.len(), 3);
        assert_eq!(a.paths[0], PathBuf::from("-rf"));
        assert!(parse(&["post-file"]).is_err(), "at least one path");
        assert!(parse(&["upload"]).is_err());
    }

    #[test]
    fn usage_errors_are_reported_by_clap_with_code_2() {
        let e = parse(&["capture", "nonsense"]).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert_eq!(parse(&["--version"]).unwrap_err().exit_code(), 0);
        assert!(parse(&[]).is_err(), "no arguments prints help");
    }
}
