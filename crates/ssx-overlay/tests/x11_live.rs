//! Live tests of the X11 backend against a private Xvfb, driven with xdotool.
//!
//! The overlay runs in the real helper process (`select_via_helper`), so these tests cover
//! the memfd hand-off, the JSON protocol, the X11 window, grabs, key decoding and the
//! outcome path end to end. Assertions are pixel-exact.
#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use common::{Xvfb, coded_desktop, have, input, monitor, pixel, window};
use ssx_overlay::{
    HelperError, OverlayError, OverlayOptions, OverlayOutcome, SelectMode, SelectionShape,
    select_via_helper,
};
use ssx_types::{Point, Rect};

const W: u32 = 800;
const H: u32 = 600;

/// Runs the overlay on a fresh Xvfb, calls `script` once its window exists, and returns the
/// outcome.
fn run(
    options: OverlayOptions,
    windows: Vec<ssx_types::WindowInfo>,
    script: impl FnOnce(&Xvfb) + Send,
) -> Option<OverlayOutcome> {
    let x = Xvfb::start(W, H)?;
    let dir = tempfile::tempdir().unwrap();
    let helper = x.helper_wrapper(dir.path());
    let inp = input(coded_desktop(W, H, Point::new(0, 0)), vec![], windows, OverlayOptions { timeout_ms: Some(20_000), ..options });
    let out = std::thread::scope(|s| {
        let h = s.spawn(|| select_via_helper(&inp, &helper));
        assert!(x.wait_for_window("ssx-overlay"), "overlay window never appeared");
        script(&x);
        h.join().unwrap()
    });
    Some(out.expect("overlay failed"))
}

fn drag(x: &Xvfb, from: (i32, i32), to: (i32, i32)) {
    let (fx, fy, tx, ty) = (from.0.to_string(), from.1.to_string(), to.0.to_string(), to.1.to_string());
    let mx = ((from.0 + to.0) / 2).to_string();
    let my = ((from.1 + to.1) / 2).to_string();
    assert!(x.xdotool(&["mousemove", &fx, &fy, "sleep", "0.05", "mousedown", "1", "sleep", "0.05", "mousemove", &mx, &my, "sleep", "0.05", "mousemove", &tx, &ty, "sleep", "0.1", "mouseup", "1", "sleep", "0.1"]));
}

fn key(x: &Xvfb, k: &str) {
    assert!(x.xdotool(&["key", k, "sleep", "0.1"]));
}

fn selected_rect(o: &OverlayOutcome) -> Rect {
    match o {
        OverlayOutcome::Selected(s) => s.rect,
        other => panic!("expected a selection, got {other:?}"),
    }
}

#[test]
fn drag_and_enter_returns_the_exact_rectangle() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (100, 100), (300, 250));
        key(x, "Return");
    }) else {
        return;
    };
    assert_eq!(selected_rect(&out), Rect::new(100, 100, 200, 150));
    match out {
        OverlayOutcome::Selected(s) => {
            assert_eq!(s.shape, SelectionShape::Rect);
            assert!(s.snapped_window.is_none());
        }
        _ => unreachable!(),
    }
}

#[test]
fn reverse_drag_and_double_click_confirm() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (400, 300), (250, 200));
        assert!(x.xdotool(&["mousemove", "300", "250", "click", "--repeat", "2", "--delay", "60", "1", "sleep", "0.2"]));
    }) else {
        return;
    };
    assert_eq!(selected_rect(&out), Rect::new(250, 200, 150, 100));
}

#[test]
fn escape_cancels() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (100, 100), (300, 250));
        key(x, "Escape");
    }) else {
        return;
    };
    assert_eq!(out, OverlayOutcome::Cancelled);
}

#[test]
fn right_click_clears_then_cancels() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (100, 100), (300, 250));
        assert!(x.xdotool(&["click", "3", "sleep", "0.15"]));
        // Selection is gone: Enter must not confirm anything.
        key(x, "Return");
        assert!(x.xdotool(&["click", "3", "sleep", "0.15"]));
    }) else {
        return;
    };
    assert_eq!(out, OverlayOutcome::Cancelled);
}

