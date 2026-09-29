//! End-to-end tests of the KWin `ScreenShot2` strategy against a mock KWin service that
//! follows the real protocol: it replies with the image description first, then writes
//! raw `QImage` bytes to the pipe file descriptor it was passed, from another thread.
//!
//! Real KWin behaviour (authorisation, HDR outputs, fractional scaling) is not covered;
//! see the manual checklist in the report.
#![cfg(target_os = "linux")]
#![allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable

#[allow(dead_code)] // each test binary uses a subset of the harness
mod common;

use std::time::{Duration, Instant};

use common::{
    Bus, KwinBehavior, KwinImage, MockMonitor, PortalBehavior, kwin_alpha, pattern, start_kwin,
    start_mutter, start_portal,
};
use ssx_capture::{CaptureBackend, CaptureError, CaptureOptions};
use ssx_capture_portal::{InteractiveKind, PortalCapture, PortalConfig, Strategy};
use ssx_types::{Frame, Point, Rect};

fn image(format: u32, w: u32, h: u32, stride: u32) -> KwinImage {
    KwinImage { width: w, height: h, format, stride, truncate_to: None, scale: 1.0 }
}

fn ok(img: KwinImage) -> KwinBehavior {
    KwinBehavior::Image(img)
}

fn detect(bus: &Bus) -> PortalCapture {
    PortalCapture::with_config(bus.config()).expect("detect")
}

/// Independent statement of the expected straight-alpha result for a mock pixel.
fn expected(format: u32, x: u32, y: u32) -> [u8; 4] {
    let [r, g, b, _] = pattern(x, y);
    let a = kwin_alpha(format, x, y);
    match format {
        4 | 13 | 16 | 29 => [r, g, b, 255],
        5 | 17 => [r, g, b, a],
        6 | 18 => {
            let back = |c: u8| {
                let pre = (u32::from(c) * u32::from(a) + 127) / 255;
                ((pre * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
            };
            [back(r), back(g), back(b), a]
        }
        _ => unreachable!(),
    }
}

fn assert_kwin_frame(f: &Frame, format: u32, ox: u32, oy: u32) {
    let f = f.clone().into_rgba8().expect("sdr");
    for y in 0..f.height() {
        for x in 0..f.width() {
            let r = f.row(y);
            let i = 4 * x as usize;
            assert_eq!(
                [r[i], r[i + 1], r[i + 2], r[i + 3]],
                expected(format, ox + x, oy + y),
                "format {format} pixel ({x},{y})"
            );
        }
    }
}

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

#[test]
fn detect_prefers_kwin_and_reports_it_truthfully() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &["DP-2", "eDP-1"], ok(image(4, 4, 4, 16)));
    let cap = detect(&bus);
    assert_eq!(cap.strategy(), Strategy::KWin);
    assert_eq!(cap.name(), "kwin-screenshot2");
    assert_eq!(cap.kwin_version(), Some(5));
    assert!(!cap.portal_available());
    let c = cap.capabilities();
    assert!(c.cursor && c.native_desktop);
    assert!(!c.needs_user_interaction, "KWin capture is silent");
    assert!(!c.enumerate_windows && !c.capture_windows);
}

#[test]
fn forced_portal_strategy_ignores_kwin() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &["a"], ok(image(4, 4, 4, 16)));
    let _portal = start_portal(&bus, PortalBehavior::Cancelled);
    let mut cfg = bus.config();
    cfg.strategy = Some(Strategy::Portal);
    let cap = PortalCapture::with_config(cfg).unwrap();
    assert_eq!(cap.strategy(), Strategy::Portal);
    assert_eq!(cap.kwin_version(), None);
}

#[test]
fn workspace_capture_decodes_every_format_with_stride_padding() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 5, &["DP-2"], ok(image(4, 1, 1, 4)));
    let cap = detect(&bus);
    // (format, bytes per pixel, extra stride padding per row)
    let cases = [
        (4, 4, 0),
        (4, 4, 12),
        (5, 4, 0),
        (5, 4, 4),
        (6, 4, 0),
        (6, 4, 8),
        (16, 4, 4),
        (17, 4, 0),
        (18, 4, 16),
        (13, 3, 0),
        (13, 3, 5),
        (29, 3, 1),
    ];
    for (format, bpp, pad) in cases {
        let (w, h) = (23u32, 17u32);
        kwin.set(ok(image(format, w, h, w * bpp + pad)));
        let f = cap
            .capture_desktop(&CaptureOptions::default())
            .unwrap_or_else(|e| panic!("format {format} pad {pad}: {e}"));
        assert_eq!((f.width(), f.height()), (w, h));
        assert_eq!(f.stride(), (w * 4) as usize, "padding must be dropped");
        assert_kwin_frame(&f, format, 0, 0);
    }
}

