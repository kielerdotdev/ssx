//! Interaction tests for the selection model: scripted gestures plus property tests over
//! random event streams.

use proptest::prelude::*;
use ssx_types::{Monitor, Point, Rect, WindowInfo};

use super::events::{InputEvent, Key, KeyEvent, PointerButton, PointerEvent};
use super::scene::CursorHint;
use super::state::{Finish, FinishShape, ModelConfig, SelectionModel};
use crate::types::{OverlayOptions, SelectMode, UiScale};

const B: Rect = Rect { x: 0, y: 0, width: 1000, height: 800 };

fn mon(id: &str, r: Rect, scale: f64) -> Monitor {
    Monitor {
        id: id.into(),
        name: id.into(),
        rect: r,
        scale_factor: scale,
        primary: false,
        refresh_hz: None,
        hdr: None,
    }
}

fn win(id: &str, r: Rect) -> WindowInfo {
    WindowInfo {
        id: id.into(),
        title: id.into(),
        app_name: None,
        rect: r,
        minimized: false,
        focused: false,
    }
}

fn opts(mode: SelectMode) -> OverlayOptions {
    OverlayOptions { mode, snap_to_windows: false, ..OverlayOptions::default() }
}

fn model_with(bounds: Rect, monitors: Vec<Monitor>, windows: Vec<WindowInfo>, o: OverlayOptions) -> SelectionModel {
    SelectionModel::new(ModelConfig::new(bounds, monitors, windows, o, 1.0))
}

fn model(mode: SelectMode) -> SelectionModel {
    model_with(B, vec![], vec![], opts(mode))
}

fn ptr(m: &mut SelectionModel, ev: PointerEvent) {
    m.handle(InputEvent::Pointer(ev));
}
fn mv(m: &mut SelectionModel, x: i32, y: i32) {
    ptr(m, PointerEvent::Move { pos: Point::new(x, y) });
}
fn down_at(m: &mut SelectionModel, x: i32, y: i32, t: u64) {
    ptr(m, PointerEvent::Down { pos: Point::new(x, y), button: PointerButton::Left, time_ms: t });
}
fn up(m: &mut SelectionModel, x: i32, y: i32) {
    ptr(m, PointerEvent::Up { pos: Point::new(x, y), button: PointerButton::Left });
}
fn right_click(m: &mut SelectionModel, x: i32, y: i32) {
    ptr(m, PointerEvent::Down { pos: Point::new(x, y), button: PointerButton::Right, time_ms: 0 });
    ptr(m, PointerEvent::Up { pos: Point::new(x, y), button: PointerButton::Right });
}
fn key(m: &mut SelectionModel, k: Key) {
    m.handle(InputEvent::Key(KeyEvent { key: k, pressed: true }));
    m.handle(InputEvent::Key(KeyEvent { key: k, pressed: false }));
}
fn hold(m: &mut SelectionModel, k: Key, pressed: bool) {
    m.handle(InputEvent::Key(KeyEvent { key: k, pressed }));
}
/// Press at (x0,y0), move through the path, release at the last point.
fn drag(m: &mut SelectionModel, from: (i32, i32), to: (i32, i32)) {
    mv(m, from.0, from.1);
    down_at(m, from.0, from.1, 10_000);
    mv(m, (from.0 + to.0) / 2, (from.1 + to.1) / 2);
    mv(m, to.0, to.1);
    up(m, to.0, to.1);
}

fn selected(m: &SelectionModel) -> Rect {
    match m.finished() {
        Some(Finish::Selected { rect, .. }) => *rect,
        other => panic!("expected Selected, got {other:?}"),
    }
}

#[test]
fn drag_creates_exact_rect_and_enter_confirms() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (300, 250));
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 200, 150)));
    assert!(m.finished().is_none());
    key(&mut m, Key::Enter);
    assert_eq!(selected(&m), Rect::new(100, 100, 200, 150));
    match m.finished() {
        Some(Finish::Selected { shape, window, .. }) => {
            assert_eq!(*shape, FinishShape::Rect);
            assert_eq!(*window, None);
        }
        _ => unreachable!(),
    }
}

#[test]
fn drag_in_every_direction_normalises() {
    for (from, to) in [((300, 250), (100, 100)), ((100, 250), (300, 100)), ((300, 100), (100, 250))] {
        let mut m = model(SelectMode::Rect);
        drag(&mut m, from, to);
        assert_eq!(m.selection(), Some(Rect::new(100, 100, 200, 150)), "{from:?}->{to:?}");
    }
}

