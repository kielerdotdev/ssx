//! End-to-end tests against a real `Xvfb`: exact pixels for root, region, window, cursor
//! and other depths, plus monitors, window listing and reconnecting. Every test starts its
//! own server and skips (printing why) when `Xvfb` is unavailable.
#![cfg(all(unix, not(target_vendor = "apple")))]
#![allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable

mod common;

use common::{Client, ROOT_BG, Xvfb, assert_pixels, expected_scene, pixels, px, xvfb};
use ssx_capture::{CaptureBackend, CaptureError, CaptureOptions};
use ssx_capture_x11::{WindowCaptureMode, X11Capture, X11Config};
use ssx_types::{Point, Rect};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, Window},
    wrapper::ConnectionExt as _,
};

const NO_CURSOR: CaptureOptions = CaptureOptions { include_cursor: false };

fn capture(x: &Xvfb, tweak: impl FnOnce(&mut X11Config)) -> X11Capture {
    let mut cfg = X11Config::for_display(&x.display);
    tweak(&mut cfg);
    X11Capture::with_config(cfg).expect("connect X11Capture")
}

/// The standard scene: three coloured windows on the coloured root.
fn scene(c: &Client) -> Vec<(i32, i32, u32, u32, u32)> {
    c.window(10, 20, 100, 80, 0xff_00_00);
    c.window(300, 300, 50, 60, 0x00_ff_00);
    c.window(790, 590, 10, 10, 0x00_00_ff); // flush with the bottom-right corner
    vec![
        (10, 20, 100, 80, 0xff_00_00),
        (300, 300, 50, 60, 0x00_ff_00),
        (790, 590, 10, 10, 0x00_00_ff),
    ]
}

#[test]
fn desktop_capture_has_exact_pixels_with_shm_and_without() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    let expected = expected_scene(800, 600, (0, 0), ROOT_BG, &rects);

    let shm = capture(&x, |_| {});
    assert!(shm.features().unwrap().shm, "Xvfb supports MIT-SHM fd passing; test would be vacuous");
    let f = shm.capture_desktop(&NO_CURSOR).unwrap();
    assert_eq!((f.width(), f.height(), f.origin), (800, 600, Point::new(0, 0)));
    assert_pixels(&f, &expected);

    let plain = capture(&x, |c| c.use_shm = false);
    assert!(!plain.features().unwrap().shm);
    assert_pixels(&plain.capture_desktop(&NO_CURSOR).unwrap(), &expected);
}

#[test]
fn capabilities_and_name() {
    let Some(x) = xvfb() else { return };
    let cap = capture(&x, |_| {});
    let caps = cap.capabilities();
    assert!(caps.native_desktop && caps.enumerate_monitors && caps.enumerate_windows);
    assert!(caps.capture_windows && caps.cursor && !caps.hdr_float && !caps.needs_user_interaction);
    assert_eq!(cap.name(), "x11");
}

#[test]
fn regions_are_exact_and_clipped_to_the_screen() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    for use_shm in [true, false] {
        let cap = capture(&x, |c| c.use_shm = use_shm);
        // Interior region crossing the red window's corner.
        let r = Rect::new(5, 15, 120, 100);
        let f = cap.capture_region(r, &NO_CURSOR).unwrap();
        assert_eq!(f.rect(), r);
        assert_pixels(&f, &expected_scene(120, 100, (5, 15), ROOT_BG, &rects));

        // 1x1 regions, including the last pixel of the screen.
        let f = cap.capture_region(Rect::new(799, 599, 1, 1), &NO_CURSOR).unwrap();
        assert_eq!(px(&f, 0, 0), [255, 0, 0, 255], "blue window pixel as BGRA");
        let f = cap.capture_region(Rect::new(0, 0, 1, 1), &NO_CURSOR).unwrap();
        assert_eq!(px(&f, 0, 0), [0x30, 0x20, 0x10, 255]);

        // Partly off-screen on every side: clipped, origin moves with the clip.
        let f = cap.capture_region(Rect::new(-10, -10, 50, 50), &NO_CURSOR).unwrap();
        assert_eq!(f.rect(), Rect::new(0, 0, 40, 40));
        let f = cap.capture_region(Rect::new(780, 580, 100, 100), &NO_CURSOR).unwrap();
        assert_eq!(f.rect(), Rect::new(780, 580, 20, 20));
        assert_pixels(&f, &expected_scene(20, 20, (780, 580), ROOT_BG, &rects));
        let f = cap.capture_region(Rect::new(-5000, -5000, 20000, 20000), &NO_CURSOR).unwrap();
        assert_eq!((f.width(), f.height()), (800, 600));

        // Nothing to capture.
        for bad in [
            Rect::new(0, 0, 0, 10),
            Rect::new(0, 0, 10, 0),
            Rect::new(800, 0, 10, 10),
            Rect::new(-10, 0, 10, 10),
            Rect::new(0, 600, 10, 10),
        ] {
            assert!(
                matches!(cap.capture_region(bad, &NO_CURSOR), Err(CaptureError::InvalidRegion(r)) if r == bad),
                "{bad:?}"
            );
        }
    }
}

