//! Hotkeys: every shortcut in one place, how this desktop delivers them, clashes, and the
//! generated compositor bindings with an explicit, reversible "Apply".

use std::path::Path;

use egui::{Color32, RichText, Ui};
use ssx_core::settings::{Settings, Severity};
use ssx_editor_ui::ui::widgets::input_style;
use ssx_hotkeys::bindings::{Dirs, Target, files};

use super::Cx;
use crate::{
    hotkey_plan::{
        ApplyOutcome, Owner, SessionCheck, Snippet, SnippetError, apply, check_session, duplicates,
        explain, installed_state, others_using, remove, session_target, snippet,
    },
    hotkey_widget::hotkey_field,
    ui_kit::{self, Answer, Field},
    validation::workflow_path,
};

/// A step waiting for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Write the bindings for this target.
    Apply(Target),
    /// Remove them again.
    Remove(Target),
}

/// State of the Hotkeys page.
#[derive(Debug, Default)]
pub struct State {
    /// The generator target the user picked (`None`: what this session recommends).
    pub target: Option<Target>,
    /// A confirmation dialog.
    pub confirm: Option<Confirm>,
    /// The opt-in to add ssx's include block to the user's own config.
    pub add_main_include: bool,
    /// What the last apply or remove did.
    pub last: Option<Result<ApplyOutcome, String>>,
    /// The cached clash check with the chords it was made for.
    session: Option<(Vec<ssx_hotkeys::Chord>, SessionCheck)>,
}

/// The four generator targets in menu order.
pub const TARGETS: [Target; 4] = [Target::Sway, Target::Hyprland, Target::Gnome, Target::Kde];

/// The target to show for `st`: the chosen one, else the session's, else sway.
pub fn effective_target(st: &State, detection: &ssx_hotkeys::Detection) -> Target {
    st.target.or_else(|| session_target(detection)).unwrap_or(Target::Sway)
}

/// `path` with the home directory written as `~`.
pub fn tilde(path: &Path, dirs: &Dirs) -> String {
    let home = dirs.config_home.parent().map(Path::to_path_buf);
    if let Some(h) = home
        && let Ok(rest) = path.strip_prefix(&h)
    {
        // Always `/`: these are Linux config paths, whatever separator `join` produced.
        let rest: Vec<_> = rest.components().map(|c| c.as_os_str().to_string_lossy()).collect();
        return format!("~/{}", rest.join("/"));
    }
    path.display().to_string()
}

/// What confirming "Apply" for `target` will change, one line each (shown before the user
/// agrees).
pub fn describe_apply(target: Target, dirs: &Dirs, add_main_include: bool) -> Vec<String> {
    match target {
        Target::Sway | Target::Hyprland => {
            let mut v = Vec::new();
            if let Some(p) = files::include_file_path(dirs, target) {
                v.push(format!("Writes {}, a file that belongs to ssx (it is replaced each time you apply).", tilde(&p, dirs)));
            }
            let main = files::main_config_path(dirs, target).map(|p| tilde(&p, dirs)).unwrap_or_default();
            if add_main_include {
                v.push(format!("Adds one marked block to {main} that loads that file. \"Remove\" takes the block out again and restores the text exactly."));
            } else {
                v.push(format!("Does not touch {main}. You add one line yourself; it is shown afterwards."));
            }
            v
        }
        Target::Gnome => vec![
            "Adds ssx custom keybindings to GNOME's settings (gsettings), named ssx-...".to_owned(),
            "Your other custom shortcuts are left as they are. \"Remove\" deletes only the ssx ones.".to_owned(),
        ],
        Target::Kde => vec![
            "Writes one launcher file per shortcut to ~/.local/share/applications and registers it with kwriteconfig.".to_owned(),
            "The shortcuts become active after your next login, or after restarting the shortcut daemon. \"Remove\" deletes only the ssx ones.".to_owned(),
        ],
    }
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let ctx = ui.ctx().clone();
    ui_kit::page_scroll(ui, "hotkeys", |ui| {
        shortcuts_card(ui, cx);
        strategy_card(ui, st, cx);
        conflicts_card(ui, st, cx);
        bindings_card(ui, st, cx);
    });
    dialogs(&ctx, st, cx);
}