#[test]
fn tiny_movements_are_clicks_not_drags() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 50, 50);
    down_at(&mut m, 50, 50, 0);
    mv(&mut m, 52, 51);
    up(&mut m, 52, 51);
    assert_eq!(m.selection(), None);
    assert!(m.finished().is_none());
}

#[test]
fn enter_without_selection_does_nothing() {
    let mut m = model(SelectMode::Rect);
    key(&mut m, Key::Enter);
    assert!(m.finished().is_none());
}

#[test]
fn zero_size_drag_leaves_no_selection() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 50, 50);
    down_at(&mut m, 50, 50, 0);
    mv(&mut m, 50, 90);
    mv(&mut m, 50, 50);
    up(&mut m, 50, 50);
    // Straight line out and back: rect is empty.
    assert_eq!(m.selection(), None);
}

#[test]
fn last_pixel_counts_as_desktop_edge() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (0, 0), (999, 799));
    assert_eq!(m.selection(), Some(B), "dragging to the last pixel selects the whole desktop");
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (999, 799), (0, 0));
    assert_eq!(m.selection(), Some(B));
    // Beyond the desktop clamps.
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (10, 10), (5000, -300));
    assert_eq!(m.selection(), Some(Rect::new(10, 0, 990, 10)));
}

#[test]
fn shift_constrains_to_square_and_reverts() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 100, 100);
    down_at(&mut m, 100, 100, 0);
    hold(&mut m, Key::Shift, true);
    mv(&mut m, 300, 150);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 200, 200)));
    mv(&mut m, 40, 30);
    assert_eq!(m.selection(), Some(Rect::new(30, 30, 70, 70)));
    hold(&mut m, Key::Shift, false);
    assert_eq!(m.selection(), Some(Rect::new(40, 30, 60, 70)), "releasing Shift reverts at once");
    up(&mut m, 40, 30);
}

#[test]
fn shift_square_is_clamped_to_the_desktop() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 900, 100);
    down_at(&mut m, 900, 100, 0);
    hold(&mut m, Key::Shift, true);
    mv(&mut m, 990, 700);
    let r = m.selection().unwrap();
    assert_eq!(r.width, r.height);
    assert!(r.right() <= 1000 && r.bottom() <= 800);
    assert_eq!(r, Rect::new(900, 100, 100, 100));
}

#[test]
fn space_moves_the_rectangle_while_dragging() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 100, 100);
    down_at(&mut m, 100, 100, 0);
    mv(&mut m, 200, 180);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 100, 80)));
    hold(&mut m, Key::Space, true);
    mv(&mut m, 250, 200);
    assert_eq!(m.selection(), Some(Rect::new(150, 120, 100, 80)));
    hold(&mut m, Key::Space, false);
    mv(&mut m, 300, 250);
    // Resizing continues from the moved anchor.
    assert_eq!(m.selection(), Some(Rect::new(150, 120, 150, 130)));
    up(&mut m, 300, 250);
}

#[test]
fn alt_also_moves_and_move_is_clamped() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 900, 700);
    down_at(&mut m, 900, 700, 0);
    mv(&mut m, 950, 750);
    hold(&mut m, Key::Alt, true);
    mv(&mut m, 1500, 1500);
    assert_eq!(m.selection(), Some(Rect::new(950, 750, 50, 50)));
    up(&mut m, 1500, 1500);
}

#[test]
fn move_body_and_clamp() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (200, 200));
    down_at(&mut m, 150, 150, 50_000);
    mv(&mut m, 250, 190);
    assert_eq!(m.selection(), Some(Rect::new(200, 140, 100, 100)));
    mv(&mut m, 5000, 5000);
    assert_eq!(m.selection(), Some(Rect::new(900, 700, 100, 100)));
    mv(&mut m, -5000, -5000);
    assert_eq!(m.selection(), Some(Rect::new(0, 0, 100, 100)));
    up(&mut m, 0, 0);
    key(&mut m, Key::Enter);
    assert_eq!(selected(&m), Rect::new(0, 0, 100, 100));
}

fn resize_case(handle_pos: (i32, i32), to: (i32, i32)) -> Rect {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (300, 250)); // 200 x 150
    mv(&mut m, handle_pos.0, handle_pos.1);
    down_at(&mut m, handle_pos.0, handle_pos.1, 50_000);
    mv(&mut m, to.0, to.1);
    up(&mut m, to.0, to.1);
    m.selection().unwrap()
}

