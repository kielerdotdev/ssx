//! Canvas behaviour: partial rendering, checkerboard, pixel grid, `HiDPI`, snap guides, plus the
//! colour popup, history dropdown, cut-out and window title.

mod common;

use common::*;
use egui::{Event, Key, Modifiers, PointerButton};
use egui_kittest::kittest::Queryable;
use ssx_editor::{Color, Fill};
use ssx_editor_ui::{
    action::{Action, DialogKind},
    state::Dialog,
    tools::ToolId,
};

fn ready() -> egui_kittest::Harness<'static, ssx_editor_ui::app::EditorApp> {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    h
}

fn render(h: &mut egui_kittest::Harness<'_, ssx_editor_ui::app::EditorApp>) -> image::RgbaImage {
    h.render().expect("render")
}

#[test]
fn a_4k_image_only_renders_what_is_visible_and_dirty_rects_are_partial() {
    let mut h =
        window(app_for(ssx_imgfx::solid_frame(3840, 2160, [200, 210, 225, 255])), [1280.0, 800.0]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    settle(&mut h);
    let s = h.state().canvas.stats;
    let full = 3840u64 * 2160;
    assert!(
        s.pixels_rendered < full / 2,
        "rendered {} of {full} pixels for a 1280x680 view",
        s.pixels_rendered
    );
    assert!(s.pixels_rendered > 1_000_000, "the visible area must have been rendered");

    h.key_press(Key::R);
    settle(&mut h);
    let before = h.state().canvas.stats.pixels_rendered;
    let vis = h.state().canvas.viewport.visible_canvas_rect();
    let (x, y) = (vis.min.x + 200.0, vis.min.y + 200.0);
    drag_image(&mut h, (x, y), (x + 60.0, y + 40.0));
    let extra = h.state().canvas.stats.pixels_rendered - before;
    assert!(extra > 0, "the new rectangle is painted");
    assert!(extra < 250_000, "a small edit re-rendered {extra} pixels (a tile is 262144)");
    // And the picture really shows the rectangle at the dragged place (deselect so the
    // selection outline does not cover its edge).
    h.key_press(Key::Escape);
    settle(&mut h);
    let img = render(&mut h);
    let p = screen_of(&h, x, y + 10.0);
    let px = img.get_pixel(p.x.round() as u32, p.y.round() as u32);
    assert!(px[0] > 180 && px[1] < 90 && px[2] < 90, "left edge of the red rectangle: {px:?}");
}

#[test]
fn scrolling_renders_only_the_newly_exposed_tiles() {
    let mut h =
        window(app_for(ssx_imgfx::solid_frame(3840, 2160, [200, 210, 225, 255])), [1280.0, 800.0]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    settle(&mut h);
    let before = h.state().canvas.stats.pixels_rendered;
    h.state_mut().canvas.viewport.pan_by(egui::vec2(-300.0, 0.0));
    settle(&mut h);
    let extra = h.state().canvas.stats.pixels_rendered - before;
    assert!(extra < 3 * 512 * 512 * 2, "panning by 300 px rendered {extra} pixels");
}

#[test]
fn transparent_pixels_show_a_checkerboard() {
    let mut app = app_for(ssx_imgfx::solid_frame(400, 300, [0, 0, 0, 0]));
    app.doc.session.set_background(Fill::None).unwrap();
    let mut h = window(app, [1000.0, 640.0]);
    settle(&mut h);
    let img = render(&mut h);
    let c = h.state().canvas.viewport.content_screen_rect();
    let mid = c.center();
    let mut seen = std::collections::HashSet::new();
    for dx in 0..40 {
        seen.insert(img.get_pixel(mid.x as u32 + dx, mid.y as u32).0);
    }
    assert!(seen.len() >= 2, "expected two checker colours, got {seen:?}");
    assert!(
        seen.iter().all(|p| p[0] < 130 && p[0] > 70),
        "checker colours should be mid greys: {seen:?}"
    );
}

#[test]
fn pixel_grid_appears_at_high_zoom_and_can_be_switched_off() {
    let mut app = app_for(flat(200, 150));
    app.state.prefs.pixel_grid = true;
    let mut h = window(app, [1000.0, 640.0]);
    settle(&mut h);
    for _ in 0..7 {
        h.key_press_modifiers(Modifiers::COMMAND, Key::Equals);
        settle(&mut h);
    }
    assert!(
        h.state().canvas.viewport.show_pixel_grid(),
        "zoom {}",
        h.state().canvas.viewport.zoom()
    );
    let with_grid = render(&mut h);
    h.state_mut().state.push(Action::TogglePixelGrid);
    settle(&mut h);
    let without = render(&mut h);
    let mut diff = 0;
    for (a, b) in with_grid.pixels().zip(without.pixels()) {
        if a != b {
            diff += 1;
        }
    }
    assert!(diff > 2000, "the grid should change a lot of pixels ({diff})");
}

#[test]
fn hidpi_2x_renders_at_physical_resolution_and_maps_input_correctly() {
    let mut h = window(app_for(dashboard()), [2000.0, 1400.0]);
    h.set_pixels_per_point(2.0);
    settle(&mut h);
    let app = h.state();
    let rect = app.canvas.last_rect;
    let view = app.canvas.viewport.view();
    assert!(
        (view.x - rect.width() * 2.0).abs() < 1.0 && (view.y - rect.height() * 2.0).abs() < 1.0,
        "the viewport is in physical pixels"
    );
    // The image is drawn crisply: 1 image px = zoom physical px, fitted into the physical view.
    let z = app.canvas.viewport.zoom();
    assert!(z > 0.8 && z < 1.6, "zoom {z}, rect {rect:?}, view {view:?}");
    h.key_press(Key::R);
    settle(&mut h);
    drag_image(&mut h, (300.0, 120.0), (560.0, 300.0));
    let o = &h.state().doc.doc().objects()[0];
    let b = o.bounds();
    assert!(
        (b.x - 300.0).abs() < 2.0 && (b.w - 260.0).abs() < 2.0 && (b.h - 180.0).abs() < 2.0,
        "{b:?}"
    );
    let img = render(&mut h);
    assert_eq!((img.width(), img.height()), (2000, 1400), "the snapshot is at 2 pixels per point");
}

#[test]
fn ctrl_drag_shows_snap_guides() {
    let mut h = ready();
    h.key_press(Key::R);
    settle(&mut h);
    drag_image(&mut h, (300.0, 300.0), (500.0, 400.0));
    // Second rectangle, Ctrl held, whose edge passes within snapping range of the first's.
    let (a, b) = (screen_of(&h, 302.0, 500.0), screen_of(&h, 497.0, 403.0));
    let m = Modifiers::COMMAND;
    // Hold Ctrl for the whole drag (kittest resets modifiers after `event_modifiers`).
    h.input_mut().modifiers = m;
    h.hover_at(a);
    h.step();
    h.event(Event::PointerButton {
        pos: a,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: m,
    });
    h.step();
    h.hover_at(b);
    h.step();
    let guides = h.state_mut().doc.session.overlay().guides;
    assert!(
        !guides.is_empty(),
        "Ctrl-snapping should raise a guide near the first rectangle's edge"
    );
    let img = render(&mut h);
    let magenta = img.pixels().filter(|p| p[0] > 230 && p[1] < 110 && p[2] > 150).count();
    assert!(magenta > 50, "the guide is drawn ({magenta} px)");
    h.event(Event::PointerButton {
        pos: b,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: m,
    });
    h.step();
    h.input_mut().modifiers = Modifiers::NONE;
}

#[test]
fn colour_popup_applies_a_palette_colour_and_remembers_it() {
    let mut h = ready();
    h.key_press(Key::R);
    settle(&mut h);
    h.get_by_label("Stroke colour").click();
    settle(&mut h);
    let blue = Color::rgb(30, 110, 240);
    h.get_by_label(blue.to_hex().as_str()).click();
    settle(&mut h);
    let preset = h.state().doc.session.styles().get(ssx_editor::Tool::Rectangle).unwrap();
    assert_eq!(preset.style.stroke, blue);
    // Closing the popup records the colour as recent.
    h.key_press(Key::Escape);
    settle(&mut h);
    assert!(
        h.state().state.prefs.recent_colors.contains(&Color::rgb(255, 0, 0))
            || h.state().state.prefs.recent_colors.contains(&blue)
    );
}

#[test]
fn colour_popup_eyedropper_button_arms_the_eyedropper() {
    let mut h = ready();
    h.key_press(Key::R);
    settle(&mut h);
    h.get_by_label("Stroke colour").click();
    settle(&mut h);
    h.get_by_label("Pick a colour from the image").click();
    settle(&mut h);
    assert_eq!(h.state().state.eyedropper, Some(ssx_editor_ui::props::ColorField::Stroke));
}

#[test]
fn history_dropdown_undoes_several_steps_at_once() {
    let mut h = ready();
    for (k, x) in [(Key::R, 300.0), (Key::E, 420.0), (Key::L, 540.0)] {
        h.key_press(k);
        settle(&mut h);
        drag_image(&mut h, (x, 300.0), (x + 90.0, 380.0));
    }
    assert_eq!(h.state().doc.doc().objects().len(), 3);
    h.get_by_label("Undo history").click();
    settle(&mut h);
    // Entries are newest first: "1. Add line", "2. Add ellipse", "3. Add rectangle".
    h.get_by_label_contains("2. Add ellipse").click();
    settle(&mut h);
    assert_eq!(kinds(h.state()), ["rectangle"], "two steps undone");
    assert_eq!(h.state().doc.log.redo_len(), 2);
}

#[test]
fn cut_out_by_dialog_and_by_tool() {
    let mut h = ready();
    h.state_mut().state.push(Action::OpenDialog(DialogKind::CutOut));
    settle(&mut h);
    if let Some(Dialog::CutOut(f)) = &mut h.state_mut().state.dialog {
        f.start = 100;
        f.end = 200;
    } else {
        panic!("cut-out dialog expected");
    }
    settle(&mut h);
    click_dialog_button(&mut h, "Cut out");
    assert_eq!(h.state().doc.doc().image_size(), (1180, 760));

    h.state_mut().state.push(Action::SetTool(ToolId::CutOut));
    settle(&mut h);
    let (a, b) = (screen_of(&h, 300.0, 200.0), screen_of(&h, 400.0, 210.0));
    drag(&mut h, a, b);
    let (w, _) = h.state().doc.doc().image_size();
    assert!((1070..=1090).contains(&w), "a ~100 px strip was removed: {w}");
}

#[test]
fn window_title_carries_the_dirty_marker() {
    let mut h = ready();
    assert_eq!(h.state().window_title(), "Untitled - ssx editor");
    h.key_press(Key::R);
    settle(&mut h);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    settle(&mut h);
    assert_eq!(h.state().window_title(), "Untitled* - ssx editor");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    settle(&mut h);
    assert_eq!(h.state().window_title(), "Untitled - ssx editor");
}

#[test]
fn opening_a_named_file_titles_the_window_with_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("report.png");
    flat(80, 60).save(&p).unwrap();
    let doc = ssx_editor_ui::document::EditorDoc::open(&p).unwrap();
    let mut h = window(ssx_editor_ui::app::EditorApp::for_test(doc), [1000.0, 640.0]);
    settle(&mut h);
    assert_eq!(h.state().window_title(), "report.png - ssx editor");
}