#[test]
fn large_images_larger_than_the_pipe_buffer_are_read_concurrently() {
    let Some(bus) = Bus::start() else { return };
    // 1500 x 900 x 4 = 5.4 MB, far beyond the 64 KiB pipe buffer.
    let _kwin = start_kwin(&bus, 5, &[], ok(image(4, 1500, 900, 1500 * 4 + 8)));
    let cap = detect(&bus);
    let f = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!((f.width(), f.height()), (1500, 900));
    assert_kwin_frame(&f.crop(Rect::new(1400, 850, 100, 50)).unwrap(), 4, 1400, 850);
    assert_kwin_frame(&f.crop(Rect::new(0, 0, 50, 50)).unwrap(), 4, 0, 0);
}

#[test]
fn native_resolution_is_always_requested_and_cursor_on_demand() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 5, &["DP-2"], ok(image(4, 8, 8, 32)));
    let cap = detect(&bus);
    cap.capture_desktop(&CaptureOptions { include_cursor: false }).unwrap();
    cap.capture_desktop(&CaptureOptions { include_cursor: true }).unwrap();
    let calls = kwin.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].method, "CaptureWorkspace");
    assert!(calls.iter().all(|c| c.native_resolution == Some(true)));
    assert_eq!(calls[0].include_cursor, None);
    assert_eq!(calls[1].include_cursor, Some(true));
}

#[test]
fn monitor_capture_passes_the_screen_name_and_uses_the_layout() {
    let Some(bus) = Bus::start() else { return };
    let mut img = image(4, 120, 60, 480);
    img.scale = 1.0;
    let kwin = start_kwin(&bus, 5, &["DP-2", "eDP-1"], ok(img));
    let _mutter = start_mutter(&bus, two_monitors());
    let cap = detect(&bus);
    assert!(cap.capabilities().enumerate_monitors);
    let f = cap.capture_monitor("eDP-1", &CaptureOptions::default()).unwrap();
    assert_eq!(kwin.calls()[0].method, "CaptureScreen");
    assert_eq!(kwin.calls()[0].args, vec!["eDP-1".to_owned()]);
    assert_eq!(f.origin, Point::new(0, 10));
    assert_eq!(f.scale_factor, 1.0);
    assert_kwin_frame(&f, 4, 0, 0);
    assert!(matches!(
        cap.capture_monitor("HDMI-7", &CaptureOptions::default()),
        Err(CaptureError::NotFound(n)) if n == "HDMI-7"
    ));
}

#[test]
fn scale_from_the_reply_is_used_when_no_layout_is_known() {
    let Some(bus) = Bus::start() else { return };
    let mut img = image(4, 8, 8, 32);
    img.scale = 1.5;
    let _kwin = start_kwin(&bus, 5, &["DP-2"], ok(img));
    let cap = detect(&bus);
    let f = cap.capture_monitor("DP-2", &CaptureOptions::default()).unwrap();
    assert_eq!(f.scale_factor, 1.5);
    assert_eq!(f.origin, Point::new(0, 0));
    let d = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(d.scale_factor, 1.5);
}

#[test]
fn workspace_region_capture_crops_using_the_desktop_origin() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &["DP-2", "eDP-1"], ok(image(4, 200, 70, 800)));
    let _mutter = start_mutter(&bus, two_monitors());
    let cap = detect(&bus);
    let d = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(d.origin, Point::new(-80, 0));
    let r = cap.capture_region(Rect::new(-10, 10, 30, 20), &CaptureOptions::default()).unwrap();
    assert_eq!(r.origin, Point::new(-10, 10));
    assert_kwin_frame(&r, 4, 70, 10);
}

