//! Golden-file tests: the exact text generated for each target from one fixture list.
//!
//! Regenerate after an intentional change with `UPDATE_GOLDEN=1 cargo test -p ssx-hotkeys
//! --test golden` and review the diff: these files are what users' desktops will read.

use std::path::PathBuf;

use ssx_hotkeys::{
    Chord, Command,
    bindings::{Dirs, gnome, hyprland, kde, sway},
};

fn fixture() -> Vec<(Chord, Command)> {
    let c = |s: &str| s.parse::<Chord>().expect("valid chord");
    vec![
        (c("Ctrl+Shift+S"), Command::new("ssx").args(["capture", "region"]).label("Capture region")),
        (c("Print"), Command::new("ssx").args(["capture", "screen"])),
        (c("Super+Alt+R"), Command::new("/usr/local/bin/ssx").args(["record", "--title", "My clip"])),
        (
            c("Ctrl+Alt+Shift+Super+F12"),
            Command::new("sh")
                .args(["-c", "notify-send 'ssx' \"done: $HOME\"; echo 100% # ok, really", "arg with spaces"])
                .label("Hostile: quotes, $, ; and #"),
        ),
        (c("Super+Grave"), Command::new("ssx").args(["capture", "window", "--delay=2"])),
        (c("VolumeMute"), Command::new("ssx").arg("ünïcödé 日本語")),
    ]
}

fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir golden");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file {} ({e}); run with UPDATE_GOLDEN=1", path.display()));
    assert!(
        expected == actual,
        "golden mismatch for {name}\n--- expected\n{expected}\n--- actual\n{actual}\n(UPDATE_GOLDEN=1 to accept)"
    );
}

#[test]
fn sway_include_file() {
    golden("sway.conf", &sway::render(&fixture()).expect("render"));
}

#[test]
fn hyprland_include_file() {
    golden("hyprland.conf", &hyprland::render(&fixture()).expect("render"));
}

#[test]
fn gnome_command_list_and_script() {
    let entries = gnome::entries(&fixture()).expect("entries");
    let existing = vec![format!("{}custom0/", gnome::PATH_PREFIX)];
    let mut text = String::new();
    for cmd in gnome::commands(&entries, &existing) {
        text.push_str(&cmd.join("\u{2423}"));
        text.push('\n');
    }
    golden("gnome-commands.txt", &text);
    golden("gnome-script.sh", &gnome::script(&entries, &existing));
}

#[test]
fn kde_desktop_files_and_kwriteconfig_commands() {
    let entries = kde::entries(&fixture()).expect("entries");
    let mut text = String::new();
    for e in &entries {
        text.push_str(&format!("=== {} ({})\n{}\n", e.desktop_id, e.shortcut, e.desktop_file));
    }
    for cmd in kde::kwriteconfig_commands("kwriteconfig6", &entries) {
        text.push_str(&cmd.join(" "));
        text.push('\n');
    }
    golden("kde.txt", &text);
}

#[test]
fn include_lines_use_the_injected_root() {
    let d = Dirs::under("/home/example");
    assert_eq!(
        ssx_hotkeys::bindings::files::include_line(&d, ssx_hotkeys::bindings::Target::Sway).as_deref(),
        Some("include /home/example/.config/sway/config.d/ssx.conf")
    );
    assert_eq!(
        ssx_hotkeys::bindings::files::include_line(&d, ssx_hotkeys::bindings::Target::Hyprland).as_deref(),
        Some("source = /home/example/.config/hypr/ssx.conf")
    );
}
