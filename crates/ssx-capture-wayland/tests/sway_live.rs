//! Live tests against a real headless sway (wlroots, `wlr-screencopy`).
//!
//! Each test starts its own private sway; when `sway` is missing they print a SKIP line
//! and pass (CI installs `sway grim`). `grim` is used only as an independent cross-check.
#![cfg(target_os = "linux")]
#![allow(clippy::float_cmp)] // scale factors are exact by construction (1.0, 1.5, 2.0)

mod common;

use std::{sync::Arc, time::Duration};

use common::{
    Job, OutputCfg, Painter, Sway, first_frame_mismatch, first_pattern_mismatch, pattern,
};
use ssx_capture::{CaptureBackend, CaptureError, CaptureOptions};
use ssx_capture_wayland::{Protocol, Transform, WaylandCapture};
use ssx_types::{PixelFormat, Rect, Size};

const OPTS: CaptureOptions = CaptureOptions { include_cursor: false };

fn wait_for<T>(what: &str, f: impl FnMut() -> Option<T>) -> T {
    wait_for_with(what, f, String::new)
}

fn wait_for_with<T>(what: &str, mut f: impl FnMut() -> Option<T>, why: impl Fn() -> String) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}: {}", why());
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn pixel(f: &ssx_types::Frame, x: u32, y: u32) -> [u8; 4] {
    let p = &f.row(y)[x as usize * 4..x as usize * 4 + 4];
    [p[0], p[1], p[2], p[3]]
}

/// BGRA bytes of the painter pattern.
fn bgra(seed: u8, x: u32, y: u32) -> [u8; 4] {
    let [r, g, b] = pattern(seed, x, y);
    [b, g, r, 255]
}

