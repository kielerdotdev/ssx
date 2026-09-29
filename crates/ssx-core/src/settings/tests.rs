//! Load/save/migration behaviour of [`Settings`].

use super::*;

fn custom() -> Settings {
    let mut s = Settings::default();
    s.general.save_dir = Some(PathBuf::from("/data/shots"));
    s.general.image_format = ImageFormatKind::Jpg;
    s.general.image_quality = 75;
    s.general.file_name_pattern = "%t_%y%mo%d-%i{3}".into();
    s.capture.show_cursor = true;
    s.capture.delay_ms = 1500;
    s.capture.hdr = HdrConfig {
        operator: TonemapOperator::Bt2390,
        peak: 6.5,
        knee: 0.6,
        dither: false,
        exposure: -0.5,
    };
    s.destinations.image = Some("imgur".into());
    s.destinations.video = Some("my-s3".into());
    s.destinations.extension_overrides.insert("zip".into(), "my-s3".into());
    s.uploaders.insert(
        "my-s3".into(),
        toml::from_str(
            r#"
            bucket = "shots"
            region = "eu-west-1"
            secret_key = "keyring:ssx/my-s3"
            [tags]
            team = "core"
            "#,
        )
        .unwrap(),
    );
    s.history.max_entries = 500;
    s.post_file.folders = FolderPolicy::Error;
    s.hotkeys.open_history = Some("Ctrl+Shift+H".into());
    s.workflows.push(Workflow {
        id: "custom".into(),
        name: "Custom".into(),
        trigger: Trigger { hotkey: Some("Ctrl+Alt+F9".into()), cli_name: Some("custom".into()) },
        input: InputKind::CaptureMonitor,
        after_capture: vec![AfterCapture::OpenEditor, AfterCapture::Upload],
        destination: DestinationOverride { image: Some("imgur".into()), ..Default::default() },
        after_upload: vec![
            AfterUpload::ShortenUrl,
            AfterUpload::CopyUrl,
            AfterUpload::RunCommand {
                program: "notify-send".into(),
                args: vec!["Uploaded".into(), "{url}".into()],
            },
            AfterUpload::RunCommand { program: "true".into(), args: vec![] },
        ],
    });
    s
}

#[test]
fn defaults_round_trip_without_warnings() {
    let s = Settings::default();
    let text = s.to_toml_string().unwrap();
    let loaded = Settings::from_toml_str(&text).unwrap();
    assert_eq!(loaded.settings, s);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.migrated_from, None);
}

