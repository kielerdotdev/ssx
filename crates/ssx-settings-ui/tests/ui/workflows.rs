//! The Workflows page: list operations, step editing, drag to reorder, hotkey field.

use egui::{Modifiers, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use ssx_core::settings::{AfterCapture, AfterUpload, InputKind, Settings, Severity, builtin_workflows};
use ssx_settings_ui::{SettingsApp, nav::Page};

use crate::common::*;

fn open() -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) = app(Page::Workflows);
    (window(app, vec2(1120.0, 1500.0)), fx)
}

fn lowest<'h>(h: &'h Harness<'_, SettingsApp>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(label).max_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).unwrap()
}

#[test]
fn the_list_shows_every_builtin_workflow_and_selecting_shows_its_steps() {
    let (mut h, _fx) = open();
    assert!(has_exact(&h, "Workflow Capture region, save, copy and upload"));
    click(&mut h, "Workflow Record GIF and upload");
    assert_eq!(h.state().workflows.selected, 7);
    assert!(has_exact(&h, "Workflow name"));
    assert_eq!(text_of(&h, "Workflow name"), "Record GIF and upload");
    assert_eq!(text_of(&h, "Command name"), "record-gif");
}

#[test]
fn a_new_workflow_from_a_template_is_added_valid_and_selected() {
    let (mut h, _fx) = open();
    let n = working(&h).workflows.len();
    click(&mut h, "New workflow");
    click_contains(&mut h, "Blank");
    let w = working(&h);
    assert_eq!(w.workflows.len(), n + 1);
    assert_eq!(h.state().workflows.selected, n);
    assert_eq!(w.workflows[n].after_capture, [AfterCapture::SaveToFile]);
    assert!(!h.state().model.current_issues().blocks_save());
    // from a built-in template: fresh id, no hotkey clash
    click(&mut h, "New workflow");
    click(&mut h, "Capture region, edit, save and upload");
    let w = working(&h);
    assert_eq!(w.workflows.len(), n + 2);
    let added = &w.workflows[n + 1];
    assert_eq!(added.id, "capture-region-edit-2");
    assert_eq!(added.trigger.hotkey, None);
    assert!(!h.state().model.current_issues().blocks_save());
}

#[test]
fn duplicate_copies_the_steps_but_not_the_hotkey() {
    let (mut h, _fx) = open();
    click(&mut h, "Duplicate");
    let w = working(&h);
    assert_eq!(w.workflows.len(), builtin_workflows().len() + 1);
    assert_eq!(w.workflows[1].name, "Capture region, save, copy and upload (copy)");
    assert_eq!(w.workflows[1].after_capture, w.workflows[0].after_capture);
    assert_eq!(w.workflows[1].trigger.hotkey, None);
    assert_eq!(h.state().workflows.selected, 1);
    assert!(!h.state().model.current_issues().blocks_save());
}

#[test]
fn deleting_asks_first() {
    let (mut h, _fx) = open();
    let n = working(&h).workflows.len();
    click(&mut h, "Delete");
    assert!(has_exact(&h, "Delete this workflow?"));
    click(&mut h, "Cancel");
    assert_eq!(working(&h).workflows.len(), n);
    click(&mut h, "Delete");
    lowest(&h, "Delete").click();
    settle(&mut h);
    assert_eq!(working(&h).workflows.len(), n - 1);
    assert_eq!(working(&h).workflows[0].id, "capture-region-edit");
}

#[test]
fn reset_to_defaults_asks_and_restores_the_builtins() {
    let (mut h, _fx) = open();
    click(&mut h, "Duplicate");
    click(&mut h, "Delete");
    lowest(&h, "Delete").click();
    settle(&mut h);
    click(&mut h, "Reset to defaults...");
    assert!(has_exact(&h, "Reset workflows to the defaults?"));
    click(&mut h, "Reset workflows");
    assert_eq!(working(&h).workflows, builtin_workflows());
    // and it is undoable: Revert brings back whatever was saved
    click(&mut h, "Revert");
    assert_eq!(working(&h).workflows, Settings::default().workflows);
}

