//! The Uploaders page: destinations, forms, secrets, `.sxcu` import, test uploads.

use std::{path::Path, sync::Arc, time::Duration};

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use ssx_core::settings::Settings;
use ssx_settings_ui::{
    SettingsApp,
    host::{RecordingOpener, ScriptedDialogs},
    nav::Page,
    secrets::{MemoryVault, SecretVault},
    uploader_registry::FakeTester,
};

use crate::common::*;

const SXCU: &str = r#"{"Version":"13.7.0","Name":"My Host","DestinationType":"ImageUploader","RequestMethod":"POST","RequestURL":"https://e.example.com/up","Body":"MultipartFormData","FileFormName":"f","URL":"{json:u}"}"#;

fn settings_with(name: &str, toml: &str) -> Settings {
    let mut s = Settings::default();
    s.uploaders.insert(name.into(), toml.parse().unwrap());
    s
}

fn open(settings: Settings) -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) = app_with(Page::Uploaders, settings);
    (window(app, vec2(1120.0, 1700.0)), fx)
}

fn write_sxcu(dir: &Path, file: &str, body: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(file);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn built_in_destinations_are_listed_and_cannot_be_removed() {
    let (mut h, _fx) = open(Settings::default());
    assert!(has_exact(&h, "Destination local"));
    assert!(has_exact(&h, "Destination is.gd"));
    click(&mut h, "Destination is.gd");
    assert!(has(&h, "Built-in destinations need no setup and cannot be removed"));
    assert!(!has_exact(&h, "Remove this destination..."));
}

#[test]
fn a_new_destination_is_created_named_and_selected() {
    let (mut h, _fx) = open(Settings::default());
    click(&mut h, "New destination");
    click(&mut h, "HTTP endpoint");
    assert_eq!(text_of(&h, "Destination name"), "http");
    set_text(&mut h, "Destination name", "my-server");
    click(&mut h, "Create");
    let w = working(&h);
    let table = w.uploaders.get("my-server").expect("the table was created");
    assert_eq!(table.get("type").and_then(|v| v.as_str()), Some("http"));
    assert!(h.state().model.is_dirty(), "nothing is written until Apply or Save");
    assert!(has_exact(&h, "Destination my-server"));
    // a new HTTP destination needs a URL; the form says so, and the URL is edited in place
    assert!(!has(&h, "These settings are complete."));
    set_text(&mut h, "URL", "https://files.example.com/up");
    assert_eq!(
        working(&h).uploaders["my-server"].get("url").and_then(|v| v.as_str()),
        Some("https://files.example.com/up")
    );
    assert!(has(&h, "These settings are complete."));
}

#[test]
fn a_name_that_is_taken_cannot_be_created() {
    let (mut h, _fx) = open(Settings::default());
    click(&mut h, "New destination");
    click(&mut h, "Local folder");
    set_text(&mut h, "Destination name", "local");
    click(&mut h, "Create");
    assert!(working(&h).uploaders.is_empty(), "the dialog refused");
    assert!(has(&h, "already"));
    click(&mut h, "Cancel");
    assert!(working(&h).uploaders.is_empty());
}

#[test]
fn choices_advanced_fields_and_maps_edit_the_table() {
    let (mut h, _fx) = open(settings_with("pics", "type = 'imgur'\nclient_id = 'abc'\n"));
    click_contains(&mut h, "Thumbnail size");
    click(&mut h, "large");
    assert_eq!(
        working(&h).uploaders["pics"].get("thumbnail_size").and_then(|v| v.as_str()),
        Some("large")
    );
    assert!(!has_exact(&h, "API base URL"));
    click(&mut h, "Show advanced settings");
    set_text(&mut h, "API base URL", "http://localhost:9/3");
    assert_eq!(
        working(&h).uploaders["pics"].get("api_base").and_then(|v| v.as_str()),
        Some("http://localhost:9/3")
    );
    click(&mut h, "Hide advanced settings");
    assert!(!has_exact(&h, "API base URL"));
    // clearing an optional field removes the key: "unset" means "default"
    set_text(&mut h, "Client ID", "");
    assert!(working(&h).uploaders["pics"].get("client_id").is_none());
}

fn http_with_vault(vault: Arc<MemoryVault>) -> (Harness<'static, SettingsApp>, Fixture) {
    let s = settings_with(
        "srv",
        "type = 'http'\nurl = 'https://files.example.com/up'\nauth = 'bearer'\n",
    );
    let (app, fx) = app_custom(Page::Uploaders, s, move |host, _| host.vault = vault);
    (window(app, vec2(1120.0, 1700.0)), fx)
}

#[test]
fn a_secret_goes_to_the_vault_and_is_never_shown_or_written_to_settings() {
    let vault = Arc::new(MemoryVault::keyring());
    let (mut h, fx) = http_with_vault(vault.clone());
    wait_busy(&mut h, 10);
    assert!(has(&h, "not set"));
    set_text(&mut h, "Token or password (new value)", "hunter2-very-secret");
    click(&mut h, "Store secret");
    wait_busy(&mut h, 10);
    // the vault has it, the settings only refer to it by name
    assert_eq!(vault.exists("srv-auth-secret"), Ok(true));
    assert_eq!(
        vault.store().get("srv-auth-secret").unwrap().as_deref(),
        Some("hunter2-very-secret")
    );
    let w = working(&h);
    assert_eq!(
        w.uploaders["srv"].get("auth_secret").and_then(|v| v.as_str()),
        Some("keyring:srv-auth-secret")
    );
    assert!(!w.to_toml_string().unwrap().contains("hunter2"));
    assert!(has(&h, "stored in the keyring"));
    assert!(text_of(&h, "Token or password (new value)").is_empty(), "the box is emptied");
    assert!(
        labels(&h).iter().all(|l| !l.contains("hunter2")),
        "the value is not on screen anywhere"
    );
    // saving writes the reference, not the value
    click(&mut h, "Apply");
    let on_disk = std::fs::read_to_string(fx.settings_file()).unwrap();
    assert!(on_disk.contains("keyring:srv-auth-secret"));
    assert!(!on_disk.contains("hunter2"));
    // and it can be removed again
    click(&mut h, "Remove");
    wait_busy(&mut h, 10);
    assert_eq!(vault.exists("srv-auth-secret"), Ok(false));
    assert!(working(&h).uploaders["srv"].get("auth_secret").is_none());
}

#[test]
fn without_a_keyring_the_page_says_the_secret_is_only_kept_in_memory() {
    let vault = Arc::new(MemoryVault::memory_only());
    let (mut h, _fx) = http_with_vault(vault.clone());
    wait_busy(&mut h, 10);
    assert!(has(&h, "only be kept in memory"));
    set_text(&mut h, "Token or password (new value)", "abc123");
    click(&mut h, "Store secret");
    wait_busy(&mut h, 10);
    assert!(has(&h, "stored in memory only"));
}

#[test]
fn a_vault_that_refuses_leaves_the_settings_untouched() {
    let vault = Arc::new(MemoryVault::keyring());
    vault.fail_with("keyring is locked");
    let (mut h, _fx) = http_with_vault(vault.clone());
    set_text(&mut h, "Token or password (new value)", "abc123");
    click(&mut h, "Store secret");
    wait_busy(&mut h, 10);
    assert!(working(&h).uploaders["srv"].get("auth_secret").is_none());
    assert!(!h.state().model.is_dirty());
}

#[test]
fn a_secret_from_the_environment_cannot_be_changed_here() {
    let vault = Arc::new(MemoryVault::keyring().with_env("srv-auth-secret"));
    let s = settings_with(
        "srv",
        "type = 'http'\nurl = 'https://f.example.com/up'\nauth = 'bearer'\nauth_secret = 'keyring:srv-auth-secret'\n",
    );
    let (app, _fx) = app_custom(Page::Uploaders, s, move |host, _| host.vault = vault);
    let mut h = window(app, vec2(1120.0, 1700.0));
    wait_busy(&mut h, 10);
    assert!(has(&h, "provided by the environment variable"));
    assert!(!has_exact(&h, "Store secret"));
}

#[test]
fn a_plain_text_secret_in_the_file_is_not_echoed() {
    let s = settings_with(
        "srv",
        "type = 'http'\nurl = 'https://f.example.com/up'\nauth = 'bearer'\nauth_secret = 'plaintext-hunter2'\n",
    );
    let (mut h, _fx) = open(s);
    assert!(has(&h, "plain-text value"));
    assert!(labels(&h).iter().all(|l| !l.contains("plaintext-hunter2")));
    let _ = &mut h;
}

fn recording_uploads(
    tester: FakeTester,
) -> (Harness<'static, SettingsApp>, Arc<RecordingOpener>, Fixture) {
    let opener = Arc::new(RecordingOpener::default());
    let o2 = opener.clone();
    let (app, fx) = app_custom(Page::Uploaders, Settings::default(), move |host, _| {
        host.opener = o2;
        host.uploads = Arc::new(tester);
    });
    let mut h = window(app, vec2(1120.0, 1700.0));
    click(&mut h, "Destination local");
    (h, opener, fx)
}

#[test]
fn a_test_upload_reports_the_link_and_the_links_can_be_copied_and_opened() {
    let (mut h, opener, _fx) = recording_uploads(FakeTester::default());
    click(&mut h, "Test upload");
    wait_busy(&mut h, 10);
    assert!(has_exact(&h, "Uploaded"));
    assert!(has(&h, "https://i.example.com/ssx-test.png"));
    assert!(has(&h, "https://i.example.com/delete/abc"), "the deletion link is shown too");
    // the first Copy is the link's
    h.get_all_by_label("Copy").next().unwrap().click();
    let mut copied = Vec::new();
    for _ in 0..4 {
        h.step();
        for c in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(t) = c {
                copied.push(t.clone());
            }
        }
    }
    assert_eq!(copied, ["https://i.example.com/ssx-test.png"]);
    h.get_all_by_label("Open").next().unwrap().click();
    settle(&mut h);
    assert_eq!(*opener.urls.lock().unwrap(), ["https://i.example.com/ssx-test.png"]);
    assert!(!h.state().model.is_dirty(), "testing changes nothing");
}