#[test]
fn customised_settings_round_trip() {
    let s = custom();
    assert!(s.validate().iter().all(|i| i.severity != Severity::Error), "{:?}", s.validate());
    let text = s.to_toml_string().unwrap();
    let loaded = Settings::from_toml_str(&text).unwrap();
    assert_eq!(loaded.settings, s, "\n{text}");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn serialised_file_is_readable_toml() {
    let text = Settings::default().to_toml_string().unwrap();
    assert!(text.contains("version = 1"));
    assert!(text.contains("[general]"));
    assert!(text.contains("[[workflows]]"));
    assert!(text.contains("after_capture = ["), "{text}");
    assert!(text.contains("hotkey = \"Ctrl+PrintScreen\""));
}

#[test]
fn empty_file_gives_defaults_with_builtin_workflows() {
    let loaded = Settings::from_toml_str("").unwrap();
    assert_eq!(loaded.settings, Settings::default());
    assert_eq!(loaded.migrated_from, Some(0), "unversioned files are v0");
    assert!(!loaded.settings.workflows.is_empty());
}

#[test]
fn missing_keys_default_individually() {
    let loaded = Settings::from_toml_str(
        "version = 1\n[general]\nimage_quality = 50\n[capture.hdr]\npeak = 8.0\n",
    )
    .unwrap();
    let s = loaded.settings;
    assert_eq!(s.general.image_quality, 50);
    assert_eq!(s.general.file_name_pattern, General::default().file_name_pattern);
    assert!((s.capture.hdr.peak - 8.0).abs() < 1e-6);
    assert!((s.capture.hdr.knee - HdrConfig::default().knee).abs() < 1e-6);
    assert_eq!(s.history, HistorySettings::default());
    assert!(loaded.warnings.is_empty());
}

#[test]
fn explicit_empty_workflow_list_is_respected() {
    let loaded = Settings::from_toml_str("version = 1\nworkflows = []\n").unwrap();
    assert!(loaded.settings.workflows.is_empty());
}

#[test]
fn workflow_missing_fields_default() {
    let loaded = Settings::from_toml_str(
        "version = 1\n[[workflows]]\nid = \"a\"\nname = \"A\"\ninput = \"clipboard\"\n",
    )
    .unwrap();
    let w = &loaded.settings.workflows[0];
    assert_eq!(w.input, InputKind::Clipboard);
    assert!(w.after_capture.is_empty() && w.after_upload.is_empty());
    assert_eq!(w.trigger, Trigger::default());
}

#[test]
fn unknown_keys_warn_with_full_path() {
    let loaded = Settings::from_toml_str(
        r#"
        version = 1
        surprise = 1
        [general]
        image_quality = 80
        colour_theme = "dark"
        [capture.hdr]
        gamma = 2.2
        [[workflows]]
        id = "a"
        name = "A"
        bogus = true
        [uploaders.imgur]
        anything = "goes"
        "#,
    )
    .unwrap();
    let w = loaded.warnings.join("\n");
    for key in
        ["`surprise`", "`general.colour_theme`", "`capture.hdr.gamma`", "`workflows[0].bogus`"]
    {
        assert!(w.contains(key), "missing {key} in:\n{w}");
    }
    assert!(!w.contains("uploaders"), "uploader tables are opaque: {w}");
    assert_eq!(loaded.settings.general.image_quality, 80, "known keys still applied");
}

#[test]
fn opaque_uploader_tables_survive_round_trip() {
    let loaded = Settings::from_toml_str(
        "version = 1\n[uploaders.custom]\nurl = \"https://x\"\nnums = [1, 2]\n[uploaders.custom.deep]\nk = 1.5\n",
    )
    .unwrap();
    let again = Settings::from_toml_str(&loaded.settings.to_toml_string().unwrap()).unwrap();
    assert_eq!(again.settings.uploaders, loaded.settings.uploaders);
    assert_eq!(again.settings.uploaders["custom"]["deep"]["k"].as_float(), Some(1.5));
}

#[test]
fn type_errors_report_line_and_key() {
    let e =
        Settings::from_toml_str("version = 1\n[general]\nimage_quality = \"high\"\n").unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("image_quality"), "{msg}");
    assert!(msg.contains("delete the file"), "{msg}");
    let e =
        Settings::from_toml_str("version = 1\n[[workflows]]\ninput = \"telepathy\"\n").unwrap_err();
    assert!(e.to_string().contains("telepathy"), "{e}");
}

#[test]
fn syntax_errors_are_reported() {
    let e = Settings::from_toml_str("[general\nx = ").unwrap_err();
    assert!(matches!(e, SettingsError::Parse { .. }));
    assert!(e.to_string().contains("line 1"), "{e}");
}

#[test]
fn v0_fixture_migrates() {
    let v0 = r#"
        [general]
        jpg_quality = 66
        image_format = "jpg"
        [capture]
        cursor = true
        [destinations]
        image = "imgur"
        videos = "yt"
    "#;
    let loaded = Settings::from_toml_str(v0).unwrap();
    assert_eq!(loaded.migrated_from, Some(0));
    let s = &loaded.settings;
    assert_eq!(s.version, CURRENT_VERSION);
    assert_eq!(s.general.image_quality, 66);
    assert!(s.capture.show_cursor);
    assert_eq!(s.destinations.video.as_deref(), Some("yt"));
    assert!(loaded.warnings.iter().any(|w| w.contains("migrated")));
    assert!(!loaded.warnings.iter().any(|w| w.contains("unknown")), "{:?}", loaded.warnings);
}

#[test]
fn newer_file_is_refused_and_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("settings.toml");
    let text = "version = 42\n[general]\nfuture = true\n";
    std::fs::write(&p, text).unwrap();
    let e = Settings::load(&p).unwrap_err();
    assert!(matches!(e, SettingsError::Migrate(MigrateError::TooNew { found: 42, .. })), "{e}");
    let e2 = Settings::load_or_recover(&p).unwrap_err();
    assert!(matches!(e2, SettingsError::Migrate(_)), "newer files must never be recovered over");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
}

#[test]
fn load_missing_file_gives_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let l = Settings::load(&tmp.path().join("nope.toml")).unwrap();
    assert!(!l.existed);
    assert_eq!(l.settings, Settings::default());
}

