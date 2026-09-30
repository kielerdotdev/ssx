//! Smoke test on Wayland: starts a headless `sway` (wlroots headless backend, pixman renderer),
//! runs the real `ssx-settings-ui` binary in it on the Vulkan software driver, waits for its
//! window to appear in sway's tree with the documented `app_id`, asks sway to close it (an
//! `xdg_toplevel` close request, like clicking the window's close button) and checks the
//! process exits cleanly with the JSON outcome.
//!
//! Skips (with a printed reason) when `sway`, `swaymsg` or a Vulkan software driver is missing.

mod common;

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use common::tools::*;

/// Kills the compositor when the test ends, however it ends.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn swaymsg(runtime: &Path, sock: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("swaymsg")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("SWAYSOCK", sock)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

#[test]
fn starts_under_headless_sway_and_exits_cleanly_when_asked_to_close() {
    if let Some(why) = skip_reason(&["sway", "swaymsg"]) {
        eprintln!(
            "SKIPPED starts_under_headless_sway_and_exits_cleanly_when_asked_to_close: {why}"
        );
        return;
    }
    // sway needs a private runtime dir with a short path (socket names are length limited)
    let runtime = tempfile::Builder::new().prefix("ssx-sway").tempdir().unwrap();
    let runtime_dir = runtime.path().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = tempfile::tempdir().unwrap();
    let cfg = demo_config(root.path());
    let home = private_home(root.path());
    std::fs::write(
        runtime_dir.join("sway.conf"),
        "output HEADLESS-1 resolution 1440x900\ndefault_border none\n",
    )
    .unwrap();

    let log = std::fs::File::create(root.path().join("sway.log")).unwrap();
    let sway = Command::new("sway")
        .args(["-c"])
        .arg(runtime_dir.join("sway.conf"))
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("SWAYSOCK")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn();
    let Ok(sway) = sway else {
        eprintln!(
            "SKIPPED starts_under_headless_sway_and_exits_cleanly_when_asked_to_close: sway could not be started"
        );
        return;
    };
    let _sway = KillOnDrop(sway);

    let find = |prefix: &str| -> Option<std::path::PathBuf> {
        std::fs::read_dir(&runtime_dir).ok()?.flatten().map(|e| e.path()).find(|p| {
            p.file_name().is_some_and(|n| {
                n.to_string_lossy().starts_with(prefix) && !n.to_string_lossy().ends_with(".lock")
            })
        })
    };
    let Some((display, sock)) = wait_for(15, || Some((find("wayland-")?, find("sway-ipc.")?)))
    else {
        let log = std::fs::read_to_string(root.path().join("sway.log")).unwrap_or_default();
        eprintln!(
            "SKIPPED starts_under_headless_sway_and_exits_cleanly_when_asked_to_close: headless sway did not come up:\n{log}"
        );
        return;
    };

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ssx-settings-ui"));
    cmd.args(["--page", "history", "--json", "--config-dir"])
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("WAYLAND_DISPLAY", display.file_name().unwrap())
        .env_remove("DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in process_env(&home) {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("the binary starts");

    let appeared = wait_for(60, || {
        if child.try_wait().ok().flatten().is_some() {
            return Some(false);
        }
        swaymsg(&runtime_dir, &sock, &["-t", "get_tree"])
            .filter(|t| t.contains("\"app_id\": \"ssx-settings\""))
            .map(|_| true)
    });
    if appeared != Some(true) {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        panic!(
            "the window never appeared in sway's tree ({appeared:?}); stderr:\n{}",
            out.map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default()
        );
    }
    // Give the first frames time to run (the history loads, thumbnails decode) before closing.
    std::thread::sleep(Duration::from_secs(3));
    let killed = swaymsg(&runtime_dir, &sock, &["[app_id=\"ssx-settings\"] kill"]);
    assert!(killed.is_some(), "sway accepted the close request");

    let status = wait_for(30, || child.try_wait().ok().flatten());
    let Some(status) = status else {
        let _ = child.kill();
        panic!("the window did not exit after sway asked it to close");
    };
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(status.success(), "exit status {status:?}; stderr:\n{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not the JSON outcome ({e}): {stdout:?}"));
    assert_eq!(v["saved"], false);
    assert_eq!(v["page"], "history");
    assert_eq!(v["settings_file"], cfg.join("settings.toml").display().to_string());
}