#[test]
fn the_name_the_id_and_the_command_name_are_validated_next_to_the_field() {
    let (mut h, _fx) = open();
    set_text(&mut h, "Workflow name", "");
    assert!(has(&h, "give the workflow a name"));
    set_text(&mut h, "Workflow name", "Mine");
    set_text(&mut h, "Command name", "Not Valid!");
    assert!(has(&h, "is not a valid command name"));
    set_text(&mut h, "Command name", "screen");
    assert!(has(&h, "The command name is also used by Capture screen, save and copy"), "the clash is shown on the workflow being edited");
    set_text(&mut h, "Command name", "");
    assert_eq!(working(&h).workflows[0].trigger.cli_name, None);
    // the id is locked until asked for
    assert!(!has_exact(&h, "Workflow id"));
    click(&mut h, "Change");
    set_text(&mut h, "Workflow id", "Bad Id");
    assert!(has(&h, "is not a valid workflow id"));
}

#[test]
fn add_and_remove_steps_from_the_menus() {
    let (mut h, _fx) = open();
    click(&mut h, "Add after-capture step");
    click(&mut h, "Open editor");
    assert_eq!(working(&h).workflows[0].after_capture[0..3], [AfterCapture::SaveToFile, AfterCapture::CopyImageToClipboard, AfterCapture::Upload]);
    assert_eq!(*working(&h).workflows[0].after_capture.last().unwrap(), AfterCapture::OpenEditor);
    // the editor after save is a warning the user can see
    assert!(has(&h, "open_editor runs after save_to_file"));
    click(&mut h, "Remove Open editor");
    assert!(!working(&h).workflows[0].after_capture.contains(&AfterCapture::OpenEditor));
    click(&mut h, "Remove Upload");
    assert!(!working(&h).workflows[0].uploads());
    assert!(has(&h, "needs a URL but the workflow has no upload step"));
}

#[test]
fn move_buttons_reorder_steps_and_stop_at_the_ends() {
    let (mut h, _fx) = open();
    click(&mut h, "Move Save to file down");
    assert_eq!(working(&h).workflows[0].after_capture, [AfterCapture::CopyImageToClipboard, AfterCapture::SaveToFile, AfterCapture::Upload]);
    click(&mut h, "Move Upload up");
    assert_eq!(working(&h).workflows[0].after_capture, [AfterCapture::CopyImageToClipboard, AfterCapture::Upload, AfterCapture::SaveToFile]);
    click(&mut h, "Revert");
    assert_eq!(working(&h).workflows[0].after_capture, [AfterCapture::SaveToFile, AfterCapture::CopyImageToClipboard, AfterCapture::Upload]);
}

#[test]
fn dragging_a_step_by_its_handle_reorders_the_list() {
    let (mut h, _fx) = open();
    let from = h.get_by_label_contains("Reorder Save to file").rect().center();
    let below = h.get_by_label_contains("Reorder Upload:").rect();
    // drop below the last step
    drag(&mut h, from, egui::pos2(from.x, below.bottom() + 20.0));
    assert_eq!(working(&h).workflows[0].after_capture, [AfterCapture::CopyImageToClipboard, AfterCapture::Upload, AfterCapture::SaveToFile]);
    // and back to the top
    let from = h.get_by_label_contains("Reorder Save to file").rect().center();
    let top = h.get_by_label_contains("Reorder Copy image to clipboard").rect();
    drag(&mut h, from, egui::pos2(from.x, top.top() - 4.0));
    assert_eq!(working(&h).workflows[0].after_capture, [AfterCapture::SaveToFile, AfterCapture::CopyImageToClipboard, AfterCapture::Upload]);
}

#[test]
fn dropping_a_step_where_it_already_is_changes_nothing() {
    let (mut h, _fx) = open();
    let r = h.get_by_label_contains("Reorder Copy image to clipboard").rect();
    let c = r.center();
    drag(&mut h, c, egui::pos2(c.x, c.y + 3.0));
    assert_eq!(working(&h).workflows[0].after_capture, Settings::default().workflows[0].after_capture);
    assert!(!h.state().model.is_dirty());
}