#[test]
fn two_outputs_geometry_pixels_and_regions() {
    // Outputs at different positions with a gap between them and a vertical offset.
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 900, 50),
        ],
        "",
    ) else {
        return;
    };
    let _p = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Background { output: "HEADLESS-1", scale: 1, seed: 1 },
            Job::Background { output: "HEADLESS-2", scale: 1, seed: 2 },
        ],
    );
    let cap = WaylandCapture::with_config(sway.config()).expect("detect");
    // sway 1.9 (wlroots 0.17) only has wlr-screencopy; sway >= 1.10 also has the ext
    // protocol, which is then preferred and exercised by every test below.
    match cap.protocol() {
        Protocol::WlrScreencopy => assert_eq!(cap.name(), "wayland-wlr-screencopy"),
        Protocol::ExtImageCopyCapture => assert_eq!(cap.name(), "wayland-ext-image-copy-capture"),
    }
    eprintln!("live sway protocol: {}", cap.name());
    let caps = cap.capabilities();
    assert!(
        caps.enumerate_monitors && caps.cursor && caps.enumerate_windows && caps.capture_windows
    );
    assert!(!caps.native_desktop && !caps.needs_user_interaction && !caps.hdr_float);

    // --- monitors()
    let monitors = cap.monitors().unwrap();
    assert_eq!(monitors.len(), 2);
    let m1 = monitors.iter().find(|m| m.id == "HEADLESS-1").unwrap();
    let m2 = monitors.iter().find(|m| m.id == "HEADLESS-2").unwrap();
    assert_eq!(m1.name, "HEADLESS-1");
    assert_eq!(m1.rect, Rect::new(0, 0, 800, 600));
    assert_eq!(m2.rect, Rect::new(900, 50, 640, 480));
    assert_eq!((m1.scale_factor, m2.scale_factor), (1.0, 1.0));
    assert!(m1.primary && !m2.primary, "the monitor at the layout origin is primary");
    assert!(m1.hdr.is_none());

    let outs = cap.outputs().unwrap();
    assert_eq!(outs.len(), 2);
    let o2 = outs.iter().find(|o| o.id == "HEADLESS-2").unwrap();
    assert_eq!(o2.native_size, Size::new(640, 480));
    assert_eq!(o2.integer_scale, 1);
    assert_eq!(o2.transform, Transform::Normal);

    // --- capture_monitor, pixel exact
    let f1 = cap.capture_monitor("HEADLESS-1", &OPTS).unwrap();
    assert_eq!(f1.format(), PixelFormat::Bgra8);
    assert_eq!((f1.size(), f1.origin.x, f1.origin.y), (Size::new(800, 600), 0, 0));
    assert_eq!(first_pattern_mismatch(&f1, 1, (0, 0)), None);
    let f2 = cap.capture_monitor("HEADLESS-2", &OPTS).unwrap();
    assert_eq!((f2.size(), f2.origin.x, f2.origin.y), (Size::new(640, 480), 900, 50));
    assert_eq!(first_pattern_mismatch(&f2, 2, (0, 0)), None);
    assert_eq!(f2.scale_factor, 1.0);

    // --- cursor option is accepted (no pointer exists on a headless seat)
    let with_cursor =
        cap.capture_monitor("HEADLESS-1", &CaptureOptions { include_cursor: true }).unwrap();
    assert_eq!(with_cursor.size(), Size::new(800, 600));

    // --- region inside one output (direct protocol region path at scale 1)
    let r = cap.capture_region(Rect::new(13, 21, 101, 77), &OPTS).unwrap();
    assert_eq!(r.rect(), Rect::new(13, 21, 101, 77));
    assert_eq!(first_pattern_mismatch(&r, 1, (13, 21)), None);
    let r = cap.capture_region(Rect::new(905, 57, 33, 19), &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&r, 2, (5, 7)), None, "region on the offset output");

    // --- region spanning both outputs and the gap between them, partly above output 2
    let region = Rect::new(700, 20, 300, 100);
    let span = cap.capture_region(region, &OPTS).unwrap();
    assert_eq!(span.rect(), region);
    assert_eq!(pixel(&span, 0, 0), bgra(1, 700, 20), "left output");
    assert_eq!(pixel(&span, 99, 99), bgra(1, 799, 119));
    assert_eq!(pixel(&span, 100, 50), [0, 0, 0, 0], "the gap is transparent");
    assert_eq!(pixel(&span, 199, 50), [0, 0, 0, 0]);
    assert_eq!(pixel(&span, 250, 10), [0, 0, 0, 0], "above the second output (y < 50)");
    assert_eq!(pixel(&span, 200, 30), bgra(2, 0, 0), "second output top-left, desktop (900,50)");
    assert_eq!(pixel(&span, 299, 99), bgra(2, 99, 69));

    // --- whole desktop
    let d = cap.capture_desktop(&OPTS).unwrap();
    assert_eq!(d.rect(), Rect::new(0, 0, 1540, 600));
    assert_eq!(pixel(&d, 10, 10), bgra(1, 10, 10));
    assert_eq!(pixel(&d, 1000, 100), bgra(2, 100, 50));
    assert_eq!(pixel(&d, 850, 10), [0, 0, 0, 0]);

    // --- error paths
    assert!(
        matches!(cap.capture_monitor("DP-9", &OPTS), Err(CaptureError::NotFound(id)) if id == "DP-9")
    );
    assert!(matches!(
        cap.capture_region(Rect::new(0, 0, 0, 10), &OPTS),
        Err(CaptureError::InvalidRegion(_))
    ));
    assert!(matches!(
        cap.capture_region(Rect::new(-500, -500, 10, 10), &OPTS),
        Err(CaptureError::InvalidRegion(_))
    ));
    assert!(
        matches!(
            cap.capture_region(Rect::new(820, 0, 60, 40), &OPTS),
            Err(CaptureError::InvalidRegion(_)),
        ),
        "a region entirely in the gap overlaps no monitor"
    );
    assert!(matches!(cap.capture_window("sway:99999", &OPTS), Err(CaptureError::NotFound(_))));
}