#[test]
fn all_eight_handles_resize() {
    assert_eq!(resize_case((100, 100), (80, 90)), Rect::new(80, 90, 220, 160), "NW");
    assert_eq!(resize_case((200, 100), (200, 60)), Rect::new(100, 60, 200, 190), "N");
    assert_eq!(resize_case((300, 100), (320, 120)), Rect::new(100, 120, 220, 130), "NE");
    assert_eq!(resize_case((300, 175), (350, 175)), Rect::new(100, 100, 250, 150), "E");
    assert_eq!(resize_case((300, 250), (310, 260)), Rect::new(100, 100, 210, 160), "SE");
    assert_eq!(resize_case((200, 250), (200, 300)), Rect::new(100, 100, 200, 200), "S");
    assert_eq!(resize_case((100, 250), (90, 260)), Rect::new(90, 100, 210, 160), "SW");
    assert_eq!(resize_case((100, 175), (60, 175)), Rect::new(60, 100, 240, 150), "W");
}

#[test]
fn resizing_past_the_opposite_edge_flips() {
    assert_eq!(resize_case((300, 175), (50, 175)), Rect::new(50, 100, 50, 150));
    assert_eq!(resize_case((300, 250), (20, 20)), Rect::new(20, 20, 80, 80));
}

#[test]
fn shift_keeps_aspect_ratio_when_resizing_corner() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (300, 250)); // 200x150 (4:3)
    mv(&mut m, 300, 250);
    down_at(&mut m, 300, 250, 50_000);
    hold(&mut m, Key::Shift, true);
    mv(&mut m, 500, 300);
    let r = m.selection().unwrap();
    assert_eq!((r.x, r.y), (100, 100));
    assert_eq!(r.width * 3, r.height * 4, "{r:?}");
    up(&mut m, 500, 300);
}

#[test]
fn right_click_clears_selection_first_then_cancels() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (200, 200));
    right_click(&mut m, 150, 150);
    assert_eq!(m.selection(), None);
    assert!(m.finished().is_none(), "first right-click only clears");
    right_click(&mut m, 150, 150);
    assert_eq!(m.finished(), Some(&Finish::Cancelled));
}

#[test]
fn right_click_on_empty_overlay_cancels() {
    let mut m = model(SelectMode::Rect);
    right_click(&mut m, 5, 5);
    assert_eq!(m.finished(), Some(&Finish::Cancelled));
}

#[test]
fn escape_cancels_even_with_selection() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (200, 200));
    key(&mut m, Key::Escape);
    assert_eq!(m.finished(), Some(&Finish::Cancelled));
    // Once finished, everything is ignored.
    assert!(!m.handle(InputEvent::Key(KeyEvent { key: Key::Enter, pressed: true })));
}

#[test]
fn right_click_during_drag_aborts_it() {
    let mut m = model(SelectMode::Rect);
    mv(&mut m, 100, 100);
    down_at(&mut m, 100, 100, 0);
    mv(&mut m, 200, 200);
    right_click(&mut m, 200, 200);
    assert_eq!(m.selection(), None);
    up(&mut m, 200, 200);
    assert_eq!(m.selection(), None);
    assert!(m.finished().is_none());
}

#[test]
fn arrow_keys_nudge_by_one_and_ten() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (200, 200));
    key(&mut m, Key::Right);
    key(&mut m, Key::Down);
    assert_eq!(m.selection(), Some(Rect::new(101, 101, 100, 100)));
    hold(&mut m, Key::Shift, true);
    key(&mut m, Key::Left);
    key(&mut m, Key::Up);
    hold(&mut m, Key::Shift, false);
    assert_eq!(m.selection(), Some(Rect::new(91, 91, 100, 100)));
    // Clamped at the desktop edge.
    hold(&mut m, Key::Shift, true);
    for _ in 0..20 {
        key(&mut m, Key::Left);
        key(&mut m, Key::Up);
    }
    assert_eq!(m.selection(), Some(Rect::new(0, 0, 100, 100)));
}

#[test]
fn ctrl_arrow_resizes_and_never_below_one() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (105, 105));
    hold(&mut m, Key::Control, true);
    key(&mut m, Key::Right);
    key(&mut m, Key::Down);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 6, 6)));
    for _ in 0..20 {
        key(&mut m, Key::Left);
        key(&mut m, Key::Up);
    }
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 1, 1)));
}

#[test]
fn arrows_without_selection_do_nothing() {
    let mut m = model(SelectMode::Rect);
    key(&mut m, Key::Left);
    assert_eq!(m.selection(), None);
}

#[test]
fn double_click_inside_confirms() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (300, 300));
    down_at(&mut m, 200, 200, 100_000);
    up(&mut m, 200, 200);
    assert!(m.finished().is_none());
    down_at(&mut m, 201, 200, 100_200);
    assert_eq!(selected(&m), Rect::new(100, 100, 200, 200));
}

