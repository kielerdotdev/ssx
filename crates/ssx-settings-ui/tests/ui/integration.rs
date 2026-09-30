//! Integration (file-manager menus and diagnostics) and About.

use std::path::Path;

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use ssx_settings_ui::{
    SettingsApp,
    host::{FixedDoctor, RecordingOpener, ShellHost},
    nav::Page,
    pages::{
        about::{LICENSE, NOTICES, REPOSITORY},
        integration::{report_text, sample_report},
    },
};

use crate::common::*;

fn open() -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) = demo_app(Page::Integration);
    let mut h = window(app, vec2(1200.0, 1700.0));
    wait_busy(&mut h, 20);
    (h, fx)
}

/// Every file below `dir`, sorted.
fn files_below(dir: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.sort();
    v
}

fn home_files(fx: &Fixture) -> Vec<std::path::PathBuf> {
    files_below(&fx.root().join("home"))
}

#[test]
fn the_file_managers_are_listed_with_what_was_found() {
    let (h, _fx) = open();
    assert!(has_exact(&h, "Nautilus (GNOME Files)"));
    assert!(has_exact(&h, "Dolphin (KDE)"));
    assert!(has_exact(&h, "Nemo (Cinnamon)"));
    assert!(has_exact(&h, "found"));
    assert!(has_exact(&h, "not found"), "Nemo is not on the sandbox PATH");
    assert!(has_exact(&h, "no menu"), "nothing is installed yet");
}

#[test]
fn opening_the_page_installs_nothing() {
    let (_h, fx) = open();
    assert!(home_files(&fx).is_empty());
}

#[test]
fn install_reports_each_file_manager_and_remove_takes_everything_back() {
    let (mut h, fx) = open();
    click(&mut h, "Install for my file managers");
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, "Result of the installation"));
    let installed = home_files(&fx);
    assert!(!installed.is_empty(), "entries were written below the sandbox home");
    assert!(installed.iter().all(|p| p.starts_with(fx.root())), "nothing outside the sandbox");
    assert!(has(&h, "skipped:"), "Nemo was not found and is reported as skipped");
    assert!(has_exact(&h, "menu installed"));
    // a second run changes nothing but says so
    click(&mut h, "Install for my file managers");
    wait_busy(&mut h, 20);
    assert_eq!(home_files(&fx), installed);
    assert!(has(&h, "already installed"));
    click(&mut h, "Remove all entries");
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, "Result of the removal"));
    assert!(home_files(&fx).is_empty(), "every file ssx wrote is gone: {:?}", home_files(&fx));
    assert!(has_exact(&h, "no menu"));
}

#[test]
fn file_managers_that_were_not_found_are_only_touched_when_asked() {
    let (mut h, fx) = open();
    click(&mut h, "Install for my file managers");
    wait_busy(&mut h, 20);
    let without = home_files(&fx).len();
    click(&mut h, "Remove all entries");
    wait_busy(&mut h, 20);
    click(&mut h, "Also for file managers that were not found");
    click(&mut h, "Install for my file managers");
    wait_busy(&mut h, 20);
    assert!(home_files(&fx).len() > without, "Nemo got its entries too");
}

#[test]
fn one_failing_file_manager_does_not_stop_the_others() {
    let (app, fx) = demo_app_custom(Page::Integration, |_| {}, |_| {});
    // a regular file where Nautilus needs a folder makes only its installer fail
    let blocked = fx.root().join("home/.local/share/nautilus");
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, "in the way").unwrap();
    let mut h = window(app, vec2(1200.0, 1700.0));
    wait_busy(&mut h, 20);
    click(&mut h, "Install for my file managers");
    wait_busy(&mut h, 20);
    assert!(has(&h, "failed:"), "{:?}", labels(&h));
    assert!(has(&h, "The others were processed normally."));
    assert!(!home_files(&fx).is_empty(), "Dolphin and Thunar were still installed");
    assert_eq!(
        std::fs::read_to_string(&blocked).unwrap(),
        "in the way",
        "the user's file is untouched"
    );
}

#[test]
fn details_show_what_would_be_written() {
    let (mut h, _fx) = open();
    h.get_all_by_label("Details").next().unwrap().click();
    settle(&mut h);
    assert!(has_exact(&h, "Hide details"));
    h.get_all_by_label("Hide details").next().unwrap().click();
    settle(&mut h);
    assert!(!has_exact(&h, "Hide details"));
}

#[test]
fn without_a_usable_context_the_page_explains_and_offers_no_install() {
    let (app, _fx) = demo_app_custom(
        Page::Integration,
        |_| {},
        |host| {
            let mut shell =
                ShellHost::sandboxed(std::path::Path::new("/nonexistent"), &host.ssx_exe);
            shell.context = Err("no home directory".to_owned());
            host.shell = shell;
        },
    );
    let mut h = window(app, vec2(1200.0, 1700.0));
    wait_busy(&mut h, 20);
    assert!(has(&h, "The menus cannot be installed: no home directory"));
    assert!(!has_exact(&h, "Install for my file managers"));
}

#[test]
fn the_diagnostics_are_shown_and_copy_report_gives_what_doctor_prints() {
    let (mut h, _fx) = open();
    assert!(has(&h, "DELL U2723QE"), "monitors are listed: {:?}", labels(&h));
    assert!(has(&h, "HDR on (203 nits SDR white)"));
    let copied = click_and_copied(&mut h, "Copy report");
    assert_eq!(copied, [report_text(&sample_report())]);
    assert!(copied[0].contains("DELL U2723QE"));
}

#[test]
fn a_failed_diagnosis_is_reported_and_there_is_nothing_to_copy() {
    let (app, _fx) = demo_app_custom(
        Page::Integration,
        |_| {},
        |host| host.doctor = std::sync::Arc::new(FixedDoctor(Err("probe crashed".into()))),
    );
    let mut h = window(app, vec2(1200.0, 1700.0));
    wait_busy(&mut h, 20);
    assert!(has(&h, "The diagnostics failed: probe crashed"));
    assert!(click_and_copied(&mut h, "Copy report").is_empty());
}

#[test]
fn run_again_probes_the_machine_again() {
    let (mut h, _fx) = open();
    click(&mut h, "Run again");
    wait_busy(&mut h, 20);
    assert!(has(&h, "DELL U2723QE"));
}

#[test]
fn about_shows_the_version_the_licence_and_the_third_party_notices() {
    let (app, _fx) = app(Page::About);
    let h = window(app, vec2(1200.0, 1300.0));
    assert!(has_exact(&h, &format!("Version {}", env!("CARGO_PKG_VERSION"))));
    assert!(has(&h, "GPL-3.0-or-later"));
    assert_eq!(LICENSE, "GPL-3.0-or-later");
    for n in NOTICES {
        assert!(has_exact(&h, n.name), "{} is listed", n.name);
    }
    assert!(has(&h, "settings.toml"), "where the settings live");
}

#[test]
fn the_source_link_opens_through_the_opener() {
    let opener = std::sync::Arc::new(RecordingOpener::default());
    let o2 = opener.clone();
    let (app, _fx) = app_custom(Page::About, Default::default(), move |host, _| host.opener = o2);
    let mut h = window(app, vec2(1200.0, 1300.0));
    click(&mut h, REPOSITORY);
    assert_eq!(*opener.urls.lock().unwrap(), [REPOSITORY]);
}
