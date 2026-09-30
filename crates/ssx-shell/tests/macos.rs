//! macOS Quick Action bundles: generated as text and written into a sandboxed HOME.
//! Not runnable on Linux as real Quick Actions; the plists are parsed with the `plist` crate
//! and the embedded shell script is executed with bash and a fake `ssx`.

mod common;

#[cfg(unix)]
use std::fs;
use std::path::Path;

use common::{Sandbox, assert_golden, read};
use ssx_shell::macos::QuickActions;
use ssx_shell::{InstallOutcome, Integration, Integrations, Platform, Status, UninstallOutcome};

fn sandbox() -> Sandbox {
    Sandbox::new("/Applications/ssx.app/Contents/MacOS/ssx", Platform::MacOs)
}

fn bundle(s: &Sandbox, name: &str) -> std::path::PathBuf {
    s.home().join("Library/Services").join(format!("{name}.workflow/Contents"))
}

#[test]
fn bundles_golden_and_lifecycle() {
    let s = sandbox();
    let q = QuickActions::new();
    let before = s.snapshot();
    assert!(!q.is_installed(&s.ctx).expect("check"));
    assert_eq!(q.install(&s.ctx).expect("install"), InstallOutcome::Installed);
    assert!(q.is_installed(&s.ctx).expect("check"));
    for (label, id) in [
        ("Upload with ssx", "upload"),
        ("Edit image with ssx", "edit"),
        ("Upload video with ssx", "upload-video"),
    ] {
        let dir = bundle(&s, label);
        assert_golden(&format!("macos/{id}.Info.plist"), &read(&dir.join("Info.plist")));
        assert_golden(&format!("macos/{id}.document.wflow"), &read(&dir.join("document.wflow")));
    }
    let installed = s.snapshot();
    assert_eq!(q.install(&s.ctx).expect("again"), InstallOutcome::AlreadyPresent);
    assert_eq!(s.snapshot(), installed);
    assert_eq!(q.uninstall(&s.ctx).expect("uninstall"), UninstallOutcome::Removed);
    assert_eq!(
        s.snapshot(),
        before,
        "uninstall must remove the bundles and the directories it created"
    );
    assert_eq!(q.uninstall(&s.ctx).expect("again"), UninstallOutcome::NotPresent);
    // pbs is asked to rescan after install and after uninstall, but not for no-ops.
    let calls = s.runner.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| c[0] == "/System/Library/CoreServices/pbs" && c[1] == "-update"));
}

fn dict<'a>(v: &'a plist::Value, key: &str) -> &'a plist::Value {
    v.as_dictionary().and_then(|d| d.get(key)).unwrap_or_else(|| panic!("missing key {key}"))
}

#[test]
fn info_plist_declares_a_finder_service() {
    let s = sandbox();
    QuickActions::new().install(&s.ctx).expect("install");
    for (label, uti) in [
        ("Upload with ssx", "public.item"),
        ("Edit image with ssx", "public.image"),
        ("Upload video with ssx", "public.movie"),
    ] {
        let v = plist::Value::from_file(bundle(&s, label).join("Info.plist")).expect("parse");
        let svc = &dict(&v, "NSServices").as_array().expect("array")[0];
        assert_eq!(dict(dict(svc, "NSMenuItem"), "default").as_string(), Some(label));
        assert_eq!(dict(svc, "NSMessage").as_string(), Some("runWorkflowAsService"));
        assert_eq!(
            dict(dict(svc, "NSRequiredContext"), "NSApplicationIdentifier").as_string(),
            Some("com.apple.finder")
        );
        let types = dict(svc, "NSSendFileTypes").as_array().expect("types");
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].as_string(), Some(uti), "{label}");
    }
}

fn command_string(wflow: &Path) -> String {
    let v = plist::Value::from_file(wflow).expect("parse wflow");
    let action = dict(&dict(&v, "actions").as_array().expect("actions")[0], "action");
    let params = dict(action, "ActionParameters");
    assert_eq!(
        dict(params, "inputMethod").as_signed_integer(),
        Some(1),
        "files must arrive as arguments"
    );
    assert_eq!(dict(action, "BundleIdentifier").as_string(), Some("com.apple.RunShellScript"));
    let meta = dict(&v, "workflowMetaData");
    assert_eq!(
        dict(meta, "workflowTypeIdentifier").as_string(),
        Some("com.apple.Automator.servicesMenu")
    );
    assert_eq!(dict(meta, "serviceApplicationBundleID").as_string(), Some("com.apple.finder"));
    assert_eq!(
        dict(meta, "serviceInputTypeIdentifier").as_string(),
        Some("com.apple.Automator.fileSystemObject")
    );
    dict(params, "COMMAND_STRING").as_string().expect("script").to_owned()
}

