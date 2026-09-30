//! Executes the generated shims with real interpreters and a fake `ssx` that records its argv,
//! proving hostile file names arrive as exact argv elements. Unix only; each test skips
//! cleanly (with a printed reason) when a needed program is missing.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::Sandbox;
use ssx_shell::linux::desktop_entry::DesktopEntries;
use ssx_shell::linux::nautilus::{Nautilus, NautilusVariant};
use ssx_shell::{Integration, Platform};

/// Directory name that is hostile for shells and desktop files, used for the ssx location.
const HOSTILE_DIR: &str = "it's a $HOME `id` \"dir\" back\\slash";

const HOSTILE_NAMES: &[&str] = &[
    "plain.png",
    "with space.png",
    "it's.png",
    "say \"hi\".png",
    "$(touch PWNED).png",
    "`touch PWNED`.png",
    "${IFS}.png",
    "-rf.png",
    "--help.png",
    "-n.png",
    "back\\slash.png",
    "100%.png",
    "%F.png",
    "star*.png",
    "quest?.png",
    "[bracket].png",
    "semi;colon&amp|pipe.png",
    "new\nline.png",
    "tab\there.png",
    "日本語.png",
    "🦀 crab.png",
    "~tilde.png",
    "#hash.png",
    "trailing space .png",
    " leading.png",
];

