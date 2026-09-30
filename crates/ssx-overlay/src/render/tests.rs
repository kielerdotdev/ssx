//! Renderer tests: golden images, colour handling and the damage-soundness property.

use std::path::PathBuf;

use proptest::prelude::*;
use ssx_types::{ColorSpace, Frame, Monitor, PixelFormat, Point, Rect, Size, WindowInfo};

use super::{Renderer, TargetBuf};
use crate::model::damage;
use crate::model::events::{InputEvent, Key, KeyEvent, PointerButton, PointerEvent};
use crate::model::scene::{Cutout, Scene};
use crate::model::state::{ModelConfig, SelectionModel};
use crate::types::{OverlayOptions, SelectMode};

/// A deterministic, busy test desktop: colour ramps, a checker and grid lines so edges,
/// zoomed pixels and dimming are all visible in the goldens.
pub(crate) fn test_desktop(w: u32, h: u32, origin: Point) -> Frame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let grid = x % 20 == 0 || y % 20 == 0;
            let check = ((x / 4) + (y / 4)) % 2 == 0;
            let r = (x * 255 / w.max(1)) as u8;
            let g = (y * 255 / h.max(1)) as u8;
            let b = if check { 200 } else { 60 };
            if grid {
                data.extend_from_slice(&[240, 240, 240, 255]);
            } else {
                data.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    let mut f = Frame::from_rgba8(w, h, data).unwrap();
    f.origin = origin;
    f
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
}

/// Compares `frame` with `tests/golden/<name>.png`. `UPDATE_GOLDEN=1` (re)writes it.
///
/// A tiny tolerance absorbs rasteriser differences between CPU architectures (SIMD paths);
/// the goldens themselves are exact on the machine that generated them.
fn assert_golden(name: &str, frame: &Frame) {
    let path = golden_dir().join(format!("{name}.png"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        frame.save(&path).unwrap();
        return;
    }
    let want = Frame::decode(&std::fs::read(&path).unwrap_or_else(|e| {
        panic!("missing golden {path:?}: {e}. Run with UPDATE_GOLDEN=1 to create it")
    }))
    .unwrap()
    .into_rgba8()
    .unwrap();
    assert_eq!(want.size(), frame.size(), "golden {name} has a different size");
    let (mut bad, mut worst) = (0usize, 0u8);
    for y in 0..frame.height() {
        let (a, b) = (want.row(y), frame.row(y));
        for (pa, pb) in a.iter().zip(b) {
            let d = pa.abs_diff(*pb);
            if d > 2 {
                bad += 1;
            }
            worst = worst.max(d);
        }
    }
    assert!(
        bad <= frame.data().len() / 2000,
        "golden {name}: {bad} channel values differ by more than 2 (worst {worst}); \
         inspect and run with UPDATE_GOLDEN=1 if the change is intended"
    );
}

fn renderer(w: u32, h: u32) -> Renderer {
    Renderer::new(&test_desktop(w, h, Point::new(0, 0)), 0.5).unwrap()
}

fn base_scene(w: u32, h: u32) -> Scene {
    Scene::empty(Rect::new(0, 0, w, h))
}

#[test]
fn golden_idle_crosshair_and_loupe() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    let c = Point::new(120, 80);
    s.crosshair = Some(c);
    s.loupe = crate::model::scene::layout_loupe(c, 8, 1.0, s.bounds);
    assert_golden("idle_crosshair_loupe", &r.render_to_frame(&s));
}

#[test]
fn golden_selection_with_handles_and_label() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    let sel = Rect::new(60, 50, 150, 90);
    s.selection = Some(sel);
    s.handles = true;
    s.crosshair = Some(Point::new(210, 140));
    s.label = crate::model::scene::place_label(
        sel,
        vec!["150 x 90".into(), "60, 50".into()],
        1.0,
        s.bounds,
    );
    assert_golden("selection_handles_label", &r.render_to_frame(&s));
}

#[test]
fn golden_ellipse_selection() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    s.cutout = Cutout::Ellipse;
    s.selection = Some(Rect::new(50, 30, 200, 120));
    s.handles = true;
    assert_golden("ellipse_selection", &r.render_to_frame(&s));
}

