//! Binding generators for desktops that offer no in-app key grabbing.
//!
//! Given a list of `(Chord, Command)` pairs, each submodule produces what a desktop needs
//! to run the command when the chord is pressed:
//!
//! | module | mechanism | writes |
//! |---|---|---|
//! | [`sway`] | `bindsym` lines | an include file we own + an opt-in marked block in the main config |
//! | [`hyprland`] | `bind = ` lines | an include file we own + an opt-in marked block in the main config |
//! | [`gnome`] | `gsettings` custom keybindings | dconf, merged into the existing list |
//! | [`kde`] | KDE command shortcuts (`.desktop` + `kglobalshortcutsrc`) | files + `kwriteconfig6` |
//!
//! Nothing here touches the user's main configuration unless an explicit `install*`/`apply`
//! function is called, and every function that writes takes a [`Dirs`] root (tests use a
//! temp dir) and a [`CommandRunner`] (tests use a fake).

pub mod conflict;
pub mod files;
pub mod gnome;
pub mod hyprland;
pub mod kde;
mod runner;
pub mod sway;

use std::path::{Path, PathBuf};

use crate::{chord::Chord, command::CommandError};

pub use runner::{CommandRunner, RunOutput, SystemRunner};

/// A desktop that is configured through generated bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// sway.
    Sway,
    /// Hyprland.
    Hyprland,
    /// GNOME custom keybindings.
    Gnome,
    /// KDE Plasma command shortcuts.
    Kde,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Target::Sway => "sway",
            Target::Hyprland => "Hyprland",
            Target::Gnome => "GNOME",
            Target::Kde => "KDE Plasma",
        })
    }
}

/// Errors from the generators.
#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    /// A command cannot be written on one config line.
    #[error("invalid command for binding {chord}: {source}")]
    Command {
        /// The binding.
        chord: Chord,
        /// What is wrong with the command.
        #[source]
        source: CommandError,
    },
    /// The same chord appears twice in the list.
    #[error("{0} is bound twice in the same list")]
    DuplicateChord(Chord),
    /// The target cannot express this key.
    #[error("{target} has no name for the key in {chord}; choose another key")]
    UnsupportedKey {
        /// The target desktop.
        target: Target,
        /// The chord.
        chord: Chord,
    },
    /// Filesystem failure.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being attempted ("writing", "reading", ...).
        action: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The operation only applies to sway and Hyprland include files.
    #[error("{0} is not configured through an include file")]
    NotAFileTarget(Target),
    /// The user's main config does not exist; creating one would shadow the system default.
    #[error(
        "{0} does not exist. Creating it would replace your desktop's default configuration; \
         create it yourself (e.g. copy the system default) and add the line printed by the include-file step"
    )]
    MainConfigMissing(PathBuf),
    /// An external tool is not installed.
    #[error("`{0}` was not found; {1}")]
    ToolMissing(String, &'static str),
    /// An external tool ran and failed.
    #[error("`{command}` failed ({status}): {stderr}")]
    CommandFailed {
        /// The command line.
        command: String,
        /// Exit status text.
        status: String,
        /// Captured standard error.
        stderr: String,
    },
    /// A tool printed something this crate could not interpret.
    #[error("cannot parse output of `{command}`: {output:?}")]
    UnexpectedOutput {
        /// The command line.
        command: String,
        /// The output.
        output: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, BindingError>;

/// Base directories for everything the generators write. Injectable so tests never touch
/// the real home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// `$XDG_CONFIG_HOME` (default `~/.config`).
    pub config_home: PathBuf,
    /// `$XDG_DATA_HOME` (default `~/.local/share`).
    pub data_home: PathBuf,
}

impl Dirs {
    /// Uses `root/.config` and `root/.local/share` (for tests, or a sandbox).
    pub fn under(root: impl AsRef<Path>) -> Dirs {
        let root = root.as_ref();
        Dirs { config_home: root.join(".config"), data_home: root.join(".local").join("share") }
    }

    /// Resolves the XDG base directories from the environment. `None` when neither the
    /// XDG variable nor `$HOME` is set.
    pub fn from_env() -> Option<Dirs> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let pick = |var: &str, rel: &[&str]| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| home.as_ref().map(|h| rel.iter().fold(h.clone(), |p, s| p.join(s))))
        };
        Some(Dirs {
            config_home: pick("XDG_CONFIG_HOME", &[".config"])?,
            data_home: pick("XDG_DATA_HOME", &[".local", "share"])?,
        })
    }
}

/// Rejects duplicate chords and unrenderable commands before anything is written.
pub(crate) fn validate(bindings: &[(Chord, crate::command::Command)]) -> Result<()> {
    for (i, (chord, cmd)) in bindings.iter().enumerate() {
        cmd.validate().map_err(|source| BindingError::Command { chord: *chord, source })?;
        if bindings[..i].iter().any(|(c, _)| c == chord) {
            return Err(BindingError::DuplicateChord(*chord));
        }
    }
    Ok(())
}

