//! Linux integrations against a sandboxed HOME: goldens, idempotence, reversibility,
//! detection and failure isolation.

mod common;

use std::fs;

use common::{Sandbox, assert_golden, read};
use ssx_shell::linux::desktop_entry::DesktopEntries;
use ssx_shell::linux::dolphin::Dolphin;
use ssx_shell::linux::nautilus::{Nautilus, NautilusVariant};
use ssx_shell::linux::nemo::Nemo;
use ssx_shell::linux::snippets;
use ssx_shell::linux::thunar::Thunar;
use ssx_shell::{Action, InstallOutcome, Integration, Integrations, Status, UninstallOutcome};

const USER_UCA: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n<!-- my own actions -->\n<action>\n\t<icon>utilities-terminal</icon>\n\t<name>Open Terminal Here</name>\n\t<unique-id>1690000000000000-1</unique-id>\n\t<command>exo-open --working-directory %f --launch TerminalEmulator</command>\n\t<description>Example for a custom action</description>\n\t<patterns>*</patterns>\n\t<startup-notify/>\n\t<directories/>\n</action>\n<x-vendor-extension foo=\"bar\"><nested/></x-vendor-extension>\n</actions>\n";

fn install(i: &dyn Integration, s: &Sandbox) -> InstallOutcome {
    i.install(&s.ctx).unwrap_or_else(|e| panic!("{}: {e}", i.id()))
}

/// install -> idempotent re-install -> uninstall must restore the tree byte for byte.
fn lifecycle(i: &dyn Integration, s: &Sandbox) {
    let before = s.snapshot();
    assert!(!i.is_installed(&s.ctx).expect("is_installed"));
    assert_eq!(install(i, s), InstallOutcome::Installed, "{}", i.id());
    assert!(i.is_installed(&s.ctx).expect("is_installed"));
    let after_install = s.snapshot();
    assert_ne!(before, after_install);
    assert_eq!(install(i, s), InstallOutcome::AlreadyPresent, "{} must be idempotent", i.id());
    assert_eq!(s.snapshot(), after_install, "{} re-install changed the tree", i.id());
    assert_eq!(i.uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::Removed);
    assert_eq!(s.snapshot(), before, "{} uninstall did not restore the tree", i.id());
    assert_eq!(i.uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::NotPresent);
    assert!(!i.is_installed(&s.ctx).expect("is_installed"));
}

#[test]
fn nautilus_scripts_golden_and_lifecycle() {
    let s = Sandbox::linux();
    let n = Nautilus::with_variant(NautilusVariant::Scripts);
    lifecycle(&n, &s);
    install(&n, &s);
    let dir = s.ctx.data_home.join("nautilus/scripts");
    assert_golden("nautilus/upload.sh", &read(&dir.join("Upload with ssx")));
    assert_golden("nautilus/edit.sh", &read(&dir.join("Edit image with ssx")));
    assert_golden("nautilus/upload-video.sh", &read(&dir.join("Upload video with ssx")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("Upload with ssx")).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "scripts must be executable");
    }
}

#[test]
fn nautilus_extension_golden_and_lifecycle() {
    let s = Sandbox::linux();
    let n = Nautilus::with_variant(NautilusVariant::Extension);
    lifecycle(&n, &s);
    install(&n, &s);
    let p = s.ctx.data_home.join("nautilus-python/extensions/ssx-shell.py");
    assert_golden("nautilus/ssx-shell.py", &read(&p));
}