fn have(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Installs a fake `ssx` that appends `CALL\0arg\0arg\0...` to `$SSX_OUT`.
fn fake_ssx(root: &Path) -> PathBuf {
    let dir = root.join("bin").join(HOSTILE_DIR);
    fs::create_dir_all(&dir).expect("mkdir");
    let exe = dir.join("ssx");
    fs::write(
        &exe,
        "#!/bin/sh\nprintf 'CALL\\0' >> \"$SSX_OUT\"\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> \"$SSX_OUT\"; done\n",
    )
    .expect("write fake");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");
    exe
}

fn calls(out: &Path) -> Vec<Vec<String>> {
    let raw = fs::read(out).unwrap_or_default();
    let mut calls: Vec<Vec<String>> = Vec::new();
    for tok in raw.split(|b| *b == 0).filter(|t| !t.is_empty()) {
        let tok = String::from_utf8_lossy(tok).into_owned();
        if tok == "CALL" {
            calls.push(Vec::new());
        } else if let Some(c) = calls.last_mut() {
            c.push(tok);
        }
    }
    calls
}

fn pwned(dir: &Path) -> bool {
    dir.join("PWNED").exists()
}

fn nautilus_script(s: &Sandbox, label: &str) -> PathBuf {
    Nautilus::with_variant(NautilusVariant::Scripts).install(&s.ctx).expect("install");
    s.ctx.data_home.join("nautilus/scripts").join(label)
}

#[test]
fn nautilus_script_passes_hostile_names_as_exact_argv_in_every_posix_shell() {
    let mut ran = 0;
    for shell in ["sh", "dash", "bash", "ash", "busybox"] {
        if !have(shell) {
            eprintln!("skipping {shell}: not installed");
            continue;
        }
        let root = tempfile::tempdir().expect("tmp");
        let exe = fake_ssx(root.path());
        let mut s = Sandbox::new("/usr/bin/ssx", Platform::Linux);
        s.ctx.ssx_exe = exe;
        let script = nautilus_script(&s, "Upload with ssx");
        let cwd = fs::canonicalize(s.tmp.path()).expect("canon");
        let out = cwd.join("out");
        let mut names: Vec<String> = HOSTILE_NAMES.iter().map(|n| (*n).to_owned()).collect();
        names.push(format!("{}/abs path/it's.png", cwd.display()));
        let mut cmd = Command::new(shell);
        if shell == "busybox" {
            cmd.arg("sh");
        }
        let status = cmd
            .arg(&script)
            .args(&names)
            .current_dir(&cwd)
            .env("SSX_OUT", &out)
            .status()
            .expect("run");
        assert!(status.success(), "{shell}: script failed");
        let got = calls(&out);
        assert_eq!(got.len(), 1, "{shell}: one exec for a multi-select action");
        let mut want = vec!["post-file".to_owned(), "--".to_owned()];
        want.extend(names.iter().map(|n| {
            if n.starts_with('/') { n.clone() } else { format!("{}/{n}", cwd.display()) }
        }));
        assert_eq!(got[0], want, "{shell}");
        assert!(!pwned(&cwd), "{shell}: a file name was executed");
        ran += 1;
    }
    assert!(ran > 0, "no POSIX shell available");
}

#[test]
fn nautilus_script_without_selection_does_nothing() {
    let root = tempfile::tempdir().expect("tmp");
    let mut s = Sandbox::new("/usr/bin/ssx", Platform::Linux);
    s.ctx.ssx_exe = fake_ssx(root.path());
    let script = nautilus_script(&s, "Upload with ssx");
    let out = s.tmp.path().join("out");
    let status = Command::new("sh").arg(&script).env("SSX_OUT", &out).status().expect("run");
    assert!(status.success());
    assert!(calls(&out).is_empty(), "no files: ssx must not be started");
}

#[test]
fn nautilus_edit_script_filters_by_extension_and_runs_once_per_file() {
    let root = tempfile::tempdir().expect("tmp");
    let mut s = Sandbox::new("/usr/bin/ssx", Platform::Linux);
    s.ctx.ssx_exe = fake_ssx(root.path());
    let script = nautilus_script(&s, "Edit image with ssx");
    let cwd = fs::canonicalize(s.tmp.path()).expect("canon");
    let out = cwd.join("out");
    let status = Command::new("sh")
        .arg(&script)
        .args(["a.PNG", "notes.txt", "-weird name.JpEg", "png", "movie.mkv"])
        .current_dir(&cwd)
        .env("SSX_OUT", &out)
        .status()
        .expect("run");
    assert!(status.success());
    let got = calls(&out);
    let d = cwd.display();
    assert_eq!(
        got,
        [
            vec!["edit".to_owned(), "--".to_owned(), format!("{d}/a.PNG")],
            vec!["edit".to_owned(), "--".to_owned(), format!("{d}/-weird name.JpEg")],
        ]
    );
    // Nothing matches: no launch at all.
    let out2 = cwd.join("out2");
    Command::new("sh")
        .arg(&script)
        .args(["a.txt", "b.mkv"])
        .current_dir(&cwd)
        .env("SSX_OUT", &out2)
        .status()
        .expect("run");
    assert!(calls(&out2).is_empty());
}

#[test]
fn desktop_entry_exec_is_expanded_by_glib_into_exact_argv() {
    if !have("gio") {
        eprintln!("skipping: `gio` (glib) is not installed");
        return;
    }
    let root = tempfile::tempdir().expect("tmp");
    let mut s = Sandbox::new("/usr/bin/ssx", Platform::Linux);
    let exe = fake_ssx(root.path());
    s.ctx.ssx_exe = exe;
    DesktopEntries::new().install(&s.ctx).expect("install");
    let desktop = s.ctx.data_home.join("applications/ssx-upload.desktop");
    let cwd = fs::canonicalize(s.tmp.path()).expect("canon");

    let launch = |out: &Path, files: &[String]| -> bool {
        Command::new("gio")
            .arg("launch")
            .arg(&desktop)
            .args(files)
            .current_dir(&cwd)
            .env("SSX_OUT", out)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .output()
            .is_ok_and(|o| {
                if !o.status.success() {
                    eprintln!("gio launch: {}", String::from_utf8_lossy(&o.stderr));
                }
                o.status.success()
            })
    };
    let wait_for = |out: &Path| {
        for _ in 0..100 {
            if out.exists() {
                std::thread::sleep(std::time::Duration::from_millis(50));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    // Sanity check that `gio launch` works in this environment at all.
    let probe = cwd.join("probe-out");
    if !launch(&probe, &[cwd.join("probe.png").display().to_string()]) {
        eprintln!("skipping: `gio launch` cannot start applications here");
        return;
    }
    wait_for(&probe);
    if calls(&probe).is_empty() {
        eprintln!("skipping: `gio launch` did not run the application here");
        return;
    }

    let files: Vec<String> =
        HOSTILE_NAMES.iter().map(|n| cwd.join("files").join(n).display().to_string()).collect();
    let out = cwd.join("out");
    assert!(launch(&out, &files), "gio launch failed");
    wait_for(&out);
    let got = calls(&out);
    assert_eq!(got.len(), 1, "{got:?}");
    let mut want = vec!["post-file".to_owned(), "--".to_owned()];
    want.extend(files.iter().cloned());
    assert_eq!(got[0], want, "GLib must hand every path over as one argv element");
    assert!(!pwned(&cwd));
}

#[test]
fn desktop_entry_refuses_percent_in_exe_path_because_glib_cannot_load_it() {
    let s = Sandbox::new("/opt/100%/ssx", Platform::Linux);
    let err = DesktopEntries::new().install(&s.ctx).expect_err("must refuse");
    assert!(err.to_string().contains('%'), "{err}");
    assert!(s.snapshot().is_empty());
}
