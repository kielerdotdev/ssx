//! Protocol tests against a scripted mock compositor: `ext-image-copy-capture-v1` (which
//! headless sway 1.9 lacks), `wl_shm` formats, `y_invert`, stride padding, renegotiation,
//! failures, hangs and disconnects. See `tests/common/mock.rs`.
//!
//! These run everywhere: they need no real compositor.
#![cfg(target_os = "linux")]

mod common;

use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use common::{
    first_pattern_mismatch,
    mock::{
        ARGB8888, ARGB2101010, BGR888, Behavior, MockCfg, MockOutput, MockServer, MockToplevel,
        XBGR8888, XBGR2101010, XRGB8888,
    },
    pattern,
};
use ssx_capture::{CaptureBackend, CaptureError, CaptureOptions};
use ssx_capture_wayland::{Config, Ipc, Protocol, Target, Transform, WaylandCapture};
use ssx_types::{Rect, Size};

const OPTS: CaptureOptions = CaptureOptions { include_cursor: false };

fn pixel(f: &ssx_types::Frame, x: u32, y: u32) -> [u8; 4] {
    let p = &f.row(y)[x as usize * 4..x as usize * 4 + 4];
    [p[0], p[1], p[2], p[3]]
}

fn bgra(seed: u8, x: u32, y: u32) -> [u8; 4] {
    let [r, g, b] = pattern(seed, x, y);
    [b, g, r, 255]
}

fn one_output() -> Vec<MockOutput> {
    vec![MockOutput::new("MOCK-1", 0, 0, 120, 80)]
}

#[test]
fn ext_protocol_geometry_pixels_regions_and_cursor() {
    let mock = MockServer::start(MockCfg::new(vec![
        MockOutput::new("MOCK-1", 0, 0, 320, 240),
        MockOutput::new("MOCK-2", 320, 0, 400, 300).scale(2),
    ]));
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    assert_eq!(cap.protocol(), Protocol::ExtImageCopyCapture);
    assert_eq!(cap.name(), "wayland-ext-image-copy-capture");
    let caps = cap.capabilities();
    assert!(caps.cursor && caps.enumerate_monitors);
    assert!(!caps.enumerate_windows, "IPC disabled -> no window support is advertised");
    assert!(matches!(cap.windows(), Err(CaptureError::Unsupported { .. })));
    assert!(matches!(
        cap.capture_window("x", &OPTS),
        Err(CaptureError::NotFound(_) | CaptureError::Unsupported { .. })
    ));

    let ms = cap.monitors().unwrap();
    let a = ms.iter().find(|m| m.id == "MOCK-1").unwrap();
    let b = ms.iter().find(|m| m.id == "MOCK-2").unwrap();
    // Mixed scales: S = 2.
    assert_eq!(a.rect, Rect::new(0, 0, 640, 480));
    assert_eq!(b.rect, Rect::new(640, 0, 400, 300));
    assert_eq!((a.scale_factor, b.scale_factor), (1.0, 2.0));
    assert_eq!(a.refresh_hz, Some(60.0));
    let info = cap.outputs().unwrap();
    assert_eq!(info[0].make, "MockMake");
    assert_eq!(info[0].description.as_deref(), Some("Mock MOCK-1"));

    let fb = cap.capture_monitor("MOCK-2", &OPTS).unwrap();
    assert_eq!(fb.rect(), b.rect);
    assert_eq!(first_pattern_mismatch(&fb, 2, (0, 0)), None);
    let fa = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
    assert_eq!(fa.size(), Size::new(640, 480));
    for (x, y) in [(0, 0), (1, 1), (639, 479), (301, 77)] {
        assert_eq!(pixel(&fa, x, y), bgra(1, x / 2, y / 2));
    }

    // Region across the seam: the protocol has no region request, so this is a full-output
    // capture per monitor, stitched and cropped.
    let r = cap.capture_region(Rect::new(630, 10, 20, 4), &OPTS).unwrap();
    assert_eq!(pixel(&r, 0, 0), bgra(1, 315, 5));
    assert_eq!(pixel(&r, 10, 0), bgra(2, 0, 10));
    assert_eq!(pixel(&r, 19, 3), bgra(2, 9, 13));

    let d = cap.capture_desktop(&OPTS).unwrap();
    assert_eq!(d.rect(), Rect::new(0, 0, 1040, 480));
    assert_eq!(pixel(&d, 700, 100), bgra(2, 60, 100));

    // Cursor option is forwarded as paint_cursors.
    assert_eq!(mock.logged("paint_cursors=true"), 0);
    cap.capture_monitor("MOCK-1", &CaptureOptions { include_cursor: true }).unwrap();
    assert_eq!(mock.logged("paint_cursors=true"), 1);
    assert!(mock.logged("paint_cursors=false") >= 4);

    // Unknown monitor.
    assert!(matches!(cap.capture_monitor("nope", &OPTS), Err(CaptureError::NotFound(_))));
}