#[test]
fn matches_grim_pixel_for_pixel() {
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 800, 0),
        ],
        "",
    ) else {
        return;
    };
    if !common::have("grim") {
        eprintln!("SKIP: grim not installed");
        return;
    }
    let _p = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Background { output: "HEADLESS-1", scale: 1, seed: 3 },
            Job::Background { output: "HEADLESS-2", scale: 1, seed: 4 },
        ],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();

    for name in ["HEADLESS-1", "HEADLESS-2"] {
        let ours = cap.capture_monitor(name, &OPTS).unwrap();
        let theirs = sway.grim(&["-o", name]).unwrap();
        assert_eq!(first_frame_mismatch(&ours, &theirs), None, "{name} vs grim -o");
    }

    // A region spanning the seam between the two outputs, and one inside a single output.
    for (x, y, w, h) in [(700, 100, 200, 120), (50, 60, 300, 200), (810, 5, 77, 41)] {
        let ours = cap.capture_region(Rect::new(x, y, w, h), &OPTS).unwrap();
        let geo = format!("{x},{y} {w}x{h}");
        let theirs = sway.grim(&["-g", &geo]).unwrap();
        assert_eq!(first_frame_mismatch(&ours, &theirs), None, "region {geo} vs grim -g");
    }
}

#[test]
fn mixed_scales_use_max_scale_desktop() {
    // HEADLESS-1: 800x600 at 1x. HEADLESS-2: 640x480 at 2x = 320x240 logical, right of it.
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 800, 0).scale("2"),
        ],
        "",
    ) else {
        return;
    };
    let _p = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Background { output: "HEADLESS-1", scale: 1, seed: 5 },
            Job::Background { output: "HEADLESS-2", scale: 2, seed: 6 },
        ],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let ms = cap.monitors().unwrap();
    let m1 = ms.iter().find(|m| m.id == "HEADLESS-1").unwrap();
    let m2 = ms.iter().find(|m| m.id == "HEADLESS-2").unwrap();
    // Desktop scale S = 2: the 2x monitor is pixel exact, the 1x monitor covers 2x2 desktop
    // pixels per native pixel.
    assert_eq!(m1.rect, Rect::new(0, 0, 1600, 1200));
    assert_eq!(m2.rect, Rect::new(1600, 0, 640, 480));
    assert_eq!((m1.scale_factor, m2.scale_factor), (1.0, 2.0));
    let out2 = cap.outputs().unwrap().into_iter().find(|o| o.id == "HEADLESS-2").unwrap();
    assert_eq!(out2.logical, Rect::new(800, 0, 320, 240));
    assert_eq!(out2.integer_scale, 2);

    // The high-DPI monitor is pixel exact.
    let f2 = cap.capture_monitor("HEADLESS-2", &OPTS).unwrap();
    assert_eq!((f2.size(), f2.scale_factor), (Size::new(640, 480), 2.0));
    assert_eq!(first_pattern_mismatch(&f2, 6, (0, 0)), None);
    if let Some(grim) = sway.grim(&["-o", "HEADLESS-2"]) {
        assert_eq!(first_frame_mismatch(&f2, &grim), None, "vs grim -o");
    }

    // The 1x monitor is up-sampled by pixel replication to desktop resolution.
    let f1 = cap.capture_monitor("HEADLESS-1", &OPTS).unwrap();
    assert_eq!(f1.size(), Size::new(1600, 1200));
    assert_eq!(f1.scale_factor, 2.0, "pixels per logical pixel of the frame");
    for (x, y) in [(0, 0), (1, 1), (2, 0), (1599, 1199), (801, 333), (1000, 1001)] {
        assert_eq!(pixel(&f1, x, y), bgra(5, x / 2, y / 2), "desktop px {x},{y}");
    }

    // Unaligned region inside the 2x monitor: exercises the protocol-level region path
    // (logical coordinates, enclosing box, crop).
    let r = cap.capture_region(Rect::new(1600 + 5, 7, 51, 33), &OPTS).unwrap();
    assert_eq!(r.rect(), Rect::new(1605, 7, 51, 33));
    assert_eq!(first_pattern_mismatch(&r, 6, (5, 7)), None);
    let r = cap.capture_region(Rect::new(1600 + 100, 200, 64, 64), &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&r, 6, (100, 200)), None, "aligned region");

    // Region straddling the seam between the resampled and the exact monitor.
    let r = cap.capture_region(Rect::new(1590, 10, 20, 4), &OPTS).unwrap();
    assert_eq!(pixel(&r, 0, 0), bgra(5, 795, 5));
    assert_eq!(pixel(&r, 9, 3), bgra(5, 799, 6));
    assert_eq!(pixel(&r, 10, 0), bgra(6, 0, 10));
    assert_eq!(pixel(&r, 19, 3), bgra(6, 9, 13));

    let d = cap.capture_desktop(&OPTS).unwrap();
    assert_eq!(d.rect(), Rect::new(0, 0, 2240, 1200));
    assert_eq!(pixel(&d, 2000, 300), bgra(6, 400, 300));
    assert_eq!(pixel(&d, 3, 3), bgra(5, 1, 1));
    assert_eq!(pixel(&d, 2000, 900), [0, 0, 0, 0], "below the shorter monitor");

    // With the desktop scale pinned to 1 the layout is the raw logical one.
    let mut cfg = sway.config();
    cfg.desktop_scale = Some(1.0);
    let cap1 = WaylandCapture::with_config(cfg).unwrap();
    let ms = cap1.monitors().unwrap();
    assert_eq!(ms.iter().find(|m| m.id == "HEADLESS-1").unwrap().rect, Rect::new(0, 0, 800, 600));
    assert_eq!(ms.iter().find(|m| m.id == "HEADLESS-2").unwrap().rect, Rect::new(800, 0, 320, 240));
    let down = cap1.capture_monitor("HEADLESS-2", &OPTS).unwrap();
    assert_eq!(down.size(), Size::new(320, 240), "2x monitor is down-sampled");
}

