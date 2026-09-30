//! `ssx hotkeys`: detect what works, print and install compositor bindings.
//!
//! The bindings come from the workflows in `settings.toml` that have a hotkey: pressing the
//! key runs `ssx run <workflow>`. This command **never edits your own configuration unless
//! you pass `--apply`**: sway and Hyprland get ssx's own include file (a file ssx owns) and the
//! line to add; GNOME and KDE store their shortcuts in desktop settings, so those only change
//! with `--apply`.

use std::collections::HashSet;

use serde::Serialize;
use ssx_core::settings::Settings;
use ssx_hotkeys::{
    Chord, Command, Desktop, SessionType, Strategy,
    bindings::{
        Dirs, SystemRunner, Target,
        conflict::check_main_config,
        files::{self, MainConfigChange},
        gnome, hyprland, kde, sway,
    },
    detect::{Environment, Platform, detect},
};

use crate::{
    app::{App, exe_path},
    cli::{HotkeyTarget, HotkeysCmd},
    error::{CliError, CliResult},
    output::{err_line, out_line, out_text},
};

/// The bindings for the workflows that have hotkeys, plus what had to be skipped and why.
pub fn bindings_from_settings(settings: &Settings, exe: &str) -> (Vec<(Chord, Command)>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for wf in &settings.workflows {
        let Some(hotkey) = wf.trigger.hotkey.as_deref() else { continue };
        if let Some(blocker) = crate::commands::run::run_blocker(wf) {
            // e.g. the recording workflows before recording exists: a key that only prints an
            // error would be worse than no key.
            tracing::info!("hotkey of workflow {:?} not bound: {}", wf.id, blocker.message);
            continue;
        }
        let chord: Chord = match hotkey.parse() {
            Ok(c) => c,
            Err(e) => {
                skipped.push(format!("workflow {:?}: hotkey {hotkey:?} skipped ({e})", wf.id));
                continue;
            }
        };
        if !seen.insert(chord) {
            skipped.push(format!(
                "workflow {:?}: hotkey {chord} skipped because an earlier workflow already uses it",
                wf.id
            ));
            continue;
        }
        let name = wf.trigger.cli_name.as_deref().unwrap_or(&wf.id);
        out.push((chord, Command::new(exe).args(["run", name]).label(format!("ssx: {}", wf.name))));
    }
    (out, skipped)
}

/// The generator target for a hotkey strategy, if it has one.
pub fn target_for(strategy: Strategy) -> Option<Target> {
    match strategy {
        Strategy::SwayConfig => Some(Target::Sway),
        Strategy::HyprlandConfig => Some(Target::Hyprland),
        Strategy::GnomeGsettings => Some(Target::Gnome),
        Strategy::KdeShortcuts => Some(Target::Kde),
        Strategy::GlobalHotkey | Strategy::Portal | Strategy::CliOnly => None,
    }
}

fn target_of(t: HotkeyTarget) -> Target {
    match t {
        HotkeyTarget::Sway => Target::Sway,
        HotkeyTarget::Hyprland => Target::Hyprland,
        HotkeyTarget::Gnome => Target::Gnome,
        HotkeyTarget::Kde => Target::Kde,
    }
}

/// Chooses the target: the flag, else what detection recommends.
fn choose_target(flag: Option<HotkeyTarget>) -> CliResult<Target> {
    if let Some(t) = flag {
        return Ok(target_of(t));
    }
    let d = detect(&Environment::from_env(), Platform::current());
    d.candidates.iter().find_map(|s| target_for(*s)).ok_or_else(|| {
        CliError::new("this session has no compositor or desktop that ssx can generate bindings for")
            .hint(format!(
                "pass --target sway|hyprland|gnome|kde; here hotkeys are handled {} (see `ssx hotkeys detect`)",
                match d.primary() {
                    Strategy::GlobalHotkey => "in-process by the ssx tray app",
                    Strategy::Portal => "by the desktop's GlobalShortcuts portal from the ssx tray app",
                    _ => "by binding `ssx run <workflow>` in your desktop's keyboard settings yourself",
                }
            ))
    })
}