#[test]
fn banded_capture_matches_unbanded_on_both_paths() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    let expected = expected_scene(800, 600, (0, 0), ROOT_BG, &rects);
    for use_shm in [true, false] {
        // 4 KiB = 1 row of 800 px * 4 bytes per band, the worst case for chunking.
        for chunk in [1, 3200, 4096, 100_000] {
            let cap = capture(&x, |c| {
                c.use_shm = use_shm;
                c.max_chunk_bytes = Some(chunk);
            });
            assert_pixels(&cap.capture_desktop(&NO_CURSOR).unwrap(), &expected);
            let r = Rect::new(7, 13, 333, 101);
            assert_pixels(
                &cap.capture_region(r, &NO_CURSOR).unwrap(),
                &expected_scene(333, 101, (7, 13), ROOT_BG, &rects),
            );
        }
    }
}

#[test]
fn large_desktop_crosses_default_band_boundaries() {
    let Some(x) = Xvfb::start("4096x2048x24", &[]) else { return };
    let c = Client::new(&x.display);
    // 16 KiB per row: plain bands are 256 rows (4 MiB), SHM bands 1024 rows (16 MiB).
    let stripes: Vec<(i32, i32, u32, u32, u32)> = [0, 255, 256, 257, 1023, 1024, 1025, 2047]
        .iter()
        .enumerate()
        .map(|(i, &y)| (i32::try_from(i).unwrap() * 100, y, 90, 1, 0x01_00_00 * (i as u32 + 1)))
        .collect();
    for &(sx, sy, w, h, rgb) in &stripes {
        c.window(sx as i16, sy as i16, w as u16, h as u16, rgb);
    }
    let expected = expected_scene(4096, 2048, (0, 0), ROOT_BG, &stripes);
    for use_shm in [true, false] {
        let cap = capture(&x, |c| c.use_shm = use_shm);
        let f = cap.capture_desktop(&NO_CURSOR).unwrap();
        assert_eq!((f.width(), f.height()), (4096, 2048));
        assert_pixels(&f, &expected);
    }
}

fn depth_case(screen: &str, extra: &[&str], tolerance_note: &str) {
    let Some(x) = Xvfb::start(screen, extra) else { return };
    let c = Client::new(&x.display);
    let shift = |v: u32, mask: u32| (v << mask.trailing_zeros()) & mask;
    let vis = &c.conn.setup().roots[0]
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == c.root_visual)
        .copied()
        .expect("root visual");
    let bits = |mask: u32| (mask >> mask.trailing_zeros()).count_ones();
    let pixel = |r: bool, g: bool, b: bool| {
        let full = |mask: u32| (1u32 << bits(mask)) - 1;
        shift(if r { full(vis.red_mask) } else { 0 }, vis.red_mask)
            | shift(if g { full(vis.green_mask) } else { 0 }, vis.green_mask)
            | shift(if b { full(vis.blue_mask) } else { 0 }, vis.blue_mask)
    };
    // Components are 0 or full scale, which every depth represents exactly.
    let white = pixel(true, true, true);
    // Depth 32: put garbage in the alpha bits; the capture must still be opaque.
    let alpha_junk = if c.depth == 32 { 0x40 << 24 } else { 0 };
    c.window_pixel(50, 50, 100, 100, pixel(true, false, false) | alpha_junk, false);
    c.window_pixel(200, 50, 100, 100, pixel(false, true, false) | alpha_junk, false);
    c.window_pixel(350, 50, 100, 100, white | alpha_junk, false);
    c.window_pixel(500, 50, 100, 100, pixel(false, false, true) | alpha_junk, false);
    c.window_pixel(50, 200, 100, 100, pixel(true, true, false) | alpha_junk, false);

    for use_shm in [true, false] {
        let cap = capture(&x, |c| c.use_shm = use_shm);
        let f = cap.capture_desktop(&NO_CURSOR).unwrap_or_else(|e| panic!("{screen}: {e}"));
        assert_eq!(px(&f, 100, 100), [0, 0, 255, 255], "red, {screen} {tolerance_note}");
        assert_eq!(px(&f, 250, 100), [0, 255, 0, 255], "green, {screen}");
        assert_eq!(px(&f, 400, 100), [255, 255, 255, 255], "white, {screen}");
        assert_eq!(px(&f, 550, 100), [255, 0, 0, 255], "blue, {screen}");
        assert_eq!(px(&f, 100, 250), [0, 255, 255, 255], "yellow, {screen}");
        assert!(pixels(&f).chunks_exact(4).all(|p| p[3] == 255), "alpha always opaque");
    }
}

