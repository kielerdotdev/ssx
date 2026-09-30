//! KDE Plasma: "command shortcuts", the mechanism System Settings itself uses.
//!
//! **What was researched and chosen.** System Settings > Keyboard > Shortcuts > *Add
//! Command...* (Plasma 5.27 / 6) creates, per shortcut:
//!
//! 1. a launcher `~/.local/share/applications/net.local.<name>.desktop` containing
//!    `Exec=<command>`, `NoDisplay=true` and `X-KDE-GlobalAccel-CommandShortcut=true`, and
//! 2. an entry in `~/.config/kglobalshortcutsrc`, group `[services][net.local.<name>.desktop]`,
//!    key `_launch=<Qt key sequence>`.
//!
//! `kglobalacceld` treats a component named after a `.desktop` file as an application
//! launcher and runs its `Exec` when the shortcut fires. This module writes exactly those
//! two things: the desktop files directly (a plain, fully testable file write), and the
//! `kglobalshortcutsrc` entries through `kwriteconfig6` (falling back to `kwriteconfig5`),
//! the supported way to edit KConfig files without corrupting them. Files/entries owned by
//! ssx are named `net.local.ssx-<slug>.desktop`.
//!
//! **Alternatives not chosen.** A bare `X-KDE-Shortcuts=` key in a `.desktop` file is
//! documented as a *default* shortcut for an application's own launch action, not as a way
//! to register arbitrary commands, so it is not relied on. `khotkeys` (`khotkeysrc`) is the
//! legacy Plasma 5 mechanism that Plasma 6 dropped. Calling `org.kde.KGlobalAccel` over
//! D-Bus needs Qt key *integers* and is lightly documented, so a wrong guess would be
//! silent. (On Wayland, prefer the GlobalShortcuts portal, which Plasma implements; this
//! route is the CLI-driven fallback.)
//!
//! **Caveats.** The running `kglobalacceld` reads `kglobalshortcutsrc` at start-up, so
//! new shortcuts become active after a reload ([`reload`] restarts
//! `plasma-kglobalaccel.service`) or the next login. If the key sequence is already taken
//! by another shortcut, Plasma keeps the older owner and the new one shows as unassigned
//! in System Settings. This module was validated against fakes and the documented file
//! formats; it could not be run against a live Plasma session here.

use std::{fs, path::PathBuf};

use crate::{
    chord::Chord,
    command::{Command, desktop_exec_value},
};

use super::{
    BindingError, CommandRunner, Dirs, Result, Target, comment_text, runner::run_checked, slugs,
    validate,
};

/// Prefix of the launcher name; matches what System Settings uses for command shortcuts.
pub const NAME_PREFIX: &str = "net.local.ssx-";

const HINT: &str = "install KDE Frameworks' `kwriteconfig6` (package kde-cli-tools / kconfig) and run this inside a Plasma session";

/// One command shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KdeEntry {
    /// Launcher id, e.g. `net.local.ssx-capture-region.desktop`.
    pub desktop_id: String,
    /// Contents of the launcher file.
    pub desktop_file: String,
    /// Qt key sequence, e.g. `Ctrl+Shift+S`.
    pub shortcut: String,
}

impl KdeEntry {
    /// Group name for `kglobalshortcutsrc`: `net.local.ssx-x.desktop`.
    pub fn component(&self) -> &str {
        &self.desktop_id
    }
}

/// Builds the entries. Fails for keys KDE has no name for (keypad keys).
pub fn entries(bindings: &[(Chord, Command)]) -> Result<Vec<KdeEntry>> {
    validate(bindings)?;
    bindings
        .iter()
        .zip(slugs(bindings))
        .map(|((chord, cmd), slug)| {
            let shortcut = chord
                .to_qt_sequence()
                .ok_or(BindingError::UnsupportedKey { target: Target::Kde, chord: *chord })?;
            let desktop_id =
                format!("{NAME_PREFIX}{}.desktop", slug.strip_prefix("ssx-").unwrap_or(&slug));
            Ok(KdeEntry { desktop_file: desktop_file(cmd), desktop_id, shortcut })
        })
        .collect()
}

/// The launcher `.desktop` file for a command.
pub fn desktop_file(cmd: &Command) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         Exec={}\n\
         NoDisplay=true\n\
         StartupNotify=false\n\
         Terminal=false\n\
         X-KDE-GlobalAccel-CommandShortcut=true\n",
        desktop_string(&cmd.display_name()),
        desktop_exec_value(cmd),
    )
}

/// Escapes a `string` value for a `.desktop` file (backslash, control characters).
fn desktop_string(s: &str) -> String {
    comment_text(s).replace('\\', "\\\\")
}

/// The applications directory the launchers go to.
pub fn applications_dir(dirs: &Dirs) -> PathBuf {
    dirs.data_home.join("applications")
}