#[test]
fn slow_or_distant_second_click_is_not_a_double_click() {
    for (x2, t2) in [(201, 100_900), (260, 100_100)] {
        let mut m = model(SelectMode::Rect);
        drag(&mut m, (100, 100), (300, 300));
        down_at(&mut m, 200, 200, 100_000);
        up(&mut m, 200, 200);
        down_at(&mut m, x2, 200, t2);
        assert!(m.finished().is_none(), "x2={x2} t2={t2}");
    }
}

#[test]
fn click_outside_selection_keeps_it_and_drag_replaces_it() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (200, 200));
    mv(&mut m, 500, 500);
    down_at(&mut m, 500, 500, 1);
    up(&mut m, 500, 500);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 100, 100)));
    drag(&mut m, (500, 500), (600, 650));
    assert_eq!(m.selection(), Some(Rect::new(500, 500, 100, 150)));
}

#[test]
fn initial_region_starts_selected_and_is_clamped() {
    let o = OverlayOptions { initial: Some(Rect::new(900, 700, 300, 300)), ..opts(SelectMode::Rect) };
    let mut m = model_with(B, vec![], vec![], o);
    assert_eq!(m.selection(), Some(Rect::new(900, 700, 100, 100)));
    key(&mut m, Key::Enter);
    assert_eq!(selected(&m), Rect::new(900, 700, 100, 100));
    let o = OverlayOptions { initial: Some(Rect::new(5000, 5000, 10, 10)), ..opts(SelectMode::Rect) };
    assert_eq!(model_with(B, vec![], vec![], o).selection(), None);
}

#[test]
fn ellipse_mode_reports_ellipse_shape() {
    let mut m = model(SelectMode::Ellipse);
    drag(&mut m, (100, 100), (200, 150));
    key(&mut m, Key::Enter);
    assert!(matches!(m.finished(), Some(Finish::Selected { shape: FinishShape::Ellipse, .. })));
    assert_eq!(m.scene().cutout, super::scene::Cutout::Ellipse);
}

// ------------------------------------------------------------------ windows

fn windows_model(mode: SelectMode) -> SelectionModel {
    let mut hidden = win("hidden", Rect::new(0, 0, 1000, 800));
    hidden.minimized = true;
    model_with(
        B,
        vec![],
        vec![
            win("front", Rect::new(100, 100, 300, 200)),
            win("back", Rect::new(50, 50, 600, 500)),
            hidden,
            win("offscreen", Rect::new(2000, 2000, 10, 10)),
            win("partial", Rect::new(900, 700, 300, 300)),
        ],
        OverlayOptions { mode, snap_to_windows: true, ..OverlayOptions::default() },
    )
}

#[test]
fn hover_highlights_frontmost_window_and_click_selects_its_rect() {
    let mut m = windows_model(SelectMode::Rect);
    mv(&mut m, 150, 150);
    assert_eq!(m.scene().highlight, Some(Rect::new(100, 100, 300, 200)));
    mv(&mut m, 500, 500);
    assert_eq!(m.scene().highlight, Some(Rect::new(50, 50, 600, 500)), "falls through to the back window");
    mv(&mut m, 800, 300);
    assert_eq!(m.scene().highlight, None, "minimized windows are ignored");
    mv(&mut m, 150, 150);
    down_at(&mut m, 150, 150, 0);
    up(&mut m, 150, 150);
    assert_eq!(selected(&m), Rect::new(100, 100, 300, 200));
    assert!(matches!(m.finished(), Some(Finish::Selected { window: Some(0), .. })));
}

#[test]
fn partially_offscreen_window_is_clipped_to_the_desktop() {
    let mut m = windows_model(SelectMode::Rect);
    mv(&mut m, 950, 750);
    assert_eq!(m.scene().highlight, Some(Rect::new(900, 700, 100, 100)));
    down_at(&mut m, 950, 750, 0);
    up(&mut m, 950, 750);
    assert_eq!(selected(&m), Rect::new(900, 700, 100, 100));
}

#[test]
fn dragging_from_a_highlighted_window_still_creates_a_rect() {
    let mut m = windows_model(SelectMode::Rect);
    drag(&mut m, (150, 150), (250, 260));
    assert_eq!(m.selection(), Some(Rect::new(150, 150, 100, 110)));
    assert!(m.finished().is_none());
}

#[test]
fn snap_disabled_means_no_highlight() {
    let mut m = model_with(
        B,
        vec![],
        vec![win("w", Rect::new(10, 10, 100, 100))],
        OverlayOptions { snap_to_windows: false, ..OverlayOptions::default() },
    );
    mv(&mut m, 50, 50);
    assert_eq!(m.scene().highlight, None);
    down_at(&mut m, 50, 50, 0);
    up(&mut m, 50, 50);
    assert!(m.finished().is_none());
}

