//! General and Capture & HDR pages.

use std::sync::Arc;

use egui::vec2;
use egui_kittest::kittest::Queryable;
use ssx_core::settings::{HdrConfig, ImageFormatKind, Settings, TonemapOperator};
use ssx_settings_ui::{
    autostart::{Autostart, FakeAutostart},
    hdr_scene::HdrPreset,
    nav::Page,
};

use crate::common::*;

// ---- General ------------------------------------------------------------------------------

#[test]
fn the_preview_follows_the_patterns_the_way_the_engine_renders_them() {
    let mut s = Settings::default();
    s.general.save_dir = Some("/data/shots".into());
    let (app, _fx) = app_with(Page::General, s);
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert!(has(&h, "Preview: "));
    // the sandbox clock is 2025-03-09 14:05:06
    assert!(has(&h, "/data/shots/Screenshots/2025-03/Screenshot_2025-03-09_14-05-06.png"));
    set_text(&mut h, "File name pattern", "%t_%i{4}");
    assert!(has(&h, "Example_Domain_-_Firefox_0001.png"));
    set_text(&mut h, "Folder pattern", "%y/%mon");
    assert!(has(&h, "/2025/March/"));
    click(&mut h, "JPEG");
    assert!(has(&h, ".jpg"));
}

#[test]
fn illegal_characters_in_a_pattern_are_called_out() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "File name pattern", "a:b*c");
    assert!(has(&h, "cannot be used in a file name and will be removed"));
    assert!(!h.state().model.current_issues().blocks_save(), "a warning, not an error");
    // unsupported tokens come from the validator, next to the field
    set_text(&mut h, "File name pattern", "%hh-%date");
    assert!(has(&h, "%hh is not a supported token"));
}

#[test]
fn the_token_list_inserts_into_the_field_used_last() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1500.0));
    click(&mut h, "Show the token list");
    assert!(has_exact(&h, "%guid"), "the cheat sheet lists the tokens");
    assert!(has(&h, "%hh"), "and the ones that do not exist");
    // focus the folder pattern, then click a token
    h.get_by_label("Folder pattern").focus();
    h.step();
    click(&mut h, "%d");
    assert_eq!(working(&h).general.folder_pattern, "%y-%mo%d");
    h.get_by_label("File name pattern").focus();
    h.step();
    click(&mut h, "%unix");
    assert!(working(&h).general.file_name_pattern.ends_with("%unix"));
    assert_eq!(
        working(&h).general.folder_pattern,
        "%y-%mo%d",
        "the folder pattern was not touched again"
    );
}

#[test]
fn the_format_choice_decides_whether_quality_applies() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert!(has(&h, "PNG and WebP are lossless"));
    click(&mut h, "JPEG");
    assert!(has(&h, "Lower is smaller"));
    click(&mut h, "WebP");
    assert_eq!(working(&h).general.image_format, ImageFormatKind::Webp);
}

#[test]
fn subfolders_can_be_switched_off_and_named() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "Images", "Shots");
    assert_eq!(working(&h).general.subfolders.image, "Shots");
    set_text(&mut h, "Videos", "a/b");
    assert!(has(&h, "is not a valid folder name"));
    click(&mut h, "Use a folder per type");
    assert!(!working(&h).general.use_type_subfolders);
    assert!(!has_exact(&h, "Videos"), "the four fields are hidden");
    assert!(!h.state().model.current_issues().blocks_save(), "and a bad name no longer counts");
}

#[test]
fn the_save_folder_can_be_typed_browsed_and_reset() {
    let (app, fx) = app_custom(Page::General, Settings::default(), |host, _| {
        let d = ssx_settings_ui::host::ScriptedDialogs::default();
        *d.folder.lock().unwrap() = Some("/picked/by/dialog".into());
        host.dialogs = Arc::new(d);
    });
    let _ = &fx;
    let mut h = window(app, vec2(1120.0, 1700.0));
    set_text(&mut h, "Save folder", "/tmp/somewhere");
    assert_eq!(
        working(&h).general.save_dir.as_deref(),
        Some(std::path::Path::new("/tmp/somewhere"))
    );
    assert!(has(&h, "does not exist yet"));
    click(&mut h, "Browse...");
    wait_busy(&mut h, 10);
    assert_eq!(
        working(&h).general.save_dir.as_deref(),
        Some(std::path::Path::new("/picked/by/dialog"))
    );
    click(&mut h, "Use default");
    assert_eq!(working(&h).general.save_dir, None);
    // a file where a folder is needed is an error the user sees before saving
    let file = fx.root().join("not-a-folder");
    std::fs::write(&file, "x").unwrap();
    set_text(&mut h, "Save folder", &file.display().to_string());
    assert!(has(&h, "is a file, not a folder"));
}

#[test]
fn autostart_is_applied_immediately_through_the_os_trait_and_is_not_a_setting() {
    let fake = Arc::new(FakeAutostart::new(false));
    let f2 = fake.clone();
    let (app, fx) =
        app_custom(Page::General, Settings::default(), move |host, _| host.autostart = f2);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "Start ssx when I log in");
    assert_eq!(fake.calls(), [true]);
    assert!(!h.state().model.is_dirty(), "autostart is not part of settings.toml");
    click(&mut h, "Start ssx when I log in");
    assert_eq!(fake.calls(), [true, false]);
    assert_eq!(fx.load(), Settings::default());
}