/// The `kwriteconfig` invocations that register the shortcuts (program first).
pub fn kwriteconfig_commands(tool: &str, entries: &[KdeEntry]) -> Vec<Vec<String>> {
    entries
        .iter()
        .map(|e| {
            [
                tool,
                "--file",
                "kglobalshortcutsrc",
                "--group",
                "services",
                "--group",
                e.component(),
                "--key",
                "_launch",
                &e.shortcut,
            ]
            .map(str::to_owned)
            .to_vec()
        })
        .collect()
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyReport {
    /// Launcher files written or updated.
    pub desktop_files_written: usize,
    /// `kwriteconfig` calls made.
    pub shortcuts_registered: usize,
}

fn io_err(
    action: &'static str,
    path: &std::path::Path,
) -> impl FnOnce(std::io::Error) -> BindingError {
    let path = path.to_owned();
    move |source| BindingError::Io { action, path, source }
}

/// Picks `kwriteconfig6`, falling back to `kwriteconfig5` when 6 is not installed.
fn kwriteconfig_tool(runner: &dyn CommandRunner) -> &'static str {
    match runner.run("kwriteconfig6", &["--help".to_owned()]) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "kwriteconfig5",
        _ => "kwriteconfig6",
    }
}

/// Writes the launchers and registers the shortcuts. Idempotent: unchanged launchers are
/// not rewritten, and `kwriteconfig` sets the same value again harmlessly.
pub fn apply(
    dirs: &Dirs,
    runner: &dyn CommandRunner,
    bindings: &[(Chord, Command)],
) -> Result<ApplyReport> {
    let entries = entries(bindings)?;
    let apps = applications_dir(dirs);
    fs::create_dir_all(&apps).map_err(io_err("creating", &apps))?;
    let mut report = ApplyReport::default();
    for e in &entries {
        let path = apps.join(&e.desktop_id);
        if fs::read_to_string(&path).is_ok_and(|t| t == e.desktop_file) {
            continue;
        }
        fs::write(&path, &e.desktop_file).map_err(io_err("writing", &path))?;
        report.desktop_files_written += 1;
    }
    let tool = kwriteconfig_tool(runner);
    for cmd in kwriteconfig_commands(tool, &entries) {
        run_checked(runner, &cmd[0], &cmd[1..], HINT)?;
        report.shortcuts_registered += 1;
    }
    Ok(report)
}

/// Removes every ssx-owned launcher and its `kglobalshortcutsrc` entry.
pub fn remove(dirs: &Dirs, runner: &dyn CommandRunner) -> Result<ApplyReport> {
    let apps = applications_dir(dirs);
    let mut ids = Vec::new();
    match fs::read_dir(&apps) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(NAME_PREFIX) && name.ends_with(".desktop") {
                    ids.push(name);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("reading", &apps)(e)),
    }
    ids.sort();
    let mut report = ApplyReport::default();
    if ids.is_empty() {
        return Ok(report);
    }
    let tool = kwriteconfig_tool(runner);
    for id in &ids {
        // Unregister first, so a failure leaves the launcher (and a retry possible).
        let args: Vec<String> = [
            "--file",
            "kglobalshortcutsrc",
            "--group",
            "services",
            "--group",
            id,
            "--key",
            "_launch",
            "--delete",
        ]
        .map(str::to_owned)
        .to_vec();
        run_checked(runner, tool, &args, HINT)?;
        report.shortcuts_registered += 1;
        let path = apps.join(id);
        fs::remove_file(&path).map_err(io_err("removing", &path))?;
        report.desktop_files_written += 1;
    }
    Ok(report)
}