#[test]
fn kwin_errors_are_mapped() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 5, &["DP-2"], KwinBehavior::Error("Cancelled"));
    let mut cfg = bus.config();
    cfg.kwin_fallback_to_portal = false;
    let cap = PortalCapture::with_config(cfg).unwrap();
    let opts = CaptureOptions::default();
    assert!(matches!(cap.capture_desktop(&opts), Err(CaptureError::Cancelled)));

    kwin.set(KwinBehavior::Error("NoAuthorized"));
    match cap.capture_desktop(&opts) {
        Err(CaptureError::PermissionDenied(m)) => {
            assert!(m.contains("X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2"), "{m}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    assert_eq!(cap.strategy(), Strategy::KWin, "no fallback configured: strategy stays");

    kwin.set(KwinBehavior::Error("InvalidWindow"));
    assert!(matches!(
        cap.capture_window("{not-a-window}", &opts),
        Err(CaptureError::NotFound(w)) if w == "{not-a-window}"
    ));
    kwin.set(KwinBehavior::Error("InvalidArea"));
    assert!(matches!(
        cap.kwin_capture_area(Rect::new(0, 0, 5, 5), &opts),
        Err(CaptureError::InvalidRegion(_))
    ));
    kwin.set(KwinBehavior::Error("NoActiveWindow"));
    assert!(matches!(cap.kwin_capture_active_window(&opts), Err(CaptureError::Backend { .. })));
    kwin.set(KwinBehavior::Error("FileDescriptor"));
    assert!(matches!(cap.capture_desktop(&opts), Err(CaptureError::Backend { .. })));
}

#[test]
fn permission_denied_falls_back_to_the_portal_and_stays_there() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 5, &["DP-2"], KwinBehavior::Error("NoAuthorized"));
    let portal = start_portal(
        &bus,
        PortalBehavior::Png { w: 12, h: 9, file_name: "fb.png".into(), delay: Duration::ZERO },
    );
    let cap = detect(&bus);
    assert_eq!(cap.strategy(), Strategy::KWin);
    let f = cap.capture_desktop(&CaptureOptions::default()).expect("fallback capture");
    assert_eq!((f.width(), f.height()), (12, 9));
    assert_eq!(cap.strategy(), Strategy::Portal);
    assert_eq!(cap.name(), "xdg-portal");
    assert!(cap.capabilities().needs_user_interaction);
    assert!(!cap.capabilities().cursor);
    cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(kwin.calls().len(), 1, "KWin is not asked again after being refused");
    assert_eq!(portal.requests(), 2);
    assert!(matches!(
        cap.kwin_capture_active_screen(&CaptureOptions::default()),
        Err(CaptureError::Unsupported { .. })
    ));
}

#[test]
fn old_kwin_without_workspace_capture_uses_the_portal_for_that_call_only() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 2, &["DP-2"], ok(image(4, 6, 6, 24)));
    let portal = start_portal(
        &bus,
        PortalBehavior::Png { w: 7, h: 7, file_name: "old.png".into(), delay: Duration::ZERO },
    );
    let cap = detect(&bus);
    let d = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(d.width(), 7, "CaptureWorkspace needs version 3; portal used");
    assert_eq!(cap.strategy(), Strategy::KWin, "per-monitor capture still works on v2");
    let m = cap.capture_monitor("DP-2", &CaptureOptions::default()).unwrap();
    assert_eq!(m.width(), 6);
    assert_eq!(kwin.calls().len(), 1);
    assert_eq!(kwin.calls()[0].method, "CaptureScreen");
    assert_eq!(portal.requests(), 1);
}

#[test]
fn old_kwin_without_a_portal_reports_unsupported() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 1, &["DP-2"], ok(image(4, 6, 6, 24)));
    let cap = detect(&bus);
    assert!(matches!(
        cap.capture_desktop(&CaptureOptions::default()),
        Err(CaptureError::Unsupported { .. })
    ));
    assert!(matches!(
        cap.kwin_capture_active_screen(&CaptureOptions::default()),
        Err(CaptureError::Unsupported { .. })
    ));
}

