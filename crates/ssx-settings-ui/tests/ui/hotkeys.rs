//! The Hotkeys page: strategy per session, conflicts, generated snippets and the opt-in apply.

use std::{path::PathBuf, sync::Arc};

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use ssx_core::settings::Settings;
use ssx_hotkeys::{Environment, Platform};
use ssx_settings_ui::{SettingsApp, host::RecordingRunner, nav::Page};

use crate::common::*;

fn home(fx: &Fixture) -> PathBuf {
    fx.root().join("home")
}

fn open_with(
    customise: impl FnOnce(&mut ssx_settings_ui::host::Host, &std::path::Path),
) -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) = app_custom(Page::Hotkeys, Settings::default(), customise);
    (window(app, vec2(1120.0, 2400.0)), fx)
}

fn top_most<'h>(h: &'h Harness<'_, SettingsApp>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(label).min_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).unwrap()
}

#[test]
fn opening_the_page_and_reading_it_writes_nothing_anywhere() {
    let (mut h, fx) = open_with(|_, _| {});
    click(&mut h, "GNOME");
    click(&mut h, "KDE Plasma");
    click(&mut h, "Hyprland");
    click(&mut h, "Check again");
    assert!(!home(&fx).exists(), "no file appeared in the (sandboxed) home");
    assert!(!h.state().model.is_dirty());
}

#[test]
fn the_strategy_is_explained_for_sway() {
    let (h, _fx) = open_with(|_, _| {});
    assert!(has(&h, "sway cannot be asked to bind keys"));
    assert!(has(&h, "config.d/ssx.conf"));
    assert!(has_exact(&h, "needs setup"));
    assert!(has_exact(&h, "sway (this desktop)"), "the recommended target is marked");
}

#[test]
fn the_strategy_is_explained_for_x11_wayland_portals_and_windows() {
    let (h, _fx) = open_with(|host, _| {
        host.hotkey_env = Environment::from_pairs([
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            ("XDG_SESSION_TYPE", "x11"),
        ]);
    });
    assert!(has(&h, "ssx registers your shortcuts itself"));
    assert!(has_exact(&h, "automatic"));
    let (h, _fx) = open_with(|host, _| {
        host.hotkey_env = Environment::from_pairs([
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "wayland"),
        ]);
    });
    assert!(has(&h, "GlobalShortcuts portal"));
    let (h, _fx) = open_with(|host, _| {
        host.hotkey_env = Environment::default();
        host.hotkey_platform = Platform::Windows;
    });
    assert!(has(&h, "ssx registers your shortcuts itself"));
    let (h, _fx) = open_with(|host, _| {
        host.hotkey_env = Environment::from_pairs([("XDG_SESSION_TYPE", "tty")]);
    });
    assert!(has(&h, "cannot receive shortcuts from ssx"));
}

#[test]
fn the_snippet_is_generated_from_the_workflows_and_can_be_copied() {
    let (mut h, _fx) = open_with(|_, _| {});
    assert!(has(&h, "bindsym Ctrl+Print exec /usr/local/bin/ssx run region"));
    assert!(has(&h, "4 bindings for sway"));
    let copied = click_and_copied(&mut h, "Copy");
    assert!(
        copied.iter().any(|t| t.contains("bindsym Ctrl+Print exec")),
        "the snippet went to the clipboard: {copied:?}"
    );
    click(&mut h, "Hyprland");
    assert!(has(&h, "bind = CTRL, Print, exec, /usr/local/bin/ssx run region"));
    click(&mut h, "GNOME");
    assert!(has(&h, "gsettings"));
    click(&mut h, "KDE Plasma");
    assert!(has(&h, "kwriteconfig"));
}

#[test]
fn removing_every_hotkey_says_there_is_nothing_to_generate() {
    let mut s = Settings::default();
    for w in &mut s.workflows {
        w.trigger.hotkey = None;
    }
    let (app, _fx) = app_with(Page::Hotkeys, s);
    let h = window(app, vec2(1120.0, 2400.0));
    assert!(has(&h, "No workflow has a shortcut yet"));
}

#[test]
fn apply_needs_a_confirmation_and_writes_only_ssxs_own_file() {
    let (mut h, fx) = open_with(|_, root| {
        let main = root.join("home/.config/sway/config");
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(main, "# my config\n").unwrap();
    });
    click(&mut h, "Apply...");
    assert!(has_exact(&h, "Apply the bindings for sway?"));
    assert!(!home(&fx).join(".config/sway/config.d/ssx.conf").exists(), "nothing yet");
    click(&mut h, "Cancel");
    assert!(!home(&fx).join(".config/sway/config.d/ssx.conf").exists());
    click(&mut h, "Apply...");
    top_most(&h, "Apply").click();
    settle(&mut h);
    let include = home(&fx).join(".config/sway/config.d/ssx.conf");
    assert!(include.is_file());
    assert!(
        std::fs::read_to_string(&include)
            .unwrap()
            .contains("bindsym Ctrl+Print exec /usr/local/bin/ssx run region")
    );
    assert_eq!(
        std::fs::read_to_string(home(&fx).join(".config/sway/config")).unwrap(),
        "# my config\n",
        "the user's own config is untouched"
    );
    assert!(has(&h, "wrote"));
    assert!(has(&h, "include "), "the line the user must add is shown");
}

