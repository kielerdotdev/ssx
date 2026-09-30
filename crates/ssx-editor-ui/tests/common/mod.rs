//! Shared helpers for the UI tests: a synthetic "dashboard screenshot", app/harness builders,
//! and pointer/keyboard simulation helpers that go through egui's real event path.
#![allow(dead_code)] // each test binary uses a different subset

pub mod annotate;

use std::path::PathBuf;

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2, pos2};
use egui_kittest::Harness;
use ssx_editor::{
    Color, EditorSession, Fill, Object, ObjectId, ObjectKind, PointF, RectF, Style,
    object::{BoxShape, FontSpec, TextContent, TextShape},
};
use ssx_editor_ui::{app::EditorApp, document::EditorDoc};
use ssx_types::Frame;

fn rect_obj(x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) -> Object {
    Object::new(
        ObjectId(0),
        Style {
            stroke_width: 0.0,
            fill: Fill::solid(fill),
            corner_radius: radius,
            ..Style::default()
        },
        ObjectKind::Rectangle(BoxShape { rect: RectF::new(x, y, w, h), rotation: 0.0 }),
    )
}

fn text_obj(x: f32, y: f32, size: f32, color: Color, bold: bool, text: &str) -> Object {
    Object::new(
        ObjectId(0),
        Style { stroke_width: 0.0, ..Style::default() },
        ObjectKind::Text(TextShape {
            rect: RectF::new(x, y, 10.0, 10.0),
            rotation: 0.0,
            auto_width: true,
            content: TextContent {
                text: text.into(),
                font: FontSpec { size, bold, ..FontSpec::default() },
                color,
                padding: 0.0,
                ..TextContent::default()
            },
        }),
    )
}