#[test]
fn ext_shm_formats_convert_exactly() {
    for fmt in [ARGB8888, XRGB8888, XBGR8888, XBGR2101010, ARGB2101010, BGR888] {
        let mut cfg = MockCfg::new(one_output());
        cfg.formats = vec![fmt];
        let mock = MockServer::start(cfg);
        let cap = WaylandCapture::with_config(mock.config()).unwrap();
        let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
        assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None, "format {fmt:#x}");
    }
}

#[test]
fn ext_prefers_cheap_format_when_several_are_offered() {
    let mut cfg = MockCfg::new(one_output());
    // The mock encodes with the first entry; the client must pick XRGB8888 from the list.
    cfg.formats = vec![XRGB8888, XBGR2101010];
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None);
}

#[test]
fn ext_output_transforms_are_undone() {
    for t in [
        Transform::Rot90,
        Transform::Rot180,
        Transform::Rot270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ] {
        let mock = MockServer::start(MockCfg::new(vec![
            MockOutput::new("MOCK-1", 0, 0, 120, 80).transform(t),
        ]));
        let cap = WaylandCapture::with_config(mock.config()).unwrap();
        let (w, h) = t.upright_size(120, 80);
        let m = &cap.monitors().unwrap()[0];
        assert_eq!(m.rect, Rect::new(0, 0, w, h), "{t:?}");
        let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
        assert_eq!(f.size(), Size::new(w, h), "{t:?}");
        assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None, "{t:?}");
    }
}

#[test]
fn ext_renegotiates_format_when_constraints_change() {
    for behavior in [Behavior::RenegotiateConstraintsFirst, Behavior::RenegotiateFailedFirst] {
        let mut cfg = MockCfg::new(one_output());
        cfg.behavior = behavior;
        cfg.renegotiate = (XBGR2101010, (0, 0)); // same size, different format
        let mock = MockServer::start(cfg);
        let cap = WaylandCapture::with_config(mock.config()).unwrap();
        let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
        assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None, "{behavior:?}");
        assert_eq!(mock.logged("capture #2"), 1, "{behavior:?}: exactly one retry");
    }
}

#[test]
fn ext_renegotiated_resolution_is_resampled_to_the_monitor_rect() {
    let mut cfg = MockCfg::new(one_output());
    cfg.behavior = Behavior::RenegotiateConstraintsFirst;
    cfg.renegotiate = (XRGB8888, (20, 10));
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
    // The output changed size under us; the frame still covers the rect from monitors().
    assert_eq!(f.size(), Size::new(120, 80));
}

#[test]
fn ext_unknown_failure_is_retried() {
    let mut cfg = MockCfg::new(one_output());
    cfg.behavior = Behavior::UnknownFailOnce;
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None);
    assert_eq!(mock.logged("capture #2"), 1);
}

#[test]
fn ext_stopped_session_is_an_error() {
    let mut cfg = MockCfg::new(one_output());
    cfg.behavior = Behavior::Stop;
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let err = cap.capture_monitor("MOCK-1", &OPTS).unwrap_err();
    assert!(err.to_string().contains("stopped"), "{err}");
}

#[test]
fn ext_hung_compositor_times_out() {
    let mut cfg = MockCfg::new(one_output());
    cfg.behavior = Behavior::Hang;
    let mock = MockServer::start(cfg);
    let mut c = mock.config();
    c.timeout = Duration::from_millis(400);
    let cap = WaylandCapture::with_config(c).unwrap();
    let started = Instant::now();
    let err = cap.capture_monitor("MOCK-1", &OPTS).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert!(err.to_string().contains("timed out"), "{err}");
}

#[test]
fn ext_compositor_disconnect_mid_capture() {
    let mut cfg = MockCfg::new(one_output());
    cfg.behavior = Behavior::Disconnect;
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let started = Instant::now();
    let err = cap.capture_monitor("MOCK-1", &OPTS).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    let msg = err.to_string();
    assert!(msg.contains("connection") || msg.contains("compositor"), "{msg}");
}

