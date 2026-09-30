//! The in-process `select()` path (no helper protocol) on Xvfb, via `ssx-overlay --demo`,
//! plus the documentation screenshot helper.
#![cfg(target_os = "linux")]

mod common;

use std::process::{Command, Stdio};

use common::{Xvfb, helper_bin};
use ssx_overlay::OverlayOutcome;
use ssx_types::Rect;

#[test]
fn demo_mode_runs_select_in_process() {
    // `ssx-overlay --demo` calls `ssx_overlay::select` directly, so this covers the
    // in-process code path with real X11 input.
    let Some(x) = Xvfb::start(800, 600) else { return };
    let child = Command::new(helper_bin())
        .args(["--demo", "800x600"])
        .env("DISPLAY", &x.display)
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(x.wait_for_window("ssx-overlay"));
    assert!(x.xdotool(&[
        "mousemove",
        "120",
        "80",
        "sleep",
        "0.1",
        "mousedown",
        "1",
        "sleep",
        "0.1",
        "mousemove",
        "270",
        "230",
        "sleep",
        "0.1",
        "mousemove",
        "420",
        "380",
        "sleep",
        "0.1",
        "mouseup",
        "1",
        "sleep",
        "0.1",
        "key",
        "Return",
        "sleep",
        "0.2",
    ]));
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let outcome: OverlayOutcome = serde_json::from_slice(&out.stdout).expect("json outcome");
    match outcome {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(120, 80, 300, 300)),
        other => panic!("{other:?}"),
    }
}

/// Regenerates `docs/overlay-x11.png`:
/// `cargo test -p ssx-overlay --test x11_inprocess capture_readme_screenshot -- --ignored`.
#[test]
#[ignore = "documentation helper; writes docs/overlay-x11.png"]
fn capture_readme_screenshot() {
    let Some(x) = Xvfb::start(1280, 720) else { return };
    let child = Command::new(helper_bin())
        .args(["--demo", "1280x720"])
        .env("DISPLAY", &x.display)
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(x.wait_for_window("ssx-overlay"));
    assert!(x.xdotool(&[
        "mousemove",
        "330",
        "170",
        "sleep",
        "0.1",
        "mousedown",
        "1",
        "sleep",
        "0.1",
        "mousemove",
        "600",
        "300",
        "sleep",
        "0.1",
        "mousemove",
        "830",
        "470",
        "sleep",
        "0.1",
        "mouseup",
        "1",
        "sleep",
        "0.1",
        "mousemove",
        "930",
        "540",
        "sleep",
        "0.4",
    ]));
    let shot = x.screenshot();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    std::fs::create_dir_all(&dir).unwrap();
    shot.save(dir.join("overlay-x11.png")).unwrap();
    assert!(x.xdotool(&["key", "Escape"]));
    let _ = child.wait_with_output();
}
