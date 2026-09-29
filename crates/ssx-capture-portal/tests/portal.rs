//! End-to-end tests of the portal strategy against a mock `xdg-desktop-portal` (and a
//! mock Mutter for monitor layout) on a private D-Bus bus.
//!
//! They exercise the real `ashpd` client, the real D-Bus wire format and the real file
//! handling. What they cannot show is how GNOME/KDE portal backends behave; see the crate
//! README in the report for the manual checklist.
#![cfg(target_os = "linux")]
#![allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable

#[allow(dead_code)] // each test binary uses a subset of the harness
mod common;

use std::time::{Duration, Instant};

use common::{Bus, MockMonitor, PortalBehavior, pattern, scratch_dir, start_mutter, start_portal};
use ssx_capture::{CaptureBackend, CaptureError, CaptureOptions};
use ssx_capture_portal::{DESKTOP_MONITOR_ID, PortalCapture, PortalConfig, Strategy};
use ssx_types::{Frame, Point, Rect};

fn png(w: u32, h: u32, name: &str) -> PortalBehavior {
    PortalBehavior::Png { w, h, file_name: name.into(), delay: Duration::ZERO }
}

/// Asserts that `f` equals the reference pattern shifted by `(ox, oy)`.
fn assert_pattern(f: &Frame, ox: u32, oy: u32) {
    let f = f.clone().into_rgba8().expect("sdr frame");
    for y in 0..f.height() {
        for x in 0..f.width() {
            let r = f.row(y);
            let got = [
                r[4 * x as usize],
                r[4 * x as usize + 1],
                r[4 * x as usize + 2],
                r[4 * x as usize + 3],
            ];
            assert_eq!(got, pattern(ox + x, oy + y), "pixel ({x},{y}) of crop at ({ox},{oy})");
        }
    }
}

/// Two monitors: left `DP-2` at logical (-80,0) 80x60, right `eDP-1` at (0,10) 120x60.
fn two_monitors() -> Vec<MockMonitor> {
    vec![
        MockMonitor {
            connector: "DP-2",
            width: 80,
            height: 60,
            x: -80,
            y: 0,
            scale: 1.0,
            primary: false,
        },
        MockMonitor {
            connector: "eDP-1",
            width: 120,
            height: 60,
            x: 0,
            y: 10,
            scale: 1.0,
            primary: true,
        },
    ]
}

fn detect(bus: &Bus) -> PortalCapture {
    PortalCapture::with_config(bus.config()).expect("detect")
}

#[test]
fn desktop_capture_returns_exact_pixels_and_deletes_the_file() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, png(37, 23, "shot.png"));
    let cap = detect(&bus);
    assert_eq!(cap.strategy(), Strategy::Portal);
    assert_eq!(cap.name(), "xdg-portal");

    let frame = cap.capture_desktop(&CaptureOptions::default()).expect("capture");
    assert_eq!((frame.width(), frame.height()), (37, 23));
    assert_pattern(&frame, 0, 0);
    assert_eq!(frame.origin, Point::new(0, 0), "no layout source: origin unknown");
    assert_eq!(frame.scale_factor, 1.0);
    assert!(portal.files().is_empty(), "portal file must be deleted: {:?}", portal.files());
    assert_eq!(portal.interactive_flags(), vec![Some(false)]);
}

#[test]
fn capabilities_are_truthful_for_the_portal() {
    let Some(bus) = Bus::start() else { return };
    let _portal = start_portal(&bus, png(4, 4, "a.png"));
    let cap = detect(&bus);
    let c = cap.capabilities();
    assert!(c.native_desktop);
    assert!(c.needs_user_interaction);
    assert!(!c.cursor, "the portal has no cursor option");
    assert!(!c.enumerate_windows && !c.capture_windows && !c.hdr_float);
    assert!(!c.enumerate_monitors, "no layout source on this bus");
    assert!(matches!(cap.monitors(), Err(CaptureError::Unsupported { .. })));
    assert!(matches!(cap.windows(), Err(CaptureError::Unsupported { .. })));
    assert!(matches!(
        cap.capture_window("x", &CaptureOptions::default()),
        Err(CaptureError::Unsupported { .. })
    ));
}