#[test]
fn nautilus_auto_prefers_extension_only_when_nautilus_python_is_installed() {
    let s = Sandbox::linux();
    let n = Nautilus::new();
    install(&n, &s);
    let scripts = s.ctx.data_home.join("nautilus/scripts/Upload with ssx");
    let ext = s.ctx.data_home.join("nautilus-python/extensions/ssx-shell.py");
    assert!(scripts.exists() && !ext.exists(), "no nautilus-python: scripts");
    assert!(n.describe(&s.ctx).notes.iter().any(|x| x.contains("Scripts")));

    // nautilus-python appears: re-install switches variant and removes the scripts.
    let lib = s.ctx.lib_dirs[0].join("nautilus/extensions-4");
    fs::create_dir_all(&lib).expect("mkdir");
    fs::write(lib.join("libnautilus-python.so"), b"").expect("touch");
    assert_eq!(install(&n, &s), InstallOutcome::Installed);
    assert!(!scripts.exists() && ext.exists(), "nautilus-python: extension only");
    assert!(n.describe(&s.ctx).notes.iter().any(|x| x.contains("nautilus-python")));
    assert_eq!(install(&n, &s), InstallOutcome::AlreadyPresent);
    n.uninstall(&s.ctx).expect("uninstall");
    assert!(!ext.exists());
}

#[test]
fn nautilus_both_variant_installs_both_and_uninstall_removes_both() {
    let s = Sandbox::linux();
    let n = Nautilus::with_variant(NautilusVariant::Both);
    lifecycle(&n, &s);
}

#[test]
fn dolphin_golden_lifecycle_and_executable_bit() {
    let s = Sandbox::linux();
    let d = Dolphin::new();
    lifecycle(&d, &s);
    install(&d, &s);
    let dir = s.ctx.data_home.join("kio/servicemenus");
    for id in ["upload", "edit", "upload-video"] {
        assert_golden(
            &format!("dolphin/ssx-{id}.desktop"),
            &read(&dir.join(format!("ssx-{id}.desktop"))),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("ssx-upload.desktop")).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "KDE ignores non-executable service menus in user dirs");
    }
}

#[test]
fn dolphin_legacy_location_is_optional_and_reversible() {
    let s = Sandbox::linux();
    let d = Dolphin::new().with_legacy_kservices5(true);
    lifecycle(&d, &s);
    install(&d, &s);
    assert!(s.ctx.data_home.join("kservices5/ServiceMenus/ssx-upload.desktop").exists());
    // Turning it off later must not leave a duplicate behind.
    assert_eq!(install(&Dolphin::new(), &s), InstallOutcome::Updated);
    assert!(!s.ctx.data_home.join("kservices5").exists());
    assert!(s.ctx.data_home.join("kio/servicemenus/ssx-upload.desktop").exists());
}

#[test]
fn nemo_golden_and_lifecycle() {
    let s = Sandbox::linux();
    lifecycle(&Nemo, &s);
    install(&Nemo, &s);
    let dir = s.ctx.data_home.join("nemo/actions");
    for id in ["upload", "edit", "upload-video"] {
        assert_golden(
            &format!("nemo/ssx-{id}.nemo_action"),
            &read(&dir.join(format!("ssx-{id}.nemo_action"))),
        );
    }
}

#[test]
fn desktop_entry_golden_lifecycle_and_database_refresh() {
    let s = Sandbox::linux();
    let d = DesktopEntries::new();
    lifecycle(&d, &s);
    let calls = s.runner.calls();
    assert!(
        calls.iter().all(|c| c[0] == "update-desktop-database"),
        "only the desktop database is refreshed: {calls:?}"
    );
    assert_eq!(calls.len(), 2, "one refresh after install, one after uninstall (none for no-ops)");
    install(&d, &s);
    let dir = s.ctx.data_home.join("applications");
    for id in ["upload", "edit", "upload-video"] {
        assert_golden(
            &format!("desktop/ssx-{id}.desktop"),
            &read(&dir.join(format!("ssx-{id}.desktop"))),
        );
    }
}

#[test]
fn desktop_entry_can_be_hidden_from_menus() {
    let s = Sandbox::linux();
    install(&DesktopEntries::new().hide_from_menus(true), &s);
    let text = read(&s.ctx.data_home.join("applications/ssx-upload.desktop"));
    assert!(text.contains("NoDisplay=true\n"));
    assert!(
        !read(
            &s.ctx
                .data_home
                .join("applications/ssx-upload.desktop")
                .with_file_name("ssx-edit.desktop")
        )
        .is_empty()
    );
}

