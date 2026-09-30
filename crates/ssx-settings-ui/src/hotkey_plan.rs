//! Everything the Hotkeys page knows that does not need a screen: duplicate hotkeys inside
//! the settings, clashes with the desktop's own configuration, how hotkeys reach ssx on this
//! session, the generated compositor snippets, and the explicit "apply" and "remove" actions.
//!
//! **Nothing here edits a compositor configuration by itself.** [`apply`] and [`remove`] are
//! only called from a confirmed button. For sway and Hyprland "apply" writes the include file
//! ssx owns (`config.d/ssx.conf` / `ssx.conf`) and, only if the caller asks, the marked
//! removable block in the user's main config; for GNOME and KDE it registers ssx-owned
//! custom shortcuts. `ssx_hotkeys` does the writing; this module chooses what to call.

use std::collections::BTreeMap;

use ssx_cli::commands::hotkeys::{bindings_from_settings, render as render_snippet, target_for};
use ssx_core::settings::{Hotkey, Settings};
use ssx_hotkeys::{
    Chord, Command, Desktop, Detection, Environment, SessionType, Strategy,
    bindings::{
        BindingError, CommandRunner, Dirs, Target,
        conflict::{Conflict, check_main_config},
        files::{self, MainConfigChange},
        gnome, kde,
    },
};

// ---- duplicates inside the settings -------------------------------------------------------

/// Who owns a hotkey.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// `workflows[index]`.
    Workflow {
        /// Position in the list.
        index: usize,
        /// The workflow's name (for messages).
        name: String,
    },
    /// One of the global hotkeys (`hotkeys.open_history`, ...).
    Global(&'static str),
}

impl Owner {
    /// The validator path of this owner's hotkey field.
    pub fn path(&self) -> String {
        match self {
            Owner::Workflow { index, .. } => format!("workflows[{index}].trigger.hotkey"),
            Owner::Global(name) => (*name).to_owned(),
        }
    }

    /// A short label.
    pub fn label(&self) -> String {
        match self {
            Owner::Workflow { name, .. } => name.clone(),
            Owner::Global(name) => match *name {
                "hotkeys.open_history" => "Open history window".to_owned(),
                "hotkeys.open_settings" => "Open settings window".to_owned(),
                "hotkeys.pause_recording" => "Pause / resume recording".to_owned(),
                other => other.to_owned(),
            },
        }
    }
}

/// One hotkey and everything that uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// The shared hotkey (canonical spelling).
    pub hotkey: String,
    /// The owners, at least two.
    pub owners: Vec<Owner>,
}

/// Every configured hotkey with its owner, unparsable ones excluded.
pub fn bindings(settings: &Settings) -> Vec<(Owner, Hotkey)> {
    let mut out = Vec::new();
    for (index, w) in settings.workflows.iter().enumerate() {
        if let Some(hk) = w.trigger.hotkey.as_deref().and_then(|h| h.parse::<Hotkey>().ok()) {
            out.push((Owner::Workflow { index, name: w.name.clone() }, hk));
        }
    }
    for (name, text) in settings.hotkeys.entries() {
        if let Ok(hk) = text.parse::<Hotkey>() {
            out.push((Owner::Global(name), hk));
        }
    }
    out
}

/// Hotkeys used more than once, compared in canonical form (`Shift+Ctrl+PrtSc` equals
/// `ctrl+shift+printscreen`).
pub fn duplicates(settings: &Settings) -> Vec<DuplicateGroup> {
    let mut by_key: BTreeMap<Hotkey, Vec<Owner>> = BTreeMap::new();
    for (owner, hk) in bindings(settings) {
        by_key.entry(hk).or_default().push(owner);
    }
    by_key
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(hk, owners)| DuplicateGroup { hotkey: hk.to_string(), owners })
        .collect()
}

/// The other owners of the hotkey `owner` has (empty when it is unique or unset).
pub fn others_using(settings: &Settings, owner: &Owner) -> Vec<Owner> {
    let all = bindings(settings);
    let Some((_, mine)) = all.iter().find(|(o, _)| o == owner) else { return Vec::new() };
    all.iter().filter(|(o, hk)| hk == mine && o != owner).map(|(o, _)| o.clone()).collect()
}

