//! Apply / Revert / Save, dirty tracking, validation blocking, external changes, closing.

use egui::vec2;
use egui_kittest::kittest::Queryable;
use ssx_core::settings::{ImageFormatKind, Settings};
use ssx_settings_ui::nav::Page;

use crate::common::*;

#[test]
fn editing_and_applying_writes_exactly_the_edit() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "File name pattern", "shot-%y%mo%d");
    click(&mut h, "JPEG");
    assert!(h.state().model.is_dirty());
    assert_eq!(
        fx.load().general.file_name_pattern,
        "Screenshot_%y-%mo-%d_%h-%mi-%s",
        "nothing is written before Apply"
    );
    click(&mut h, "Apply");
    let saved = fx.load();
    assert_eq!(saved.general.file_name_pattern, "shot-%y%mo%d");
    assert_eq!(saved.general.image_format, ImageFormatKind::Jpg);
    // only what was edited differs from the defaults
    let mut expected = Settings::default();
    expected.general.file_name_pattern = "shot-%y%mo%d".into();
    expected.general.image_format = ImageFormatKind::Jpg;
    assert_eq!(saved, expected);
    assert!(!h.state().model.is_dirty());
    assert_eq!(h.state().outcome().writes, 1);
}

#[test]
fn the_footer_and_the_nav_show_unsaved_changes_and_revert_undoes_them() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    assert!(has(&h, "Unsaved changes in General"));
    assert!(has_exact(&h, "General, unsaved changes"), "the nav item says so too");
    click(&mut h, "Revert");
    assert_eq!(working(&h).general.image_format, ImageFormatKind::Png);
    assert!(!h.state().model.is_dirty());
    assert!(has(&h, "All changes saved"));
    assert_eq!(fx.load(), Settings::default());
}

#[test]
fn an_invalid_edit_blocks_apply_and_save_and_says_why() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "File name pattern", "");
    assert!(has(&h, "must not be empty"), "the message sits next to the field");
    assert!(has(&h, "1 problem must be fixed before saving"));
    // Apply is disabled; clicking does nothing
    click(&mut h, "Apply");
    assert_eq!(h.state().outcome().writes, 0);
    click(&mut h, "Save");
    assert_eq!(h.state().outcome().writes, 0);
    assert!(!h.state().closing());
    assert_eq!(fx.load(), Settings::default(), "the file is untouched");
    // Ctrl+S is blocked too
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::S);
    assert_eq!(h.state().outcome().writes, 0);
    // fixing it unblocks
    set_text(&mut h, "File name pattern", "ok-%s");
    click(&mut h, "Apply");
    assert_eq!(fx.load().general.file_name_pattern, "ok-%s");
}

#[test]
fn errors_on_other_pages_are_reachable_from_the_footer() {
    let (mut app, _fx) = app(Page::General);
    app.model.working_mut().capture.hdr.knee = 3.0;
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert!(has(&h, "Capture & HDR, unsaved changes, 1 problem"));
    click(&mut h, "Show the first problem");
    assert_eq!(h.state().page, Page::Capture);
    assert!(has(&h, "knee must be between 0 and 1"));
}

#[test]
fn applying_with_a_problem_elsewhere_jumps_to_it() {
    let (mut app, _fx) = app(Page::General);
    app.model.working_mut().capture.delay_ms = 999_999;
    let mut h = window(app, vec2(1120.0, 1700.0));
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::S);
    assert_eq!(h.state().page, Page::Capture);
    assert_eq!(h.state().outcome().writes, 0);
}

#[test]
fn save_writes_and_closes() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    click(&mut h, "Save");
    assert!(h.state().closing());
    assert_eq!(fx.load().general.image_format, ImageFormatKind::Jpg);
    let o = h.state().outcome();
    assert!(o.saved && !o.discarded);
}

#[test]
fn ctrl_s_applies_without_closing() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::S);
    assert_eq!(fx.load().general.image_format, ImageFormatKind::Jpg);
    assert!(!h.state().closing());
}

fn press_close(h: &mut egui_kittest::Harness<'_, ssx_settings_ui::SettingsApp>) {
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    settle(h);
}

#[test]
fn closing_a_clean_window_just_closes() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    press_close(&mut h);
    assert!(h.state().closing());
    assert!(!has_exact(&h, "Save your changes?"));
}

#[test]
fn closing_with_unsaved_changes_asks_and_discard_leaves_the_file_alone() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    press_close(&mut h);
    assert!(!h.state().closing(), "the close was cancelled while asking");
    assert!(has_exact(&h, "Save your changes?"));
    let discard = h.get_all_by_label("Discard").last().unwrap();
    discard.click();
    settle(&mut h);
    assert!(h.state().outcome().discarded);
    assert_eq!(fx.load(), Settings::default());
}

#[test]
fn closing_with_unsaved_changes_and_choosing_save_writes_them() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    press_close(&mut h);
    // two "Save" buttons exist now (footer and dialog); the dialog is centred, so higher up
    let saves: Vec<_> = h.get_all_by_label("Save").collect();
    saves.iter().min_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).unwrap().click();
    settle(&mut h);
    assert_eq!(fx.load().general.image_format, ImageFormatKind::Jpg);
}