#[test]
fn golden_freeform_in_progress() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    s.freeform = vec![
        Point::new(60, 40),
        Point::new(200, 30),
        Point::new(250, 110),
        Point::new(180, 170),
        Point::new(90, 150),
    ];
    assert_golden("freeform_in_progress", &r.render_to_frame(&s));
}

#[test]
fn golden_window_highlight() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    let h = Rect::new(40, 30, 200, 140);
    s.highlight = Some(h);
    s.label = crate::model::scene::place_label(
        h,
        vec!["200 x 140".into(), "40, 30".into()],
        1.0,
        s.bounds,
    );
    assert_golden("window_highlight", &r.render_to_frame(&s));
}

#[test]
fn golden_hidpi_ui_scale_two() {
    let mut r = renderer(480, 300);
    let mut s = base_scene(480, 300);
    s.ui_scale = 2.0;
    let sel = Rect::new(80, 80, 240, 140);
    s.selection = Some(sel);
    s.handles = true;
    s.active_handle = Some(crate::model::geometry::Handle::SouthEast);
    let c = Point::new(320, 220);
    s.crosshair = Some(c);
    s.loupe = crate::model::scene::layout_loupe(c, 8, 2.0, s.bounds);
    s.label = crate::model::scene::place_label(
        sel,
        vec!["240 x 140".into(), "80, 80".into()],
        2.0,
        s.bounds,
    );
    assert_golden("hidpi_scale2", &r.render_to_frame(&s));
}

#[test]
fn golden_loupe_at_desktop_corner_shows_out_of_bounds_checker() {
    let mut r = renderer(320, 200);
    let mut s = base_scene(320, 200);
    let c = Point::new(2, 3);
    s.crosshair = Some(c);
    s.loupe = crate::model::scene::layout_loupe(c, 8, 1.0, s.bounds);
    assert_golden("loupe_corner", &r.render_to_frame(&s));
}

#[test]
fn inside_selection_is_bright_and_outside_is_dimmed() {
    let mut r = renderer(200, 100);
    let mut s = base_scene(200, 100);
    s.selection = Some(Rect::new(50, 20, 100, 60));
    let f = r.render_to_frame(&s);
    let px = |x: u32, y: u32| {
        let p = &f.row(y)[x as usize * 4..x as usize * 4 + 3];
        [p[0], p[1], p[2]]
    };
    let orig = test_desktop(200, 100, Point::new(0, 0));
    let o = |x: u32, y: u32| {
        let p = &orig.row(y)[x as usize * 4..x as usize * 4 + 3];
        [p[0], p[1], p[2]]
    };
    assert_eq!(px(100, 50), o(100, 50), "inside is untouched");
    for c in 0..3 {
        let want = (f32::from(o(10, 10)[c]) * 0.5 + 0.5) as u8;
        assert_eq!(px(10, 10)[c], want, "outside is dimmed by exactly dim");
    }
    // The selection border sits just outside the selection, not over its pixels.
    assert_eq!(px(50, 50), o(50, 50));
    assert_ne!(px(49, 50), o(49, 50));
}

#[test]
fn bgra_and_rgba_inputs_render_identically_and_alpha_is_forced_opaque() {
    let rgba = test_desktop(64, 48, Point::new(0, 0));
    let mut bgra_data = rgba.data().to_vec();
    for p in bgra_data.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 0; // capture backends may leave alpha 0 in gaps
    }
    let bgra =
        Frame::from_raw(Size::new(64, 48), 64 * 4, PixelFormat::Bgra8, ColorSpace::Srgb, bgra_data)
            .unwrap();
    let mut a = Renderer::new(&rgba, 0.4).unwrap();
    let mut b = Renderer::new(&bgra, 0.4).unwrap();
    let s = base_scene(64, 48);
    assert_eq!(a.render_to_frame(&s).data(), b.render_to_frame(&s).data());
    assert!(a.render_to_frame(&s).data().chunks_exact(4).all(|p| p[3] == 255));
}