#[test]
fn synthetic_desktop_monitor_works_without_layout() {
    let Some(bus) = Bus::start() else { return };
    let _portal = start_portal(&bus, png(20, 10, "a.png"));
    let cap = detect(&bus);
    let f = cap.capture_monitor(DESKTOP_MONITOR_ID, &CaptureOptions::default()).unwrap();
    assert_pattern(&f, 0, 0);
    assert!(matches!(
        cap.capture_monitor("DP-1", &CaptureOptions::default()),
        Err(CaptureError::Unsupported { .. })
    ));
}

#[test]
fn monitors_crops_and_regions_use_the_mutter_layout() {
    let Some(bus) = Bus::start() else { return };
    // desktop bounds: x -80..120, y 0..70 -> 200 x 70 image
    let _portal = start_portal(&bus, png(200, 70, "desk.png"));
    let _mutter = start_mutter(&bus, two_monitors());
    let cap = detect(&bus);
    assert!(cap.capabilities().enumerate_monitors);

    let monitors = cap.monitors().unwrap();
    assert_eq!(monitors.len(), 2);
    assert_eq!(monitors[0].id, "DP-2");
    assert_eq!(monitors[0].rect, Rect::new(-80, 0, 80, 60));
    assert_eq!(monitors[1].id, "eDP-1");
    assert_eq!(monitors[1].rect, Rect::new(0, 10, 120, 60));
    assert!(monitors[1].primary);
    assert_eq!(monitors[1].name, "Mock eDP-1");
    assert_eq!(monitors[1].refresh_hz, Some(60.0));

    let opts = CaptureOptions::default();
    let desktop = cap.capture_desktop(&opts).unwrap();
    assert_eq!(desktop.origin, Point::new(-80, 0));
    assert_pattern(&desktop, 0, 0);

    let right = cap.capture_monitor("eDP-1", &opts).unwrap();
    assert_eq!((right.width(), right.height()), (120, 60));
    assert_eq!(right.origin, Point::new(0, 10));
    assert_eq!(right.scale_factor, 1.0);
    assert_pattern(&right, 80, 10);

    let left = cap.capture_monitor("DP-2", &opts).unwrap();
    assert_eq!(left.origin, Point::new(-80, 0));
    assert_pattern(&left, 0, 0);

    assert!(
        matches!(cap.capture_monitor("HDMI-9", &opts), Err(CaptureError::NotFound(id)) if id == "HDMI-9")
    );

    // A region straddling both monitors, in virtual-desktop coordinates.
    let region = cap.capture_region(Rect::new(-10, 10, 30, 20), &opts).unwrap();
    assert_eq!(region.origin, Point::new(-10, 10));
    assert_pattern(&region, 70, 10);
    assert!(matches!(
        cap.capture_region(Rect::new(0, 0, 0, 5), &opts),
        Err(CaptureError::InvalidRegion(_))
    ));
}

#[test]
fn hidpi_layout_is_measured_from_the_image() {
    let Some(bus) = Bus::start() else { return };
    // One monitor: physical 100x80 at scale 2 -> logical 50x40; screenshot is 100x80.
    let _portal = start_portal(&bus, png(100, 80, "hi.png"));
    let _mutter = start_mutter(
        &bus,
        vec![MockMonitor {
            connector: "eDP-1",
            width: 100,
            height: 80,
            x: 0,
            y: 0,
            scale: 2.0,
            primary: true,
        }],
    );
    let cap = detect(&bus);
    let m = cap.monitors().unwrap();
    assert_eq!(m[0].rect, Rect::new(0, 0, 100, 80), "public rect is in physical pixels");
    assert_eq!(m[0].scale_factor, 2.0);
    let f = cap.capture_monitor("eDP-1", &CaptureOptions::default()).unwrap();
    assert_eq!(f.scale_factor, 2.0);
    assert_pattern(&f, 0, 0);
}

