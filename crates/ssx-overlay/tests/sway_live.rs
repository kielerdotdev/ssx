//! Live tests of the Wayland backends against a private headless sway (pixman renderer).
//!
//! Real protocol traffic end to end: the helper process binds `wlr-layer-shell` (or
//! `xdg_shell` fullscreen), the test injects pointer and keyboard input through
//! `zwlr_virtual_pointer_v1` / `zwp_virtual_keyboard_v1`, and `grim` (an independent
//! implementation of `wlr-screencopy`) verifies what is actually on each output.
#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

use common::{
    coded_desktop, input, monitor, pixel, window,
    sway::{OutputCfg, Sway, evdev},
};
use ssx_overlay::{
    BackendPreference, OverlayInput, OverlayOptions, OverlayOutcome, SelectMode, select_via_helper,
};
use ssx_types::{Point, Rect};

fn dim(p: [u8; 3]) -> [u8; 3] {
    p.map(|c| (f32::from(c) * 0.5 + 0.5) as u8)
}

/// Runs the helper against `sway`, waits until `ready` reports the overlay on screen, runs
/// `script`, and returns the outcome.
fn run(
    sway: &Sway,
    inp: &OverlayInput,
    ready: impl Fn() -> bool + Sync,
    script: impl FnOnce(&Sway) + Send,
) -> OverlayOutcome {
    let dir = tempfile::tempdir().unwrap();
    let helper = sway.helper_wrapper(dir.path());
    std::thread::scope(|s| {
        let h = s.spawn(|| select_via_helper(inp, &helper));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "the overlay never appeared on screen");
            assert!(!h.is_finished(), "helper exited early: {:?}", h.is_finished());
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        script(sway);
        h.join().unwrap().expect("overlay failed")
    })
}

fn opts(backend: BackendPreference) -> OverlayOptions {
    OverlayOptions { backend, timeout_ms: Some(30_000), ..OverlayOptions::default() }
}

/// Ready = grim shows the dimmed desktop at a probe pixel of `output`.
fn dimmed_at(sway: &Sway, output: &str, at: (u32, u32), want: [u8; 3]) -> bool {
    match sway.grim(output) {
        Some(f) => pixel(&f, at.0, at.1) == want,
        None => {
            std::thread::sleep(Duration::from_secs(2));
            true
        }
    }
}

const LEFT: u32 = 800;

#[test]
fn layer_shell_two_outputs_cover_drag_across_and_return_exact_pixels() {
    let Some(sway) = Sway::start(&[
        OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
        OutputCfg::new("HEADLESS-2", 640, 480, 900, 50),
    ]) else {
        return;
    };
    // Desktop in physical px == logical here (both outputs at scale 1), with a gap between.
    let desk = coded_desktop(1540, 600, Point::new(0, 0));
    let monitors = vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0), monitor("HEADLESS-2", Rect::new(900, 50, 640, 480), 1.0)];
    let inp = input(desk.clone(), monitors, vec![], opts(BackendPreference::WaylandLayerShell));
    sway.input().set_layout(1540, 600);
    let ready = || dimmed_at(&sway, "HEADLESS-1", (790, 590), dim(pixel(&desk, 790, 590)));
    let mut covered = None;
    let out = run(&sway, &inp, ready, |sw| {
        // The overlay covers each output with its slice of the frozen desktop, dimmed.
        covered = Some((sw.grim("HEADLESS-1"), sw.grim("HEADLESS-2")));
        sw.input().move_to(700.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(750.0, 150.0);
        // Continue the drag past the edge of output 1 onto output 2 (implicit grab).
        sw.input().move_to(1000.0, 300.0);
        sw.input().left_up();
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(700, 100, 300, 200)),
        other => panic!("{other:?}"),
    }
    if let Some((Some(a), Some(b))) = covered {
        assert_eq!((a.width(), a.height()), (800, 600));
        assert_eq!((b.width(), b.height()), (640, 480));
        for (x, y) in [(10, 10), (400, 400), (790, 590), (300, 200)] {
            assert_eq!(pixel(&a, x, y), dim(pixel(&desk, x, y)), "output 1 at {x},{y}");
        }
        for (x, y) in [(5, 5), (300, 300), (630, 470)] {
            assert_eq!(pixel(&b, x, y), dim(pixel(&desk, 900 + x, 50 + y)), "output 2 at {x},{y}");
        }
    } else {
        eprintln!("grim unavailable: coverage not verified");
    }
}