#[test]
fn thunar_creates_file_when_missing_and_removes_it_on_uninstall() {
    let s = Sandbox::linux();
    lifecycle(&Thunar, &s);
    install(&Thunar, &s);
    assert_golden("thunar/uca-fresh.xml", &read(&s.ctx.config_home.join("Thunar/uca.xml")));
    assert!(!s.ctx.config_home.join("Thunar/uca.xml.bak").exists());
}

#[test]
fn thunar_merges_into_existing_file_preserving_everything_else() {
    let s = Sandbox::linux();
    let path = s.write_home(".config/Thunar/uca.xml", USER_UCA);
    lifecycle(&Thunar, &s);
    install(&Thunar, &s);
    let merged = read(&path);
    assert_golden("thunar/uca-merged.xml", &merged);
    assert!(
        merged.starts_with(&USER_UCA[..USER_UCA.len() - "</actions>\n".len()]),
        "user content untouched, ours appended"
    );
    assert_eq!(merged.matches("<unique-id>ssx-shell-").count(), 3);
    assert_eq!(fs::read_to_string(&path).expect("read"), merged);
    // Uninstall gives the exact original bytes back.
    Thunar.uninstall(&s.ctx).expect("uninstall");
    assert_eq!(read(&path), USER_UCA);
}

#[test]
fn thunar_updates_stale_entries_when_the_exe_moves() {
    let mut s = Sandbox::linux();
    install(&Thunar, &s);
    s.ctx.ssx_exe = "/opt/ssx new/ssx".into();
    assert!(!Thunar.is_installed(&s.ctx).expect("check"));
    assert_eq!(install(&Thunar, &s), InstallOutcome::Updated);
    let text = read(&s.ctx.config_home.join("Thunar/uca.xml"));
    assert!(text.contains("'/opt/ssx new/ssx'"), "{text}");
    assert!(!text.contains("/usr/bin/ssx"));
    assert_eq!(text.matches("<action>").count(), 3, "updated in place, not duplicated");
}

#[test]
fn thunar_refuses_to_touch_a_malformed_file() {
    let s = Sandbox::linux();
    let path = s.write_home(".config/Thunar/uca.xml", "<actions><action></actions>");
    let err = Thunar.install(&s.ctx).expect_err("must refuse");
    assert!(matches!(err, ssx_shell::ShellError::Parse { .. }), "{err}");
    assert_eq!(read(&path), "<actions><action></actions>", "file must be untouched");
    let err = Thunar.uninstall(&s.ctx).expect_err("must refuse");
    assert!(matches!(err, ssx_shell::ShellError::Parse { .. }), "{err}");
}

#[cfg(unix)]
#[test]
fn thunar_writes_through_a_symlinked_uca_xml() {
    let s = Sandbox::linux();
    let real = s.write_home("dotfiles/uca.xml", USER_UCA);
    let link = s.home().join(".config/Thunar/uca.xml");
    fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    install(&Thunar, &s);
    assert!(
        fs::symlink_metadata(&link).expect("meta").file_type().is_symlink(),
        "link must survive"
    );
    assert!(read(&real).contains("ssx-shell-upload"));
    Thunar.uninstall(&s.ctx).expect("uninstall");
    assert_eq!(read(&real), USER_UCA);
}