fn shortcuts_card(ui: &mut Ui, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Workflow shortcuts"), |ui| {
        ui_kit::hint(
            ui,
            "The key that starts each workflow. Click Record and press the keys, or set them with the buttons.",
        );
        ui.add_space(6.0);
        let issues = cx.issues.clone();
        for i in 0..cx.settings.workflows.len() {
            let others = {
                let w = &cx.settings.workflows[i];
                others_using(cx.settings, &Owner::Workflow { index: i, name: w.name.clone() })
            };
            let w = &mut cx.settings.workflows[i];
            let name =
                if w.name.is_empty() { format!("Workflow {}", i + 1) } else { w.name.clone() };
            let path = format!("{}.trigger.hotkey", workflow_path(i));
            Field::new(&name).label_width(250.0).issues(&issues, &path).show(ui, |ui| {
                hotkey_field(ui, ("hk-wf", i), &format!("Hotkey of {name}"), &mut w.trigger.hotkey);
                if !others.is_empty() {
                    let names: Vec<String> = others.iter().map(Owner::label).collect();
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, Severity::Error);
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("Also used by {}", names.join(", ")))
                                    .size(12.0)
                                    .color(ui_kit::ERROR_TEXT),
                            )
                            .wrap(),
                        );
                    });
                }
            });
            ui.add_space(4.0);
        }
    });
    ui_kit::card(ui, Some("Other shortcuts"), |ui| {
        let issues = cx.issues.clone();
        let names = ["hotkeys.open_history", "hotkeys.open_settings", "hotkeys.pause_recording"];
        for name in names {
            let label = Owner::Global(name).label();
            let hk = &mut cx.settings.hotkeys;
            let value = match name {
                "hotkeys.open_history" => &mut hk.open_history,
                "hotkeys.open_settings" => &mut hk.open_settings,
                _ => &mut hk.pause_recording,
            };
            Field::new(&label).label_width(250.0).issues(&issues, name).show(ui, |ui| {
                hotkey_field(ui, ("hk-global", name), &format!("Hotkey: {label}"), value);
            });
            ui.add_space(4.0);
        }
    });
}

fn strategy_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let detection = cx.host.hotkey_detection();
    let info = explain(&detection, &cx.host.hotkey_env);
    ui_kit::card(ui, Some("How shortcuts work on this desktop"), |ui| {
        ui.horizontal(|ui| {
            ui_kit::badge(ui, info.desktop, ui_kit::ACCENT_TEXT);
            ui_kit::badge(ui, info.session, ui_kit::ACCENT_TEXT);
            ui_kit::badge(
                ui,
                if info.automatic { "automatic" } else { "needs setup" },
                if info.automatic { ui_kit::OK_TEXT } else { ui_kit::WARN_TEXT },
            );
        });
        ui.add_space(6.0);
        ui.label(RichText::new(&info.headline).strong().color(Color32::WHITE));
        ui.add_space(2.0);
        for d in &info.details {
            ui_kit::hint(ui, d);
        }
        if !info.automatic {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui_kit::link(ui, "Show the generated bindings").clicked() {
                    st.target = info.targets.first().copied().or(st.target);
                }
            });
        }
    });
}

fn conflicts_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let detection = cx.host.hotkey_detection();
    let chords = crate::hotkey_plan::chords(cx.settings);
    let stale = st.session.as_ref().is_none_or(|(c, _)| *c != chords);
    if stale {
        let check = check_session(&detection, cx.host.hotkey_dirs.as_ref(), cx.settings);
        st.session = Some((chords, check));
    }
    ui_kit::card(ui, Some("Conflicts"), |ui| {
        let dups = duplicates(cx.settings);
        if dups.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                ui.label("No shortcut is used twice in your settings.");
            });
        }
        for d in &dups {
            ui.horizontal_top(|ui| {
                ui_kit::severity_icon(ui, Severity::Error);
                let names: Vec<String> = d.owners.iter().map(Owner::label).collect();
                ui.add(egui::Label::new(RichText::new(format!("{} is used by {}. Each shortcut can start only one thing; change one of them.", d.hotkey, names.join(" and "))).color(ui_kit::ERROR_TEXT)).wrap());
            });
        }
        ui.add_space(6.0);
        match st.session.as_ref().map(|(_, c)| c.clone()) {
            Some(SessionCheck::Checked { target, conflicts }) => {
                if conflicts.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                        ui.label(format!(
                            "None of your shortcuts is already bound in your {target} config."
                        ));
                    });
                }
                for c in conflicts {
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, Severity::Warning);
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} is already bound in your {target} config, line {}: {}",
                                    c.chord, c.line_number, c.line
                                ))
                                .color(ui_kit::WARN_TEXT),
                            )
                            .wrap(),
                        );
                    });
                }
            }
            Some(SessionCheck::NotDetectable(why)) => ui_kit::hint(ui, &why),
            Some(SessionCheck::Failed(why)) => {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Warning);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "Could not check your desktop's bindings: {why}"
                            ))
                            .color(ui_kit::WARN_TEXT),
                        )
                        .wrap(),
                    );
                });
            }
            None => {}
        }
        ui.add_space(4.0);
        if ui_kit::button(ui, "Check again").clicked() {
            st.session = None;
        }
    });
}