#[test]
fn every_transform_yields_the_upright_image() {
    // A 640x480 output; the painter draws the *upright* desktop, sway renders it rotated
    // into the output's scan-out buffer, and we must undo exactly that.
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 640, 480, 0, 0)], "") else {
        return;
    };
    let _p = Painter::spawn(
        &sway.wayland_socket,
        vec![Job::Background { output: "HEADLESS-1", scale: 1, seed: 7 }],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let cases = [
        ("normal", Transform::Normal, (640, 480)),
        // sway names rotations clockwise, wl_output counter-clockwise: sway's "90" is the
        // wl_output value 270 (and the flipped variants likewise).
        ("90", Transform::Rot270, (480, 640)),
        ("180", Transform::Rot180, (640, 480)),
        ("270", Transform::Rot90, (480, 640)),
        ("flipped", Transform::Flipped, (640, 480)),
        ("flipped-90", Transform::Flipped270, (480, 640)),
        ("flipped-180", Transform::Flipped180, (640, 480)),
        ("flipped-270", Transform::Flipped90, (480, 640)),
    ];
    for (name, t, (w, h)) in cases {
        sway.command(&format!("output HEADLESS-1 transform {name}"));
        // Wait for sway to apply it and for the painter to repaint at the new logical size.
        let last = std::cell::RefCell::new(String::new());
        let frame = wait_for_with(
            &format!("transform {name}"),
            || {
                let o = cap.outputs().ok()?.into_iter().next()?;
                if o.transform != t || o.native_size != Size::new(w, h) {
                    *last.borrow_mut() = format!("output is {:?} {:?}", o.transform, o.native_size);
                    return None;
                }
                let f = cap.capture_monitor("HEADLESS-1", &OPTS).ok()?;
                if name.starts_with("flipped") {
                    // sway 1.9's pixman renderer draws every flipped output as uniform grey,
                    // so only geometry (and agreement with grim) can be checked live; the
                    // flip maths themselves are unit tested in `transform.rs`.
                    return Some(f);
                }
                match first_pattern_mismatch(&f, 7, (0, 0)) {
                    None => Some(f),
                    Some(m) => {
                        *last.borrow_mut() = m;
                        None
                    }
                }
            },
            || last.borrow().clone(),
        );
        assert_eq!(frame.size(), Size::new(w, h), "{name}");
        assert_eq!(frame.rect(), Rect::new(0, 0, w, h), "{name}: monitor rect matches frame");
        let m = &cap.monitors().unwrap()[0];
        assert_eq!(m.rect, Rect::new(0, 0, w, h), "{name}");
        if let Some(grim) = sway.grim(&["-o", "HEADLESS-1"]) {
            assert_eq!(first_frame_mismatch(&frame, &grim), None, "{name} vs grim -o");
        }
        // A region on a transformed output goes through capture + crop, not the protocol
        // region request.
        let r = cap.capture_region(Rect::new(11, 13, 40, 30), &OPTS).unwrap();
        if !name.starts_with("flipped") {
            assert_eq!(first_pattern_mismatch(&r, 7, (11, 13)), None, "{name} region");
        }
        assert_eq!(r.size(), Size::new(40, 30));
    }
}