#[test]
fn arrow_keys_nudge_and_shift_moves_by_ten() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (100, 100), (200, 200));
        key(x, "Right");
        key(x, "Down");
        key(x, "Down");
        assert!(x.xdotool(&["keydown", "shift", "key", "Left", "keyup", "shift", "sleep", "0.15"]));
        key(x, "Return");
    }) else {
        return;
    };
    assert_eq!(selected_rect(&out), Rect::new(91, 102, 100, 100));
}

#[test]
fn shift_drag_is_square() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        assert!(x.xdotool(&["mousemove", "100", "100", "sleep", "0.05", "mousedown", "1", "sleep", "0.05", "keydown", "shift", "sleep", "0.05", "mousemove", "260", "160", "sleep", "0.1", "mouseup", "1", "keyup", "shift", "sleep", "0.1"]));
        key(x, "Return");
    }) else {
        return;
    };
    assert_eq!(selected_rect(&out), Rect::new(100, 100, 160, 160));
}

#[test]
fn hover_snap_selects_the_window_rectangle() {
    let wins = vec![window("front", Rect::new(50, 60, 300, 200)), window("back", Rect::new(0, 0, 700, 500))];
    let Some(out) = run(OverlayOptions::default(), wins, |x| {
        assert!(x.xdotool(&["mousemove", "500", "400", "sleep", "0.1", "mousemove", "120", "120", "sleep", "0.1", "click", "1", "sleep", "0.2"]));
    }) else {
        return;
    };
    match out {
        OverlayOutcome::Selected(s) => {
            assert_eq!(s.rect, Rect::new(50, 60, 300, 200));
            assert_eq!(s.snapped_window.unwrap().title, "front");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn window_mode_returns_the_window() {
    let wins = vec![window("front", Rect::new(50, 60, 300, 200)), window("back", Rect::new(0, 0, 700, 500))];
    let opts = OverlayOptions { mode: SelectMode::Window, ..OverlayOptions::default() };
    let Some(out) = run(opts, wins, |x| {
        assert!(x.xdotool(&["mousemove", "600", "450", "sleep", "0.1", "click", "1", "sleep", "0.2"]));
    }) else {
        return;
    };
    match out {
        OverlayOutcome::Window(w) => assert_eq!(w.title, "back"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn ellipse_and_freeform_modes() {
    let opts = OverlayOptions { mode: SelectMode::Ellipse, ..OverlayOptions::default() };
    let Some(out) = run(opts, vec![], |x| {
        drag(x, (100, 100), (300, 200));
        key(x, "Return");
    }) else {
        return;
    };
    match out {
        OverlayOutcome::Selected(s) => {
            assert_eq!(s.rect, Rect::new(100, 100, 200, 100));
            assert_eq!(s.shape, SelectionShape::Ellipse);
            assert!(s.contains(Point::new(200, 150)) && !s.contains(Point::new(100, 100)));
        }
        other => panic!("{other:?}"),
    }

    let opts = OverlayOptions { mode: SelectMode::Freeform, ..OverlayOptions::default() };
    let Some(out) = run(opts, vec![], |x| {
        assert!(x.xdotool(&[
            "mousemove", "100", "100", "sleep", "0.05", "mousedown", "1", "sleep", "0.05",
            "mousemove", "200", "100", "sleep", "0.05", "mousemove", "220", "180", "sleep", "0.05",
            "mousemove", "120", "200", "sleep", "0.1", "mouseup", "1", "sleep", "0.2",
        ]));
    }) else {
        return;
    };
    match out {
        OverlayOutcome::Selected(s) => {
            assert_eq!(s.rect, Rect::new(100, 100, 120, 100));
            let SelectionShape::Freeform(pts) = &s.shape else { panic!("{:?}", s.shape) };
            assert_eq!(pts.first(), Some(&Point::new(100, 100)));
            assert_eq!(pts.last(), Some(&Point::new(120, 200)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn colour_pick_reports_the_pixel_under_the_pointer() {
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        assert!(x.xdotool(&["mousemove", "123", "77", "sleep", "0.15"]));
        key(x, "c");
    }) else {
        return;
    };
    let want = pixel(&coded_desktop(W, H, Point::new(0, 0)), 123, 77);
    match out {
        OverlayOutcome::ColorPicked(c) => {
            assert_eq!(c.point, Point::new(123, 77));
            assert_eq!(c.rgb, want);
            assert_eq!(c.hex().len(), 7);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn tab_cycles_monitors_and_enter_confirms() {
    let x = match Xvfb::start(W, H) {
        Some(x) => x,
        None => return,
    };
    let dir = tempfile::tempdir().unwrap();
    let helper = x.helper_wrapper(dir.path());
    let mons = vec![monitor("L", Rect::new(0, 0, 500, 600), 1.0), monitor("R", Rect::new(500, 0, 300, 600), 1.0)];
    let inp = input(coded_desktop(W, H, Point::new(0, 0)), mons, vec![], OverlayOptions { mode: SelectMode::Monitor, timeout_ms: Some(20_000), ..OverlayOptions::default() });
    let out = std::thread::scope(|s| {
        let h = s.spawn(|| select_via_helper(&inp, &helper));
        assert!(x.wait_for_window("ssx-overlay"));
        assert!(x.xdotool(&["mousemove", "100", "100", "sleep", "0.1", "key", "Tab", "sleep", "0.1", "key", "Return", "sleep", "0.1"]));
        h.join().unwrap()
    })
    .unwrap();
    match out {
        OverlayOutcome::Monitor(m) => assert_eq!(m.name, "R"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn initial_region_is_restored_and_confirmed() {
    let opts = OverlayOptions { initial: Some(Rect::new(10, 20, 30, 40)), ..OverlayOptions::default() };
    let Some(out) = run(opts, vec![], |x| {
        key(x, "Return");
    }) else {
        return;
    };
    assert_eq!(selected_rect(&out), Rect::new(10, 20, 30, 40));
}

#[test]
fn overlay_covers_the_desktop_dimmed_with_a_bright_selection() {
    let mut shot = None;
    let Some(out) = run(OverlayOptions::default(), vec![], |x| {
        drag(x, (100, 100), (300, 250));
        // Park the pointer away from the guides we sample.
        assert!(x.xdotool(&["mousemove", "500", "500", "sleep", "0.2"]));
        shot = Some(x.screenshot());
        key(x, "Escape");
    }) else {
        return;
    };
    assert_eq!(out, OverlayOutcome::Cancelled);
    let shot = shot.unwrap();
    let want = coded_desktop(W, H, Point::new(0, 0));
    // Inside the selection: untouched.
    assert_eq!(pixel(&shot, 200, 150), pixel(&want, 200, 150));
    assert_eq!(pixel(&shot, 110, 120), pixel(&want, 110, 120), "selection interior is bright");
    assert_eq!(pixel(&shot, 299, 220), pixel(&want, 299, 220), "last column is bright");
    assert_eq!(pixel(&shot, 250, 249), pixel(&want, 250, 249), "last row is bright");
    assert_ne!(pixel(&shot, 300, 220), pixel(&want, 300, 220), "one pixel further out is border");
    // Outside: dimmed by exactly 50%.
    for (x, y) in [(20, 20), (700, 100), (60, 400), (650, 580)] {
        let w = pixel(&want, x, y);
        let dimmed = [0, 1, 2].map(|c| (f32::from(w[c]) * 0.5 + 0.5) as u8);
        assert_eq!(pixel(&shot, x, y), dimmed, "pixel {x},{y}");
    }
    // Crosshair guides pass through the pointer at (500,500): the pixel is lighter than dimmed.
    let p = pixel(&shot, 500, 300);
    let w = pixel(&want, 500, 300);
    assert!(p[0] > w[0] / 2 + 20, "guide line visible at {p:?} vs {w:?}");
    // The overlay is gone afterwards (window destroyed, grabs released).
}

#[test]
fn timeout_reports_cancelled_and_the_window_goes_away() {
    let Some(x) = Xvfb::start(W, H) else { return };
    let dir = tempfile::tempdir().unwrap();
    let helper = x.helper_wrapper(dir.path());
    let inp = input(coded_desktop(W, H, Point::new(0, 0)), vec![], vec![], OverlayOptions { timeout_ms: Some(700), ..OverlayOptions::default() });
    let out = select_via_helper(&inp, &helper).unwrap();
    assert_eq!(out, OverlayOutcome::Cancelled);
    assert!(!x.xdotool(&["search", "--name", "ssx-overlay"]), "overlay window must be destroyed");
}

#[test]
fn window_origin_offset_maps_pointer_to_desktop_pixels() {
    // Frame whose origin is not (0,0): desktop pixel = root pixel here because the overlay
    // window is placed at the frame origin.
    let Some(x) = Xvfb::start(W, H) else { return };
    let dir = tempfile::tempdir().unwrap();
    let helper = x.helper_wrapper(dir.path());
    let inp = input(coded_desktop(500, 400, Point::new(120, 90)), vec![], vec![], OverlayOptions { timeout_ms: Some(20_000), ..OverlayOptions::default() });
    let out = std::thread::scope(|s| {
        let h = s.spawn(|| select_via_helper(&inp, &helper));
        assert!(x.wait_for_window("ssx-overlay"));
        drag(&x, (200, 150), (300, 250));
        key(&x, "Return");
        h.join().unwrap()
    })
    .unwrap();
    assert_eq!(selected_rect(&out), Rect::new(200, 150, 100, 100));
}

// ---------------------------------------------------------------- helper-process robustness

fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-helper.sh");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn tiny_input(timeout_ms: Option<u64>) -> ssx_overlay::OverlayInput {
    input(coded_desktop(16, 16, Point::new(0, 0)), vec![], vec![], OverlayOptions { timeout_ms, ..OverlayOptions::default() })
}

#[test]
fn a_crashing_helper_is_reported_not_propagated() {
    let dir = tempfile::tempdir().unwrap();
    let h = script(dir.path(), "echo 'boom: compositor gone' >&2; exit 139");
    match select_via_helper(&tiny_input(Some(5_000)), &h) {
        Err(OverlayError::Helper(HelperError::Crashed { status, stderr })) => {
            assert!(status.contains("139"), "{status}");
            assert!(stderr.contains("compositor gone"), "{stderr}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_wedged_helper_is_killed_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let h = script(dir.path(), &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()));
    let t = std::time::Instant::now();
    match select_via_helper(&tiny_input(Some(600)), &h) {
        Err(OverlayError::Helper(HelperError::Timeout(d))) => assert_eq!(d, Duration::from_millis(600) + ssx_overlay::helper::HELPER_KILL_GRACE),
        other => panic!("{other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(8));
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "helper {pid} still running");
}

#[test]
fn garbage_and_error_answers_are_distinguished() {
    let dir = tempfile::tempdir().unwrap();
    let h = script(dir.path(), "cat >/dev/null; echo '{\"nonsense\": 1}'");
    assert!(matches!(
        select_via_helper(&tiny_input(Some(5_000)), &h),
        Err(OverlayError::Helper(HelperError::BadAnswer(_)))
    ));
    let h = script(dir.path(), "cat >/dev/null; echo '{\"Error\":{\"message\":\"no display\"}}'; exit 2");
    match select_via_helper(&tiny_input(Some(5_000)), &h) {
        Err(OverlayError::Helper(HelperError::Reported(m))) => assert_eq!(m, "no display"),
        other => panic!("{other:?}"),
    }
    match select_via_helper(&tiny_input(None), std::path::Path::new("/definitely/missing")) {
        Err(OverlayError::Helper(HelperError::Spawn { .. })) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn helper_with_no_display_reports_an_actionable_error() {
    if !have("sh") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let h = common::wrapper(dir.path(), &[], &["DISPLAY", "WAYLAND_DISPLAY"], &common::helper_bin());
    match select_via_helper(&tiny_input(Some(5_000)), &h) {
        Err(OverlayError::Helper(HelperError::Reported(m))) => {
            assert!(m.contains("DISPLAY") && m.contains("WAYLAND_DISPLAY"), "{m}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn hdr_frames_are_refused_before_spawning_anything() {
    let f = ssx_types::Frame::new(ssx_types::Size::new(4, 4), ssx_types::PixelFormat::Rgba16F, ssx_types::ColorSpace::ScRgbLinear);
    let inp = input(f, vec![], vec![], OverlayOptions::default());
    match select_via_helper(&inp, std::path::Path::new("/definitely/missing")) {
        Err(OverlayError::InvalidInput(m)) => assert!(m.contains("tone-map"), "{m}"),
        other => panic!("{other:?}"),
    }
}