#[test]
fn depth_24() {
    depth_case("640x480x24", &[], "");
}

#[test]
fn depth_16_rgb565() {
    depth_case("640x480x16", &[], "packed 565");
}

#[test]
fn depth_15_rgb555() {
    depth_case("640x480x15", &[], "packed 555");
}

#[test]
fn depth_30_ten_bit_channels() {
    depth_case("640x480x30", &[], "10 bpc");
}

#[test]
fn no_shm_extension_falls_back_transparently() {
    let Some(x) = Xvfb::start("800x600x24", &["-extension", "MIT-SHM"]) else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    let cap = capture(&x, |_| {});
    assert!(!cap.features().unwrap().shm);
    assert_pixels(
        &cap.capture_desktop(&NO_CURSOR).unwrap(),
        &expected_scene(800, 600, (0, 0), ROOT_BG, &rects),
    );
}

#[test]
fn scale_factor_follows_xft_dpi() {
    let Some(x) = xvfb() else { return };
    let cap = capture(&x, |_| {});
    let c = Client::new(&x.display);
    assert_eq!(cap.capture_desktop(&NO_CURSOR).unwrap().scale_factor, 1.0);
    let rm = c.atom("RESOURCE_MANAGER");
    for (dpi, scale) in [(192, 2.0), (144, 1.5), (96, 1.0)] {
        let text = format!("Xft.antialias:\t1\nXft.dpi:\t{dpi}\n");
        c.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                c.root,
                rm,
                x11rb::protocol::xproto::AtomEnum::STRING,
                text.as_bytes(),
            )
            .unwrap();
        c.sync();
        let f = cap.capture_desktop(&NO_CURSOR).unwrap();
        assert_eq!(f.scale_factor, scale);
        assert_eq!(cap.monitors().unwrap()[0].scale_factor, scale);
    }
}

#[test]
fn default_server_reports_one_full_screen_monitor() {
    let Some(x) = xvfb() else { return };
    let cap = capture(&x, |_| {});
    let mons = cap.monitors().unwrap();
    assert!(!mons.is_empty());
    assert!(mons.iter().any(|m| m.primary), "exactly one is flagged primary");
    let total = ssx_types::Rect::bounding(mons.iter().map(|m| m.rect)).unwrap();
    assert_eq!(total, Rect::new(0, 0, 800, 600));
    let f = cap.capture_monitor(&mons[0].id, &NO_CURSOR).unwrap();
    assert_eq!(f.rect(), mons[0].rect);
    assert!(matches!(
        cap.capture_monitor("no-such-monitor", &NO_CURSOR),
        Err(CaptureError::NotFound(id)) if id == "no-such-monitor"
    ));
}

#[test]
fn without_randr_a_single_root_monitor_is_synthesised() {
    let Some(x) = Xvfb::start("800x600x24", &["-extension", "RANDR"]) else { return };
    let cap = capture(&x, |_| {});
    assert_eq!(cap.features().unwrap().randr, None);
    let mons = cap.monitors().unwrap();
    assert_eq!(mons.len(), 1);
    assert_eq!(mons[0].rect, Rect::new(0, 0, 800, 600));
    assert!(mons[0].primary);
    assert_eq!(cap.capture_monitor(&mons[0].id, &NO_CURSOR).unwrap().width(), 800);
}