#[test]
fn hdr_and_empty_frames_are_rejected_with_a_clear_error() {
    let hdr = Frame::new(Size::new(4, 4), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
    let e = Renderer::new(&hdr, 0.5).unwrap_err().to_string();
    assert!(e.contains("tone-map"), "{e}");
    let empty = Frame::new(Size::new(0, 0), PixelFormat::Rgba8, ColorSpace::Srgb);
    assert!(Renderer::new(&empty, 0.5).is_err());
}

#[test]
fn stride_padded_frames_are_handled() {
    let w = 10u32;
    let stride = 64usize;
    let mut data = vec![7u8; stride * 5];
    for y in 0..5usize {
        for x in 0..w as usize {
            data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[
                x as u8 * 10,
                y as u8 * 10,
                3,
                255,
            ]);
        }
    }
    let f = Frame::from_raw(Size::new(w, 5), stride, PixelFormat::Rgba8, ColorSpace::Srgb, data)
        .unwrap();
    let r = Renderer::new(&f, 0.0).unwrap();
    assert_eq!(r.pixel(Point::new(3, 2)), Some([30, 20, 3]));
    assert_eq!(r.pixel(Point::new(10, 2)), None);
}

#[test]
fn negative_origin_desktop_maps_pixels_correctly() {
    let f = test_desktop(100, 80, Point::new(-40, -10));
    let r = Renderer::new(&f, 0.5).unwrap();
    let want = {
        let p = &f.row(20)[30 * 4..30 * 4 + 3];
        [p[0], p[1], p[2]]
    };
    assert_eq!(r.pixel(Point::new(-40 + 30, -10 + 20)), Some(want));
    assert_eq!(r.pixel(Point::new(-41, 0)), None);
}

#[test]
fn rendering_a_sub_area_matches_the_same_area_of_a_full_render() {
    let mut r = renderer(200, 120);
    let mut s = base_scene(200, 120);
    s.selection = Some(Rect::new(30, 20, 120, 70));
    s.handles = true;
    s.crosshair = Some(Point::new(100, 60));
    s.loupe = crate::model::scene::layout_loupe(Point::new(100, 60), 8, 1.0, s.bounds);
    let full = r.render_to_frame(&s);
    let area = Rect::new(20, 10, 90, 60);
    let mut buf = vec![0u8; 200 * 120 * 4];
    let mut t = TargetBuf { origin: Point::new(0, 0), size: Size::new(200, 120), data: &mut buf };
    r.render(&s, area, &mut t);
    for y in area.y..area.bottom() as i32 {
        for x in area.x..area.right() as i32 {
            let o = (y as usize * 200 + x as usize) * 4;
            let mut px = [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]];
            px.swap(0, 2);
            assert_eq!(&full.data()[o..o + 4], &px, "pixel {x},{y}");
        }
    }
    // Nothing outside the area was touched.
    assert!(buf[..(10 * 200 * 4)].iter().all(|b| *b == 0));
}

// ---------------------------------------------------------------- damage soundness

#[derive(Debug, Clone)]
enum Op {
    Move(i32, i32),
    Down(i32, i32, bool),
    Up(i32, i32),
    Key(u8, bool),
    Wheel(i32),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (-5i32..215, -5i32..135).prop_map(|(x, y)| Op::Move(x, y)),
        2 => (0i32..200, 0i32..120, any::<bool>()).prop_map(|(x, y, r)| Op::Down(x, y, r)),
        2 => (0i32..200, 0i32..120).prop_map(|(x, y)| Op::Up(x, y)),
        2 => (0u8..8, any::<bool>()).prop_map(|(k, p)| Op::Key(k, p)),
        1 => (-2i32..3).prop_map(Op::Wheel),
    ]
}

fn apply(m: &mut SelectionModel, op: &Op, t: u64) {
    let ev = match *op {
        Op::Move(x, y) => InputEvent::Pointer(PointerEvent::Move { pos: Point::new(x, y) }),
        Op::Down(x, y, right) => InputEvent::Pointer(PointerEvent::Down {
            pos: Point::new(x, y),
            button: if right { PointerButton::Right } else { PointerButton::Left },
            time_ms: t,
        }),
        Op::Up(x, y) => InputEvent::Pointer(PointerEvent::Up {
            pos: Point::new(x, y),
            button: PointerButton::Left,
        }),
        Op::Wheel(d) => InputEvent::Pointer(PointerEvent::Wheel { delta: d }),
        Op::Key(k, pressed) => InputEvent::Key(KeyEvent {
            key: [
                Key::Shift,
                Key::Control,
                Key::Alt,
                Key::Space,
                Key::Left,
                Key::Down,
                Key::Tab,
                Key::Other,
            ][usize::from(k)],
            pressed,
        }),
    };
    m.handle(ev);
}