#[test]
fn alt_arrows_on_a_focused_handle_reorder_and_the_focus_follows() {
    let (mut h, _fx) = open();
    h.get_by_label_contains("Reorder Save to file").focus();
    h.step();
    key(&mut h, Modifiers::ALT, egui::Key::ArrowDown);
    assert_eq!(working(&h).workflows[0].after_capture[1], AfterCapture::SaveToFile);
    // the handle that has focus now belongs to the moved step, so pressing again moves it on
    key(&mut h, Modifiers::ALT, egui::Key::ArrowDown);
    assert_eq!(working(&h).workflows[0].after_capture[2], AfterCapture::SaveToFile);
    key(&mut h, Modifiers::ALT, egui::Key::ArrowDown);
    assert_eq!(working(&h).workflows[0].after_capture[2], AfterCapture::SaveToFile, "already last");
    key(&mut h, Modifiers::ALT, egui::Key::ArrowUp);
    assert_eq!(working(&h).workflows[0].after_capture[1], AfterCapture::SaveToFile);
}

#[test]
fn dragging_a_workflow_in_the_list_reorders_it_and_the_selection_follows() {
    let (mut h, _fx) = open();
    let from = h.get_by_label_contains("Reorder Capture region, save, copy and upload").rect().center();
    let target = h.get_by_label_contains("Reorder Capture screen, save and copy").rect();
    drag(&mut h, from, egui::pos2(from.x, target.bottom() + 6.0));
    let ids: Vec<String> = working(&h).workflows.iter().take(4).map(|w| w.id.clone()).collect();
    assert_eq!(ids, ["capture-region-edit", "capture-fullscreen", "capture-region", "capture-window"]);
    assert_eq!(h.state().workflows.selected, 2, "the selected workflow moved, and is still selected");
    assert_eq!(text_of(&h, "Workflow name"), "Capture region, save, copy and upload");
}

#[test]
fn switching_to_a_recording_removes_image_only_steps_and_says_so() {
    let (mut h, _fx) = open();
    click(&mut h, "Input");
    click(&mut h, "Record screen");
    let w = &working(&h).workflows[0];
    assert_eq!(w.input, InputKind::RecordScreen);
    assert_eq!(w.after_capture, [AfterCapture::SaveToFile, AfterCapture::Upload]);
    assert!(has(&h, "Removed Copy image to clipboard"));
    // and the menu no longer offers them
    click(&mut h, "Add after-capture step");
    assert!(!has_exact(&h, "Open editor"));
    assert!(!h.state().model.current_issues().blocks_save());
}

#[test]
fn run_command_steps_have_their_own_editor_and_are_validated() {
    let (mut h, fx) = open();
    click(&mut h, "Add after-upload step");
    click(&mut h, "Run command");
    assert!(has(&h, "run_command needs a program"));
    set_text(&mut h, "Program to run", "/usr/bin/notify-send");
    click(&mut h, "Add argument");
    set_text(&mut h, "Argument 1", "Uploaded {url}");
    let steps = &working(&h).workflows[0].after_upload;
    assert_eq!(
        *steps.last().unwrap(),
        AfterUpload::RunCommand { program: "/usr/bin/notify-send".into(), args: vec!["Uploaded {url}".into()] }
    );
    assert!(!h.state().model.current_issues().blocks_save());
    click(&mut h, "Apply");
    let saved = fx.load();
    assert_eq!(saved.workflows[0].after_upload.last(), steps.last());
    click(&mut h, "Remove argument 1");
    assert!(matches!(working(&h).workflows[0].after_upload.last(), Some(AfterUpload::RunCommand { args, .. }) if args.is_empty()));
}

#[test]
fn a_hotkey_can_be_recorded_from_the_keyboard() {
    let (mut h, fx) = open();
    click(&mut h, "Hotkey of Capture region, save, copy and upload: record a shortcut by pressing it");
    h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, egui::Key::F9);
    settle(&mut h);
    assert_eq!(working(&h).workflows[0].trigger.hotkey.as_deref(), Some("Ctrl+Shift+F9"));
    click(&mut h, "Apply");
    assert_eq!(fx.load().workflows[0].trigger.hotkey.as_deref(), Some("Ctrl+Shift+F9"));
}