/// A deterministic 1280x760 fake analytics dashboard: header, sidebar, KPI cards, a bar chart,
/// a table and a login-like form. Real text (rendered by the engine) so blur, magnify and
/// highlight demos look like a genuine screenshot.
pub fn dashboard() -> Frame {
    let base = ssx_imgfx::solid_frame(1280, 760, [244, 246, 250, 255]);
    let mut s = EditorSession::from_frame(base).expect("base image");
    let ink = Color::rgb(34, 40, 52);
    let dim = Color::rgb(112, 120, 138);
    let white = Color::WHITE;
    let mut add = |o: Object| {
        s.add_object(o);
    };
    // Header and sidebar.
    add(rect_obj(0.0, 0.0, 1280.0, 56.0, Color::rgb(24, 34, 64), 0.0));
    add(text_obj(24.0, 15.0, 24.0, white, true, "Acme Analytics"));
    add(text_obj(1080.0, 19.0, 15.0, Color::rgb(190, 200, 225), false, "marius@example.com"));
    add(rect_obj(0.0, 56.0, 210.0, 704.0, Color::rgb(232, 236, 244), 0.0));
    for (i, label) in
        ["Overview", "Traffic", "Keywords", "Backlinks", "Reports", "Settings"].iter().enumerate()
    {
        let y = 84.0 + i as f32 * 44.0;
        if i == 0 {
            add(rect_obj(12.0, y - 8.0, 186.0, 36.0, Color::rgb(210, 222, 250), 8.0));
        }
        add(text_obj(
            32.0,
            y,
            16.0,
            if i == 0 { Color::rgb(30, 70, 170) } else { ink },
            i == 0,
            label,
        ));
    }
    // KPI cards.
    for (i, (title, value, delta)) in [
        ("Visitors", "48,210", "+12.4%"),
        ("Revenue", "$9,840", "+3.1%"),
        ("Conversion", "4.7%", "-0.6%"),
    ]
    .iter()
    .enumerate()
    {
        let x = 240.0 + i as f32 * 340.0;
        add(rect_obj(x, 84.0, 320.0, 120.0, white, 14.0));
        add(text_obj(x + 24.0, 100.0, 15.0, dim, false, title));
        add(text_obj(x + 24.0, 128.0, 40.0, ink, true, value));
        add(text_obj(
            x + 24.0,
            176.0,
            15.0,
            if delta.starts_with('+') { Color::rgb(20, 150, 80) } else { Color::rgb(210, 60, 60) },
            true,
            delta,
        ));
    }
    // Bar chart card.
    add(rect_obj(240.0, 226.0, 660.0, 300.0, white, 14.0));
    add(text_obj(264.0, 242.0, 18.0, ink, true, "Traffic, last 12 weeks"));
    let heights =
        [90.0, 120.0, 105.0, 150.0, 140.0, 170.0, 160.0, 190.0, 175.0, 210.0, 200.0, 230.0];
    for (i, h) in heights.iter().enumerate() {
        let x = 268.0 + i as f32 * 51.0;
        add(rect_obj(
            x,
            500.0 - h,
            34.0,
            *h,
            if i == 11 { Color::rgb(60, 110, 240) } else { Color::rgb(160, 186, 245) },
            5.0,
        ));
    }
    // Form card (with an email and a "password" to hide).
    add(rect_obj(930.0, 226.0, 320.0, 300.0, white, 14.0));
    add(text_obj(954.0, 242.0, 18.0, ink, true, "Account"));
    add(text_obj(954.0, 284.0, 13.0, dim, false, "Email"));
    add(rect_obj(954.0, 304.0, 272.0, 38.0, Color::rgb(240, 243, 249), 8.0));
    add(text_obj(966.0, 314.0, 16.0, ink, false, "marius@example.com"));
    add(text_obj(954.0, 360.0, 13.0, dim, false, "API key"));
    add(rect_obj(954.0, 380.0, 272.0, 38.0, Color::rgb(240, 243, 249), 8.0));
    add(text_obj(966.0, 390.0, 16.0, ink, false, "sk_live_51Hx9aQ2eZvKp"));
    add(rect_obj(954.0, 446.0, 130.0, 42.0, Color::rgb(60, 110, 240), 10.0));
    add(text_obj(982.0, 457.0, 16.0, white, true, "Save"));
    // Table.
    add(rect_obj(240.0, 548.0, 1010.0, 190.0, white, 14.0));
    add(text_obj(264.0, 562.0, 18.0, ink, true, "Top pages"));
    for (i, (page, visits, share)) in [
        ("/pricing", "12,304", "25.5%"),
        ("/blog/seo-checklist", "9,871", "20.5%"),
        ("/features", "7,420", "15.4%"),
        ("/login", "5,113", "10.6%"),
    ]
    .iter()
    .enumerate()
    {
        let y = 598.0 + i as f32 * 32.0;
        if i % 2 == 0 {
            add(rect_obj(252.0, y - 6.0, 986.0, 30.0, Color::rgb(246, 248, 252), 6.0));
        }
        add(text_obj(264.0, y, 15.0, ink, false, page));
        add(text_obj(760.0, y, 15.0, ink, false, visits));
        add(text_obj(1000.0, y, 15.0, dim, false, share));
    }
    ssx_editor_ui::export::flatten(s.document())
}

/// A small flat image for tests that do not need the full dashboard.
pub fn flat(w: u32, h: u32) -> Frame {
    ssx_imgfx::solid_frame(w, h, [236, 238, 242, 255])
}

/// The app around an image that counts as saved (like a file opened from disk), with fake
/// services and default preferences.
pub fn app_for(frame: Frame) -> EditorApp {
    let mut doc = EditorDoc::from_frame(frame).expect("doc");
    doc.mark_saved();
    EditorApp::for_test(doc)
}

/// Like [`app_for`] but the image is a fresh capture that was never saved.
pub fn app_for_capture(frame: Frame) -> EditorApp {
    EditorApp::for_test(EditorDoc::from_frame(frame).expect("doc"))
}

/// Runs frames until the UI is idle. Continuous repaint requests (caret blink, toasts, the
/// spinner, progressive tile rendering) are normal here, so hitting the step limit is fine.
pub fn settle<S>(h: &mut Harness<'_, S>) {
    let _ = h.try_run();
}