#[test]
fn enter_over_highlighted_window_confirms_it() {
    let mut m = windows_model(SelectMode::Rect);
    mv(&mut m, 150, 150);
    key(&mut m, Key::Enter);
    assert_eq!(selected(&m), Rect::new(100, 100, 300, 200));
}

#[test]
fn window_mode_picks_window_index() {
    let mut m = windows_model(SelectMode::Window);
    mv(&mut m, 500, 500);
    down_at(&mut m, 500, 500, 0);
    up(&mut m, 500, 500);
    assert_eq!(m.finished(), Some(&Finish::Window(1)));
    let mut m = windows_model(SelectMode::Window);
    mv(&mut m, 990, 10);
    down_at(&mut m, 990, 10, 0);
    up(&mut m, 990, 10);
    assert!(m.finished().is_none(), "clicking empty desktop in window mode does nothing");
    key(&mut m, Key::Enter);
    assert!(m.finished().is_none());
    mv(&mut m, 150, 150);
    key(&mut m, Key::Enter);
    assert_eq!(m.finished(), Some(&Finish::Window(0)));
}

#[test]
fn window_mode_drag_does_not_create_a_rectangle() {
    let mut m = windows_model(SelectMode::Window);
    drag(&mut m, (150, 150), (250, 260));
    assert_eq!(m.selection(), None);
}

// ------------------------------------------------------------------ monitors

fn two_monitors(mode: SelectMode) -> SelectionModel {
    let l = Rect::new(-1920, 0, 1920, 1080);
    let r = Rect::new(0, 0, 2560, 1440);
    model_with(
        Rect::new(-1920, 0, 4480, 1440),
        vec![mon("L", l, 1.0), mon("R", r, 1.0)],
        vec![],
        opts(mode),
    )
}

#[test]
fn monitor_mode_hover_click_and_tab() {
    let mut m = two_monitors(SelectMode::Monitor);
    mv(&mut m, -500, 500);
    assert_eq!(m.scene().highlight, Some(Rect::new(-1920, 0, 1920, 1080)));
    mv(&mut m, 100, 500);
    assert_eq!(m.scene().highlight, Some(Rect::new(0, 0, 2560, 1440)));
    key(&mut m, Key::Tab);
    assert_eq!(m.scene().highlight, Some(Rect::new(-1920, 0, 1920, 1080)), "tab wraps to the next monitor");
    key(&mut m, Key::Enter);
    assert_eq!(m.finished(), Some(&Finish::Monitor(0)));
}

#[test]
fn monitor_mode_click_picks_monitor_under_pointer() {
    let mut m = two_monitors(SelectMode::Monitor);
    mv(&mut m, 2000, 1400);
    down_at(&mut m, 2000, 1400, 0);
    up(&mut m, 2000, 1400);
    assert_eq!(m.finished(), Some(&Finish::Monitor(1)));
}

#[test]
fn tab_in_rect_mode_selects_whole_monitors_in_turn() {
    let mut m = two_monitors(SelectMode::Rect);
    mv(&mut m, -100, 100);
    key(&mut m, Key::Tab);
    assert_eq!(m.selection(), Some(Rect::new(0, 0, 2560, 1440)));
    key(&mut m, Key::Tab);
    assert_eq!(m.selection(), Some(Rect::new(-1920, 0, 1920, 1080)));
    hold(&mut m, Key::Shift, true);
    key(&mut m, Key::Tab);
    assert_eq!(m.selection(), Some(Rect::new(0, 0, 2560, 1440)), "shift+tab goes back");
    hold(&mut m, Key::Shift, false);
    key(&mut m, Key::Enter);
    assert_eq!(selected(&m), Rect::new(0, 0, 2560, 1440));
}

#[test]
fn negative_origin_desktop_drag_across_monitors() {
    let mut m = two_monitors(SelectMode::Rect);
    drag(&mut m, (-100, 50), (200, 300));
    assert_eq!(m.selection(), Some(Rect::new(-100, 50, 300, 250)));
    drag(&mut m, (-1919, 1), (-1920, 0));
    assert_eq!(
        m.selection(),
        Some(Rect::new(-100, 50, 300, 250)),
        "a 1px twitch is a click and keeps the selection"
    );
}

// ------------------------------------------------------------------ misc keys, freeform

