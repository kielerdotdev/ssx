//! End-to-end start-up timing of the helper process on a 4K desktop.
//!
//! Prints (run with `--nocapture`): time to write the frame into the memfd, and, measured
//! inside the helper from its `main`, the time until the frame is converted/dimmed
//! ("ready") and until the first frame has been presented ("first frame"), on Xvfb (X11
//! `PutImage`) and on headless sway (layer-shell `wl_shm`). The assertions are deliberately
//! loose (CI machines vary); the numbers are what matters.
#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

use common::{
    Xvfb, input, monitor,
    sway::{OutputCfg, Sway, evdev},
};
use ssx_overlay::{
    BackendPreference, OverlayOptions, OverlayOutcome, demo::demo_frame, select_via_helper_timed,
    shm::SharedFrame,
};
use ssx_types::{Point, Rect};

const W: u32 = 3840;
const H: u32 = 2160;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[test]
fn time_to_first_frame_4k_x11() {
    let Some(x) = Xvfb::start(W, H) else { return };
    let dir = tempfile::tempdir().unwrap();
    let helper = x.helper_wrapper(dir.path());
    let frame = demo_frame(W, H, Point::new(0, 0));
    let t = Instant::now();
    drop(SharedFrame::create(&frame).unwrap());
    let memfd_ms = t.elapsed().as_secs_f64() * 1e3;
    let opts = OverlayOptions {
        timeout_ms: Some(30_000),
        backend: BackendPreference::X11,
        ..OverlayOptions::default()
    };
    let (mut ready, mut first, mut wall) = (vec![], vec![], vec![]);
    for _ in 0..5 {
        let inp = input(frame.clone(), vec![], vec![], opts.clone());
        let t = Instant::now();
        let (out, timing) = std::thread::scope(|s| {
            let h = s.spawn(|| select_via_helper_timed(&inp, &helper));
            assert!(x.wait_for_window("ssx-overlay"));
            assert!(x.xdotool(&["key", "Escape"]));
            h.join().unwrap().unwrap()
        });
        wall.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(out, OverlayOutcome::Cancelled);
        ready.push(timing.ready_ms);
        first.push(timing.first_frame_ms.expect("first frame"));
    }
    eprintln!(
        "X11 4K ({}x{}): memfd write {memfd_ms:.1} ms | helper ready {:.1} ms | first frame on screen {:.1} ms after helper start (median of 5; whole call incl. driving/closing {:.0} ms)",
        W,
        H,
        median(ready),
        median(first.clone()),
        median(wall)
    );
    assert!(median(first) < 3000.0);
}

#[test]
fn time_to_first_frame_4k_wayland_layer_shell() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", W, H, 0, 0)]) else { return };
    let dir = tempfile::tempdir().unwrap();
    let helper = sway.helper_wrapper(dir.path());
    let frame = demo_frame(W, H, Point::new(0, 0));
    let mons = vec![monitor("HEADLESS-1", Rect::new(0, 0, W, H), 1.0)];
    let opts = OverlayOptions {
        timeout_ms: Some(30_000),
        backend: BackendPreference::WaylandLayerShell,
        ..OverlayOptions::default()
    };
    sway.input().set_layout(W, H);
    let (mut ready, mut first) = (vec![], vec![]);
    for _ in 0..5 {
        let inp = input(frame.clone(), mons.clone(), vec![], opts.clone());
        let (out, timing) = std::thread::scope(|s| {
            let h = s.spawn(|| select_via_helper_timed(&inp, &helper));
            // Wait until the surface is up, then close it with Escape.
            let deadline = Instant::now() + Duration::from_secs(20);
            std::thread::sleep(Duration::from_millis(800));
            while !h.is_finished() && Instant::now() < deadline {
                sway.input().tap(evdev::ESC);
                std::thread::sleep(Duration::from_millis(200));
            }
            h.join().unwrap().unwrap()
        });
        assert_eq!(out, OverlayOutcome::Cancelled);
        ready.push(timing.ready_ms);
        first.push(timing.first_frame_ms.expect("first frame"));
    }
    eprintln!(
        "Wayland layer-shell 4K ({}x{}): helper ready {:.1} ms | first buffer committed {:.1} ms after helper start (median of 5)",
        W,
        H,
        median(ready),
        median(first.clone())
    );
    assert!(median(first) < 3000.0);
}