/// The chords of every hotkey in `settings` that the hotkey manager can parse.
pub fn chords(settings: &Settings) -> Vec<Chord> {
    let mut out: Vec<Chord> = bindings(settings)
        .into_iter()
        .filter_map(|(_, hk)| hk.to_string().parse::<Chord>().ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

// ---- clashes with the desktop's own configuration -----------------------------------------

/// What could be checked about the desktop's own key bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCheck {
    /// The main config was read; these lines already use one of our keys.
    Checked {
        /// Which config.
        target: Target,
        /// The clashes (empty = none found).
        conflicts: Vec<Conflict>,
    },
    /// This desktop's bindings cannot be read by ssx.
    NotDetectable(String),
    /// Reading failed.
    Failed(String),
}

/// The generator target for the session, if there is one.
pub fn session_target(detection: &Detection) -> Option<Target> {
    detection.candidates.iter().find_map(|s| target_for(*s))
}

/// Looks for clashes between our hotkeys and what the session already binds. Only sway and
/// Hyprland configs are readable; GNOME and KDE keep their bindings in dconf / kglobalshortcuts.
pub fn check_session(
    detection: &Detection,
    dirs: Option<&Dirs>,
    settings: &Settings,
) -> SessionCheck {
    let Some(target) = session_target(detection) else {
        return SessionCheck::NotDetectable(match detection.primary() {
            Strategy::GlobalHotkey | Strategy::Portal => {
                "ssx grabs its keys itself here; if another program already owns a key, registering it fails and the tray app reports it".to_owned()
            }
            _ => "this desktop has no configuration ssx can read".to_owned(),
        });
    };
    match target {
        Target::Gnome | Target::Kde => SessionCheck::NotDetectable(format!(
            "{target} keeps its shortcuts in its own settings database, which ssx cannot search for clashes; check System Settings > Keyboard > Shortcuts after applying"
        )),
        Target::Sway | Target::Hyprland => {
            let Some(dirs) = dirs else {
                return SessionCheck::Failed("cannot find your home directory".to_owned());
            };
            match check_main_config(dirs, target, &chords(settings)) {
                Ok(conflicts) => SessionCheck::Checked { target, conflicts },
                Err(e) => SessionCheck::Failed(e.to_string()),
            }
        }
    }
}

// ---- how hotkeys reach ssx here -----------------------------------------------------------

/// How to explain the strategy of this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyInfo {
    /// The desktop as a name.
    pub desktop: &'static str,
    /// X11 / Wayland / unknown.
    pub session: &'static str,
    /// One line: what happens.
    pub headline: String,
    /// The details, one sentence each.
    pub details: Vec<String>,
    /// `true` if ssx registers the keys itself (nothing for the user to do).
    pub automatic: bool,
    /// The generator targets that make sense here, best first.
    pub targets: Vec<Target>,
}

/// Human name of a desktop.
pub const fn desktop_name(d: Desktop) -> &'static str {
    match d {
        Desktop::Gnome => "GNOME",
        Desktop::Kde => "KDE Plasma",
        Desktop::Sway => "sway",
        Desktop::Hyprland => "Hyprland",
        Desktop::Niri => "niri",
        Desktop::Cosmic => "COSMIC",
        Desktop::Xfce => "Xfce",
        Desktop::Other => "another desktop",
        Desktop::Unknown => "an unknown desktop",
    }
}

/// Human name of a session type.
pub const fn session_name(s: SessionType) -> &'static str {
    match s {
        SessionType::X11 => "X11",
        SessionType::Wayland => "Wayland",
        SessionType::Unknown => "unknown session",
    }
}