#[test]
fn user_defined_monitors_are_listed_and_capturable() {
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    let define = |name: &str, x0: i16, w: u16, primary: bool| {
        let atom = c.atom(name);
        c.conn
            .randr_set_monitor(
                c.root,
                randr::MonitorInfo {
                    name: atom,
                    primary,
                    automatic: false,
                    x: x0,
                    y: 0,
                    width: w,
                    height: 600,
                    width_in_millimeters: 300,
                    height_in_millimeters: 400,
                    outputs: vec![],
                },
            )
            .unwrap()
            .check()
            .unwrap();
    };
    define("LEFT", 0, 400, false);
    define("RIGHT", 400, 400, true);
    let cap = capture(&x, |_| {});
    let mons = cap.monitors().unwrap();
    let left = mons.iter().find(|m| m.id == "LEFT").expect("LEFT listed");
    let right = mons.iter().find(|m| m.id == "RIGHT").expect("RIGHT listed");
    assert_eq!(left.rect, Rect::new(0, 0, 400, 600));
    assert_eq!(right.rect, Rect::new(400, 0, 400, 600));
    assert!(right.primary && !left.primary);
    let details = cap.monitor_details().unwrap();
    let d = details.iter().find(|m| m.monitor.id == "RIGHT").unwrap();
    assert_eq!(d.size_mm, Some((300, 400)));

    let f = cap.capture_monitor("LEFT", &NO_CURSOR).unwrap();
    assert_eq!(f.rect(), left.rect);
    assert_pixels(&f, &expected_scene(400, 600, (0, 0), ROOT_BG, &rects));
    let f = cap.capture_monitor("RIGHT", &NO_CURSOR).unwrap();
    assert_eq!(f.origin, Point::new(400, 0));
    assert_pixels(&f, &expected_scene(400, 600, (400, 0), ROOT_BG, &rects));
}

#[test]
fn connecting_to_nothing_is_a_clean_error() {
    let err = X11Capture::with_config(X11Config::for_display(":9999")).unwrap_err();
    assert!(matches!(err, CaptureError::NoBackend(_)), "{err}");
}

#[test]
fn survives_server_restart() {
    let Some(mut x) = xvfb() else { return };
    let display = x.display.clone();
    let cap = capture(&x, |_| {});
    assert!(cap.capture_desktop(&NO_CURSOR).is_ok());

    x.kill();
    // Server gone: an error, promptly, not a hang or panic.
    let err = cap.capture_desktop(&NO_CURSOR).unwrap_err();
    assert!(matches!(err, CaptureError::Backend { .. } | CaptureError::NoBackend(_)), "{err}");
    assert!(cap.windows().is_err());
    assert!(cap.monitors().is_err());

    // Same display comes back: the backend reconnects by itself.
    let Some(_x2) = Xvfb::start_on(Some(&display), "640x480x24", &[]) else { return };
    let f = cap.capture_desktop(&NO_CURSOR).expect("reconnect after restart");
    assert_eq!((f.width(), f.height()), (640, 480));
}

#[test]
fn concurrent_captures_are_safe() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let rects = scene(&c);
    let expected = expected_scene(800, 600, (0, 0), ROOT_BG, &rects);
    let cap = std::sync::Arc::new(capture(&x, |_| {}));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let cap = cap.clone();
            std::thread::spawn(move || {
                (0..5)
                    .map(|_| pixels(&cap.capture_desktop(&NO_CURSOR).unwrap()))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for h in handles {
        for got in h.join().unwrap() {
            assert!(got == expected, "concurrent capture differs");
        }
    }
}

// ---- windows -------------------------------------------------------------------------

fn no_wm_ids(cap: &X11Capture) -> Vec<String> {
    cap.windows().unwrap().into_iter().map(|w| w.id).collect()
}

#[test]
fn window_listing_without_ewmh_falls_back_to_mapped_children() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let a = c.window(0, 0, 10, 10, 0xff_00_00);
    let b = c.window(20, 0, 10, 10, 0x00_ff_00);
    let _unmapped = c.window_pixel(40, 0, 10, 10, 0, true);
    let cap = capture(&x, |_| {});
    // Front-to-back: last mapped is on top.
    assert_eq!(no_wm_ids(&cap), vec![format!("{b:#x}"), format!("{a:#x}")]);
}