#[test]
fn wlr_y_invert_stride_padding_and_10_bit() {
    for version in [3, 1] {
        let mut cfg = MockCfg::new(one_output());
        cfg.ext = false;
        cfg.wlr = true;
        cfg.wlr_version = version;
        cfg.y_invert = true;
        cfg.stride_pad = 12;
        cfg.formats = vec![XBGR2101010];
        let mock = MockServer::start(cfg);
        let cap = WaylandCapture::with_config(mock.config()).unwrap();
        assert_eq!(cap.protocol(), Protocol::WlrScreencopy);
        let f = cap.capture_monitor("MOCK-1", &CaptureOptions { include_cursor: true }).unwrap();
        assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None, "wlr v{version}");
        assert_eq!(mock.logged("overlay=1"), 1, "overlay_cursor forwarded");
    }
}

#[test]
fn wlr_region_uses_logical_coordinates() {
    let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 200, 120).scale(2)]);
    cfg.ext = false;
    cfg.wlr = true;
    cfg.stride_pad = 8;
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    // Desktop px (5,7)..(12,10) at scale 2 -> logical (2,3)..(6,5).
    let f = cap.capture_region(Rect::new(5, 7, 7, 3), &OPTS).unwrap();
    assert_eq!(f.rect(), Rect::new(5, 7, 7, 3));
    assert_eq!(first_pattern_mismatch(&f, 1, (5, 7)), None);
    assert_eq!(mock.logged("region=Some((2, 3, 4, 2))"), 1, "{:?}", mock.log.lock().unwrap());
}

#[test]
fn wlr_failed_frame_is_retried_once() {
    let mut cfg = MockCfg::new(one_output());
    cfg.ext = false;
    cfg.wlr = true;
    cfg.behavior = Behavior::WlrFailOnce;
    let mock = MockServer::start(cfg);
    let cap = WaylandCapture::with_config(mock.config()).unwrap();
    let f = cap.capture_monitor("MOCK-1", &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None);
    assert_eq!(mock.logged("wlr capture"), 2);
}

#[test]
fn protocol_selection_and_no_backend_errors() {
    // Both advertised: ext wins, but wlr can be forced.
    let mut cfg = MockCfg::new(one_output());
    cfg.wlr = true;
    let mock = MockServer::start(cfg);
    assert_eq!(
        WaylandCapture::with_config(mock.config()).unwrap().protocol(),
        Protocol::ExtImageCopyCapture
    );
    let mut c = mock.config();
    c.protocol = Some(Protocol::WlrScreencopy);
    assert_eq!(WaylandCapture::with_config(c).unwrap().protocol(), Protocol::WlrScreencopy);

    // Neither advertised (GNOME/KDE-like): actionable NoBackend error.
    let mut cfg = MockCfg::new(one_output());
    cfg.ext = false;
    let mock = MockServer::start(cfg);
    let err = WaylandCapture::with_config(mock.config()).unwrap_err();
    assert!(matches!(err, CaptureError::NoBackend(_)), "{err:?}");
    assert!(err.to_string().contains("portal"), "{err}");

    // Forcing a protocol the compositor lacks.
    let mut c = mock.config();
    c.protocol = Some(Protocol::WlrScreencopy);
    assert!(matches!(WaylandCapture::with_config(c), Err(CaptureError::NoBackend(_))));

    // No compositor at all.
    let missing = Config {
        target: Target::Path(PathBuf::from("/nonexistent/ssx-wayland-socket")),
        ..Config::default()
    };
    let err = WaylandCapture::with_config(missing).unwrap_err();
    assert!(matches!(err, CaptureError::NoBackend(_)), "{err:?}");
}