#[test]
fn load_migrates_in_place_and_keeps_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("settings.toml");
    let v0 = "[general]\njpg_quality = 33\n";
    std::fs::write(&p, v0).unwrap();
    let l = Settings::load(&p).unwrap();
    assert_eq!(l.migrated_from, Some(0));
    assert_eq!(std::fs::read_to_string(tmp.path().join("settings.toml.v0.bak")).unwrap(), v0);
    // second load: already current, nothing to migrate
    let l2 = Settings::load(&p).unwrap();
    assert_eq!(l2.migrated_from, None);
    assert_eq!(l2.settings.general.image_quality, 33);
    assert_eq!(l2.settings, l.settings);
}

#[test]
fn corrupt_file_recovery_moves_it_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("settings.toml");
    std::fs::write(&p, "this is = = not toml").unwrap();
    assert!(Settings::load(&p).is_err(), "plain load reports the error");
    let l = Settings::load_or_recover(&p).unwrap();
    assert_eq!(l.settings, Settings::default());
    assert!(l.warnings[0].contains("moved to"));
    assert!(!p.exists());
    let aside: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(aside.len(), 1);
    assert!(aside[0].starts_with("settings.toml.corrupt-"));
}

#[test]
fn save_and_load_round_trip_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("nested/dir/settings.toml");
    let s = custom();
    s.save(&p).unwrap();
    let l = Settings::load(&p).unwrap();
    assert!(l.existed);
    assert_eq!(l.settings, s);
    assert!(l.warnings.is_empty());
}

#[test]
fn save_refuses_invalid_settings_and_keeps_old_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("settings.toml");
    Settings::default().save(&p).unwrap();
    let before = std::fs::read(&p).unwrap();
    let mut bad = Settings::default();
    bad.general.image_quality = 0;
    bad.uploaders.insert("x".into(), toml::from_str("api_key = \"hunter2\"").unwrap());
    let e = bad.save(&p).unwrap_err();
    let SettingsError::Invalid { issues } = &e else { panic!("{e}") };
    assert_eq!(issues.len(), 2);
    let msg = e.to_string();
    assert!(msg.contains("general.image_quality") && msg.contains("uploaders.x.api_key"), "{msg}");
    assert_eq!(std::fs::read(&p).unwrap(), before);
}

#[test]
fn warnings_do_not_block_saving() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Settings::default();
    s.general.file_name_pattern = "%zzz".into();
    assert_eq!(s.validate().len(), 1);
    s.save(&tmp.path().join("s.toml")).unwrap();
}

#[test]
fn secrets_never_reach_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("s.toml");
    let mut s = Settings::default();
    s.uploaders.insert("x".into(), toml::from_str("password = \"hunter2\"").unwrap());
    assert!(s.save(&p).is_err());
    assert!(!p.exists(), "nothing written at all");
    s.uploaders.insert("x".into(), toml::from_str("password = \"keyring:ssx/x\"").unwrap());
    s.save(&p).unwrap();
    assert!(!std::fs::read_to_string(&p).unwrap().contains("hunter2"));
}

#[test]
fn workflow_lookup() {
    let s = Settings::default();
    assert_eq!(
        s.workflow_by_id("capture-region").unwrap().trigger.cli_name.as_deref(),
        Some("region")
    );
    assert_eq!(s.workflow_by_cli_name("screen").unwrap().id, "capture-fullscreen");
    assert_eq!(s.find_workflow("record").unwrap().id, "record-screen");
    assert_eq!(
        s.find_workflow("CAPTURE ACTIVE WINDOW, SAVE AND UPLOAD").unwrap().id,
        "capture-window"
    );
    assert!(s.find_workflow("nope").is_none());
    let hk: Hotkey = "printscreen".parse().unwrap();
    assert_eq!(s.workflow_for_hotkey(&hk).unwrap().id, "capture-fullscreen");
    let hk: Hotkey = "shift+ctrl+printscreen".parse().unwrap();
    assert_eq!(s.workflow_for_hotkey(&hk).unwrap().id, "capture-region-edit");
    let hk: Hotkey = "F1".parse().unwrap();
    assert!(s.workflow_for_hotkey(&hk).is_none());
}

#[test]
fn env_override_drives_file_locations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let paths =
        Paths::discover_with(|k| (k == CONFIG_DIR_ENV).then(|| root.clone().into_os_string()))
            .unwrap();
    paths.ensure().unwrap();
    Settings::default().save(&paths.settings_file()).unwrap();
    assert!(root.join("settings.toml").is_file());
}

#[test]
fn every_builtin_workflow_validates_cleanly() {
    for w in builtin_workflows() {
        let s = Settings { workflows: vec![w.clone()], ..Settings::default() };
        assert!(s.validate().is_empty(), "{}: {:?}", w.id, s.validate());
    }
}