fn bindings_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let detection = cx.host.hotkey_detection();
    let current = effective_target(st, &detection);
    let recommended = session_target(&detection);
    ui_kit::card(ui, Some("Desktop bindings"), |ui| {
        ui_kit::hint(
            ui,
            "For desktops that cannot receive shortcuts from an app, ssx generates the lines your desktop needs; each runs `ssx run <workflow>`. Nothing is written until you press Apply and confirm.",
        );
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for t in TARGETS {
                let label = if Some(t) == recommended {
                    format!("{t} (this desktop)")
                } else {
                    t.to_string()
                };
                if ui_kit::chip(ui, &label, t == current).clicked() {
                    st.target = Some(t);
                }
            }
        });
        ui.add_space(8.0);
        let exe = cx.host.ssx_exe.display().to_string();
        match snippet(cx.settings, current, &exe) {
            Ok(Snippet { text, skipped, bindings, .. }) => {
                ui.label(
                    RichText::new(format!(
                        "{bindings} binding{} for {current}",
                        if bindings == 1 { "" } else { "s" }
                    ))
                    .strong()
                    .color(Color32::WHITE),
                );
                ui.add_space(4.0);
                if ui_kit::code_block(ui, &format!("snippet-{current:?}"), &text, 260.0) {
                    let t = cx.time(ui.ctx());
                    cx.toasts.success(t, "Copied the bindings to the clipboard");
                }
                for s in skipped {
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, Severity::Warning);
                        ui.add(
                            egui::Label::new(RichText::new(s).size(12.0).color(ui_kit::WARN_TEXT))
                                .wrap(),
                        );
                    });
                }
            }
            Err(SnippetError::NoBindings) => {
                ui_kit::hint(
                    ui,
                    "No workflow has a shortcut yet. Set one above and the bindings appear here.",
                );
            }
            Err(e) => {
                ui.label(RichText::new(e.to_string()).color(ui_kit::ERROR_TEXT));
            }
        }
        ui.add_space(8.0);
        ui_kit::divider(ui);
        if let Some(dirs) = &cx.host.hotkey_dirs {
            for line in describe_apply(current, dirs, st.add_main_include) {
                ui_kit::hint(ui, &line);
            }
            if let Some(s) = installed_state(dirs, current)
                && matches!(current, Target::Sway | Target::Hyprland)
            {
                ui_kit::hint(
                    ui,
                    &format!(
                        "Now: bindings file {}; your config {}.",
                        if s.include_file_exists { "written" } else { "not written" },
                        if s.block_installed {
                            "loads it (ssx block)"
                        } else if s.manually_included {
                            "loads it (by hand)"
                        } else {
                            "does not load it"
                        },
                    ),
                );
            }
        }
        ui.add_space(6.0);
        let usable = cx.host.hotkey_dirs.is_some() && snippet(cx.settings, current, &exe).is_ok();
        ui.horizontal(|ui| {
            if ui_kit::button_if(ui, "Apply...", usable, "Give a workflow a shortcut first")
                .on_hover_text("Write these bindings after you confirm")
                .clicked()
            {
                st.confirm = Some(Confirm::Apply(current));
            }
            if ui_kit::button_if(
                ui,
                "Remove...",
                cx.host.hotkey_dirs.is_some(),
                "No home directory",
            )
            .clicked()
            {
                st.confirm = Some(Confirm::Remove(current));
            }
        });
        match &st.last {
            Some(Ok(o)) => {
                ui.add_space(6.0);
                for l in &o.lines {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                        ui.add(egui::Label::new(l).wrap());
                    });
                }
                if let Some(step) = &o.manual_step {
                    ui.add_space(4.0);
                    ui_kit::hint(ui, "One step is left for you: add this line to your own config.");
                    if ui_kit::code_block(ui, "manual-step", step, 60.0) {
                        let t = cx.time(ui.ctx());
                        cx.toasts.success(t, "Copied the line");
                    }
                }
            }
            Some(Err(e)) => {
                ui.add_space(6.0);
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Error);
                    ui.add(egui::Label::new(RichText::new(e).color(ui_kit::ERROR_TEXT)).wrap());
                });
            }
            None => {}
        }
    });
}