#[test]
fn unresponsive_socket_times_out_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent");
    let listener = UnixListener::bind(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let t = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let mut held = Vec::new();
        while !stop2.load(Ordering::SeqCst) {
            if let Ok((s, _)) = listener.accept() {
                held.push(s); // accept and stay silent
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let cfg = Config {
        target: Target::Path(path),
        timeout: Duration::from_millis(300),
        ..Config::default()
    };
    let started = Instant::now();
    let err = WaylandCapture::with_config(cfg).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert!(err.to_string().contains("timed out"), "{err}");
    stop.store(true, Ordering::SeqCst);
    t.join().unwrap();
}

// ---- window capture through ext toplevel sources, with a fake sway IPC ------------------------

/// A minimal i3-ipc server answering every request with `tree`.
struct FakeSwayIpc {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl FakeSwayIpc {
    fn start(windows: &[(i64, &str, &str, Rect)]) -> Self {
        let leaves: Vec<String> = windows
            .iter()
            .map(|(id, title, app_id, r)| {
                format!(
                    r#"{{"id":{id},"type":"floating_con","name":"{title}","app_id":"{app_id}","pid":{id},
                    "visible":true,"focused":false,"shell":"xdg_shell",
                    "rect":{{"x":{},"y":{},"width":{},"height":{}}},
                    "window_rect":{{"x":0,"y":0,"width":{},"height":{}}},
                    "nodes":[],"floating_nodes":[]}}"#,
                    r.x, r.y, r.width, r.height, r.width, r.height
                )
            })
            .collect();
        let tree = format!(
            r#"{{"id":1,"type":"root","name":"root","nodes":[{{"id":3,"type":"output","name":"MOCK-1",
            "nodes":[{{"id":4,"type":"workspace","name":"1","visible":true,"nodes":[],
            "floating_nodes":[{}]}}],"floating_nodes":[]}}],"floating_nodes":[]}}"#,
            leaves.join(",")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sway-ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                if let Ok((mut s, _)) = listener.accept() {
                    s.set_nonblocking(false).unwrap();
                    let mut h = [0u8; 14];
                    if s.read_exact(&mut h).is_ok() {
                        let len = u32::from_ne_bytes([h[6], h[7], h[8], h[9]]) as usize;
                        let mut body = vec![0u8; len];
                        let _ = s.read_exact(&mut body);
                        let mut reply = b"i3-ipc".to_vec();
                        reply.extend_from_slice(&(tree.len() as u32).to_ne_bytes());
                        reply.extend_from_slice(&4u32.to_ne_bytes());
                        reply.extend_from_slice(tree.as_bytes());
                        let _ = s.write_all(&reply);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self { path, stop, thread: Some(thread), _dir: dir }
    }
}

impl Drop for FakeSwayIpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn window_capture_uses_the_toplevel_source_when_unambiguous() {
    let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 640, 480)]);
    cfg.toplevels = vec![
        MockToplevel { title: "Alpha", app_id: "app.a", w: 200, h: 100, seed: 30 },
        MockToplevel { title: "Beta", app_id: "app.b", w: 120, h: 90, seed: 31 },
        MockToplevel { title: "Dup", app_id: "app.d", w: 50, h: 50, seed: 32 },
        MockToplevel { title: "Dup", app_id: "app.d", w: 50, h: 50, seed: 33 },
    ];
    let mock = MockServer::start(cfg);
    let ipc = FakeSwayIpc::start(&[
        (11, "Alpha", "app.a", Rect::new(100, 50, 210, 110)),
        (12, "Beta", "app.b", Rect::new(300, 200, 120, 90)),
        (13, "Dup", "app.d", Rect::new(10, 10, 60, 60)),
        (14, "NoToplevel", "app.z", Rect::new(400, 300, 80, 60)),
    ]);
    let mut c = mock.config();
    c.ipc = Ipc::Sway(ipc.path.clone());
    let cap = WaylandCapture::with_config(c.clone()).unwrap();
    assert!(cap.capabilities().capture_windows);
    let wins = cap.windows().unwrap();
    assert_eq!(wins.len(), 4);

    // Unambiguous match: the window's own pixels (seed 30), at its own size, placed at the
    // IPC rect's origin. No occlusion, no decorations.
    let f = cap.capture_window("sway:11", &OPTS).unwrap();
    assert_eq!(f.size(), Size::new(200, 100));
    assert_eq!((f.origin.x, f.origin.y), (100, 50));
    assert_eq!(first_pattern_mismatch(&f, 30, (0, 0)), None);
    let f = cap.capture_window("sway:12", &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&f, 31, (0, 0)), None);

    // Two toplevels with the same title and app_id: ambiguous, so fall back to cropping
    // the screen (seed 1 is the output's pattern).
    let f = cap.capture_window("sway:13", &OPTS).unwrap();
    assert_eq!(f.rect(), Rect::new(10, 10, 60, 60));
    assert_eq!(first_pattern_mismatch(&f, 1, (10, 10)), None);

    // No matching toplevel at all: same fallback.
    let f = cap.capture_window("sway:14", &OPTS).unwrap();
    assert_eq!(first_pattern_mismatch(&f, 1, (400, 300)), None);

    // Feature switch off: always crop.
    c.toplevel_capture = false;
    let cap = WaylandCapture::with_config(c).unwrap();
    let f = cap.capture_window("sway:11", &OPTS).unwrap();
    assert_eq!(f.rect(), Rect::new(100, 50, 210, 110));
    assert_eq!(first_pattern_mismatch(&f, 1, (100, 50)), None);
}