#[test]
fn wheel_changes_and_clamps_zoom() {
    let mut m = model(SelectMode::Rect);
    assert_eq!(m.zoom(), 8);
    ptr(&mut m, PointerEvent::Wheel { delta: 1 });
    assert_eq!(m.zoom(), 9);
    ptr(&mut m, PointerEvent::Wheel { delta: -120 });
    assert_eq!(m.zoom(), 8);
    for _ in 0..100 {
        ptr(&mut m, PointerEvent::Wheel { delta: 1 });
    }
    assert_eq!(m.zoom(), 24);
    for _ in 0..100 {
        ptr(&mut m, PointerEvent::Wheel { delta: -1 });
    }
    assert_eq!(m.zoom(), 2);
}

#[test]
fn c_picks_colour_only_when_allowed_and_over_the_overlay() {
    let mut m = model(SelectMode::Rect);
    key(&mut m, Key::Char('c'));
    assert!(m.finished().is_none(), "no pointer yet");
    mv(&mut m, 33, 44);
    key(&mut m, Key::Char('c'));
    assert_eq!(m.finished(), Some(&Finish::PickColor(Point::new(33, 44))));
    let mut m = model_with(B, vec![], vec![], OverlayOptions { allow_color_pick: false, ..opts(SelectMode::Rect) });
    mv(&mut m, 33, 44);
    key(&mut m, Key::Char('c'));
    assert!(m.finished().is_none());
}

fn freeform_stroke(m: &mut SelectionModel, pts: &[(i32, i32)]) {
    let (x0, y0) = pts[0];
    mv(m, x0, y0);
    down_at(m, x0, y0, 0);
    for &(x, y) in &pts[1..] {
        mv(m, x, y);
    }
    let (xl, yl) = *pts.last().unwrap();
    up(m, xl, yl);
}

#[test]
fn freeform_returns_polygon_and_bounding_rect() {
    let mut m = model(SelectMode::Freeform);
    freeform_stroke(&mut m, &[(100, 100), (200, 100), (200, 180), (120, 220)]);
    match m.finished() {
        Some(Finish::Selected { rect, shape: FinishShape::Freeform(pts), .. }) => {
            assert_eq!(*rect, Rect::new(100, 100, 100, 120));
            assert_eq!(pts.len(), 4);
            assert_eq!(pts[0], Point::new(100, 100));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn freeform_degenerate_strokes_are_rejected() {
    let mut m = model(SelectMode::Freeform);
    freeform_stroke(&mut m, &[(100, 100), (200, 100)]);
    assert!(m.finished().is_none(), "two points");
    freeform_stroke(&mut m, &[(100, 100), (200, 100), (300, 100)]);
    assert!(m.finished().is_none(), "collinear = zero area");
}

#[test]
fn freeform_drops_points_closer_than_two_pixels() {
    let mut m = model(SelectMode::Freeform);
    mv(&mut m, 10, 10);
    down_at(&mut m, 10, 10, 0);
    for x in 11..40 {
        mv(&mut m, x, 10);
    }
    assert!(m.scene().freeform.len() < 20);
    assert_eq!(m.scene().freeform[0], Point::new(10, 10));
}

#[test]
fn freeform_right_click_clears_stroke() {
    let mut m = model(SelectMode::Freeform);
    mv(&mut m, 10, 10);
    down_at(&mut m, 10, 10, 0);
    mv(&mut m, 60, 60);
    right_click(&mut m, 60, 60);
    assert!(m.scene().freeform.is_empty());
    assert!(m.finished().is_none());
    right_click(&mut m, 60, 60);
    assert_eq!(m.finished(), Some(&Finish::Cancelled));
}

// ------------------------------------------------------------------ snapping

#[test]
fn ctrl_snaps_created_edges_to_window_edges() {
    let mut m = model_with(
        B,
        vec![],
        vec![win("w", Rect::new(300, 200, 200, 200))],
        OverlayOptions { snap_to_windows: false, ..OverlayOptions::default() },
    );
    hold(&mut m, Key::Control, true);
    mv(&mut m, 100, 100);
    down_at(&mut m, 100, 100, 0);
    mv(&mut m, 296, 205);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 200, 100)));
    mv(&mut m, 500, 395);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 400, 300)), "snaps to right/bottom of window");
    mv(&mut m, 450, 350);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 350, 250)), "out of range: no snap");
    hold(&mut m, Key::Control, false);
    assert_eq!(m.selection(), Some(Rect::new(100, 100, 350, 250)));
    up(&mut m, 450, 350);
}