/// Generated text for `target` (what `print` shows).
pub fn render(target: Target, bindings: &[(Chord, Command)]) -> CliResult<String> {
    let e = |e: ssx_hotkeys::bindings::BindingError| CliError::new(e.to_string());
    Ok(match target {
        Target::Sway => sway::render(bindings).map_err(e)?,
        Target::Hyprland => hyprland::render(bindings).map_err(e)?,
        Target::Gnome => {
            let entries = gnome::entries(bindings).map_err(e)?;
            gnome::script(&entries, &[])
        }
        Target::Kde => {
            let entries = kde::entries(bindings).map_err(e)?;
            let mut out = String::new();
            for entry in &entries {
                out.push_str(&format!("# {} ({})\n{}\n", entry.desktop_id, entry.shortcut, entry.desktop_file));
            }
            for c in kde::kwriteconfig_commands("kwriteconfig6", &entries) {
                out.push_str(&c.join(" "));
                out.push('\n');
            }
            out
        }
    })
}

#[derive(Serialize)]
struct DetectJson {
    desktop: String,
    session: String,
    strategies: Vec<String>,
    recommended: String,
}

/// Human names for detection results.
pub fn describe_detection(d: &ssx_hotkeys::Detection) -> (String, String) {
    let desktop = match d.desktop {
        Desktop::Gnome => "GNOME",
        Desktop::Kde => "KDE Plasma",
        Desktop::Sway => "sway",
        Desktop::Hyprland => "Hyprland",
        Desktop::Niri => "niri",
        Desktop::Cosmic => "COSMIC",
        Desktop::Xfce => "Xfce",
        Desktop::Other => "other",
        Desktop::Unknown => "unknown",
    };
    let session = match d.session {
        SessionType::X11 => "X11",
        SessionType::Wayland => "Wayland",
        SessionType::Unknown => "unknown",
    };
    (desktop.to_owned(), session.to_owned())
}

/// Describes a strategy in a sentence.
pub fn strategy_text(s: Strategy) -> &'static str {
    match s {
        Strategy::GlobalHotkey => "grab keys in the ssx app (Windows, macOS, X11)",
        Strategy::Portal => "the desktop's GlobalShortcuts portal, from the ssx app",
        Strategy::SwayConfig => "sway bindsym include file (`ssx hotkeys install`)",
        Strategy::HyprlandConfig => "Hyprland bind include file (`ssx hotkeys install`)",
        Strategy::GnomeGsettings => "GNOME custom keybindings (`ssx hotkeys install --apply`)",
        Strategy::KdeShortcuts => "KDE command shortcuts (`ssx hotkeys install --apply`)",
        Strategy::CliOnly => "bind `ssx run <workflow>` in your desktop's keyboard settings",
    }
}

fn dirs() -> CliResult<Dirs> {
    Dirs::from_env().ok_or_else(|| CliError::new("cannot find your home directory (HOME is not set)"))
}

fn binding_error(e: &ssx_hotkeys::bindings::BindingError) -> CliError {
    CliError::new(e.to_string())
}