#[test]
fn truncated_pixel_data_is_an_error_not_a_hang() {
    let Some(bus) = Bus::start() else { return };
    let mut img = image(4, 40, 40, 160);
    img.truncate_to = Some(1000);
    let _kwin = start_kwin(&bus, 5, &[], ok(img));
    let cap = detect(&bus);
    let t = Instant::now();
    match cap.capture_desktop(&CaptureOptions::default()) {
        Err(CaptureError::Backend { message, .. }) => {
            assert!(message.contains("truncated"), "{message}");
        }
        other => panic!("expected truncation error, got {other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn unsupported_pixel_formats_are_rejected_with_an_explanation() {
    let Some(bus) = Bus::start() else { return };
    // 35 = QImage::Format_RGBA32FPx4_Premultiplied
    let _kwin = start_kwin(&bus, 5, &[], ok(image(35, 2, 2, 32)));
    let cap = detect(&bus);
    match cap.capture_desktop(&CaptureOptions::default()) {
        Err(CaptureError::Backend { message, .. }) => {
            assert!(message.contains("not supported"), "{message}");
        }
        other => panic!("expected format error, got {other:?}"),
    }
}

#[test]
fn hung_kwin_times_out() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &[], KwinBehavior::Hang);
    let mut cfg = bus.config();
    cfg.timeout = Duration::from_millis(400);
    cfg.interactive_timeout = Duration::from_millis(800);
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
    assert!(
        cap.kwin_capture_interactive(InteractiveKind::Window, &CaptureOptions::default()).is_err()
    );
    assert!(t.elapsed() >= Duration::from_millis(750), "interactive timeout applies");
}

#[test]
fn oversized_screenshots_are_refused() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &[], ok(image(4, 100, 100, 400)));
    let mut cfg = bus.config();
    cfg.max_image_bytes = 10_000;
    let cap = PortalCapture::with_config(cfg).unwrap();
    match cap.capture_desktop(&CaptureOptions::default()) {
        Err(CaptureError::Backend { message, .. }) => {
            assert!(message.contains("limit"), "{message}");
        }
        other => panic!("expected limit error, got {other:?}"),
    }
}

#[test]
fn kwin_specific_calls_send_the_right_arguments() {
    let Some(bus) = Bus::start() else { return };
    let kwin = start_kwin(&bus, 5, &["DP-2"], ok(image(4, 10, 10, 40)));
    let cap = detect(&bus);
    let o = CaptureOptions::default();
    cap.kwin_capture_active_screen(&o).unwrap();
    cap.kwin_capture_active_window(&o).unwrap();
    cap.kwin_capture_area(Rect::new(-5, 7, 10, 10), &o).unwrap();
    cap.kwin_capture_interactive(InteractiveKind::Window, &o).unwrap();
    cap.kwin_capture_interactive(InteractiveKind::Screen, &o).unwrap();
    cap.capture_window("{1234-abcd}", &o).unwrap();
    let calls: Vec<_> = kwin.calls().into_iter().map(|c| (c.method, c.args)).collect();
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        calls,
        vec![
            ("CaptureActiveScreen", s(&[])),
            ("CaptureActiveWindow", s(&[])),
            ("CaptureArea", s(&["-5", "7", "10", "10"])),
            ("CaptureInteractive", s(&["0"])),
            ("CaptureInteractive", s(&["1"])),
            ("CaptureWindow", s(&["{1234-abcd}"])),
        ]
    );
}

#[test]
fn concurrent_kwin_captures() {
    let Some(bus) = Bus::start() else { return };
    let _kwin = start_kwin(&bus, 5, &[], ok(image(6, 300, 200, 1200 + 16)));
    let cap = std::sync::Arc::new(detect(&bus));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let cap = cap.clone();
            std::thread::spawn(move || cap.capture_desktop(&CaptureOptions::default()))
        })
        .collect();
    for h in handles {
        let f = h.join().expect("thread").expect("capture");
        assert_kwin_frame(&f, 6, 0, 0);
    }
}

#[test]
fn defaults_match_the_documented_values() {
    let c = PortalConfig::default();
    assert_eq!(c.timeout, Duration::from_secs(15));
    assert_eq!(c.interactive_timeout, Duration::from_secs(60));
    assert!(c.delete_portal_file && c.kwin_fallback_to_portal && c.wayland_outputs);
    assert_eq!(c.max_image_bytes, 1 << 30);
}