#[test]
fn ctrl_snaps_moved_selection_edges() {
    let mut m = model_with(
        B,
        vec![],
        vec![win("w", Rect::new(300, 200, 200, 200))],
        OverlayOptions { snap_to_windows: false, ..OverlayOptions::default() },
    );
    drag(&mut m, (100, 100), (200, 200));
    hold(&mut m, Key::Control, true);
    down_at(&mut m, 150, 150, 99_999);
    mv(&mut m, 246, 154); // rect.x = 196, right = 296 -> snaps right edge to 300 (x=200)
    assert_eq!(m.selection().unwrap().x, 200);
    up(&mut m, 246, 154);
}

// ------------------------------------------------------------------ scene / ui

#[test]
fn scene_reports_cursor_hints() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 100), (300, 300));
    mv(&mut m, 200, 200);
    assert_eq!(m.scene().cursor, CursorHint::Move);
    mv(&mut m, 100, 100);
    assert!(matches!(m.scene().cursor, CursorHint::Resize(_)));
    mv(&mut m, 600, 600);
    assert_eq!(m.scene().cursor, CursorHint::Crosshair);
}

#[test]
fn crosshair_hidden_until_pointer_seen_and_after_leave() {
    let mut m = model(SelectMode::Rect);
    assert_eq!(m.scene().crosshair, None);
    assert!(m.scene().loupe.is_none());
    mv(&mut m, 10, 10);
    assert_eq!(m.scene().crosshair, Some(Point::new(10, 10)));
    ptr(&mut m, PointerEvent::Leave);
    assert_eq!(m.scene().crosshair, None);
}

#[test]
fn per_monitor_ui_scale_follows_the_pointer() {
    let l = Rect::new(0, 0, 1000, 800);
    let r = Rect::new(1000, 0, 2000, 1600);
    let mut m = SelectionModel::new(ModelConfig::new(
        Rect::new(0, 0, 3000, 1600),
        vec![mon("a", l, 1.0), mon("b", r, 2.0)],
        vec![],
        OverlayOptions { ui_scale: UiScale::PerMonitor, ..OverlayOptions::default() },
        1.0,
    ));
    mv(&mut m, 10, 10);
    assert!((m.scene().ui_scale - 1.0).abs() < 1e-6);
    mv(&mut m, 1500, 10);
    assert!((m.scene().ui_scale - 2.0).abs() < 1e-6);
}

#[test]
fn loupe_and_label_stay_on_the_pointer_monitor() {
    let mut m = two_monitors(SelectMode::Rect);
    for (x, y) in [(-1919, 1), (-1, 1079), (0, 0), (2559, 1439), (1000, 700)] {
        mv(&mut m, x, y);
        let s = m.scene();
        let area = m.monitors().iter().find(|mo| mo.rect.contains(Point::new(x, y))).unwrap().rect;
        let l = s.loupe.unwrap();
        assert_eq!(l.outer.intersect(area), Some(l.outer), "loupe at {x},{y}: {:?}", l.outer);
        assert!(!l.outer.contains(Point::new(x, y)), "loupe covers the pointer at {x},{y}");
    }
    drag(&mut m, (-1000, 500), (-500, 800));
    let s = m.scene();
    let lab = s.label.unwrap();
    assert_eq!(lab.lines[0], "500 x 300");
    assert_eq!(lab.lines[1], "-1000, 500");
}

#[test]
fn label_goes_inside_when_selection_touches_the_top() {
    let mut m = model(SelectMode::Rect);
    drag(&mut m, (100, 0), (300, 100));
    let lab = m.scene().label.unwrap();
    assert!(lab.rect.y >= 0);
    assert!(Rect::new(100, 0, 200, 100).intersect(lab.rect).is_some(), "label sits inside");
}

// ------------------------------------------------------------------ properties

#[derive(Debug, Clone)]
enum Op {
    Move(i32, i32),
    Down(i32, i32, u8),
    Up(i32, i32, u8),
    Wheel(i32),
    Key(u8, bool),
    Leave,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (-50i32..1100, -50i32..900).prop_map(|(x, y)| Op::Move(x, y)),
        2 => (-50i32..1100, -50i32..900, 0u8..3).prop_map(|(x, y, b)| Op::Down(x, y, b)),
        2 => (-50i32..1100, -50i32..900, 0u8..3).prop_map(|(x, y, b)| Op::Up(x, y, b)),
        1 => (-3i32..4).prop_map(Op::Wheel),
        3 => (0u8..14, any::<bool>()).prop_map(|(k, p)| Op::Key(k, p)),
        1 => Just(Op::Leave),
    ]
}