#[test]
fn window_listing_uses_ewmh_properties() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let a = c.window(100, 100, 200, 150, 0xff_00_00);
    let b = c.window(50, 60, 100, 100, 0x00_ff_00);
    let d = c.window(-30, -20, 100, 100, 0x00_00_ff);
    c.set_utf8_title(a, "héllo — wörld 日本");
    c.set_class(a, "xterm", "XTerm");
    c.set_cardinals(a, "_NET_FRAME_EXTENTS", &[1, 2, 30, 4]);
    c.set_wm_name(b, b"caf\xe9 latin1");
    c.set_class(b, "solo", "");
    c.set_atoms_prop(b, "_NET_WM_STATE", &["_NET_WM_STATE_MAXIMIZED_VERT", "_NET_WM_STATE_HIDDEN"]);
    // bottom-to-top per EWMH
    c.set_windows_prop(c.root, "_NET_CLIENT_LIST_STACKING", &[a, b, d]);
    c.set_windows_prop(c.root, "_NET_ACTIVE_WINDOW", &[d]);
    c.sync();

    let cap = capture(&x, |_| {});
    let list = cap.windows().unwrap();
    assert_eq!(
        list.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
        [d, b, a].map(|w| format!("{w:#x}"))
    );

    let (wd, wb, wa) = (&list[0], &list[1], &list[2]);
    assert!(wd.focused && !wb.focused && !wa.focused);
    assert_eq!(wd.rect, Rect::new(-30, -20, 100, 100), "negative origin preserved");
    assert_eq!(wd.title, "");
    assert_eq!(wd.app_name, None);

    assert_eq!(wb.title, "café latin1", "WM_NAME is Latin-1");
    assert!(wb.minimized && !wa.minimized);
    assert_eq!(wb.app_name.as_deref(), Some("solo"), "falls back to the instance name");

    assert_eq!(wa.title, "héllo — wörld 日本");
    assert_eq!(wa.app_name.as_deref(), Some("XTerm"));
    // Client at (100,100) 200x150 plus extents l=1 r=2 t=30 b=4.
    assert_eq!(wa.rect, Rect::new(99, 70, 203, 184));
}

#[test]
fn stale_ids_in_client_list_are_skipped() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let a = c.window(0, 0, 10, 10, 0xff_00_00);
    c.set_windows_prop(c.root, "_NET_CLIENT_LIST_STACKING", &[0x000d_ead0, a]);
    c.sync();
    let cap = capture(&x, |_| {});
    assert_eq!(no_wm_ids(&cap), vec![format!("{a:#x}")]);
}

#[test]
fn window_capture_crops_from_root_and_clips_at_edges() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let a = c.window(50, 60, 100, 80, 0xff_00_00);
    let cap = capture(&x, |_| {});
    let id = format!("{a:#x}");
    for use_shm in [true, false] {
        let cap2 = capture(&x, |c| c.use_shm = use_shm);
        let f = cap2.capture_window(&id, &NO_CURSOR).unwrap();
        assert_eq!(f.rect(), Rect::new(50, 60, 100, 80));
        assert_pixels(&f, &expected_scene(100, 80, (50, 60), 0xff_00_00, &[]));
    }
    // Decimal ids work too.
    assert!(cap.capture_window(&a.to_string(), &NO_CURSOR).is_ok());

    // Occlusion caveat (no compositor): the covering window is captured.
    let _b = c.window(100, 100, 100, 100, 0x00_00_ff);
    let f = cap.capture_window(&id, &NO_CURSOR).unwrap();
    assert_pixels(
        &f,
        &expected_scene(100, 80, (50, 60), 0xff_00_00, &[(100, 100, 100, 100, 0x00_00_ff)]),
    );

    // Partially off-screen (BadMatch if requested naively): clipped to the root.
    c.move_window(a, -20, -10);
    let f = cap.capture_window(&id, &NO_CURSOR).unwrap();
    assert_eq!(f.rect(), Rect::new(0, 0, 80, 70));
    assert_pixels(&f, &expected_scene(80, 70, (0, 0), 0xff_00_00, &[]));
    c.move_window(a, 760, 570);
    let f = cap.capture_window(&id, &NO_CURSOR).unwrap();
    assert_eq!(f.rect(), Rect::new(760, 570, 40, 30));

    // Entirely off-screen: nothing to capture.
    c.move_window(a, 2000, 2000);
    assert!(cap.capture_window(&id, &NO_CURSOR).is_err());
}

