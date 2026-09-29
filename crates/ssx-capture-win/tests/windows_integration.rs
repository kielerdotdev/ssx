//! Integration tests against a real Windows desktop.
//!
//! They are `#[ignore]`d because they need an interactive session with a display (a
//! headless CI service or session 0 has none). Run them on a Windows machine or an
//! interactive CI runner with:
//!
//! ```text
//! cargo test -p ssx-capture-win --test windows_integration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` keeps two tests from creating desktop duplications at the same time.
//! Tests that need a specific setup (an HDR display, a visible window) print why they are
//! skipping and pass, so the suite is usable on any machine.

#![cfg(windows)]

use ssx_capture::{CaptureBackend, CaptureOptions};
use ssx_capture_win::WindowsCapture;
use ssx_types::{ColorSpace, Frame, Monitor, PixelFormat, Point, Rect};

fn backend() -> WindowsCapture {
    WindowsCapture::new().expect("an interactive Windows session with at least one display")
}

fn primary(monitors: &[Monitor]) -> &Monitor {
    monitors.iter().find(|m| m.primary).expect("Windows always has a primary monitor")
}

/// Sanity checks that hold for any capture of a real desktop.
fn assert_sane(frame: &Frame, what: &str) {
    assert!(frame.width() > 0 && frame.height() > 0, "{what}: empty frame");
    assert!(frame.scale_factor >= 0.5, "{what}: implausible scale {}", frame.scale_factor);
    assert!(frame.data().iter().any(|b| *b != 0), "{what}: frame is entirely zero bytes");
    match frame.format() {
        PixelFormat::Bgra8 | PixelFormat::Rgba8 => {
            assert_eq!(frame.color_space(), ColorSpace::Srgb, "{what}");
            assert_eq!(frame.sdr_white_nits, None, "{what}: 8-bit frames carry no SDR white");
            for y in 0..frame.height() {
                assert!(
                    frame.row(y).chunks_exact(4).all(|p| p[3] == 255),
                    "{what}: alpha is not opaque on row {y}"
                );
            }
        }
        PixelFormat::Rgba16F => {
            assert_eq!(frame.color_space(), ColorSpace::ScRgbLinear, "{what}");
            let nits = frame.sdr_white_nits.expect("float frames must carry sdr_white_nits");
            assert!((80.0..=480.0).contains(&nits), "{what}: SDR white {nits} nits out of range");
            for y in 0..frame.height() {
                for px in frame.row(y).chunks_exact(8) {
                    for ch in px.chunks_exact(2) {
                        let v = half::f16::from_le_bytes([ch[0], ch[1]]).to_f32();
                        assert!(v.is_finite(), "{what}: non-finite float pixel on row {y}");
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn enumerate_monitors() {
    let backend = backend();
    let monitors = backend.monitors().expect("monitors");
    assert!(!monitors.is_empty());
    assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1, "exactly one primary");
    for m in &monitors {
        println!("{m:#?}");
        assert!(!m.id.is_empty() && !m.name.is_empty());
        assert!(!m.rect.is_empty(), "{}: empty rect", m.id);
        assert!(m.scale_factor >= 0.5, "{}: scale {}", m.id, m.scale_factor);
        if let Some(hdr) = m.hdr {
            assert!((80.0..=480.0).contains(&hdr.sdr_white_nits), "{}: {hdr:?}", m.id);
        }
    }
    let mut ids: Vec<_> = monitors.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), monitors.len(), "monitor ids must be unique");
    assert_eq!(
        primary(&monitors).rect.origin(),
        Point::new(0, 0),
        "the primary monitor sits at the desktop origin"
    );
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn capture_primary_monitor() {
    let backend = backend();
    let monitors = backend.monitors().expect("monitors");
    let m = primary(&monitors);
    let frame = backend.capture_monitor(&m.id, &CaptureOptions::default()).expect("capture");
    assert_sane(&frame, "primary monitor");
    assert_eq!(frame.size(), m.rect.size(), "frame size equals the monitor's physical size");
    assert_eq!(frame.origin, m.rect.origin());
    assert!((frame.scale_factor - m.scale_factor).abs() < 1e-9);
    let hdr_active = m.hdr.is_some_and(|h| h.active);
    if hdr_active {
        println!("primary monitor is HDR-active; expecting a float frame");
    }
    // Either way the frame's labels must agree with what the backend says about the display,
    // unless a fallback API could only deliver 8-bit (then it must be labelled 8-bit).
    if frame.format() == PixelFormat::Rgba16F {
        assert!(hdr_active || frame.sdr_white_nits == Some(80.0));
    }
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn capture_every_monitor_with_and_without_cursor() {
    let backend = backend();
    for m in backend.monitors().expect("monitors") {
        for include_cursor in [false, true] {
            let frame = backend
                .capture_monitor(&m.id, &CaptureOptions { include_cursor })
                .unwrap_or_else(|e| panic!("{} (cursor={include_cursor}): {e}", m.id));
            assert_sane(&frame, &format!("{} cursor={include_cursor}", m.id));
            assert_eq!(frame.size(), m.rect.size(), "{}", m.id);
        }
    }
}

#[test]
#[ignore = "needs an interactive Windows desktop with an HDR display"]
fn capture_hdr_monitor_if_active() {
    let backend = backend();
    let monitors = backend.monitors().expect("monitors");
    let Some(m) = monitors.iter().find(|m| m.hdr.is_some_and(|h| h.active)) else {
        println!("SKIP: no HDR-active monitor; enable HDR in Settings > System > Display");
        return;
    };
    let hdr = m.hdr.expect("checked above");
    println!("HDR monitor {}: {hdr:?}", m.id);
    let frame = backend.capture_monitor(&m.id, &CaptureOptions::default()).expect("capture");
    assert_sane(&frame, "HDR monitor");
    assert_eq!(
        frame.format(),
        PixelFormat::Rgba16F,
        "an HDR-active display must be captured as float scRGB (check the log for fallbacks)"
    );
    assert_eq!(frame.color_space(), ColorSpace::ScRgbLinear);
    assert_eq!(frame.sdr_white_nits, Some(hdr.sdr_white_nits));
}

#[test]
#[ignore = "needs an interactive Windows desktop with a visible window"]
fn capture_a_window() {
    let backend = backend();
    let windows = backend.windows().expect("windows");
    println!("{} windows listed", windows.len());
    for w in &windows {
        println!(
            "  [{}] {:?} ({:?}) {:?} min={} focus={}",
            w.id, w.title, w.app_name, w.rect, w.minimized, w.focused
        );
    }
    let Some(w) =
        windows.iter().find(|w| !w.minimized && w.rect.width >= 100 && w.rect.height >= 100)
    else {
        println!("SKIP: no visible, non-minimised window of at least 100x100");
        return;
    };
    let frame = backend
        .capture_window(&w.id, &CaptureOptions::default())
        .unwrap_or_else(|e| panic!("capturing {:?}: {e}", w.title));
    assert_sane(&frame, &w.title);
    // WGC's window size can differ slightly from DWM's frame bounds (border/shadow policy
    // differs by Windows build), so only require it to be close.
    let (dw, dh) = (
        i64::from(frame.width()).abs_diff(i64::from(w.rect.width)),
        i64::from(frame.height()).abs_diff(i64::from(w.rect.height)),
    );
    assert!(dw <= 40 && dh <= 40, "window frame {:?} vs bounds {:?}", frame.size(), w.rect.size());
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn window_list_is_alt_tab_like() {
    let backend = backend();
    let windows = backend.windows().expect("windows");
    for w in &windows {
        assert!(!w.title.trim().is_empty(), "untitled window listed: {w:?}");
        assert!(w.id.parse::<isize>().is_ok(), "id should be a decimal HWND: {w:?}");
        assert!(w.minimized || !w.rect.is_empty(), "empty bounds on a non-minimised window: {w:?}");
    }
    assert!(windows.iter().filter(|w| w.focused).count() <= 1, "at most one focused window");
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn capture_region_inside_primary_monitor() {
    let backend = backend();
    let monitors = backend.monitors().expect("monitors");
    let m = primary(&monitors);
    let region = Rect::new(m.rect.x + 10, m.rect.y + 10, 100, 60);
    let frame = backend.capture_region(region, &CaptureOptions::default()).expect("region");
    assert_sane(&frame, "region");
    assert_eq!(frame.rect(), region);
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn capture_region_outside_every_monitor_is_an_error() {
    let backend = backend();
    let err = backend
        .capture_region(Rect::new(-1_000_000, -1_000_000, 10, 10), &CaptureOptions::default())
        .expect_err("no monitor there");
    assert!(matches!(err, ssx_capture::CaptureError::InvalidRegion(_)), "{err}");
}

#[test]
#[ignore = "needs an interactive Windows desktop"]
fn unknown_ids_are_not_found() {
    let backend = backend();
    let opts = CaptureOptions::default();
    assert!(matches!(
        backend.capture_monitor(r"\\.\DISPLAY999", &opts),
        Err(ssx_capture::CaptureError::NotFound(_))
    ));
    assert!(matches!(
        backend.capture_window("not-a-handle", &opts),
        Err(ssx_capture::CaptureError::NotFound(_))
    ));
    assert!(matches!(
        backend.capture_window("123456789", &opts),
        Err(ssx_capture::CaptureError::NotFound(_))
    ));
}

#[test]
fn capabilities_are_accurate() {
    // Needs no display: only constructs nothing and inspects static claims via a real
    // backend if one can be created; skips otherwise.
    let Ok(backend) = WindowsCapture::new() else {
        println!("SKIP: no interactive display in this session");
        return;
    };
    let caps = backend.capabilities();
    assert!(caps.enumerate_monitors && caps.enumerate_windows && caps.capture_windows);
    assert!(caps.cursor && caps.hdr_float);
    assert!(!caps.native_desktop && !caps.needs_user_interaction);
    assert_eq!(backend.name(), "windows-wgc");
}