fn to_event(op: &Op, t: u64) -> InputEvent {
    let button = |b: u8| match b {
        0 => PointerButton::Left,
        1 => PointerButton::Right,
        _ => PointerButton::Middle,
    };
    match *op {
        Op::Move(x, y) => InputEvent::Pointer(PointerEvent::Move { pos: Point::new(x, y) }),
        Op::Down(x, y, b) => InputEvent::Pointer(PointerEvent::Down { pos: Point::new(x, y), button: button(b), time_ms: t }),
        Op::Up(x, y, b) => InputEvent::Pointer(PointerEvent::Up { pos: Point::new(x, y), button: button(b) }),
        Op::Wheel(d) => InputEvent::Pointer(PointerEvent::Wheel { delta: d }),
        Op::Leave => InputEvent::Pointer(PointerEvent::Leave),
        Op::Key(k, pressed) => {
            let key = [
                Key::Escape, Key::Enter, Key::Tab, Key::Space, Key::Left, Key::Right, Key::Up,
                Key::Down, Key::Shift, Key::Control, Key::Alt, Key::Char('c'), Key::Char('x'), Key::Other,
            ][usize::from(k)];
            InputEvent::Key(KeyEvent { key, pressed })
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn random_streams_keep_invariants(
        mode in 0usize..5,
        ops in proptest::collection::vec(arb_op(), 0..80),
    ) {
        let mode = [SelectMode::Rect, SelectMode::Ellipse, SelectMode::Freeform, SelectMode::Monitor, SelectMode::Window][mode];
        let mut m = model_with(
            B,
            vec![mon("a", Rect::new(0, 0, 600, 800), 1.0), mon("b", Rect::new(600, 0, 400, 800), 1.0)],
            vec![win("w1", Rect::new(100, 100, 300, 300)), win("w2", Rect::new(-50, -50, 200, 200))],
            OverlayOptions { mode, snap_to_windows: true, ..OverlayOptions::default() },
        );
        let mut finished_seen: Option<Finish> = None;
        for (i, op) in ops.iter().enumerate() {
            m.handle(to_event(op, i as u64 * 50));
            if let Some(r) = m.selection() {
                prop_assert_eq!(r.intersect(B).map_or(0, Rect::area), r.area(), "selection {:?} escapes bounds", r);
            }
            let s = m.scene();
            if let Some(l) = &s.loupe {
                prop_assert!(l.outer.intersect(B) == Some(l.outer));
            }
            if let Some(l) = &s.label {
                prop_assert!(l.rect.intersect(B) == Some(l.rect));
            }
            if let Some(h) = s.highlight {
                prop_assert!(h.intersect(B) == Some(h));
            }
            for p in &s.freeform {
                prop_assert!(p.x >= 0 && p.x <= 1000 && p.y >= 0 && p.y <= 800);
            }
            match (&finished_seen, m.finished()) {
                (Some(a), Some(b)) => prop_assert_eq!(a, b, "outcome changed after finishing"),
                (Some(_), None) => prop_assert!(false, "finish was retracted"),
                (None, Some(b)) => finished_seen = Some(b.clone()),
                (None, None) => {}
            }
            if let Some(Finish::Selected { rect, .. }) = m.finished() {
                prop_assert!(!rect.is_empty());
                prop_assert_eq!(rect.intersect(B), Some(*rect));
            }
        }
    }

    #[test]
    fn plain_drag_is_exact_for_any_two_points(
        x0 in 0i32..999, y0 in 0i32..799, x1 in 0i32..999, y1 in 0i32..799,
    ) {
        prop_assume!((x0 - x1).abs() >= 4 || (y0 - y1).abs() >= 4);
        let mut m = model(SelectMode::Rect);
        mv(&mut m, x0, y0);
        down_at(&mut m, x0, y0, 0);
        mv(&mut m, x1, y1);
        up(&mut m, x1, y1);
        let want = Rect::from_points(Point::new(x0, y0), Point::new(x1, y1));
        if want.is_empty() {
            prop_assert_eq!(m.selection(), None);
        } else {
            prop_assert_eq!(m.selection(), Some(want));
        }
    }

    #[test]
    fn nudge_round_trip(dx in -30i32..30, dy in -30i32..30) {
        let mut m = model(SelectMode::Rect);
        drag(&mut m, (400, 300), (500, 400));
        let before = m.selection().unwrap();
        let k = |m: &mut SelectionModel, n: i32, pos: Key, neg: Key| {
            for _ in 0..n.abs() { key(m, if n > 0 { pos } else { neg }); }
        };
        k(&mut m, dx, Key::Right, Key::Left);
        k(&mut m, dy, Key::Down, Key::Up);
        k(&mut m, -dx, Key::Right, Key::Left);
        k(&mut m, -dy, Key::Down, Key::Up);
        prop_assert_eq!(m.selection(), Some(before));
    }
}