/// Explains what `detection` means for the user.
pub fn explain(detection: &Detection, env: &Environment) -> StrategyInfo {
    let targets: Vec<Target> = detection.candidates.iter().filter_map(|s| target_for(*s)).collect();
    let desktop = desktop_name(detection.desktop);
    let session = session_name(detection.session);
    let (headline, details, automatic) = match detection.primary() {
        Strategy::GlobalHotkey => (
            "ssx registers your shortcuts itself".to_owned(),
            vec![
                if cfg!(windows) {
                    "Global hotkeys are registered with Windows (RegisterHotKey) by the ssx tray app; nothing else is needed."
                        .to_owned()
                } else if cfg!(target_os = "macos") {
                    "Global hotkeys are registered with macOS by the ssx tray app; macOS may ask for permission the first time."
                        .to_owned()
                } else {
                    "On X11 the ssx tray app grabs the keys directly, like any global-hotkey program.".to_owned()
                },
                "A key that another program already owns cannot be registered; the tray app reports which one.".to_owned(),
            ],
            true,
        ),
        Strategy::Portal => (
            format!("{desktop} registers your shortcuts through the XDG GlobalShortcuts portal"),
            vec![
                "ssx asks the desktop for each shortcut; the key you choose is only a suggestion and the desktop may let you change it in its own settings.".to_owned(),
                "If your desktop version has no GlobalShortcuts portal, use the generated bindings below instead.".to_owned(),
            ],
            true,
        ),
        Strategy::SwayConfig => (
            "sway cannot be asked to bind keys: ssx writes a bindings file you include".to_owned(),
            vec![
                "Applying writes ~/.config/sway/config.d/ssx.conf, a file that belongs to ssx.".to_owned(),
                "Your own sway config is never edited unless you tick the box that adds one removable `include` block.".to_owned(),
            ],
            false,
        ),
        Strategy::HyprlandConfig => (
            "Hyprland cannot be asked to bind keys: ssx writes a bindings file you source".to_owned(),
            vec![
                "Applying writes ~/.config/hypr/ssx.conf, a file that belongs to ssx.".to_owned(),
                "Your own hyprland.conf is never edited unless you tick the box that adds one removable `source` block.".to_owned(),
            ],
            false,
        ),
        Strategy::GnomeGsettings => (
            "GNOME registers custom shortcuts in its settings".to_owned(),
            vec![
                "Applying adds ssx-owned custom keybindings through gsettings; other custom shortcuts are left alone.".to_owned(),
            ],
            false,
        ),
        Strategy::KdeShortcuts => (
            "KDE Plasma registers command shortcuts in its own configuration".to_owned(),
            vec![
                "Applying writes launcher files and registers them with kwriteconfig; they become active after the next login (or restart the shortcut daemon).".to_owned(),
            ],
            false,
        ),
        Strategy::CliOnly => (
            "This desktop cannot receive shortcuts from ssx".to_owned(),
            vec![
                "Bind `ssx run <workflow>` to a key in your desktop's keyboard settings; the generated commands below show the exact lines for the desktops ssx knows.".to_owned(),
            ],
            false,
        ),
    };
    let mut details = details;
    if let Some(sock) = &env.swaysock
        && detection.desktop == Desktop::Sway
    {
        details.push(format!("Found the sway socket at {sock}."));
    }
    if detection.candidates.len() > 1 {
        let others: Vec<&str> =
            detection.candidates[1..].iter().map(|s| strategy_short(*s)).collect();
        details.push(format!("Also possible here: {}.", others.join("; ")));
    }
    StrategyInfo { desktop, session, headline, details, automatic, targets }
}

/// A few words per strategy.
pub const fn strategy_short(s: Strategy) -> &'static str {
    match s {
        Strategy::GlobalHotkey => "in-app key grabbing",
        Strategy::Portal => "the GlobalShortcuts portal",
        Strategy::SwayConfig => "a sway include file",
        Strategy::HyprlandConfig => "a Hyprland source file",
        Strategy::GnomeGsettings => "GNOME custom shortcuts",
        Strategy::KdeShortcuts => "KDE command shortcuts",
        Strategy::CliOnly => "binding `ssx run` by hand",
    }
}

// ---- snippets and apply -------------------------------------------------------------------

/// The generated text for `target`, plus what had to be left out and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// For which desktop.
    pub target: Target,
    /// The text to show / copy.
    pub text: String,
    /// Workflows whose hotkey was skipped (unparsable, duplicate).
    pub skipped: Vec<String>,
    /// How many bindings the text contains.
    pub bindings: usize,
}