/// Restarts `kglobalacceld` so it re-reads `kglobalshortcutsrc` (explicit, opt-in: it
/// briefly drops every global shortcut in the session).
pub fn reload(runner: &dyn CommandRunner) -> Result<()> {
    let args = ["--user", "restart", "plasma-kglobalaccel.service"].map(str::to_owned);
    run_checked(
        runner,
        "systemctl",
        &args,
        "run this inside a systemd user session, or log out and in again",
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{RunOutput, runner::fake::Fake, tests::fixture};

    fn temp(tag: &str) -> Dirs {
        let root =
            std::env::temp_dir().join(format!("ssx-hotkeys-kde-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Dirs::under(&root)
    }

    #[test]
    fn entries_use_kde_naming_and_qt_sequences() {
        let e = entries(&fixture()).unwrap();
        assert_eq!(e[0].desktop_id, "net.local.ssx-capture-region.desktop");
        assert_eq!(e[0].shortcut, "Ctrl+Shift+S");
        assert_eq!(e[1].shortcut, "Print");
        assert_eq!(e[2].shortcut, "Alt+Meta+R");
    }

    #[test]
    fn desktop_file_matches_what_system_settings_writes() {
        let f =
            desktop_file(&Command::new("ssx").args(["capture", "region"]).label("Capture region"));
        assert_eq!(
            f,
            "[Desktop Entry]\nType=Application\nName=Capture region\nExec=ssx capture region\n\
             NoDisplay=true\nStartupNotify=false\nTerminal=false\nX-KDE-GlobalAccel-CommandShortcut=true\n"
        );
        let hostile =
            desktop_file(&Command::new("ssx").arg("a b").arg("%f").label("line1\nX-Injected=1"));
        assert!(hostile.contains("Exec=ssx \"a b\" %%f\n"), "{hostile}");
        assert_eq!(hostile.matches('\n').count(), 8, "a newline in the label must not add a line");
        assert!(!hostile.contains("\nX-Injected"));
    }

    #[test]
    fn keypad_keys_are_rejected_with_a_clear_error() {
        let b = vec![("Ctrl+Numpad1".parse().unwrap(), Command::new("x"))];
        let e = entries(&b).unwrap_err();
        assert!(matches!(e, BindingError::UnsupportedKey { target: Target::Kde, .. }), "{e}");
    }

    #[test]
    fn kwriteconfig_invocations_are_exact() {
        let e = entries(&fixture()[..1]).unwrap();
        let cmds: Vec<String> =
            kwriteconfig_commands("kwriteconfig6", &e).iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            cmds,
            [
                "kwriteconfig6 --file kglobalshortcutsrc --group services --group net.local.ssx-capture-region.desktop --key _launch Ctrl+Shift+S"
            ]
        );
    }

    #[test]
    fn apply_writes_files_and_registers_and_is_idempotent() {
        let d = temp("apply");
        let fake = Fake::default();
        let r = apply(&d, &fake, &fixture()).unwrap();
        assert_eq!(r, ApplyReport { desktop_files_written: 3, shortcuts_registered: 3 });
        let apps = applications_dir(&d);
        let text = fs::read_to_string(apps.join("net.local.ssx-capture-region.desktop")).unwrap();
        assert!(text.contains("X-KDE-GlobalAccel-CommandShortcut=true"));
        let kw: Vec<String> =
            fake.calls_text().into_iter().filter(|c| c.contains("--key")).collect();
        assert_eq!(kw.len(), 3);
        // Second run: no file rewrites; registration is re-sent (kwriteconfig is idempotent).
        let r2 = apply(&d, &fake, &fixture()).unwrap();
        assert_eq!(r2.desktop_files_written, 0);
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 3);
    }

    #[test]
    fn falls_back_to_kwriteconfig5() {
        struct Only5(std::cell::RefCell<Vec<String>>);
        impl CommandRunner for Only5 {
            fn run(&self, program: &str, args: &[String]) -> std::io::Result<RunOutput> {
                self.0.borrow_mut().push(format!("{program} {}", args.join(" ")));
                if program == "kwriteconfig6" {
                    Err(std::io::ErrorKind::NotFound.into())
                } else {
                    Ok(RunOutput::ok(""))
                }
            }
        }
        let d = temp("kw5");
        let r = Only5(std::cell::RefCell::default());
        apply(&d, &r, &fixture()[..1]).unwrap();
        assert!(
            r.0.borrow().iter().any(|c| c.starts_with("kwriteconfig5 --file kglobalshortcutsrc"))
        );
    }

    #[test]
    fn remove_only_touches_owned_launchers() {
        let d = temp("remove");
        let fake = Fake::default();
        apply(&d, &fake, &fixture()).unwrap();
        let apps = applications_dir(&d);
        fs::write(apps.join("net.local.other.desktop"), "x").unwrap();
        fs::write(apps.join("firefox.desktop"), "x").unwrap();
        let fake2 = Fake::default();
        let r = remove(&d, &fake2).unwrap();
        assert_eq!(r.desktop_files_written, 3);
        let mut left: Vec<String> = fs::read_dir(&apps)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["firefox.desktop", "net.local.other.desktop"]);
        assert!(
            fake2
                .calls_text()
                .iter()
                .filter(|c| c.contains("--file"))
                .all(|c| c.contains("--delete") && c.contains("net.local.ssx-"))
        );
        assert_eq!(remove(&d, &Fake::default()).unwrap(), ApplyReport::default());
    }

    #[test]
    fn missing_tool_is_reported() {
        struct Missing;
        impl CommandRunner for Missing {
            fn run(&self, _: &str, _: &[String]) -> std::io::Result<RunOutput> {
                Err(std::io::ErrorKind::NotFound.into())
            }
        }
        let d = temp("missing");
        let e = apply(&d, &Missing, &fixture()).unwrap_err();
        assert!(matches!(e, BindingError::ToolMissing(..)), "{e}");
    }

    #[test]
    fn reload_restarts_the_user_service() {
        let f = Fake::default();
        reload(&f).unwrap();
        assert_eq!(f.calls_text(), ["systemctl --user restart plasma-kglobalaccel.service"]);
    }
}