#[test]
fn fractional_scale_layout() {
    // 1.5x on a 960x720 mode: logical 640x480.
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 960, 720, 0, 0).scale("1.5")], "")
    else {
        return;
    };
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let o = &cap.outputs().unwrap()[0];
    assert_eq!(o.scale_factor, 1.5);
    assert_eq!(o.logical.size(), Size::new(640, 480));
    assert_eq!(o.integer_scale, 2, "wl_output.scale is the ceiling");
    let m = &cap.monitors().unwrap()[0];
    assert_eq!(m.rect, Rect::new(0, 0, 960, 720));
    assert_eq!(m.scale_factor, 1.5);
    // Region on a fractional-scale output must be pixel-exact via full capture + crop.
    let f = cap.capture_region(Rect::new(101, 203, 50, 50), &OPTS).unwrap();
    assert_eq!(f.size(), Size::new(50, 50));
    if let Some(grim) = sway.grim(&["-g", "101,203 50x50"]) {
        // grim takes logical coordinates; only check dimensions are sane instead.
        assert!(grim.width() > 0);
    }
    let full = cap.capture_monitor("HEADLESS-1", &OPTS).unwrap();
    let crop = full.crop(Rect::new(101, 203, 50, 50)).unwrap();
    assert_eq!(first_frame_mismatch(&f, &crop), None, "region == crop of the full capture");
}

#[test]
fn hotplug_between_monitors_and_capture() {
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 800, 0),
        ],
        "",
    ) else {
        return;
    };
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    assert_eq!(cap.monitors().unwrap().len(), 2);
    sway.command("output HEADLESS-2 unplug");
    wait_for("output removal", || (cap.monitors().ok()?.len() == 1).then_some(()));
    assert!(matches!(
        cap.capture_monitor("HEADLESS-2", &OPTS),
        Err(CaptureError::NotFound(id)) if id == "HEADLESS-2"
    ));
    assert!(cap.capture_monitor("HEADLESS-1", &OPTS).is_ok());
    sway.command("create_output");
    let ms = wait_for("new output", || {
        let ms = cap.monitors().ok()?;
        (ms.len() == 2).then_some(ms)
    });
    assert!(ms.iter().any(|m| m.id.starts_with("HEADLESS-")));
}