#[test]
fn image_that_does_not_fit_the_layout_is_not_guessed_at() {
    let Some(bus) = Bus::start() else { return };
    // Layout says 200x70, the portal returns 100x50: aspect differs.
    let _portal = start_portal(&bus, png(100, 50, "odd.png"));
    let _mutter = start_mutter(&bus, two_monitors());
    let cap = detect(&bus);
    let opts = CaptureOptions::default();
    let d = cap.capture_desktop(&opts).unwrap();
    assert_eq!(d.origin, Point::new(0, 0), "mismatch: origin is left unset");
    assert!(matches!(cap.capture_monitor("eDP-1", &opts), Err(CaptureError::Backend { .. })));
}

#[test]
fn cancel_and_denial_are_mapped() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, PortalBehavior::Cancelled);
    let cap = detect(&bus);
    let opts = CaptureOptions::default();
    assert!(matches!(cap.capture_desktop(&opts), Err(CaptureError::Cancelled)));
    assert!(matches!(cap.capture_interactive(), Err(CaptureError::Cancelled)));

    portal.set(PortalBehavior::Denied);
    match cap.capture_desktop(&opts) {
        Err(CaptureError::PermissionDenied(m)) => assert!(m.contains("response code 2"), "{m}"),
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    assert!(matches!(cap.capture_interactive(), Err(CaptureError::Backend { .. })));
}

#[test]
fn bad_portal_answers_are_backend_errors_and_files_are_cleaned() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(
        &bus,
        PortalBehavior::RawUri { uri: "file:///nonexistent/ssx/shot.png".into() },
    );
    let cap = detect(&bus);
    let opts = CaptureOptions::default();
    assert!(
        matches!(cap.capture_desktop(&opts), Err(CaptureError::Backend { .. })),
        "missing file"
    );

    portal.set(PortalBehavior::Bytes {
        bytes: b"definitely not a png".to_vec(),
        file_name: "junk.png".into(),
    });
    assert!(matches!(cap.capture_desktop(&opts), Err(CaptureError::Backend { .. })), "bad image");
    assert!(portal.files().is_empty(), "undecodable file is still removed");

    portal.set(PortalBehavior::RawUri { uri: "https://example.com/x.png".into() });
    assert!(
        matches!(cap.capture_desktop(&opts), Err(CaptureError::Backend { .. })),
        "not a file URI"
    );

    portal.set(PortalBehavior::RawUri { uri: "file:///tmp/bad%zzescape.png".into() });
    assert!(matches!(cap.capture_desktop(&opts), Err(CaptureError::Backend { .. })), "bad escape");
}

#[test]
fn percent_encoded_paths_with_spaces_and_unicode() {
    let Some(bus) = Bus::start() else { return };
    let portal =
        start_portal(&bus, png(9, 9, "Screenshot from 2025-01-01 12-00-00 \u{e4}\u{f6} #1.png"));
    let cap = detect(&bus);
    let f = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_pattern(&f, 0, 0);
    assert!(portal.files().is_empty());
}

#[test]
fn huge_images_are_refused_before_decoding() {
    let Some(bus) = Bus::start() else { return };
    let _portal = start_portal(&bus, png(64, 64, "big.png"));
    let mut cfg = bus.config();
    cfg.max_image_bytes = 64 * 64 * 4 - 1;
    let cap = PortalCapture::with_config(cfg).unwrap();
    match cap.capture_desktop(&CaptureOptions::default()) {
        Err(CaptureError::Backend { message, .. }) => {
            assert!(message.contains("limit"), "{message}");
        }
        other => panic!("expected a limit error, got {other:?}"),
    }
    let mut cfg = bus.config();
    cfg.max_image_bytes = 64 * 64 * 4;
    let cap = PortalCapture::with_config(cfg).unwrap();
    assert!(cap.capture_desktop(&CaptureOptions::default()).is_ok());
}

#[test]
fn delayed_response_is_awaited() {
    let Some(bus) = Bus::start() else { return };
    let _portal = start_portal(
        &bus,
        PortalBehavior::Png {
            w: 8,
            h: 8,
            file_name: "slow.png".into(),
            delay: Duration::from_millis(500),
        },
    );
    let cap = detect(&bus);
    let t = Instant::now();
    let f = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(450), "returned before the portal answered");
    assert_pattern(&f, 0, 0);
}