#[test]
fn an_autostart_failure_is_shown() {
    let fake = Arc::new(FakeAutostart::new(false));
    fake.fail_with("access denied");
    let f2 = fake.clone();
    let (app, _fx) =
        app_custom(Page::General, Settings::default(), move |host, _| host.autostart = f2);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "Start ssx when I log in");
    assert!(has(&h, "access denied"));
    assert!(!fake.is_enabled().unwrap());
}

#[test]
fn history_retention_fields_edit_the_settings() {
    let (app, _fx) = app(Page::General);
    let mut h = window(app, vec2(1120.0, 1500.0));
    click(&mut h, "Remember what was captured and uploaded");
    assert!(!working(&h).history.enabled);
}

// ---- Capture & HDR ------------------------------------------------------------------------

#[test]
fn the_cursor_and_delay_are_editable() {
    let (app, fx) = app(Page::Capture);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "Include the cursor in screenshots");
    click(&mut h, "3 s");
    assert!(working(&h).capture.show_cursor);
    assert_eq!(working(&h).capture.delay_ms, 3000);
    click(&mut h, "Apply");
    assert_eq!(fx.load().capture.delay_ms, 3000);
}

#[test]
fn choosing_a_preset_sets_the_values_and_the_preview_shows_the_trade_off() {
    let (app, _fx) = app(Page::Capture);
    let mut h = window(app, vec2(1120.0, 1700.0));
    wait_preview(&mut h);
    assert!(has(&h, "UI stays byte-identical"), "Faithful keeps the window untouched");
    click(&mut h, "Preserve highlights");
    assert_eq!(working(&h).capture.hdr, HdrConfig::preserve_highlights());
    wait_preview(&mut h);
    assert!(has(&h, "darker"), "a lower knee dims UI white and says so");
    click(&mut h, "Faithful");
    wait_preview(&mut h);
    assert_eq!(working(&h).capture.hdr, HdrConfig::faithful());
    assert!(has(&h, "UI stays byte-identical"));
}

#[test]
fn editing_a_value_moves_the_preset_to_custom_and_the_operator_applies() {
    let (app, _fx) = app(Page::Capture);
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "BT.2390");
    assert_eq!(working(&h).capture.hdr.operator, TonemapOperator::Bt2390);
    assert_eq!(HdrPreset::classify(&working(&h).capture.hdr), HdrPreset::Custom);
    click(&mut h, "Faithful");
    assert_eq!(working(&h).capture.hdr.operator, TonemapOperator::ReinhardExtended);
    click(&mut h, "Custom");
    assert_eq!(working(&h).capture.hdr, HdrConfig::faithful(), "choosing Custom keeps the values");
}

#[test]
fn a_slider_can_be_moved_with_the_keyboard_and_the_preview_follows() {
    let (app, _fx) = app(Page::Capture);
    let mut h = window(app, vec2(1120.0, 1700.0));
    wait_preview(&mut h);
    h.get_by_label("Knee").focus();
    h.step();
    for _ in 0..5 {
        h.key_press(egui::Key::ArrowLeft);
        h.step();
    }
    settle(&mut h);
    assert!(working(&h).capture.hdr.knee < 1.0, "{}", working(&h).capture.hdr.knee);
    wait_preview(&mut h);
    let shown = h.state().capture.latest().cloned().unwrap();
    assert!(
        (shown.params.config.knee - working(&h).capture.hdr.knee).abs() < 1e-6,
        "the newest settings were rendered"
    );
}

#[test]
fn the_sdr_white_level_changes_the_preview_but_not_the_settings() {
    let (app, _fx) = app(Page::Capture);
    let mut h = window(app, vec2(1120.0, 1700.0));
    wait_preview(&mut h);
    let before = working(&h);
    click(&mut h, "480 nits");
    wait_preview(&mut h);
    assert_eq!(working(&h), before);
    assert_eq!(h.state().capture.latest().unwrap().params.sdr_white_nits, 480.0);
    assert!(has(&h, "at 480 nits SDR white"));
    // and the guarantee holds at that level too
    assert!(has(&h, "UI stays byte-identical"));
}

#[test]
fn an_out_of_range_value_pauses_the_preview_and_blocks_saving() {
    let mut s = Settings::default();
    s.capture.hdr.knee = 2.5;
    let (app, _fx) = app_with(Page::Capture, s);
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert!(has(&h, "The preview is paused until the values above are in range"));
    assert!(has(&h, "knee must be between 0 and 1"));
}

#[test]
fn the_reset_button_returns_to_faithful() {
    let mut s = Settings::default();
    s.capture.hdr = HdrConfig {
        peak: 9.0,
        knee: 0.3,
        exposure: 1.5,
        dither: false,
        operator: TonemapOperator::Clip,
    };
    let (app, _fx) = app_with(Page::Capture, s);
    let mut h = window(app, vec2(1120.0, 1700.0));
    assert_eq!(h.state().capture.preset(&working(&h).capture.hdr), HdrPreset::Custom);
    click(&mut h, "Reset to Faithful");
    assert_eq!(working(&h).capture.hdr, HdrConfig::faithful());
}