/// Why no snippet could be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnippetError {
    /// No workflow has a usable hotkey.
    #[error("no workflow has a hotkey yet; give one a shortcut on the Workflows page")]
    NoBindings,
    /// The generator refused.
    #[error("{0}")]
    Render(String),
}

/// The `(chord, command)` pairs for the workflows with hotkeys; `exe` is what the desktop
/// should run (`ssx`, or an absolute path).
pub fn command_bindings(settings: &Settings, exe: &str) -> (Vec<(Chord, Command)>, Vec<String>) {
    bindings_from_settings(settings, exe)
}

/// Generates the snippet for `target`.
pub fn snippet(settings: &Settings, target: Target, exe: &str) -> Result<Snippet, SnippetError> {
    let (b, skipped) = command_bindings(settings, exe);
    if b.is_empty() {
        return Err(SnippetError::NoBindings);
    }
    let text = render_snippet(target, &b).map_err(|e| SnippetError::Render(e.message))?;
    Ok(Snippet { target, text, skipped, bindings: b.len() })
}

/// What a confirmed "apply" or "remove" did, as lines for the user.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyOutcome {
    /// What happened, one line each.
    pub lines: Vec<String>,
    /// The line the user must add to their own config (when they did not opt in to ssx
    /// adding it).
    pub manual_step: Option<String>,
}

/// Applies the bindings for `target`. `add_main_include` is the explicit opt-in to add ssx's
/// removable block to the user's sway / Hyprland config.
pub fn apply(
    settings: &Settings,
    target: Target,
    exe: &str,
    dirs: &Dirs,
    runner: &dyn CommandRunner,
    add_main_include: bool,
) -> Result<ApplyOutcome, BindingError> {
    let (b, skipped) = command_bindings(settings, exe);
    let mut out = ApplyOutcome::default();
    for s in skipped {
        out.lines.push(format!("skipped: {s}"));
    }
    match target {
        Target::Sway | Target::Hyprland => {
            let report = files::write_include_file(dirs, target, &b)?;
            out.lines.push(format!(
                "{} {}",
                if report.changed { "wrote" } else { "already up to date:" },
                report.path.display()
            ));
            if add_main_include {
                out.lines.push(
                    match files::install_main_include(dirs, target)? {
                        MainConfigChange::Changed => "added a removable ssx block to your config",
                        MainConfigChange::Unchanged => "your config already has the ssx block",
                        MainConfigChange::AlreadyIncludedManually => {
                            "your config already includes the file by hand"
                        }
                    }
                    .to_owned(),
                );
            } else {
                out.manual_step = Some(report.include_line);
            }
        }
        Target::Gnome => {
            let r = gnome::apply(runner, &b)?;
            out.lines.push(if r.is_noop() {
                "GNOME keybindings were already up to date".to_owned()
            } else {
                format!("registered {} GNOME setting(s)", r.values_written)
            });
        }
        Target::Kde => {
            let r = kde::apply(dirs, runner, &b)?;
            out.lines.push(format!(
                "wrote {} launcher(s), registered {} shortcut(s); active after the next login",
                r.desktop_files_written, r.shortcuts_registered
            ));
        }
    }
    Ok(out)
}

/// Removes what [`apply`] installed for `target`.
pub fn remove(
    target: Target,
    dirs: &Dirs,
    runner: &dyn CommandRunner,
) -> Result<ApplyOutcome, BindingError> {
    let mut out = ApplyOutcome::default();
    match target {
        Target::Sway | Target::Hyprland => {
            files::uninstall(dirs, target)?;
            out.lines.push(format!("removed the ssx block and include file for {target}"));
        }
        Target::Gnome => {
            let r = gnome::remove(runner)?;
            out.lines.push(if r.is_noop() {
                "no ssx keybindings were registered".to_owned()
            } else {
                "removed the ssx GNOME keybindings".to_owned()
            });
        }
        Target::Kde => {
            kde::remove(dirs, runner)?;
            out.lines.push("removed the ssx KDE shortcuts".to_owned());
        }
    }
    Ok(out)
}