#[test]
fn concurrent_captures_are_independent() {
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 800, 0),
        ],
        "",
    ) else {
        return;
    };
    let _p = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Background { output: "HEADLESS-1", scale: 1, seed: 8 },
            Job::Background { output: "HEADLESS-2", scale: 1, seed: 9 },
        ],
    );
    let cap = Arc::new(WaylandCapture::with_config(sway.config()).unwrap());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let cap = cap.clone();
            std::thread::spawn(move || {
                let (name, seed) = if i % 2 == 0 { ("HEADLESS-1", 8) } else { ("HEADLESS-2", 9) };
                for _ in 0..3 {
                    let f = cap.capture_monitor(name, &OPTS).unwrap();
                    assert_eq!(first_pattern_mismatch(&f, seed, (0, 0)), None);
                    let r = cap.capture_region(Rect::new(700, 10, 200, 20), &OPTS).unwrap();
                    assert_eq!(pixel(&r, 0, 0), bgra(8, 700, 10));
                    assert_eq!(pixel(&r, 199, 19), bgra(9, 99, 29));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("capture thread panicked");
    }
}

#[test]
fn compositor_gone_is_an_error_not_a_hang() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)], "") else {
        return;
    };
    let mut cfg = sway.config();
    cfg.timeout = Duration::from_secs(2);
    let cap = WaylandCapture::with_config(cfg).unwrap();
    assert!(cap.capture_monitor("HEADLESS-1", &OPTS).is_ok());
    let socket = sway.wayland_socket.clone();
    drop(sway); // kills sway
    let started = std::time::Instant::now();
    let err = cap.capture_monitor("HEADLESS-1", &OPTS).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    assert!(
        matches!(err, CaptureError::NoBackend(_) | CaptureError::Backend { .. }),
        "{err:?} (socket {socket:?})"
    );
}

#[test]
fn windows_enumeration_and_capture() {
    // Two floating windows with known size/position and one tiled window. Borders are off
    // (see the harness config) so the container is exactly the client area.
    let extra = "\
for_window [app_id=\"ssx.a\"] floating enable, resize set 300 200, move position 50 60\n\
for_window [app_id=\"ssx.b\"] floating enable, resize set 200 150, move position 500 300\n";
    let Some(sway) = Sway::start(
        &[
            OutputCfg::new("HEADLESS-1", 800, 600, 0, 0),
            OutputCfg::new("HEADLESS-2", 640, 480, 800, 0),
        ],
        extra,
    ) else {
        return;
    };
    let _bg = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Background { output: "HEADLESS-1", scale: 1, seed: 10 },
            Job::Background { output: "HEADLESS-2", scale: 1, seed: 11 },
        ],
    );
    let _w = Painter::spawn(
        &sway.wayland_socket,
        vec![
            Job::Window { title: "Alpha window", app_id: "ssx.a", w: 300, h: 200, seed: 12 },
            Job::Window { title: "Beta window", app_id: "ssx.b", w: 200, h: 150, seed: 13 },
        ],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let wins = wait_for("both windows", || {
        let w = cap.windows().ok()?;
        (w.iter().filter(|w| w.title.ends_with("window")).count() == 2).then_some(w)
    });
    let a = wins.iter().find(|w| w.title == "Alpha window").unwrap();
    let b = wins.iter().find(|w| w.title == "Beta window").unwrap();
    assert_eq!(a.app_name.as_deref(), Some("ssx.a"));
    assert_eq!(a.rect, Rect::new(50, 60, 300, 200), "logical == desktop pixels at 1x");
    assert_eq!(b.rect, Rect::new(500, 300, 200, 150));
    assert!(!a.minimized && !b.minimized);
    assert_eq!(wins.iter().filter(|w| w.focused).count(), 1, "exactly one focused window");
    assert!(a.id.starts_with("sway:"));

    // Cross-check geometry against the raw sway tree (global logical coordinates).
    let tree = sway.tree().to_string();
    assert!(tree.contains("\"Alpha window\"") && tree.contains("ssx.b"));

    // Pixel-exact window capture (no borders): content is the painter's pattern.
    let fa = cap.capture_window(&a.id, &OPTS).unwrap();
    assert_eq!(fa.rect(), a.rect);
    assert_eq!(first_pattern_mismatch(&fa, 12, (0, 0)), None);
    let fb = cap.capture_window(&b.id, &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&fb, 13, (0, 0)), None);
    if let Some(grim) = sway.grim(&["-g", "50,60 300x200"]) {
        assert_eq!(first_frame_mismatch(&fa, &grim), None, "window vs grim -g");
    }

    // Front-to-back order: floating windows are listed newest (topmost) first.
    let pos = |t: &str| wins.iter().position(|w| w.title == t).unwrap();
    assert!(pos("Beta window") < pos("Alpha window"), "{wins:?}");

    // Move a window to the 2nd output: coordinates stay global.
    sway.command("[app_id=\"ssx.a\"] move position 820 20");
    let moved = wait_for("moved window", || {
        let w = cap.windows().ok()?;
        w.into_iter().find(|w| w.title == "Alpha window" && w.rect.x == 820)
    });
    assert_eq!(moved.rect, Rect::new(820, 20, 300, 200));

    // A window on a hidden workspace is reported as not on screen and refuses capture.
    sway.command("[app_id=\"ssx.b\"] move container to workspace 7");
    let hidden = wait_for("hidden window", || {
        let w = cap.windows().ok()?;
        w.into_iter().find(|w| w.title == "Beta window" && w.minimized)
    });
    assert!(matches!(cap.capture_window(&hidden.id, &OPTS), Err(CaptureError::Backend { .. })));
    let wins = cap.windows().unwrap();
    assert!(wins.last().is_some_and(|w| w.minimized), "hidden windows sort last");
}