#[test]
fn full_linux_lifecycle_leaves_preexisting_user_files_untouched() {
    let s = Sandbox::linux();
    s.write_home(".config/Thunar/uca.xml", USER_UCA);
    s.write_home(".local/share/applications/other.desktop", "[Desktop Entry]\nName=Other\n");
    s.write_home(".local/share/nautilus/scripts/My Script", "#!/bin/sh\necho hi\n");
    s.write_home(".local/share/kio/servicemenus/theirs.desktop", "[Desktop Entry]\n");
    s.write_home(".local/share/nemo/actions/mine.nemo_action", "[Nemo Action]\n");
    let before = s.snapshot();
    let all = Integrations::for_platform(ssx_shell::Platform::Linux);
    let report = all.install_all_with(&s.ctx, true);
    assert!(!report.has_failures(), "{report}");
    assert!(report.entries.iter().all(|e| e.status == Status::Installed), "{report}");
    let again = all.install_all_with(&s.ctx, true);
    assert!(again.entries.iter().all(|e| e.status == Status::AlreadyPresent), "{again}");
    let removed = all.uninstall_all(&s.ctx);
    assert!(removed.entries.iter().all(|e| e.status == Status::Removed), "{removed}");
    assert_eq!(s.snapshot(), before, "uninstall_all must restore the pre-install tree exactly");
}

#[test]
fn detection_matrix() {
    let mut s = Sandbox::linux();
    let all = Integrations::for_platform(ssx_shell::Platform::Linux);
    let detect = |s: &Sandbox| -> Vec<(&'static str, bool)> {
        all.detect_all(&s.ctx).into_iter().map(|(id, d)| (id, d.available)).collect()
    };
    // Bare system: only the generic entry.
    assert_eq!(
        detect(&s),
        [
            ("nautilus", false),
            ("dolphin", false),
            ("thunar", false),
            ("nemo", false),
            ("desktop-entry", true)
        ]
    );
    s.add_binary("nautilus");
    s.add_binary("thunar");
    assert_eq!(
        detect(&s),
        [
            ("nautilus", true),
            ("dolphin", false),
            ("thunar", true),
            ("nemo", false),
            ("desktop-entry", true)
        ]
    );
    // Config directories count as evidence even when the binary is not on PATH.
    fs::create_dir_all(s.ctx.data_home.join("nemo")).expect("mkdir");
    assert_eq!(detect(&s)[3], ("nemo", true));
    // XDG_CURRENT_DESKTOP alone is enough for Dolphin.
    s.ctx.desktops = ssx_shell::context::parse_desktops("KDE");
    assert_eq!(detect(&s)[1], ("dolphin", true));
    // A non-executable file named like the binary is not a binary.
    let fake = s.tmp.path().join("bin/dolphin");
    fs::write(&fake, "").expect("write");
    #[cfg(unix)]
    {
        s.ctx.desktops.clear();
        assert_eq!(detect(&s)[1], ("dolphin", false));
    }
    // Detection never writes anything.
    let before = s.snapshot();
    let _ = detect(&s);
    assert_eq!(s.snapshot(), before);
}

#[test]
fn install_all_skips_undetected_and_force_overrides() {
    let s = Sandbox::linux();
    s.add_binary("nemo");
    let all = Integrations::for_platform(ssx_shell::Platform::Linux);
    let report = all.install_all(&s.ctx);
    assert_eq!(report.get("nemo").expect("nemo").status, Status::Installed);
    assert_eq!(report.get("desktop-entry").expect("de").status, Status::Installed);
    for id in ["nautilus", "dolphin", "thunar"] {
        assert!(
            matches!(report.get(id).expect("entry").status, Status::Skipped(_)),
            "{id}: {report}"
        );
    }
    assert!(!s.ctx.data_home.join("kio").exists(), "skipped integrations must not touch the disk");
    let forced = all.install_all_with(&s.ctx, true);
    assert_eq!(forced.get("dolphin").expect("dolphin").status, Status::Installed);
}