#[test]
fn a_failed_test_upload_shows_the_reason() {
    let t = FakeTester { result: Err("HTTP 403: forbidden".into()), ..FakeTester::default() };
    let (mut h, _opener, _fx) = recording_uploads(t);
    click(&mut h, "Test upload");
    wait_busy(&mut h, 10);
    assert!(has(&h, "HTTP 403: forbidden"));
    assert!(!has_exact(&h, "Uploaded"));
}

#[test]
fn a_running_test_shows_progress_and_can_be_cancelled() {
    let t = FakeTester {
        steps: vec![(0, 1000), (500, 1000), (700, 1000), (900, 1000), (1000, 1000)],
        delay: Duration::from_millis(250),
        ..FakeTester::default()
    };
    let (mut h, _opener, _fx) = recording_uploads(t);
    click(&mut h, "Test upload");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while h.state().uploaders.test.progress().is_none_or(|(sent, _)| sent < 500)
        && std::time::Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
    assert!(h.state().uploaders.test.running(), "still uploading");
    assert!(has(&h, "of 1000 B"), "the bar says how much was sent: {:?}", labels(&h));
    assert!(!has_exact(&h, "Test upload"), "no second test can start meanwhile");
    click(&mut h, "Cancel");
    wait_busy(&mut h, 10);
    assert!(has(&h, "cancelled"));
    assert!(!h.state().uploaders.test.running());
    assert!(has_exact(&h, "Test upload"), "the button is back");
}