#[test]
fn window_capture_error_cases() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let cap = capture(&x, |_| {});
    let gone: Window = c.window(0, 0, 10, 10, 0xff_00_00);
    let unmapped = c.window_pixel(0, 0, 10, 10, 0, true);
    let hidden = c.window(20, 0, 10, 10, 0);
    c.set_atoms_prop(hidden, "_NET_WM_STATE", &["_NET_WM_STATE_HIDDEN"]);
    c.conn.destroy_window(gone).unwrap();
    c.sync();

    assert!(matches!(
        cap.capture_window(&format!("{gone:#x}"), &NO_CURSOR),
        Err(CaptureError::NotFound(_))
    ));
    assert!(matches!(cap.capture_window("banana", &NO_CURSOR), Err(CaptureError::NotFound(_))));
    assert!(matches!(cap.capture_window("0", &NO_CURSOR), Err(CaptureError::NotFound(_))));
    for w in [unmapped, hidden] {
        let e = cap.capture_window(&format!("{w:#x}"), &NO_CURSOR).unwrap_err();
        assert!(e.to_string().contains("not viewable"), "{e}");
    }
    // The backend is still healthy afterwards.
    assert!(cap.capture_desktop(&NO_CURSOR).is_ok());
}

// ---- compositing -----------------------------------------------------------------------

/// Turns the test client into a (very) minimal compositing manager: redirects every
/// top-level window off-screen and claims `_NET_WM_CM_S0`.
fn become_compositor(c: &Client) -> Window {
    use x11rb::protocol::composite::{self, ConnectionExt as _};
    let owner = c.window_pixel(-10, -10, 1, 1, 0, true);
    let sel = c.atom("_NET_WM_CM_S0");
    c.conn.set_selection_owner(owner, sel, x11rb::CURRENT_TIME).unwrap();
    c.conn.composite_redirect_subwindows(c.root, composite::Redirect::MANUAL).unwrap();
    c.sync();
    owner
}

#[test]
fn composite_capture_ignores_occlusion() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let _cm = become_compositor(&c);
    let a = c.window(50, 60, 100, 80, 0xff_00_00);
    let b = c.window(80, 80, 200, 200, 0x00_00_ff); // covers part of `a`
    c.set_windows_prop(c.root, "_NET_CLIENT_LIST_STACKING", &[a, b]);
    c.sync();

    for use_shm in [true, false] {
        let cap = capture(&x, |c| c.use_shm = use_shm);
        assert!(cap.features().unwrap().compositor_running);
        let f = cap.capture_window(&format!("{a:#x}"), &NO_CURSOR).unwrap();
        assert_eq!(f.rect(), Rect::new(50, 60, 100, 80));
        assert_pixels(&f, &expected_scene(100, 80, (50, 60), 0xff_00_00, &[]));
    }
}

#[test]
fn composite_capture_reads_offscreen_windows_in_full() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let _cm = become_compositor(&c);
    let a = c.window(-40, -30, 100, 100, 0x00_ff_00);
    let cap = capture(&x, |_| {});
    let f = cap.capture_window(&format!("{a:#x}"), &NO_CURSOR).unwrap();
    // Not clipped: the whole window, at its (negative) position.
    assert_eq!(f.rect(), Rect::new(-40, -30, 100, 100));
    assert_pixels(&f, &expected_scene(100, 100, (-40, -30), 0x00_ff_00, &[]));
}

#[test]
fn composite_mode_requires_a_redirected_window_but_auto_falls_back() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let a = c.window(10, 10, 50, 50, 0xff_00_00);
    let id = format!("{a:#x}");
    // No compositor: NameWindowPixmap on an unredirected window is a BadMatch.
    let strict = capture(&x, |c| c.window_capture = WindowCaptureMode::Composite);
    assert!(strict.capture_window(&id, &NO_CURSOR).is_err());
    let auto = capture(&x, |_| {});
    assert!(!auto.features().unwrap().compositor_running);
    assert_pixels(
        &auto.capture_window(&id, &NO_CURSOR).unwrap(),
        &expected_scene(50, 50, (10, 10), 0xff_00_00, &[]),
    );
}