#[test]
fn silent_portal_times_out_instead_of_hanging() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, PortalBehavior::Silent);
    let mut cfg = bus.config();
    cfg.timeout = Duration::from_millis(400);
    cfg.interactive_timeout = Duration::from_millis(900);
    let cap = PortalCapture::with_config(cfg).unwrap();

    let t = Instant::now();
    match cap.capture_desktop(&CaptureOptions::default()) {
        Err(CaptureError::Backend { message, .. }) => {
            assert!(message.contains("timed out"), "{message}");
        }
        other => panic!("expected timeout, got {other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(5));

    let t = Instant::now();
    assert!(matches!(cap.capture_interactive(), Err(CaptureError::Backend { .. })));
    let e = t.elapsed();
    assert!(
        e >= Duration::from_millis(850) && e < Duration::from_secs(5),
        "interactive timeout used: {e:?}"
    );
    assert_eq!(portal.interactive_flags(), vec![Some(false), Some(true)]);
}

#[test]
fn interactive_capture_sets_the_interactive_option() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, png(30, 20, "sel.png"));
    let _mutter = start_mutter(&bus, two_monitors());
    let cap = detect(&bus);
    let f = cap.capture_interactive().unwrap();
    assert_eq!((f.width(), f.height()), (30, 20));
    assert_pattern(&f, 0, 0);
    assert_eq!(f.origin, Point::new(0, 0), "a selection has no known origin");
    assert_eq!(portal.interactive_flags(), vec![Some(true)]);
}

#[test]
fn concurrent_captures_do_not_interfere() {
    let Some(bus) = Bus::start() else { return };
    // "{n}" makes the mock write a distinct file per request.
    let portal = start_portal(
        &bus,
        PortalBehavior::Png {
            w: 60,
            h: 40,
            file_name: "concurrent-{n}.png".into(),
            delay: Duration::from_millis(150),
        },
    );
    let cap = std::sync::Arc::new(detect(&bus));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let cap = cap.clone();
            std::thread::spawn(move || cap.capture_desktop(&CaptureOptions::default()))
        })
        .collect();
    for h in handles {
        let f = h.join().expect("thread").expect("every concurrent capture succeeds");
        assert_eq!((f.width(), f.height()), (60, 40));
        assert_pattern(&f, 0, 0);
    }
    assert_eq!(portal.requests(), 6);
    assert!(portal.files().is_empty(), "all files cleaned up: {:?}", portal.files());
}

#[test]
fn delete_can_be_disabled() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, png(5, 5, "keep.png"));
    let mut cfg = bus.config();
    cfg.delete_portal_file = false;
    let cap = PortalCapture::with_config(cfg).unwrap();
    cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(portal.files().len(), 1);
    std::fs::remove_dir_all(&portal.dir).ok();
}

#[test]
fn no_portal_on_the_bus_is_no_backend() {
    let Some(bus) = Bus::start() else { return };
    match PortalCapture::with_config(bus.config()) {
        Err(CaptureError::NoBackend(m)) => assert!(m.contains("xdg-desktop-portal"), "{m}"),
        other => panic!("expected NoBackend, got {other:?}"),
    }
    let mut cfg = bus.config();
    cfg.strategy = Some(Strategy::KWin);
    assert!(matches!(PortalCapture::with_config(cfg), Err(CaptureError::NoBackend(_))));
}

#[test]
fn no_session_bus_is_no_backend() {
    let dir = scratch_dir("nobus");
    let cfg = PortalConfig {
        bus_address: Some(format!("unix:path={}", dir.join("does-not-exist").display())),
        timeout: Duration::from_secs(3),
        ..PortalConfig::default()
    };
    match PortalCapture::with_config(cfg) {
        Err(CaptureError::NoBackend(m)) => assert!(m.contains("session bus"), "{m}"),
        other => panic!("expected NoBackend, got {other:?}"),
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn portal_that_disappears_after_detection_is_reported() {
    let Some(bus) = Bus::start() else { return };
    let portal = start_portal(&bus, png(4, 4, "x.png"));
    let cap = detect(&bus);
    drop(portal);
    // The name is gone; the call must fail cleanly rather than hang.
    let r = cap.capture_desktop(&CaptureOptions::default());
    assert!(
        matches!(r, Err(CaptureError::NoBackend(_) | CaptureError::Backend { .. })),
        "got {r:?}"
    );
}