#[test]
fn an_sxcu_file_is_imported_through_the_dialog() {
    let dialogs = Arc::new(ScriptedDialogs::default());
    let d2 = dialogs.clone();
    let (app, fx) =
        app_custom(Page::Uploaders, Settings::default(), move |host, _| host.dialogs = d2);
    let mut h = window(app, vec2(1120.0, 1700.0));
    let file = write_sxcu(&fx.root().join("dl"), "my host.sxcu", SXCU);
    *dialogs.file.lock().unwrap() = Some(file.clone());
    click(&mut h, "Import .sxcu...");
    wait_busy(&mut h, 10);
    assert!(has(&h, "Valid: \"My Host\""));
    assert_eq!(text_of(&h, "Destination name"), "My-Host");
    click(&mut h, "Import");
    let imported = ssx_services::upload::sxcu_dir(&fx.paths().config_dir).join("My-Host.sxcu");
    assert!(imported.is_file(), "copied into the config folder: {imported:?}");
    assert!(file.is_file(), "the original is left alone");
    assert!(has_exact(&h, "Destination My-Host"));
    assert!(has(&h, "imported file My-Host.sxcu"));
    assert!(!h.state().model.is_dirty(), "the settings are not involved");
}

#[test]
fn cancelling_the_file_dialog_does_nothing() {
    let (mut h, _fx) = open(Settings::default());
    click(&mut h, "Import .sxcu...");
    wait_busy(&mut h, 10);
    assert!(!has(&h, "Import a ShareX custom uploader"));
}