/// Dispatches `ssx hotkeys ...`.
pub fn run(app: &App, cmd: HotkeysCmd) -> CliResult<()> {
    match cmd {
        HotkeysCmd::Detect { json } => {
            let d = detect(&Environment::from_env(), Platform::current());
            let (desktop, session) = describe_detection(&d);
            if json {
                out_line(&serde_json::to_string_pretty(&DetectJson {
                    desktop,
                    session,
                    strategies: d.candidates.iter().map(|s| format!("{s:?}")).collect(),
                    recommended: format!("{:?}", d.primary()),
                })?);
            } else {
                out_line(&format!("desktop: {desktop}, session: {session}"));
                for (i, s) in d.candidates.iter().enumerate() {
                    out_line(&format!("  {}. {}", i + 1, strategy_text(*s)));
                }
            }
            Ok(())
        }
        HotkeysCmd::Print { target, exe } => {
            let settings = app.load_settings()?;
            let target = choose_target(target)?;
            let (bindings, skipped) = bindings_from_settings(&settings, &exe);
            for s in &skipped {
                err_line(&format!("{} {s}", app.err.yellow("warning:")));
            }
            if bindings.is_empty() {
                return Err(CliError::new("no workflow has a hotkey")
                    .hint("give one a hotkey: ssx config set 'workflows[0].trigger.hotkey' Ctrl+Shift+S"));
            }
            out_text(&render(target, &bindings)?);
            Ok(())
        }
        HotkeysCmd::Install { target, apply, reload, exe } => install(app, target, apply, reload, exe),
        HotkeysCmd::Uninstall { target } => uninstall(app, target),
    }
}

fn install(app: &App, target: Option<HotkeyTarget>, apply: bool, reload: bool, exe: Option<String>) -> CliResult<()> {
    let settings = app.load_settings()?;
    let target = choose_target(target)?;
    let exe = match exe {
        Some(e) => e,
        None => exe_path()?.display().to_string(),
    };
    let (bindings, skipped) = bindings_from_settings(&settings, &exe);
    for s in &skipped {
        err_line(&format!("{} {s}", app.err.yellow("warning:")));
    }
    if bindings.is_empty() {
        return Err(CliError::new("no workflow has a hotkey")
            .hint("give one a hotkey: ssx config set 'workflows[0].trigger.hotkey' Ctrl+Shift+S"));
    }
    match target {
        Target::Sway | Target::Hyprland => {
            let dirs = dirs()?;
            let chords: Vec<Chord> = bindings.iter().map(|b| b.0).collect();
            for c in check_main_config(&dirs, target, &chords).map_err(|e| binding_error(&e))? {
                err_line(&format!(
                    "{} {} is already bound on line {} of your config: {}",
                    app.err.yellow("warning:"),
                    c.chord,
                    c.line_number,
                    c.line
                ));
            }
            let report = files::write_include_file(&dirs, target, &bindings).map_err(|e| binding_error(&e))?;
            out_line(&format!(
                "{} {}",
                if report.changed { "wrote" } else { "already up to date:" },
                report.path.display()
            ));
            if apply {
                match files::install_main_include(&dirs, target).map_err(|e| binding_error(&e))? {
                    MainConfigChange::Changed => out_line("added a removable ssx block to your config"),
                    MainConfigChange::Unchanged => out_line("your config already has the ssx block"),
                    MainConfigChange::AlreadyIncludedManually => {
                        out_line("your config already includes the file by hand");
                    }
                }
            } else {
                out_line(&format!(
                    "add this line to your {target} config, or re-run with --apply:\n\n    {}\n",
                    report.include_line
                ));
            }
            if reload {
                files::reload(&SystemRunner, target).map_err(|e| binding_error(&e))?;
                out_line("reloaded");
            }
        }
        Target::Gnome | Target::Kde => {
            if !apply {
                out_text(&render(target, &bindings)?);
                err_line(&format!(
                    "nothing was changed: {target} keeps shortcuts in its own settings. Run again with --apply to make these changes."
                ));
                return Ok(());
            }
            if target == Target::Gnome {
                let r = gnome::apply(&SystemRunner, &bindings).map_err(|e| binding_error(&e))?;
                out_line(&if r.is_noop() {
                    "GNOME keybindings were already up to date".to_owned()
                } else {
                    format!("registered {} GNOME setting(s)", r.values_written)
                });
            } else {
                let r = kde::apply(&dirs()?, &SystemRunner, &bindings).map_err(|e| binding_error(&e))?;
                out_line(&format!(
                    "wrote {} launcher(s), registered {} shortcut(s)",
                    r.desktop_files_written, r.shortcuts_registered
                ));
                if reload {
                    kde::reload(&SystemRunner).map_err(|e| binding_error(&e))?;
                    out_line("reloaded the shortcut daemon");
                } else {
                    err_line("shortcuts become active after the next login (or run again with --reload)");
                }
            }
        }
    }
    Ok(())
}