#[test]
fn escape_right_click_and_keyboard_nudge_on_layer_shell() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)]) else { return };
    let desk = coded_desktop(800, 600, Point::new(0, 0));
    let mk = || input(desk.clone(), vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0)], vec![], opts(BackendPreference::Auto));
    sway.input().set_layout(800, 600);
    let ready = || dimmed_at(&sway, "HEADLESS-1", (790, 590), dim(pixel(&desk, 790, 590)));

    // Esc cancels.
    let out = run(&sway, &mk(), ready, |sw| {
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(200.0, 200.0);
        sw.input().left_up();
        sw.input().tap(evdev::ESC);
    });
    assert_eq!(out, OverlayOutcome::Cancelled);

    // Right-click clears, second right-click cancels.
    let out = run(&sway, &mk(), ready, |sw| {
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(200.0, 200.0);
        sw.input().left_up();
        sw.input().right_click();
        sw.input().tap(evdev::ENTER); // nothing selected: ignored
        sw.input().right_click();
    });
    assert_eq!(out, OverlayOutcome::Cancelled);

    // Arrow nudge (1 px, Shift = 10 px) then Enter.
    let out = run(&sway, &mk(), ready, |sw| {
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(200.0, 200.0);
        sw.input().left_up();
        sw.input().tap(evdev::RIGHT);
        sw.input().tap(evdev::DOWN);
        sw.input().tap(evdev::DOWN);
        sw.input().key(evdev::LSHIFT, true);
        sw.input().tap(evdev::LEFT);
        sw.input().key(evdev::LSHIFT, false);
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(91, 102, 100, 100)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn shift_square_wheel_colour_pick_and_window_snap() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)]) else { return };
    let desk = coded_desktop(800, 600, Point::new(0, 0));
    sway.input().set_layout(800, 600);
    let ready = || dimmed_at(&sway, "HEADLESS-1", (790, 590), dim(pixel(&desk, 790, 590)));
    let base = |o: OverlayOptions, w: Vec<ssx_types::WindowInfo>| {
        input(desk.clone(), vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0)], w, o)
    };

    let out = run(&sway, &base(opts(BackendPreference::Auto), vec![]), ready, |sw| {
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().key(evdev::LSHIFT, true);
        sw.input().move_to(260.0, 160.0);
        sw.input().left_up();
        sw.input().key(evdev::LSHIFT, false);
        sw.input().wheel(true); // loupe zoom: must not disturb the selection
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(100, 100, 160, 160)),
        other => panic!("{other:?}"),
    }

    let out = run(&sway, &base(opts(BackendPreference::Auto), vec![]), ready, |sw| {
        sw.input().move_to(123.0, 77.0);
        sw.input().tap(evdev::C);
    });
    match out {
        OverlayOutcome::ColorPicked(c) => {
            assert_eq!(c.point, Point::new(123, 77));
            assert_eq!(c.rgb, pixel(&desk, 123, 77));
        }
        other => panic!("{other:?}"),
    }

    let wins = vec![window("front", Rect::new(50, 60, 300, 200)), window("back", Rect::new(0, 0, 700, 500))];
    let out = run(&sway, &base(opts(BackendPreference::Auto), wins), ready, |sw| {
        sw.input().move_to(500.0, 400.0);
        sw.input().move_to(120.0, 120.0);
        sw.input().left_down();
        sw.input().left_up();
    });
    match out {
        OverlayOutcome::Selected(s) => {
            assert_eq!(s.rect, Rect::new(50, 60, 300, 200));
            assert_eq!(s.snapped_window.unwrap().title, "front");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn mixed_scale_layout_maps_pointer_to_desktop_pixels() {
    // The coordinate model of ssx-capture-wayland: 800x600 @1x at (0,0) and 640x480 @2x
    // (logical 320x240) at (800,0) give S = 2, so the desktop is 2240x1200 and the monitors
    // are (0,0,1600,1200) (resampled 2x) and (1600,0,640,480) (native).
    let Some(sway) = Sway::start(&[
        OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
        OutputCfg::new("HEADLESS-2", 640, 480, 800, 0).scale("2"),
    ]) else {
        return;
    };
    let mut desk = coded_desktop(2240, 1200, Point::new(0, 0));
    desk.scale_factor = 2.0;
    let monitors = vec![monitor("HEADLESS-1", Rect::new(0, 0, 1600, 1200), 1.0), monitor("HEADLESS-2", Rect::new(1600, 0, 640, 480), 2.0)];
    sway.input().set_layout(1120, 600);
    let inp = input(desk.clone(), monitors.clone(), vec![], opts(BackendPreference::WaylandLayerShell));
    // Output 2 is native resolution, so it can be verified pixel-exactly.
    let ready = || dimmed_at(&sway, "HEADLESS-2", (100, 100), dim(pixel(&desk, 1700, 100)));
    let mut b_shot = None;
    let out = run(&sway, &inp, ready, |sw| {
        b_shot = sw.grim("HEADLESS-2");
        // Drag on output 1 (logical 100,100 -> 200,150) = desktop (200,200) -> (400,300).
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(150.0, 120.0);
        sw.input().move_to(200.0, 150.0);
        sw.input().left_up();
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(200, 200, 200, 100)),
        other => panic!("{other:?}"),
    }
    if let Some(b) = b_shot {
        assert_eq!((b.width(), b.height()), (640, 480));
        for (x, y) in [(3, 3), (320, 240), (600, 400)] {
            assert_eq!(pixel(&b, x, y), dim(pixel(&desk, 1600 + x, y)), "output 2 at {x},{y}");
        }
    }
    // Pointer on output 2 (logical 810,10 => local 10,10 => desktop 1600+20, 20).
    let inp = input(desk.clone(), monitors, vec![], opts(BackendPreference::WaylandLayerShell));
    let out = run(&sway, &inp, ready, |sw| {
        sw.input().move_to(810.0, 10.0);
        sw.input().tap(evdev::C);
    });
    match out {
        OverlayOutcome::ColorPicked(c) => {
            assert_eq!(c.point, Point::new(1620, 20));
            assert_eq!(c.rgb, pixel(&desk, 1620, 20));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn fullscreen_toplevel_flavour_used_on_gnome_like_compositors() {
    // sway also implements xdg_shell fullscreen, so the Mutter path can be exercised here.
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)]) else { return };
    let desk = coded_desktop(800, 600, Point::new(0, 0));
    sway.input().set_layout(800, 600);
    let inp = input(desk.clone(), vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0)], vec![], opts(BackendPreference::WaylandFullscreen));
    let ready = || dimmed_at(&sway, "HEADLESS-1", (790, 590), dim(pixel(&desk, 790, 590)));
    let out = run(&sway, &inp, ready, |sw| {
        sw.input().move_to(100.0, 100.0);
        sw.input().left_down();
        sw.input().move_to(250.0, 220.0);
        sw.input().left_up();
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Selected(s) => assert_eq!(s.rect, Rect::new(100, 100, 150, 120)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn monitor_pick_mode_with_tab_on_two_outputs() {
    let Some(sway) = Sway::start(&[
        OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
        OutputCfg::new("HEADLESS-2", 640, 480, 800, 0),
    ]) else {
        return;
    };
    let desk = coded_desktop(1440, 600, Point::new(0, 0));
    sway.input().set_layout(1440, 600);
    let monitors = vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0), monitor("HEADLESS-2", Rect::new(800, 0, 640, 480), 1.0)];
    let o = OverlayOptions { mode: SelectMode::Monitor, ..opts(BackendPreference::Auto) };
    let inp = input(desk.clone(), monitors, vec![], o);
    let ready = || dimmed_at(&sway, "HEADLESS-2", (300, 300), dim(pixel(&desk, 1100, 300)));
    let out = run(&sway, &inp, ready, |sw| {
        sw.input().move_to(100.0, 100.0); // output 1
        sw.input().tap(evdev::TAB); // -> output 2
        sw.input().tap(evdev::ENTER);
    });
    match out {
        OverlayOutcome::Monitor(m) => assert_eq!(m.name, "HEADLESS-2"),
        other => panic!("{other:?}"),
    }
    let _ = LEFT;
}

#[test]
fn timeout_closes_the_overlay_and_reports_cancelled() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)]) else { return };
    let desk = coded_desktop(800, 600, Point::new(0, 0));
    let o = OverlayOptions { timeout_ms: Some(1500), ..opts(BackendPreference::Auto) };
    let inp = input(desk.clone(), vec![monitor("HEADLESS-1", Rect::new(0, 0, 800, 600), 1.0)], vec![], o);
    let dir = tempfile::tempdir().unwrap();
    let helper = sway.helper_wrapper(dir.path());
    let out = select_via_helper(&inp, &helper).unwrap();
    assert_eq!(out, OverlayOutcome::Cancelled);
    if let Some(f) = sway.grim("HEADLESS-1") {
        assert_ne!(pixel(&f, 300, 200), dim(pixel(&desk, 300, 200)), "overlay must be gone");
    }
}