#[test]
fn a_broken_sxcu_file_is_explained_and_cannot_be_imported() {
    let dialogs = Arc::new(ScriptedDialogs::default());
    let d2 = dialogs.clone();
    let (app, fx) =
        app_custom(Page::Uploaders, Settings::default(), move |host, _| host.dialogs = d2);
    let mut h = window(app, vec2(1120.0, 1700.0));
    *dialogs.file.lock().unwrap() = Some(write_sxcu(fx.root(), "bad.sxcu", "{ not json"));
    click(&mut h, "Import .sxcu...");
    wait_busy(&mut h, 10);
    assert!(has(&h, "This file cannot be imported"));
    assert!(!has_exact(&h, "Destination name"));
    click(&mut h, "Close");
    assert!(!has(&h, "This file cannot be imported"));
    assert!(!ssx_services::upload::sxcu_dir(&fx.paths().config_dir).exists());
}

fn drop_file(h: &mut Harness<'_, SettingsApp>, path: &Path) {
    h.input_mut()
        .dropped_files
        .push(egui::DroppedFile { path: Some(path.to_path_buf()), ..Default::default() });
    h.step();
    h.input_mut().dropped_files.clear();
    settle(h);
}

#[test]
fn dropping_an_sxcu_file_on_the_window_starts_an_import() {
    let (mut h, fx) = open(Settings::default());
    click(&mut h, "Defaults");
    let file = write_sxcu(fx.root(), "dropped.sxcu", SXCU);
    drop_file(&mut h, &file);
    assert!(has(&h, "Valid: \"My Host\""), "{:?}", labels(&h));
    click(&mut h, "Import");
    assert!(ssx_services::upload::sxcu_dir(&fx.paths().config_dir).join("My-Host.sxcu").is_file());
    assert!(has_exact(&h, "Destination My-Host"), "the page switched back to the list");
}

#[test]
fn dropping_something_else_is_refused_with_a_message() {
    let (mut h, fx) = open(Settings::default());
    let file = write_sxcu(fx.root(), "notes.txt", "hello");
    drop_file(&mut h, &file);
    assert!(has(&h, "notes.txt is not a .sxcu file"), "{:?}", labels(&h));
    assert!(!has(&h, "Import a ShareX custom uploader"));
}

#[test]
fn importing_over_an_earlier_import_needs_a_deliberate_yes() {
    let (mut h, fx) = open(Settings::default());
    let file = write_sxcu(fx.root(), "again.sxcu", SXCU);
    let cfg = fx.paths().config_dir;
    ssx_settings_ui::uploader_registry::import(&cfg, &file, "My-Host", false).unwrap();
    // the page picks the new file up on its own
    settle(&mut h);
    drop_file(&mut h, &file);
    assert!(has_exact(&h, "Replace the file I imported earlier under this name"));
    click(&mut h, "Import");
    assert!(has(&h, "Valid:"), "the dialog stays open until the switch is on");
    click(&mut h, "Replace the file I imported earlier under this name");
    click(&mut h, "Import");
    assert!(!has(&h, "Valid:"));
}