#[test]
fn the_include_block_is_an_explicit_opt_in_and_removal_restores_the_config() {
    let (mut h, fx) = open_with(|_, root| {
        let main = root.join("home/.config/sway/config");
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(main, "set $mod Mod4\n").unwrap();
    });
    click(&mut h, "Apply...");
    click(&mut h, "Also add the include block to my own config");
    top_most(&h, "Apply").click();
    settle(&mut h);
    let main = home(&fx).join(".config/sway/config");
    assert!(std::fs::read_to_string(&main).unwrap().contains("ssx hotkeys"));
    click(&mut h, "Remove...");
    assert!(has(&h, "Removes the ssx block from your sway config"));
    click(&mut h, "Remove");
    assert_eq!(std::fs::read_to_string(&main).unwrap(), "set $mod Mod4\n");
    assert!(!home(&fx).join(".config/sway/config.d/ssx.conf").exists());
}

#[test]
fn conflicts_with_the_desktops_own_bindings_are_reported_with_the_line() {
    let (h, _fx) = open_with(|_, root| {
        let main = root.join("home/.config/sway/config");
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(main, "set $mod Mod4\nbindsym Ctrl+Print exec grim\n").unwrap();
    });
    assert!(has(&h, "already bound in your sway config, line 2"));
}

#[test]
fn duplicates_inside_the_settings_are_listed_by_name() {
    let mut s = Settings::default();
    s.workflows[1].trigger.hotkey = Some("ctrl+printscreen".into());
    let (app, _fx) = app_with(Page::Hotkeys, s);
    let h = window(app, vec2(1120.0, 2400.0));
    assert!(has(
        &h,
        "Ctrl+PrintScreen is used by Capture region, save, copy and upload and Capture region, edit, save and upload"
    ));
}

#[test]
fn gnome_bindings_go_through_gsettings_with_a_confirmation_and_nothing_else() {
    let runner = Arc::new(RecordingRunner::default());
    let r2 = runner.clone();
    let (mut h, fx) = open_with(move |host, _| {
        host.hotkey_runner = r2;
        host.hotkey_env = Environment::from_pairs([
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            ("XDG_SESSION_TYPE", "wayland"),
        ]);
    });
    click(&mut h, "Apply...");
    assert!(runner.calls.lock().unwrap().is_empty(), "the dialog alone runs nothing");
    top_most(&h, "Apply").click();
    settle(&mut h);
    let calls = runner.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.starts_with("gsettings set")), "{calls:?}");
    assert!(calls.iter().all(|c| c.starts_with("gsettings")), "{calls:?}");
    assert!(has(&h, "registered"));
    assert!(!home(&fx).exists());
}

#[test]
fn a_missing_tool_is_reported_in_the_page() {
    #[derive(Debug)]
    struct Missing;
    impl ssx_hotkeys::bindings::CommandRunner for Missing {
        fn run(&self, _: &str, _: &[String]) -> std::io::Result<ssx_hotkeys::bindings::RunOutput> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }
    let (mut h, _fx) = open_with(|host, _| {
        host.hotkey_runner = Arc::new(Missing);
        host.hotkey_env = Environment::from_pairs([
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            ("XDG_SESSION_TYPE", "wayland"),
        ]);
    });
    click(&mut h, "Apply...");
    top_most(&h, "Apply").click();
    settle(&mut h);
    assert!(has(&h, "`gsettings` was not found"));
}

#[test]
fn the_other_shortcuts_can_be_set_and_are_checked_against_the_workflows() {
    let (mut h, _fx) = open_with(|_, _| {});
    click(&mut h, "Hotkey: Open history window: record a shortcut by pressing it");
    h.key_press_modifiers(egui::Modifiers::CTRL | egui::Modifiers::ALT, egui::Key::H);
    settle(&mut h);
    assert_eq!(working(&h).hotkeys.open_history.as_deref(), Some("Ctrl+Alt+H"));
    assert!(!h.state().model.current_issues().blocks_save());
    // use the same as a workflow: both sides report it
    click(&mut h, "Hotkey: Open settings window: record a shortcut by pressing it");
    h.key_press_modifiers(egui::Modifiers::CTRL | egui::Modifiers::ALT, egui::Key::H);
    settle(&mut h);
    assert!(h.state().model.current_issues().blocks_save());
    assert!(has(&h, "is used by Open history window and Open settings window"));
}