#[test]
fn cancel_keeps_the_window_open_with_the_edits() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    press_close(&mut h);
    click(&mut h, "Cancel");
    assert!(!has_exact(&h, "Save your changes?"));
    assert!(h.state().model.is_dirty());
    assert!(!h.state().closing());
}

#[test]
fn an_outside_change_with_edits_is_offered_reload_or_overwrite() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    let mut other = Settings::default();
    other.general.image_quality = 33;
    other.save(&fx.settings_file()).unwrap();
    h.state_mut().model.check_external();
    settle(&mut h);
    assert!(has(&h, "changed by another program while you had unsaved edits"));
    assert!(has(&h, "choose Reload or Overwrite"));
    // Apply is blocked until a decision is made
    click(&mut h, "Apply");
    assert_eq!(fx.load().general.image_quality, 33);
    assert_eq!(fx.load().general.image_format, ImageFormatKind::Png);
    // Reload: their file wins and the edit is gone
    click(&mut h, "Reload from disk");
    assert_eq!(working(&h).general.image_quality, 33);
    assert_eq!(working(&h).general.image_format, ImageFormatKind::Png);
    assert!(!has(&h, "changed by another program"));
}

#[test]
fn overwrite_keeps_my_edits_and_replaces_their_file() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    let mut other = Settings::default();
    other.general.image_quality = 33;
    other.save(&fx.settings_file()).unwrap();
    h.state_mut().model.check_external();
    settle(&mut h);
    click(&mut h, "Overwrite the file");
    click(&mut h, "Apply");
    let saved = fx.load();
    assert_eq!(saved.general.image_format, ImageFormatKind::Jpg);
    assert_eq!(saved.general.image_quality, 90, "their quality was replaced by ours");
}

#[test]
fn an_outside_change_without_edits_is_adopted_and_announced() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    let mut other = Settings::default();
    other.general.show_notifications = false;
    other.save(&fx.settings_file()).unwrap();
    h.state_mut().model.check_external();
    settle(&mut h);
    assert!(!working(&h).general.show_notifications);
    assert!(!h.state().model.is_dirty());
}

#[test]
fn the_window_starts_on_the_requested_page_and_pages_switch_by_click_and_key() {
    let (app, _fx) = app(Page::Hotkeys);
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert_eq!(h.state().page, Page::Hotkeys);
    click(&mut h, "About");
    assert_eq!(h.state().page, Page::About);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Num2);
    assert_eq!(h.state().page, Page::Capture);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::PageDown);
    assert_eq!(h.state().page, Page::Workflows);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::PageUp);
    assert_eq!(h.state().page, Page::Capture);
}

#[test]
fn load_warnings_are_shown_and_can_be_dismissed() {
    let fx = tempfile::tempdir().unwrap();
    let host = ssx_settings_ui::host::Host::sandboxed(fx.path());
    std::fs::create_dir_all(&host.paths.config_dir).unwrap();
    std::fs::write(host.paths.settings_file(), "[general]\nimage_quality = 80\nmystery_key = 1\n")
        .unwrap();
    let model = ssx_settings_ui::model::SettingsModel::load(host.paths.settings_file()).unwrap();
    let app = ssx_settings_ui::SettingsApp::new(
        model,
        host,
        Page::General,
        ssx_settings_ui::task::no_wake(),
    );
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert!(has(&h, "unknown setting `general.mystery_key`"));
    click(&mut h, "Dismiss");
    assert!(!has(&h, "unknown setting"));
}

#[test]
fn a_file_write_failure_is_reported_not_swallowed() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "JPEG");
    // Make the settings path a directory so the atomic rename fails.
    std::fs::remove_file(fx.settings_file()).unwrap();
    std::fs::create_dir(fx.settings_file()).unwrap();
    h.state_mut().model.keep_edits_and_overwrite();
    click(&mut h, "Apply");
    assert!(has(&h, "Could not save"), "{:?}", h.state().save_error);
    assert_eq!(h.state().outcome().writes, 0);
}

#[test]
fn edits_on_several_pages_are_saved_together_and_a_new_window_sees_them() {
    let (app, fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "File name pattern", "multi-%s");
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Num2);
    click(&mut h, "3 s");
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Num3);
    click(&mut h, "Duplicate");
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Num5);
    click(&mut h, "New destination");
    click(&mut h, "Local folder");
    click(&mut h, "Create");
    assert!(h.state().model.dirty_pages().len() >= 4, "{:?}", h.state().model.dirty_pages());
    let expected = working(&h);
    click(&mut h, "Save");
    assert!(h.state().outcome().saved);
    // what is on disk is exactly the working copy, and a brand new window starts from it
    let on_disk = fx.load();
    assert_eq!(on_disk, expected);
    assert_eq!(on_disk.general.file_name_pattern, "multi-%s");
    assert_eq!(on_disk.capture.delay_ms, 3000);
    assert_eq!(on_disk.workflows.len(), Settings::default().workflows.len() + 1);
    assert!(on_disk.uploaders.contains_key("local-2"), "the built-in name was avoided");
    let model = ssx_settings_ui::model::SettingsModel::load(fx.settings_file()).unwrap();
    assert_eq!(model.working(), &expected);
    assert!(!model.is_dirty());
}