#[test]
fn without_composite_extension_auto_uses_root() {
    let Some(x) = Xvfb::start("800x600x24", &["-extension", "Composite"]) else { return };
    let c = Client::new(&x.display);
    let a = c.window(10, 10, 50, 50, 0xff_00_00);
    let cap = capture(&x, |_| {});
    assert!(!cap.features().unwrap().composite);
    assert!(cap.capture_window(&format!("{a:#x}"), &NO_CURSOR).is_ok());
    let strict = capture(&x, |c| c.window_capture = WindowCaptureMode::Composite);
    assert!(strict.capture_window(&format!("{a:#x}"), &NO_CURSOR).is_err());
}

// ---- cursor ----------------------------------------------------------------------------

/// Installs a 4x4 ARGB root cursor with hotspot (1,1) and returns its premultiplied pixels.
fn install_argb_cursor(c: &Client) -> Option<[u32; 16]> {
    use x11rb::protocol::{
        render::{self, ConnectionExt as _},
        xproto::ImageFormat,
    };
    let formats = c.conn.render_query_pict_formats().ok()?.reply().ok()?;
    let argb = formats.formats.iter().find(|f| {
        f.depth == 32
            && f.direct.alpha_mask == 0xff
            && f.direct.alpha_shift == 24
            && f.direct.red_shift == 16
    })?;
    let pm = c.conn.generate_id().ok()?;
    c.conn.create_pixmap(32, pm, c.root, 4, 4).ok()?;
    let mut px = [0u32; 16];
    for (i, p) in px.iter_mut().enumerate() {
        *p = match i % 4 {
            0 => 0xff_ff_00_00, // opaque red
            1 => 0x80_80_80_80, // 50% white, premultiplied
            2 => 0x00_00_00_00, // transparent
            _ => 0xff_00_00_ff, // opaque blue
        };
    }
    let bytes: Vec<u8> = px.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let gc = c.conn.generate_id().ok()?;
    c.conn.create_gc(gc, pm, &x11rb::protocol::xproto::CreateGCAux::new()).ok()?;
    c.conn.put_image(ImageFormat::Z_PIXMAP, pm, gc, 4, 4, 0, 0, 0, 32, &bytes).ok()?;
    let pic = c.conn.generate_id().ok()?;
    c.conn.render_create_picture(pic, pm, argb.id, &render::CreatePictureAux::new()).ok()?;
    let cursor = c.conn.generate_id().ok()?;
    c.conn.render_create_cursor(cursor, pic, 1, 1).ok()?.check().ok()?;
    c.conn
        .change_window_attributes(
            c.root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new().cursor(cursor),
        )
        .ok()?
        .check()
        .ok()?;
    c.sync();
    Some(px)
}

fn warp(c: &Client, x: i16, y: i16) {
    c.conn.warp_pointer(x11rb::NONE, c.root, 0, 0, 0, 0, x, y).unwrap();
    c.sync();
}

#[test]
fn cursor_is_composited_with_premultiplied_alpha_and_hotspot() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let Some(cursor) = install_argb_cursor(&c) else {
        eprintln!("SKIP: Xvfb has no RENDER ARGB cursor support");
        return;
    };
    warp(&c, 200, 150);
    let cap = capture(&x, |_| {});
    let bg = [0x30u8, 0x20, 0x10];

    let plain = cap.capture_desktop(&NO_CURSOR).unwrap();
    assert_eq!(px(&plain, 200, 150), [0x30, 0x20, 0x10, 255], "no cursor unless asked");

    let with = cap.capture_desktop(&CaptureOptions { include_cursor: true }).unwrap();
    // Hotspot (1,1) at (200,150): image top-left is (199,149).
    let expect = |i: usize| -> [u8; 4] {
        let v = cursor[i];
        let a = v >> 24;
        let ch = |shift: u32, b: u8| {
            let s = (v >> shift) & 0xff;
            (s + (u32::from(b) * (255 - a) + 127) / 255).min(255) as u8
        };
        [ch(0, bg[0]), ch(8, bg[1]), ch(16, bg[2]), 255]
    };
    for cy in 0..4u32 {
        for cx in 0..4u32 {
            let got = px(&with, 199 + cx, 149 + cy);
            assert_eq!(got, expect((cy * 4 + cx) as usize), "cursor pixel ({cx},{cy})");
        }
    }
    // Sanity on the semantic values: opaque red, half white, untouched, opaque blue.
    assert_eq!(px(&with, 199, 149), [0, 0, 255, 255]);
    assert_eq!(px(&with, 201, 149), [0x30, 0x20, 0x10, 255]);
    assert_eq!(px(&with, 202, 149), [255, 0, 0, 255]);
    // Outside the cursor nothing changed.
    assert_eq!(px(&with, 198, 149), px(&plain, 198, 149));
    assert_eq!(px(&with, 203, 153), px(&plain, 203, 153));

    // Region capture places the cursor relative to the region.
    let r = cap
        .capture_region(Rect::new(190, 140, 20, 20), &CaptureOptions { include_cursor: true })
        .unwrap();
    assert_eq!(px(&r, 9, 9), [0, 0, 255, 255]);
    // A region that only clips the cursor keeps the visible part; one that misses is clean.
    let r = cap
        .capture_region(Rect::new(201, 151, 5, 5), &CaptureOptions { include_cursor: true })
        .unwrap();
    assert_eq!(px(&r, 0, 0), expect(2 * 4 + 2));
    let r = cap
        .capture_region(Rect::new(0, 0, 10, 10), &CaptureOptions { include_cursor: true })
        .unwrap();
    assert_pixels(&r, &expected_scene(10, 10, (0, 0), ROOT_BG, &[]));
}