#[test]
fn window_borders_are_part_of_the_container_rect() {
    // With default borders sway adds a title bar; `rect` is the whole container.
    let extra = "default_border normal 2\nfor_window [app_id=\"ssx.c\"] floating enable, resize set 300 200, move position 40 40\n";
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)], extra) else {
        return;
    };
    let _bg = Painter::spawn(
        &sway.wayland_socket,
        vec![Job::Background { output: "HEADLESS-1", scale: 1, seed: 14 }],
    );
    let _w = Painter::spawn(
        &sway.wayland_socket,
        vec![Job::Window { title: "Bordered", app_id: "ssx.c", w: 300, h: 200, seed: 15 }],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let w = wait_for("window", || cap.windows().ok()?.into_iter().find(|w| w.title == "Bordered"));
    let f = cap.capture_window(&w.id, &OPTS).unwrap();
    assert_eq!(f.rect(), w.rect);
    // sway sizes the floating container so the *content* is 300x200; the container is larger.
    assert!(w.rect.width >= 300 && w.rect.height >= 200, "{:?}", w.rect);
    if let Some(grim) =
        sway.grim(&["-g", &format!("{},{} {}x{}", w.rect.x, w.rect.y, w.rect.width, w.rect.height)])
    {
        assert_eq!(first_frame_mismatch(&f, &grim), None, "bordered window vs grim -g");
    }
}

#[test]
fn window_rects_are_scaled_on_hidpi_output() {
    let extra =
        "for_window [app_id=\"ssx.d\"] floating enable, resize set 100 80, move position 30 20\n";
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0).scale("2")], extra)
    else {
        return;
    };
    let _bg = Painter::spawn(
        &sway.wayland_socket,
        vec![Job::Background { output: "HEADLESS-1", scale: 2, seed: 16 }],
    );
    let _w = Painter::spawn(
        &sway.wayland_socket,
        vec![Job::Window { title: "Hidpi", app_id: "ssx.d", w: 100, h: 80, seed: 17 }],
    );
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let w = wait_for("window", || cap.windows().ok()?.into_iter().find(|w| w.title == "Hidpi"));
    // Logical (30,20,100,80) at 2x is desktop (60,40,200,160).
    assert_eq!(w.rect, Rect::new(60, 40, 200, 160));
    let f = cap.capture_window(&w.id, &OPTS).unwrap();
    assert_eq!(f.size(), Size::new(200, 160));
    if let Some(grim) = sway.grim(&["-g", "30,20 100x80"]) {
        assert_eq!(
            first_frame_mismatch(&f, &grim),
            None,
            "hidpi window vs grim -g (logical geometry)"
        );
    }
}