#[test]
fn a_hotkey_used_twice_is_an_error_that_names_the_other_workflow() {
    let (mut h, _fx) = open();
    click(&mut h, "Hotkey of Capture region, save, copy and upload: record a shortcut by pressing it");
    // PrintScreen is what "Capture screen" uses
    h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, egui::Key::S);
    settle(&mut h);
    assert!(!h.state().model.current_issues().blocks_save());
    // set it to Alt+PrintScreen through the modifier buttons and the key list: Alt+Print
    click(&mut h, "Hotkey of Capture region, save, copy and upload: Ctrl key");
    click(&mut h, "Hotkey of Capture region, save, copy and upload: Shift key");
    click(&mut h, "Hotkey of Capture region, save, copy and upload: Alt key");
    click(&mut h, "Hotkey of Capture region, save, copy and upload: choose the key from a list");
    click(&mut h, "PrintScreen");
    assert_eq!(working(&h).workflows[0].trigger.hotkey.as_deref(), Some("Alt+PrintScreen"));
    assert!(has(&h, "Also used by Capture active window, save and upload"));
    assert!(h.state().model.current_issues().blocks_save());
    assert!(h.state().model.current_issues().errors().any(|i| i.severity == Severity::Error && i.message.contains("already used")));
}

#[test]
fn escape_cancels_recording_and_backspace_clears() {
    let (mut h, _fx) = open();
    let rec = "Hotkey of Capture region, save, copy and upload: record a shortcut by pressing it";
    click(&mut h, rec);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert_eq!(working(&h).workflows[0].trigger.hotkey.as_deref(), Some("Ctrl+PrintScreen"));
    click(&mut h, rec);
    h.key_press(egui::Key::Backspace);
    settle(&mut h);
    assert_eq!(working(&h).workflows[0].trigger.hotkey, None);
}

#[test]
fn a_typing_key_alone_is_rejected_with_advice() {
    let (mut h, _fx) = open();
    click(&mut h, "Hotkey of Capture region, save, copy and upload: record a shortcut by pressing it");
    h.key_press(egui::Key::S);
    settle(&mut h);
    assert!(has(&h, "would stop you typing that key everywhere"));
    assert_eq!(working(&h).workflows[0].trigger.hotkey.as_deref(), Some("Ctrl+PrintScreen"), "unchanged");
}

#[test]
fn the_video_destination_is_separate_from_the_file_destination() {
    let (mut h, fx) = app_with_uploaders();
    assert!(has(&h, "Same as the file destination"));
    click_contains(&mut h, "Video destination");
    click(&mut h, "local  (local, built in)");
    let w = working(&h);
    assert_eq!(w.workflows[0].destination.video.as_deref(), Some("local"));
    assert_eq!(w.workflows[0].destination.file, None, "the file destination is not touched");
    click(&mut h, "Apply");
    assert_eq!(fx.load().workflows[0].destination.video.as_deref(), Some("local"));
}

#[test]
fn the_video_destination_falls_back_to_the_file_destination() {
    let mut s = Settings::default();
    s.destinations.file = Some("local".into());
    s.workflows[0].destination.video = Some("local".into());
    let (app, fx) = app_with(Page::Workflows, s);
    let mut h = window(app, vec2(1120.0, 1500.0));
    click_contains(&mut h, "Video destination");
    click(&mut h, "Same as the file destination (local)");
    assert_eq!(working(&h).workflows[0].destination.video, None);
    click(&mut h, "Apply");
    assert_eq!(fx.load().workflows[0].destination.video, None);
}

fn app_with_uploaders() -> (Harness<'static, SettingsApp>, Fixture) {
    let mut s = Settings::default();
    s.destinations.file = Some("local".into());
    let (app, fx) = app_with(Page::Workflows, s);
    (window(app, vec2(1120.0, 1500.0)), fx)
}

#[test]
fn a_destination_that_does_not_exist_is_flagged() {
    let mut s = Settings::default();
    s.workflows[0].destination.image = Some("ghost".into());
    let (app, _fx) = app_with(Page::Workflows, s);
    let mut h = window(app, vec2(1120.0, 1500.0));
    assert!(has(&h, "there is no destination called \"ghost\""));
    settle(&mut h);
}

#[test]
fn the_destination_pickers_offer_only_what_can_take_the_content() {
    let (mut h, _fx) = app_with_uploaders();
    click_contains(&mut h, "URL shortener destination");
    assert!(has(&h, "is.gd"));
    assert!(!has(&h, "local  (local"), "an uploader is not a shortener");
}
