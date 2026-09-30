//! Windows integrations against the in-memory registry and a sandboxed `%APPDATA%`.
//! (The real `windows-registry` backend is compile-checked only.)

mod common;

use std::sync::Arc;

use common::{Sandbox, assert_golden, read};
use ssx_shell::windows::manifest::{SparsePackageParams, sparse_package_manifest};
use ssx_shell::windows::{ClassicVerbs, SendTo};
use ssx_shell::{
    InstallOutcome, Integration, Integrations, MemoryRegistry, Platform, RegistryBackend, Status,
    UninstallOutcome,
};

const EXE: &str = r"C:\Program Files\ssx\ssx.exe";

fn setup() -> (Sandbox, Arc<MemoryRegistry>, ClassicVerbs) {
    let s = Sandbox::new(EXE, Platform::Windows);
    let reg = Arc::new(MemoryRegistry::new());
    let verbs = ClassicVerbs::new(reg.clone());
    (s, reg, verbs)
}

#[test]
fn classic_verbs_registry_layout_golden() {
    let (s, reg, verbs) = setup();
    assert_eq!(verbs.install(&s.ctx).expect("install"), InstallOutcome::Installed);
    assert_golden("windows/registry.txt", &(reg.snapshot().join("\n") + "\n"));
}

#[test]
fn classic_verbs_key_details() {
    let (s, reg, verbs) = setup();
    verbs.install(&s.ctx).expect("install");
    let get = |k: &str, n: &str| reg.get_string(k, n).expect("get");
    let up = r"Software\Classes\*\shell\ssx.upload";
    assert_eq!(get(up, "MUIVerb").as_deref(), Some("Upload with ssx"));
    assert_eq!(get(up, "MultiSelectModel").as_deref(), Some("Player"));
    assert_eq!(get(up, "Icon").as_deref(), Some(r#""C:\Program Files\ssx\ssx.exe",0"#));
    assert_eq!(
        get(&format!(r"{up}\command"), "").as_deref(),
        Some(r#""C:\Program Files\ssx\ssx.exe" post-file --coalesce -- "%1""#)
    );
    // Folders too, for the upload entry.
    assert!(reg.key_exists(r"Software\Classes\Directory\shell\ssx.upload").expect("dir"));
    // The image editor is single-file and lives under the perceived type plus extras.
    let edit = r"Software\Classes\SystemFileAssociations\image\shell\ssx.edit";
    assert_eq!(get(edit, "MultiSelectModel").as_deref(), Some("Single"));
    assert_eq!(
        get(&format!(r"{edit}\command"), "").as_deref(),
        Some(r#""C:\Program Files\ssx\ssx.exe" edit -- "%1""#)
    );
    assert!(
        reg.key_exists(r"Software\Classes\SystemFileAssociations\.webp\shell\ssx.edit")
            .expect("webp")
    );
    assert!(
        !reg.key_exists(r"Software\Classes\SystemFileAssociations\.png\shell\ssx.edit")
            .expect("png")
    );
    // Video entry.
    assert!(
        reg.key_exists(r"Software\Classes\SystemFileAssociations\video\shell\ssx.upload-video")
            .expect("video")
    );
    assert!(
        reg.key_exists(r"Software\Classes\SystemFileAssociations\.mkv\shell\ssx.upload-video")
            .expect("mkv")
    );
    // Every key is marked as ours.
    for line in reg
        .snapshot()
        .iter()
        .filter(|l| l.contains(r"\shell\ssx.") && !l.contains('|') && !l.ends_with("command"))
    {
        let key = line.as_str();
        assert_eq!(get(key, "SsxManaged").as_deref(), Some("1"), "{key}");
    }
}

#[test]
fn install_is_idempotent_and_uninstall_restores_the_registry() {
    let (s, reg, verbs) = setup();
    // Foreign content in the same neighbourhood must survive untouched.
    reg.set_string(r"Software\Classes\*\shell\OtherApp", "MUIVerb", "Other").expect("set");
    reg.set_string(r"Software\Classes\*\shell\OtherApp\command", "", "other.exe %1").expect("set");
    reg.set_string(r"Software\Classes\SystemFileAssociations\image\shell\open", "", "x")
        .expect("set");
    let before = reg.snapshot();
    assert!(!verbs.is_installed(&s.ctx).expect("check"));
    assert_eq!(verbs.install(&s.ctx).expect("install"), InstallOutcome::Installed);
    assert!(verbs.is_installed(&s.ctx).expect("check"));
    let installed = reg.snapshot();
    assert_ne!(before, installed);
    assert_eq!(verbs.install(&s.ctx).expect("again"), InstallOutcome::AlreadyPresent);
    assert_eq!(reg.snapshot(), installed, "idempotent");
    assert_eq!(verbs.uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::Removed);
    assert_eq!(
        reg.snapshot(),
        before,
        "uninstall must restore the registry, including created parent keys"
    );
    assert_eq!(verbs.uninstall(&s.ctx).expect("again"), UninstallOutcome::NotPresent);
}

#[test]
fn uninstall_from_a_pristine_registry_leaves_it_pristine() {
    let (s, reg, verbs) = setup();
    // A real HKCU always has Software\Classes; uninstall must not remove that far up.
    reg.set_string(r"Software\Classes\.txt", "", "txtfile").expect("seed");
    let before = reg.snapshot();
    verbs.install(&s.ctx).expect("install");
    verbs.uninstall(&s.ctx).expect("uninstall");
    assert_eq!(reg.snapshot(), before);
}

#[test]
fn moved_exe_updates_in_place() {
    let (mut s, reg, verbs) = setup();
    verbs.install(&s.ctx).expect("install");
    s.ctx.ssx_exe = r"D:\Tools\ssx\ssx.exe".into();
    assert!(!verbs.is_installed(&s.ctx).expect("check"));
    assert_eq!(verbs.install(&s.ctx).expect("update"), InstallOutcome::Updated);
    let cmd = reg.get_string(r"Software\Classes\*\shell\ssx.upload\command", "").expect("get");
    assert_eq!(cmd.as_deref(), Some(r#""D:\Tools\ssx\ssx.exe" post-file --coalesce -- "%1""#));
}

#[test]
fn foreign_key_with_our_name_is_never_overwritten_or_deleted() {
    let (s, reg, verbs) = setup();
    reg.set_string(r"Software\Classes\*\shell\ssx.upload", "MUIVerb", "Someone else's")
        .expect("set");
    let before = reg.snapshot();
    let err = verbs.install(&s.ctx).expect_err("must refuse");
    assert!(err.to_string().contains("not created by ssx"), "{err}");
    assert_eq!(reg.snapshot(), before, "nothing may be written when a conflict exists");
    assert_eq!(verbs.uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::NotPresent);
    assert_eq!(reg.snapshot(), before);
}

#[test]
fn exe_path_with_a_quote_is_rejected() {
    let (mut s, reg, verbs) = setup();
    s.ctx.ssx_exe = "C:\\bad\"dir\\ssx.exe".into();
    assert!(verbs.install(&s.ctx).is_err());
    assert!(reg.snapshot().is_empty());
}

#[test]
fn unc_exe_paths_are_accepted() {
    let (mut s, reg, verbs) = setup();
    s.ctx.ssx_exe = r"\\server\share\ssx\ssx.exe".into();
    verbs.install(&s.ctx).expect("install");
    let cmd = reg
        .get_string(r"Software\Classes\*\shell\ssx.upload\command", "")
        .expect("get")
        .expect("cmd");
    assert!(cmd.starts_with(r#""\\server\share\ssx\ssx.exe" "#), "{cmd}");
}

#[test]
fn send_to_cmd_golden_lifecycle_and_no_console_injection() {
    let s = Sandbox::new(EXE, Platform::Windows);
    let dir = s.ctx.appdata.clone().expect("appdata").join(r"Microsoft/Windows/SendTo");
    let before = s.snapshot();
    assert_eq!(SendTo::new().install(&s.ctx).expect("install"), InstallOutcome::Installed);
    let text = read(&dir.join("Upload with ssx.cmd"));
    assert_golden("windows/Upload with ssx.cmd", &text);
    assert!(text.contains("\r\n") && !text.replace("\r\n", "").contains('\n'), "CRLF only");
    assert_eq!(SendTo::new().install(&s.ctx).expect("again"), InstallOutcome::AlreadyPresent);
    // Only multi-select actions get a Send To entry.
    assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), 1);
    assert_eq!(SendTo::new().uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::Removed);
    assert_eq!(s.snapshot(), before);
}

#[test]
fn send_to_batch_file_escapes_percent_and_switches_codepage_for_unicode() {
    let mut s = Sandbox::new(EXE, Platform::Windows);
    s.ctx.ssx_exe = r"C:\Users\Zoë 100%\ssx.exe".into();
    SendTo::new().install(&s.ctx).expect("install");
    let dir = s.ctx.appdata.clone().expect("appdata").join(r"Microsoft/Windows/SendTo");
    let text = read(&dir.join("Upload with ssx.cmd"));
    assert!(text.contains("chcp 65001 >nul"), "{text}");
    assert!(text.contains(r#""C:\Users\Zoë 100%%\ssx.exe" post-file -- %*"#), "{text}");
    assert!(!text.starts_with('\u{feff}'), "a BOM would break the first command");
}

#[test]
fn full_windows_flow_through_the_integrations_registry() {
    let s = Sandbox::new(EXE, Platform::Windows);
    let reg = Arc::new(MemoryRegistry::new());
    reg.set_string(r"Software\Classes\.txt", "", "txtfile").expect("seed");
    let reg_before = reg.snapshot();
    let all = Integrations::for_platform_with_registry(Platform::Windows, Some(reg.clone()));
    let before = s.snapshot();
    let report = all.install_all(&s.ctx);
    assert!(report.entries.iter().all(|e| e.status == Status::Installed), "{report}");
    let removed = all.uninstall_all(&s.ctx);
    assert!(removed.entries.iter().all(|e| e.status == Status::Removed), "{removed}");
    assert_eq!(reg.snapshot(), reg_before);
    assert_eq!(s.snapshot(), before);
    // Without a registry backend only Send To is offered (e.g. compiled for another OS).
    let sendto_only = Integrations::for_platform_with_registry(Platform::Windows, None);
    assert_eq!(sendto_only.iter().count(), 1);
}

#[test]
fn describe_lists_hkcu_keys() {
    let (s, _reg, verbs) = setup();
    let d = verbs.describe(&s.ctx);
    assert!(d.artefacts.iter().any(|a| a == r"HKCU\Software\Classes\*\shell\ssx.upload"), "{d}");
    assert!(d.notes.iter().any(|n| n.contains("Show more options")));
}

#[test]
fn sparse_package_manifest_golden_and_well_formed() {
    let manifest =
        sparse_package_manifest(&SparsePackageParams::ssx_defaults("0.1.0.0")).expect("manifest");
    assert_golden("windows/AppxManifest.xml", &manifest);
    // Well-formed XML with the elements the design relies on.
    let mut reader = quick_xml::Reader::from_str(&manifest);
    let mut names = Vec::new();
    loop {
        match reader.read_event().expect("valid xml") {
            quick_xml::events::Event::Eof => break,
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => {
                names.push(e.name().into_inner().to_owned());
            }
            _ => {}
        }
    }
    for want in [
        "Identity",
        "uap10:AllowExternalContent",
        "desktop4:FileExplorerContextMenus",
        "desktop5:ItemType",
        "desktop5:Verb",
        "com:SurrogateServer",
        "com:Class",
    ] {
        assert!(names.iter().any(|n| n == want), "missing <{want}>");
    }
    // CLSIDs referenced by verbs are registered as classes.
    for a in ssx_shell::Action::defaults() {
        let clsid = ssx_shell::windows::manifest::clsid_for(&a);
        assert!(manifest.contains(&format!("Clsid=\"{clsid}\"")), "{clsid}");
        assert!(manifest.contains(&format!("Id=\"{}\"", clsid.trim_matches(['{', '}']))));
    }
}