fn dialogs(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(c) = st.confirm else { return };
    let Some(dirs) = cx.host.hotkey_dirs.clone() else {
        st.confirm = None;
        return;
    };
    let exe = cx.host.ssx_exe.display().to_string();
    match c {
        Confirm::Apply(target) => {
            let lines = describe_apply(target, &dirs, st.add_main_include);
            let mut answer = None;
            let frame =
                egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
            let m = egui::Modal::new(egui::Id::new("hk-apply")).frame(frame).show(ctx, |ui| {
                input_style(ui);
                ui.set_width(480.0);
                ui.label(
                    RichText::new(format!("Apply the bindings for {target}?")).heading().strong(),
                );
                ui.add_space(6.0);
                for l in &lines {
                    ui.add(egui::Label::new(l).wrap());
                    ui.add_space(2.0);
                }
                if matches!(target, Target::Sway | Target::Hyprland) {
                    ui.add_space(6.0);
                    ui_kit::switch(
                        ui,
                        "Also add the include block to my own config",
                        &mut st.add_main_include,
                    );
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::primary(ui, "Apply", true).clicked() {
                        answer = Some(Answer::Confirm);
                    }
                    if ui_kit::button(ui, "Cancel").clicked() {
                        answer = Some(Answer::Cancel);
                    }
                });
            });
            if answer.is_none() && m.should_close() {
                answer = Some(Answer::Cancel);
            }
            match answer {
                Some(Answer::Confirm) => {
                    st.last = Some(
                        apply(
                            cx.settings,
                            target,
                            &exe,
                            &dirs,
                            &*cx.host.hotkey_runner,
                            st.add_main_include,
                        )
                        .map_err(|e| e.to_string()),
                    );
                    st.session = None;
                    st.confirm = None;
                }
                Some(Answer::Cancel) => st.confirm = None,
                None => {}
            }
        }
        Confirm::Remove(target) => {
            let body = match target {
                Target::Sway | Target::Hyprland => format!("Removes the ssx block from your {target} config (if it was added) and deletes ssx's bindings file. Everything else in your config stays as it is."),
                Target::Gnome => "Removes the ssx custom keybindings from GNOME's settings. Your other shortcuts stay.".to_owned(),
                Target::Kde => "Removes the ssx launchers and their registered shortcuts. Your other shortcuts stay.".to_owned(),
            };
            match ui_kit::confirm(
                ctx,
                "hk-remove",
                &format!("Remove the ssx bindings for {target}?"),
                &body,
                "Remove",
                true,
            ) {
                Some(Answer::Confirm) => {
                    st.last = Some(
                        remove(target, &dirs, &*cx.host.hotkey_runner).map_err(|e| e.to_string()),
                    );
                    st.session = None;
                    st.confirm = None;
                }
                Some(Answer::Cancel) => st.confirm = None,
                None => {}
            }
        }
    }
    let _: &Settings = cx.settings;
}

#[cfg(test)]
mod tests {
    use ssx_hotkeys::{Environment, Platform, detect};

    use super::*;

    fn dirs() -> Dirs {
        Dirs::under("/home/demo")
    }

    #[test]
    fn the_target_defaults_to_what_the_session_recommends() {
        let d = detect(
            &Environment::from_pairs([("SWAYSOCK", "/s"), ("WAYLAND_DISPLAY", "w")]),
            Platform::Linux,
        );
        assert_eq!(effective_target(&State::default(), &d), Target::Sway);
        let d = detect(
            &Environment::from_pairs([
                ("HYPRLAND_INSTANCE_SIGNATURE", "x"),
                ("WAYLAND_DISPLAY", "w"),
            ]),
            Platform::Linux,
        );
        assert_eq!(effective_target(&State::default(), &d), Target::Hyprland);
        let d = detect(&Environment::default(), Platform::Windows);
        assert_eq!(
            effective_target(&State::default(), &d),
            Target::Sway,
            "nothing to recommend: sway is just the first tab"
        );
        let st = State { target: Some(Target::Kde), ..State::default() };
        assert_eq!(effective_target(&st, &d), Target::Kde, "the user's choice wins");
    }

    #[test]
    fn paths_are_shown_with_a_tilde() {
        let p = Path::new("/home/demo/.config/sway/config.d/ssx.conf");
        assert_eq!(tilde(p, &dirs()), "~/.config/sway/config.d/ssx.conf");
        assert_eq!(tilde(Path::new("/etc/x"), &dirs()), "/etc/x");
    }

    #[test]
    fn the_apply_description_says_exactly_what_is_written() {
        let d = dirs();
        let no = describe_apply(Target::Sway, &d, false).join("\n");
        assert!(
            no.contains("~/.config/sway/config.d/ssx.conf")
                && no.contains("Does not touch ~/.config/sway/config"),
            "{no}"
        );
        let yes = describe_apply(Target::Sway, &d, true).join("\n");
        assert!(
            yes.contains("Adds one marked block") && yes.contains("restores the text exactly"),
            "{yes}"
        );
        let h = describe_apply(Target::Hyprland, &d, false).join("\n");
        assert!(h.contains("hypr"), "{h}");
        assert!(describe_apply(Target::Gnome, &d, true).join(" ").contains("gsettings"));
        assert!(describe_apply(Target::Kde, &d, true).join(" ").contains("next login"));
        for t in TARGETS {
            assert!(!describe_apply(t, &d, false).is_empty());
        }
    }
}