/// Clicks the button `label` of the open dialog. Dialog buttons share names with toolbar
/// buttons ("Save"), so pick the match that is lowest on screen (dialogs are centred).
pub fn click_dialog_button(h: &mut Harness<'_, EditorApp>, label: &str) {
    use egui_kittest::kittest::Queryable;
    let node = h
        .get_all_by_label(label)
        .max_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .unwrap_or_else(|| panic!("no button {label}"));
    node.click();
    settle(h);
}

/// A harness running the whole window (`logic` + `show`) at `size` points.
pub fn window(app: EditorApp, size: impl Into<Vec2>) -> Harness<'static, EditorApp> {
    Harness::builder().with_size(size).with_max_steps(40).wgpu().build_ui_state(
        |ui, app: &mut EditorApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.show(ui);
        },
        app,
    )
}

/// Screen position (points) of an image-space point at the current view.
pub fn screen_of(h: &Harness<'_, EditorApp>, x: f32, y: f32) -> Pos2 {
    let app = h.state();
    let ppp = h.ctx.pixels_per_point();
    app.canvas.to_screen(app.canvas.last_rect, ppp, PointF::new(x, y), app.doc.doc())
}

/// Presses at `from`, moves through a few intermediate points, releases at `to`.
pub fn drag(h: &mut Harness<'_, EditorApp>, from: Pos2, to: Pos2) {
    drag_with(h, from, to, Modifiers::NONE);
}

/// [`drag`] with modifiers held.
pub fn drag_with(h: &mut Harness<'_, EditorApp>, from: Pos2, to: Pos2, m: Modifiers) {
    h.hover_at(from);
    h.step();
    h.event_modifiers(
        Event::PointerButton {
            pos: from,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: m,
        },
        m,
    );
    h.step();
    for i in 1..=4 {
        let t = i as f32 / 4.0;
        h.hover_at(pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t));
        h.step();
    }
    h.event_modifiers(
        Event::PointerButton {
            pos: to,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: m,
        },
        m,
    );
    h.step();
    settle(h);
}

/// A drag between two image-space points.
pub fn drag_image(h: &mut Harness<'_, EditorApp>, a: (f32, f32), b: (f32, f32)) {
    let (pa, pb) = (screen_of(h, a.0, a.1), screen_of(h, b.0, b.1));
    drag(h, pa, pb);
}

/// A click at an image-space point.
pub fn click_image(h: &mut Harness<'_, EditorApp>, p: (f32, f32)) {
    let pos = screen_of(h, p.0, p.1);
    h.hover_at(pos);
    h.step();
    h.event(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.step();
    h.event(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.step();
    settle(h);
}

/// Types text through egui's text event (as winit does for printable keys).
pub fn type_text(h: &mut Harness<'_, EditorApp>, text: &str) {
    h.event(Event::Text(text.to_owned()));
    h.step();
}

/// Where snapshot PNGs are written for inspection (`SSX_UI_DUMP`).
pub fn dump_dir() -> Option<PathBuf> {
    std::env::var_os("SSX_UI_DUMP").map(PathBuf::from)
}

/// Saves a rendering of the harness for a human to look at, when `SSX_UI_DUMP` is set.
pub fn dump(h: &mut Harness<'_, EditorApp>, name: &str) {
    if let Some(dir) = dump_dir() {
        let _ = std::fs::create_dir_all(&dir);
        match h.render() {
            Ok(img) => {
                let _ = img.save(dir.join(format!("{name}.png")));
            }
            Err(e) => eprintln!("cannot render {name}: {e}"),
        }
    }
}

/// The kinds of the objects in the document, bottom to top.
pub fn kinds(app: &EditorApp) -> Vec<&'static str> {
    app.doc.doc().objects().iter().map(|o| o.kind.name()).collect()
}

/// The canvas size of the open document as an `ssx_types::Size`.
pub fn canvas_size(app: &EditorApp) -> ssx_types::Size {
    let (w, h) = app.doc.doc().canvas_size();
    ssx_types::Size::new(w, h)
}