/// What is currently installed for a file-based target.
pub fn installed_state(dirs: &Dirs, target: Target) -> Option<files::Status> {
    files::status(dirs, target).ok()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, io};

    use ssx_core::settings::Workflow;
    use ssx_hotkeys::{
        Platform,
        bindings::{RunOutput, gnome::SCHEMA},
        detect,
    };

    use super::*;

    fn wf(id: &str, hotkey: Option<&str>) -> Workflow {
        let mut w = Workflow { id: id.into(), name: format!("WF {id}"), ..Workflow::default() };
        w.trigger.hotkey = hotkey.map(str::to_owned);
        w.trigger.cli_name = Some(id.into());
        w.after_capture = vec![ssx_core::settings::AfterCapture::SaveToFile];
        w
    }

    fn settings(ws: Vec<Workflow>) -> Settings {
        Settings { workflows: ws, ..Settings::default() }
    }

    #[test]
    fn duplicates_compare_canonical_forms() {
        let s = settings(vec![
            wf("a", Some("Ctrl+Shift+PrintScreen")),
            wf("b", Some("shift + control + prtsc")),
            wf("c", Some("F9")),
            wf("d", None),
        ]);
        let d = duplicates(&s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].hotkey, "Ctrl+Shift+PrintScreen");
        assert_eq!(d[0].owners.len(), 2);
        assert_eq!(d[0].owners[0].path(), "workflows[0].trigger.hotkey");
        assert_eq!(d[0].owners[1].path(), "workflows[1].trigger.hotkey");
    }

    #[test]
    fn global_hotkeys_take_part_in_duplicate_detection() {
        let mut s = settings(vec![wf("a", Some("F9"))]);
        s.hotkeys.open_history = Some("f9".into());
        let d = duplicates(&s);
        assert_eq!(d.len(), 1);
        assert!(d[0].owners.contains(&Owner::Global("hotkeys.open_history")));
        assert_eq!(Owner::Global("hotkeys.open_history").label(), "Open history window");
        let others = others_using(&s, &Owner::Workflow { index: 0, name: "WF a".into() });
        assert_eq!(others, [Owner::Global("hotkeys.open_history")]);
    }

    #[test]
    fn unique_and_unparsable_hotkeys_are_not_duplicates() {
        let s =
            settings(vec![wf("a", Some("banana")), wf("b", Some("banana")), wf("c", Some("F1"))]);
        assert!(duplicates(&s).is_empty());
        assert!(others_using(&s, &Owner::Workflow { index: 2, name: String::new() }).is_empty());
        assert!(others_using(&s, &Owner::Workflow { index: 9, name: String::new() }).is_empty());
    }

    #[test]
    fn builtin_defaults_have_no_duplicates() {
        assert!(duplicates(&Settings::default()).is_empty());
        assert!(chords(&Settings::default()).len() >= 5);
    }

    fn det(pairs: &[(&str, &str)], platform: Platform) -> (Detection, Environment) {
        let env = Environment::from_pairs(pairs.iter().copied());
        (detect(&env, platform), env)
    }

    #[test]
    fn strategy_for_x11_gnome_is_automatic_with_a_fallback() {
        let (d, env) =
            det(&[("XDG_CURRENT_DESKTOP", "GNOME"), ("XDG_SESSION_TYPE", "x11")], Platform::Linux);
        let i = explain(&d, &env);
        assert!(i.automatic);
        assert_eq!((i.desktop, i.session), ("GNOME", "X11"));
        assert!(i.headline.contains("itself"), "{}", i.headline);
        assert_eq!(i.targets, [Target::Gnome]);
        assert!(i.details.iter().any(|l| l.contains("Also possible")), "{:?}", i.details);
    }

    #[test]
    fn strategy_for_wayland_gnome_and_kde_uses_the_portal() {
        for (desk, name) in [("GNOME", "GNOME"), ("KDE", "KDE Plasma")] {
            let (d, env) = det(
                &[("XDG_CURRENT_DESKTOP", desk), ("XDG_SESSION_TYPE", "wayland")],
                Platform::Linux,
            );
            let i = explain(&d, &env);
            assert!(i.automatic, "{desk}");
            assert!(
                i.headline.contains("GlobalShortcuts portal") && i.headline.contains(name),
                "{}",
                i.headline
            );
            assert_eq!(i.targets.len(), 1);
        }
    }

    #[test]
    fn strategy_for_sway_and_hyprland_is_a_generated_file() {
        let (d, env) = det(
            &[("SWAYSOCK", "/run/sway.sock"), ("WAYLAND_DISPLAY", "wayland-1")],
            Platform::Linux,
        );
        let i = explain(&d, &env);
        assert!(!i.automatic);
        assert_eq!(i.targets, [Target::Sway]);
        assert!(i.details.iter().any(|l| l.contains("config.d/ssx.conf")));
        assert!(i.details.iter().any(|l| l.contains("/run/sway.sock")));
        let (d, env) = det(
            &[("HYPRLAND_INSTANCE_SIGNATURE", "abc"), ("WAYLAND_DISPLAY", "w")],
            Platform::Linux,
        );
        let i = explain(&d, &env);
        assert_eq!(i.targets, [Target::Hyprland]);
        assert!(i.headline.contains("Hyprland"));
    }

    #[test]
    fn strategy_on_windows_and_unknown_desktops() {
        let (d, env) = det(&[], Platform::Windows);
        let i = explain(&d, &env);
        assert!(i.automatic && i.targets.is_empty());
        let (d, env) = det(&[("XDG_SESSION_TYPE", "tty")], Platform::Linux);
        let i = explain(&d, &env);
        assert!(!i.automatic);
        assert!(i.headline.contains("cannot receive"), "{}", i.headline);
        assert!(i.details[0].contains("ssx run"));
    }

    #[test]
    fn session_check_reads_sway_config_and_reports_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = dirs.config_home.join("sway/config");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg,
            "set $mod Mod4\nbindsym Ctrl+Print exec grim\nbindsym $mod+Return exec foot\n",
        )
        .unwrap();
        let (d, _) = det(&[("SWAYSOCK", "/s"), ("WAYLAND_DISPLAY", "w")], Platform::Linux);
        let s = Settings::default();
        match check_session(&d, Some(&dirs), &s) {
            SessionCheck::Checked { target, conflicts } => {
                assert_eq!(target, Target::Sway);
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].line_number, 2);
                assert!(conflicts[0].line.contains("grim"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(check_session(&d, None, &s), SessionCheck::Failed(_)));
    }

    #[test]
    fn session_check_is_honest_about_what_it_cannot_see() {
        let (d, _) = det(
            &[("XDG_CURRENT_DESKTOP", "GNOME"), ("XDG_SESSION_TYPE", "wayland")],
            Platform::Linux,
        );
        assert!(matches!(
            check_session(&d, None, &Settings::default()),
            SessionCheck::NotDetectable(_)
        ));
        let (d, _) = det(&[], Platform::Windows);
        match check_session(&d, None, &Settings::default()) {
            SessionCheck::NotDetectable(why) => assert!(why.contains("registering it fails")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn snippets_render_for_every_target() {
        let s = settings(vec![wf("shot", Some("Ctrl+PrintScreen")), wf("dup", Some("ctrl+print"))]);
        for t in [Target::Sway, Target::Hyprland, Target::Gnome, Target::Kde] {
            let sn = snippet(&s, t, "ssx").unwrap();
            assert_eq!(sn.bindings, 1, "{t}");
            assert!(sn.text.contains("ssx"), "{t}: {}", sn.text);
            assert_eq!(sn.skipped.len(), 1, "duplicate reported: {:?}", sn.skipped);
        }
        let sway = snippet(&s, Target::Sway, "/opt/ssx").unwrap();
        assert!(sway.text.contains("bindsym Ctrl+Print exec /opt/ssx run shot"), "{}", sway.text);
    }

    #[test]
    fn no_hotkeys_means_no_snippet() {
        let s = settings(vec![wf("a", None)]);
        assert_eq!(snippet(&s, Target::Sway, "ssx").unwrap_err(), SnippetError::NoBindings);
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<String>>,
        list: RefCell<String>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            if line.starts_with("gsettings get")
                && line.contains(SCHEMA)
                && line.ends_with("custom-keybindings")
            {
                let l = self.list.borrow();
                return Ok(RunOutput::ok(if l.is_empty() {
                    "@as []".to_owned()
                } else {
                    l.clone()
                }));
            }
            if line.starts_with("gsettings get") {
                return Ok(RunOutput::failed("no such key"));
            }
            Ok(RunOutput::ok(""))
        }
    }

    #[test]
    fn apply_sway_writes_only_ssxs_own_file_unless_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let main = dirs.config_home.join("sway/config");
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(&main, "# mine\n").unwrap();
        let s = Settings::default();
        let runner = FakeRunner::default();
        let out = apply(&s, Target::Sway, "ssx", &dirs, &runner, false).unwrap();
        let include = dirs.config_home.join("sway/config.d/ssx.conf");
        assert!(include.is_file());
        assert_eq!(
            std::fs::read_to_string(&main).unwrap(),
            "# mine\n",
            "the user's config is untouched"
        );
        assert!(out.manual_step.as_deref().unwrap().starts_with("include "));
        assert!(runner.calls.borrow().is_empty());
        // explicit opt-in adds the marked block, and removal restores the original text
        let out = apply(&s, Target::Sway, "ssx", &dirs, &runner, true).unwrap();
        assert!(out.manual_step.is_none());
        assert!(std::fs::read_to_string(&main).unwrap().contains("ssx hotkeys"));
        remove(Target::Sway, &dirs, &runner).unwrap();
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "# mine\n");
        assert!(!include.exists());
    }

    #[test]
    fn apply_hyprland_and_status() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let runner = FakeRunner::default();
        let s = Settings::default();
        let out = apply(&s, Target::Hyprland, "ssx", &dirs, &runner, false).unwrap();
        assert!(out.manual_step.as_deref().unwrap().contains("source"));
        let st = installed_state(&dirs, Target::Hyprland).unwrap();
        assert!(st.include_file_exists && !st.block_installed);
        assert!(installed_state(&dirs, Target::Gnome).is_some());
    }

    #[test]
    fn apply_gnome_goes_through_gsettings_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let runner = FakeRunner::default();
        let out = apply(&Settings::default(), Target::Gnome, "ssx", &dirs, &runner, false).unwrap();
        assert!(out.lines[0].contains("registered"), "{:?}", out.lines);
        let calls = runner.calls.borrow();
        assert!(calls.iter().any(|c| c.starts_with("gsettings set")), "{calls:?}");
        assert!(calls.iter().all(|c| c.starts_with("gsettings")), "{calls:?}");
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none(), "no files written");
    }

    #[test]
    fn apply_kde_writes_launchers_and_calls_kwriteconfig() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let runner = FakeRunner::default();
        let out = apply(&Settings::default(), Target::Kde, "ssx", &dirs, &runner, false).unwrap();
        assert!(out.lines[0].contains("launcher"), "{:?}", out.lines);
        assert!(runner.calls.borrow().iter().any(|c| c.contains("kwriteconfig")));
        let removed = remove(Target::Kde, &dirs, &runner).unwrap();
        assert!(removed.lines[0].contains("KDE"));
    }

    #[test]
    fn apply_reports_skipped_hotkeys() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let s = settings(vec![wf("a", Some("F9")), wf("b", Some("F9"))]);
        let out = apply(&s, Target::Sway, "ssx", &dirs, &FakeRunner::default(), false).unwrap();
        assert!(out.lines.iter().any(|l| l.starts_with("skipped:")), "{:?}", out.lines);
    }

    #[test]
    fn a_missing_tool_is_an_actionable_error() {
        struct Missing;
        impl CommandRunner for Missing {
            fn run(&self, _: &str, _: &[String]) -> io::Result<RunOutput> {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let e = apply(
            &Settings::default(),
            Target::Gnome,
            "ssx",
            &Dirs::under(tmp.path()),
            &Missing,
            false,
        )
        .unwrap_err();
        assert!(matches!(e, BindingError::ToolMissing(..)), "{e}");
    }
}