#[test]
fn one_failure_never_aborts_the_batch() {
    let s = Sandbox::linux();
    // A user file where our Nemo action wants to go.
    s.write_home(".local/share/nemo/actions/ssx-upload.nemo_action", "[Nemo Action]\nName=mine\n");
    let all = Integrations::for_platform(ssx_shell::Platform::Linux);
    let report = all.install_all_with(&s.ctx, true);
    assert!(report.has_failures());
    assert!(matches!(report.get("nemo").expect("nemo").status, Status::Failed(_)));
    for id in ["nautilus", "dolphin", "thunar", "desktop-entry"] {
        assert_eq!(report.get(id).expect("entry").status, Status::Installed, "{id}: {report}");
    }
    let text = report.to_string();
    assert!(text.contains("[FAIL]") && text.contains("was not created by ssx"), "{text}");
    assert_eq!(
        read(&s.ctx.data_home.join("nemo/actions/ssx-upload.nemo_action")),
        "[Nemo Action]\nName=mine\n",
        "the user's file must survive"
    );
    // Uninstall leaves it alone too.
    let removed = all.uninstall_all(&s.ctx);
    assert!(!removed.has_failures(), "{removed}");
    assert!(s.ctx.data_home.join("nemo/actions/ssx-upload.nemo_action").exists());
}

#[test]
fn foreign_platform_integrations_are_skipped() {
    let s = Sandbox::linux();
    let reg = std::sync::Arc::new(ssx_shell::MemoryRegistry::new());
    let everything = Integrations::everything(reg.clone());
    let report = everything.install_all_with(&s.ctx, true);
    for id in ["macos-quick-actions", "windows-verbs", "windows-sendto"] {
        assert!(
            matches!(report.get(id).expect("entry").status, Status::Skipped(_)),
            "{id}: {report}"
        );
    }
    assert!(reg.snapshot().is_empty());
}

#[test]
fn invalid_exe_path_fails_everything_cleanly() {
    let mut s = Sandbox::linux();
    s.ctx.ssx_exe = "relative/ssx".into();
    let report =
        Integrations::for_platform(ssx_shell::Platform::Linux).install_all_with(&s.ctx, true);
    assert!(report.entries.iter().all(|e| e.status.is_failure()), "{report}");
    assert!(s.snapshot().is_empty(), "nothing may be written with a bad exe path");
}

#[test]
fn custom_actions_are_supported_everywhere() {
    let mut s = Sandbox::linux();
    let mut a = Action::upload();
    a.id = "shorten".into();
    a.label = "Shorten link with ssx".into();
    a.exec_args = vec!["post-url".into(), "--shorten".into()];
    a.filter = ssx_shell::Filter::custom(&["text/uri-list"], &["url", "webloc"], false);
    a.multi_select = false;
    s.ctx.actions.push(a);
    let all = Integrations::for_platform(ssx_shell::Platform::Linux);
    let report = all.install_all_with(&s.ctx, true);
    assert!(!report.has_failures(), "{report}");
    let uca = read(&s.ctx.config_home.join("Thunar/uca.xml"));
    assert!(
        uca.contains("ssx-shell-shorten") && uca.contains("<patterns>*.url;*.webloc</patterns>"),
        "{uca}"
    );
    let nemo = read(&s.ctx.data_home.join("nemo/actions/ssx-shorten.nemo_action"));
    assert!(nemo.contains("Extensions=.url;.webloc;") && nemo.contains("Selection=s"), "{nemo}");
    let removed = all.uninstall_all(&s.ctx);
    assert!(!removed.has_failures(), "{removed}");
    assert!(s.snapshot().is_empty());
}

#[test]
fn terminal_file_manager_snippets_golden() {
    let actions = Action::defaults();
    assert_golden("snippets/yazi.toml", &snippets::yazi("/usr/bin/ssx", &actions));
    assert_golden("snippets/ranger.conf", &snippets::ranger("/usr/bin/ssx", &actions));
    assert_golden("snippets/lfrc", &snippets::lf("/usr/bin/ssx", &actions));
    // Paths with quotes are shell-quoted.
    assert!(snippets::ranger("/opt/it's/ssx", &actions).contains("'/opt/it'\\''s/ssx'"));
    assert!(snippets::all("/usr/bin/ssx", &actions).contains("== lf =="));
}