fn uninstall(_app: &App, target: Option<HotkeyTarget>) -> CliResult<()> {
    let target = choose_target(target)?;
    match target {
        Target::Sway | Target::Hyprland => {
            files::uninstall(&dirs()?, target).map_err(|e| binding_error(&e))?;
            out_line(&format!("removed the ssx block and include file for {target}"));
        }
        Target::Gnome => {
            let r = gnome::remove(&SystemRunner).map_err(|e| binding_error(&e))?;
            out_line(&if r.is_noop() {
                "no ssx keybindings were registered".to_owned()
            } else {
                "removed the ssx GNOME keybindings".to_owned()
            });
        }
        Target::Kde => {
            kde::remove(&dirs()?, &SystemRunner).map_err(|e| binding_error(&e))?;
            out_line("removed the ssx KDE shortcuts");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ssx_core::settings::Workflow;

    use super::*;

    fn settings_with(hotkeys: &[(&str, &str, Option<&str>)]) -> Settings {
        let mut s = Settings::default();
        s.workflows = hotkeys
            .iter()
            .map(|(id, key, cli)| {
                let mut w = Workflow { id: (*id).into(), name: format!("Name of {id}"), ..Workflow::default() };
                w.trigger.hotkey = Some((*key).to_owned());
                w.trigger.cli_name = cli.map(str::to_owned);
                w
            })
            .collect();
        s
    }

    #[test]
    fn default_workflows_become_run_commands() {
        let (b, skipped) = bindings_from_settings(&Settings::default(), "ssx");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(b.len() >= 4);
        let (chord, cmd) = &b[0];
        assert_eq!(chord.to_string(), "Ctrl+Print");
        assert_eq!(cmd.program(), "ssx");
        assert_eq!(cmd.arguments(), ["run", "region"], "the CLI name is preferred over the id");
        assert!(cmd.display_name().contains("Capture region"));
    }

    #[test]
    fn bad_and_duplicate_hotkeys_are_skipped_with_a_reason() {
        let s = settings_with(&[
            ("a", "Ctrl+Shift+S", None),
            ("b", "ctrl + shift + s", Some("b-cli")),
            ("c", "NotAKey", None),
            ("d", "F9", Some("d")),
        ]);
        let (b, skipped) = bindings_from_settings(&s, "/opt/ssx");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1.arguments(), ["run", "a"], "falls back to the id");
        assert_eq!(b[1].1.program(), "/opt/ssx");
        assert_eq!(skipped.len(), 2);
        assert!(skipped[0].contains("already uses it") && skipped[0].contains("\"b\""), "{skipped:?}");
        assert!(skipped[1].contains("NotAKey"));
    }

    #[test]
    fn strategies_map_to_generators() {
        assert_eq!(target_for(Strategy::SwayConfig), Some(Target::Sway));
        assert_eq!(target_for(Strategy::KdeShortcuts), Some(Target::Kde));
        assert_eq!(target_for(Strategy::GlobalHotkey), None);
        assert_eq!(target_for(Strategy::Portal), None);
    }

    #[test]
    fn rendering_works_for_every_target() {
        let (b, _) = bindings_from_settings(&Settings::default(), "ssx");
        assert!(render(Target::Sway, &b).unwrap().contains("bindsym Ctrl+Print exec ssx run region"));
        assert!(render(Target::Hyprland, &b).unwrap().contains("ssx run region"));
        assert!(render(Target::Gnome, &b).unwrap().contains("gsettings"));
        let kde = render(Target::Kde, &b).unwrap();
        assert!(kde.contains("kwriteconfig6") && kde.contains("Exec=ssx run region"), "{kde}");
    }
}