#[test]
fn a_name_that_would_be_shadowed_by_a_built_in_is_refused() {
    let (mut h, fx) = open(Settings::default());
    let file = write_sxcu(fx.root(), "x.sxcu", SXCU);
    drop_file(&mut h, &file);
    set_text(&mut h, "Destination name", "local");
    assert!(has(&h, "already used by"));
    click(&mut h, "Import");
    assert!(has(&h, "Valid:"), "still open");
    assert!(!ssx_services::upload::sxcu_dir(&fx.paths().config_dir).exists());
}

#[test]
fn removing_a_table_asks_first_and_names_who_still_uses_it() {
    let mut s = settings_with("srv", "type = 'local'\ndir = '/tmp/x'\n");
    s.destinations.file = Some("srv".into());
    let (mut h, _fx) = open(s);
    click(&mut h, "Remove this destination...");
    assert!(has(&h, "Still used by"));
    click(&mut h, "Cancel");
    assert!(working(&h).uploaders.contains_key("srv"));
    click(&mut h, "Remove this destination...");
    click(&mut h, "Remove");
    assert!(!working(&h).uploaders.contains_key("srv"));
    assert!(h.state().model.is_dirty(), "removal is final only after Save");
}

#[test]
fn deleting_an_imported_file_removes_it_from_disk_after_confirming() {
    let (mut h, fx) = open(Settings::default());
    let cfg = fx.paths().config_dir;
    let file = write_sxcu(fx.root(), "gone.sxcu", SXCU);
    ssx_settings_ui::uploader_registry::import(&cfg, &file, "My-Host", false).unwrap();
    settle(&mut h);
    click(&mut h, "Destination My-Host");
    click(&mut h, "Delete the imported file...");
    let target = ssx_services::upload::sxcu_dir(&cfg).join("My-Host.sxcu");
    assert!(target.is_file(), "nothing happens before the confirmation");
    click(&mut h, "Delete file");
    assert!(!target.exists());
    assert!(!has_exact(&h, "Destination My-Host"));
}

#[test]
fn defaults_choose_a_destination_per_kind_of_content() {
    let (mut h, _fx) = open(settings_with("srv", "type = 'local'\n"));
    click(&mut h, "Defaults");
    click_contains(&mut h, "Images destination");
    click(&mut h, "local  (local, built in)");
    assert_eq!(working(&h).destinations.image.as_deref(), Some("local"));
    click_contains(&mut h, "Files destination");
    click(&mut h, "srv  (local, settings)");
    assert_eq!(working(&h).destinations.file.as_deref(), Some("srv"));
    assert!(has(&h, "Same as files (srv)"), "the video row says what it falls back to");
}

#[test]
fn a_default_that_points_at_nothing_is_flagged_next_to_the_picker() {
    let mut s = Settings::default();
    s.destinations.image = Some("ghost".into());
    let (mut h, _fx) = open(s);
    click(&mut h, "Defaults");
    assert!(has(&h, "ghost"));
    assert!(
        h.state().model.current_issues().blocks_save()
            || has(&h, "does not exist")
            || has(&h, "unknown")
    );
}

#[test]
fn extension_rules_are_added_and_removed() {
    let (mut h, _fx) = open(Settings::default());
    click(&mut h, "Defaults");
    set_text(&mut h, "New extension", ".ZIP");
    click(&mut h, "Add");
    assert_eq!(
        working(&h).destinations.extension_overrides.get("zip").map(String::as_str),
        Some("local")
    );
    assert!(text_of(&h, "New extension").is_empty());
    click(&mut h, "Remove the override for .zip");
    assert!(working(&h).destinations.extension_overrides.is_empty());
}

#[test]
fn file_upload_options_edit_the_settings() {
    let (mut h, _fx) = open(Settings::default());
    click(&mut h, "Defaults");
    let before = working(&h).post_file;
    click(&mut h, "Refuse");
    assert_eq!(working(&h).post_file.folders, ssx_core::settings::FolderPolicy::Error);
    click(&mut h, "Open the editor for image files");
    assert_ne!(working(&h).post_file.images_through_editor, before.images_through_editor);
}