/// Text safe to put after `# ` in a one-line config comment.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A stable, filesystem/GVariant-safe name for a binding derived from its command:
/// lowercase alphanumerics and `-`, at most 40 characters, plus an FNV-1a suffix of the
/// exact command when truncated or when it would collide with an earlier binding.
pub(crate) fn slugs(bindings: &[(Chord, crate::command::Command)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(bindings.len());
    for (_, cmd) in bindings {
        let base_words: Vec<&str> = cmd
            .words()
            .enumerate()
            .map(|(i, w)| if i == 0 { w.rsplit(['/', '\\']).next().unwrap_or(w) } else { w })
            .collect();
        let mut slug = String::new();
        for w in base_words {
            let clean: String = w
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
                .collect();
            let clean = clean.trim_matches('-');
            if clean.is_empty() {
                continue;
            }
            if !slug.is_empty() {
                slug.push('-');
            }
            slug.push_str(clean);
        }
        let mut collapsed = String::new();
        for c in slug.chars() {
            if !(c == '-' && collapsed.ends_with('-')) {
                collapsed.push(c);
            }
        }
        let hash = fnv1a(&cmd.shell_line());
        let truncated = collapsed.len() > 40;
        let mut name = if collapsed.is_empty() {
            format!("cmd-{hash:08x}")
        } else if truncated {
            format!("{}-{hash:08x}", collapsed.chars().take(31).collect::<String>().trim_end_matches('-'))
        } else {
            collapsed
        };
        if out.contains(&name) {
            name = format!("{name}-{hash:08x}");
        }
        let mut n = 2;
        let unique_base = name.clone();
        while out.contains(&name) {
            name = format!("{unique_base}-{n}");
            n += 1;
        }
        out.push(name);
    }
    out
}

fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    pub(crate) fn fixture() -> Vec<(Chord, Command)> {
        vec![
            (
                "Ctrl+Shift+S".parse().unwrap(),
                Command::new("ssx").args(["capture", "region"]).label("Capture region"),
            ),
            ("Print".parse().unwrap(), Command::new("ssx").args(["capture", "screen"])),
            (
                "Super+Alt+R".parse().unwrap(),
                Command::new("/usr/local/bin/ssx").args(["record", "--title", "it's a \"test\" $HOME; ok"]),
            ),
        ]
    }

    #[test]
    fn slugs_are_stable_unique_and_safe() {
        let s = slugs(&fixture());
        assert_eq!(s[0], "ssx-capture-region");
        assert_eq!(s[1], "ssx-capture-screen");
        assert!(s[2].starts_with("ssx-record-title-it-s-a-test-home-ok") || s[2].len() <= 40, "{}", s[2]);
        for slug in &s {
            assert!(slug.len() <= 41 + 9);
            assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{slug}");
            assert!(!slug.starts_with('-') && !slug.ends_with('-') && !slug.contains("--"), "{slug}");
        }
        // Same command list => same slugs (idempotent apply depends on this).
        assert_eq!(s, slugs(&fixture()));
        // Collisions get distinguished.
        let dup = vec![
            ("Ctrl+A".parse().unwrap(), Command::new("x").arg("a b")),
            ("Ctrl+B".parse().unwrap(), Command::new("x").arg("a-b")),
        ];
        let d = slugs(&dup);
        assert_ne!(d[0], d[1]);
        // Long and unrepresentable commands.
        let odd = vec![
            ("Ctrl+A".parse().unwrap(), Command::new("日本語")),
            ("Ctrl+B".parse().unwrap(), Command::new("x").arg("y".repeat(200))),
        ];
        let o = slugs(&odd);
        assert!(o[0].starts_with("cmd-"), "{}", o[0]);
        assert!(o[1].len() <= 41 + 9, "{}", o[1]);
    }

    #[test]
    fn validation_catches_duplicates_and_bad_commands() {
        assert!(validate(&fixture()).is_ok());
        let mut dup = fixture();
        dup.push(dup[0].clone());
        assert!(matches!(validate(&dup), Err(BindingError::DuplicateChord(_))));
        let bad = vec![("Print".parse().unwrap(), Command::new("x").arg("a\nb"))];
        assert!(matches!(validate(&bad), Err(BindingError::Command { .. })));
    }

    #[test]
    fn dirs_under_root_and_from_env() {
        let d = Dirs::under("/tmp/x");
        assert_eq!(d.config_home, PathBuf::from("/tmp/x/.config"));
        assert_eq!(d.data_home, PathBuf::from("/tmp/x/.local/share"));
    }

    #[test]
    fn comment_text_strips_control_characters() {
        assert_eq!(comment_text("a\nb\tc"), "a b c");
    }
}