#[test]
fn workflow_documents_parse_and_carry_the_script() {
    let s = sandbox();
    QuickActions::new().install(&s.ctx).expect("install");
    let script = command_string(&bundle(&s, "Upload with ssx").join("document.wflow"));
    assert_eq!(script, "exec /Applications/ssx.app/Contents/MacOS/ssx post-file -- \"$@\"\n");
    let edit = command_string(&bundle(&s, "Edit image with ssx").join("document.wflow"));
    assert!(edit.contains("for f in \"$@\"; do") && edit.contains("edit -- \"$f\""), "{edit}");
}

#[cfg(unix)]
#[test]
fn embedded_script_passes_hostile_paths_as_exact_argv() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let root = tempfile::tempdir().expect("tmp");
    let dir = root.path().join("Apps/it's a $HOME `id` \"dir\"");
    fs::create_dir_all(&dir).expect("mkdir");
    let exe = dir.join("ssx");
    fs::write(
        &exe,
        "#!/bin/sh\nprintf 'CALL\\0' >> \"$SSX_OUT\"\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> \"$SSX_OUT\"; done\n",
    )
    .expect("write");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut s = sandbox();
    s.ctx.ssx_exe = exe;
    QuickActions::new().install(&s.ctx).expect("install");
    let names = [
        "/Users/a b/plain.png",
        "/Users/x/it's.png",
        "/Users/x/$(touch PWNED).png",
        "/Users/x/`touch PWNED`.png",
        "/Users/x/-rf",
        "/Users/x/new\nline.png",
        "/Users/x/日本語 🦀.png",
    ];
    for (label, id, per_file) in
        [("Upload with ssx", "post-file", false), ("Edit image with ssx", "edit", true)]
    {
        let script = command_string(&bundle(&s, label).join("document.wflow"));
        let out = root.path().join(format!("out-{id}"));
        let status = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .arg("bash")
            .args(names)
            .current_dir(root.path())
            .env("SSX_OUT", &out)
            .status()
            .expect("bash");
        assert!(status.success());
        let raw = fs::read(&out).expect("out");
        let mut calls: Vec<Vec<String>> = Vec::new();
        for tok in raw.split(|b| *b == 0).filter(|t| !t.is_empty()) {
            let t = String::from_utf8_lossy(tok).into_owned();
            if t == "CALL" {
                calls.push(Vec::new());
            } else {
                calls.last_mut().expect("call").push(t);
            }
        }
        if per_file {
            assert_eq!(calls.len(), names.len());
            for (c, n) in calls.iter().zip(names) {
                assert_eq!(c, &[id, "--", n]);
            }
        } else {
            assert_eq!(calls.len(), 1);
            let mut want = vec![id.to_owned(), "--".to_owned()];
            want.extend(names.iter().map(|n| (*n).to_owned()));
            assert_eq!(calls[0], want);
        }
        assert!(!root.path().join("PWNED").exists());
    }
}

#[test]
fn user_files_in_services_are_untouched_and_conflicts_are_refused() {
    let s = sandbox();
    s.write_home("Library/Services/Mine.workflow/Contents/Info.plist", "mine");
    s.write_home("Library/Services/Upload with ssx.workflow/Contents/Info.plist", "not ours");
    let before = s.snapshot();
    let err = QuickActions::new().install(&s.ctx).expect_err("conflict");
    assert!(err.to_string().contains("not created by ssx"), "{err}");
    assert_eq!(s.snapshot(), before, "all-or-nothing");
    assert_eq!(
        QuickActions::new().uninstall(&s.ctx).expect("uninstall"),
        UninstallOutcome::NotPresent
    );
    assert_eq!(s.snapshot(), before);
}

#[test]
fn labels_that_cannot_be_bundle_names_are_rejected() {
    let mut s = sandbox();
    s.ctx.actions[0].label = "Upload/with ssx".into();
    assert!(QuickActions::new().install(&s.ctx).is_err());
    assert!(s.snapshot().is_empty());
}

#[test]
fn integrations_registry_for_macos() {
    let s = sandbox();
    let all = Integrations::for_platform(Platform::MacOs);
    let report = all.install_all(&s.ctx);
    assert_eq!(
        report.get("macos-quick-actions").expect("entry").status,
        Status::Installed,
        "{report}"
    );
    let removed = all.uninstall_all(&s.ctx);
    assert_eq!(removed.get("macos-quick-actions").expect("entry").status, Status::Removed);
}