fn soundness_run(mode: SelectMode, ui: f32, ops: &[Op]) -> Result<(), TestCaseError> {
    let (w, h) = (200u32, 120u32);
    let bounds = Rect::new(0, 0, w, h);
    let monitors = vec![
        Monitor {
            id: "a".into(),
            name: "a".into(),
            rect: Rect::new(0, 0, 120, 120),
            scale_factor: 1.0,
            primary: true,
            refresh_hz: None,
            hdr: None,
        },
        Monitor {
            id: "b".into(),
            name: "b".into(),
            rect: Rect::new(120, 0, 80, 120),
            scale_factor: 1.0,
            primary: false,
            refresh_hz: None,
            hdr: None,
        },
    ];
    let windows = vec![WindowInfo {
        id: "w".into(),
        title: "w".into(),
        app_name: None,
        rect: Rect::new(20, 20, 90, 60),
        minimized: false,
        focused: false,
    }];
    let options =
        OverlayOptions { mode, snap_to_windows: true, dim: 0.5, ..OverlayOptions::default() };
    let mut cfg = ModelConfig::new(bounds, monitors, windows, options, ui);
    cfg.global_ui_scale = ui;
    let mut m = SelectionModel::new(cfg);
    let mut r = Renderer::new(&test_desktop(w, h, Point::new(0, 0)), 0.5).unwrap();
    let mut inc = vec![0u8; (w * h * 4) as usize];
    let mut prev: Option<Scene> = None;
    for (i, op) in ops.iter().enumerate() {
        apply(&mut m, op, i as u64 * 40);
        let scene = m.scene();
        let dirty = damage::between(prev.as_ref(), &scene);
        for d in &dirty {
            let mut t =
                TargetBuf { origin: Point::new(0, 0), size: Size::new(w, h), data: &mut inc };
            r.render(&scene, *d, &mut t);
        }
        let mut full = vec![0u8; (w * h * 4) as usize];
        let mut t = TargetBuf { origin: Point::new(0, 0), size: Size::new(w, h), data: &mut full };
        r.render(&scene, bounds, &mut t);
        if let Some(pos) = inc.iter().zip(&full).position(|(a, b)| a != b) {
            let px = pos / 4;
            return Err(TestCaseError::fail(format!(
                "after op #{i} {op:?}: pixel ({}, {}) channel {} differs (inc {} vs full {}); dirty {:?}\nprev {:?}\nnow  {:?}",
                px % w as usize,
                px / w as usize,
                pos % 4,
                inc[pos],
                full[pos],
                dirty,
                prev,
                scene
            )));
        }
        prev = Some(scene);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    #[test]
    fn incremental_render_equals_full_render_rect(ops in proptest::collection::vec(arb_op(), 1..60)) {
        soundness_run(SelectMode::Rect, 1.0, &ops)?;
    }

    #[test]
    fn incremental_render_equals_full_render_ellipse_hidpi(ops in proptest::collection::vec(arb_op(), 1..50)) {
        soundness_run(SelectMode::Ellipse, 2.0, &ops)?;
    }

    #[test]
    fn incremental_render_equals_full_render_freeform(ops in proptest::collection::vec(arb_op(), 1..60)) {
        soundness_run(SelectMode::Freeform, 1.0, &ops)?;
    }

    #[test]
    fn incremental_render_equals_full_render_window_and_monitor(ops in proptest::collection::vec(arb_op(), 1..40), mon in any::<bool>()) {
        soundness_run(if mon { SelectMode::Monitor } else { SelectMode::Window }, 1.0, &ops)?;
    }
}

#[test]
fn undersized_or_misstrided_buffers_are_rejected_not_panicked_on() {
    let data = vec![0u8; 100];
    let view = |stride: usize, w: u32, h: u32| super::PixelView {
        size: Size::new(w, h),
        stride,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        data: &data,
        origin: Point::new(0, 0),
        scale_factor: 1.0,
    };
    assert!(Renderer::from_view(view(40, 10, 10), 0.5).is_err(), "100 bytes < 10 rows of 40");
    assert!(Renderer::from_view(view(8, 4, 2), 0.5).is_err(), "stride smaller than a row");
    assert!(Renderer::from_view(view(usize::MAX, 4, 3), 0.5).is_err(), "overflowing stride");
    assert!(Renderer::from_view(view(16, 4, 6), 0.5).is_ok(), "exactly enough");
}