#[test]
fn cursor_at_the_screen_edge_is_clipped_not_fatal() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    if install_argb_cursor(&c).is_none() {
        eprintln!("SKIP: Xvfb has no RENDER ARGB cursor support");
        return;
    }
    warp(&c, 0, 0);
    let cap = capture(&x, |_| {});
    let f = cap.capture_desktop(&CaptureOptions { include_cursor: true }).unwrap();
    // Hotspot at (0,0): cursor pixel (1,1) is the top-left screen pixel (transparent
    // column 2 is at x=1, pixel (1,1) is 50% white).
    assert_ne!(px(&f, 0, 0), [0x30, 0x20, 0x10, 255]);
    assert_eq!(px(&f, 2, 0), [255, 0, 0, 255]);
}

#[test]
fn window_capture_can_include_the_cursor() {
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    if install_argb_cursor(&c).is_none() {
        eprintln!("SKIP: Xvfb has no RENDER ARGB cursor support");
        return;
    }
    // The cursor is over the *window*, whose own cursor is None => inherits the root's.
    let a = c.window(100, 100, 100, 100, 0x00_ff_00);
    warp(&c, 150, 150);
    let cap = capture(&x, |_| {});
    let f =
        cap.capture_window(&format!("{a:#x}"), &CaptureOptions { include_cursor: true }).unwrap();
    assert_eq!(
        px(&f, 49, 49),
        [0, 0, 255, 255],
        "cursor pixel (0,0), red, at window-local (49,49)"
    );
    assert_eq!(px(&f, 10, 10), [0, 255, 0, 255]);
}

// ---- 32-bit ARGB windows ---------------------------------------------------------------

#[test]
fn argb_depth32_window_is_captured_opaque_through_composite() {
    use x11rb::protocol::xproto::{ColormapAlloc, CreateWindowAux, WindowClass};
    let Some(x) = xvfb() else { return };
    let c = Client::new(&x.display);
    let Some(visual) = c.conn.setup().roots[0]
        .allowed_depths
        .iter()
        .find(|d| d.depth == 32)
        .and_then(|d| d.visuals.first().copied())
    else {
        eprintln!("SKIP: this Xvfb offers no depth-32 visual");
        return;
    };
    let _cm = become_compositor(&c);
    let cmap = c.conn.generate_id().unwrap();
    c.conn.create_colormap(ColormapAlloc::NONE, cmap, c.root, visual.visual_id).unwrap();
    let win = c.conn.generate_id().unwrap();
    // Premultiplied 50% blue: the capture keeps the colour and forces alpha opaque.
    c.conn
        .create_window(
            32,
            win,
            c.root,
            100,
            100,
            40,
            30,
            0,
            WindowClass::INPUT_OUTPUT,
            visual.visual_id,
            &CreateWindowAux::new().background_pixel(0x80_00_00_80).border_pixel(0).colormap(cmap),
        )
        .unwrap();
    c.conn.map_window(win).unwrap();
    c.sync();
    let cap = capture(&x, |_| {});
    let f = cap.capture_window(&format!("{win:#x}"), &NO_CURSOR).unwrap();
    assert_eq!(f.rect(), Rect::new(100, 100, 40, 30));
    assert!(pixels(&f).chunks_exact(4).all(|p| p == [0x80, 0, 0, 255]));
}
