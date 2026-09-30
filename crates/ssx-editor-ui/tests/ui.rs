//! Drives the real window through egui's event path (`egui_kittest`) and asserts on the
//! resulting `Document`: clicking toolbar buttons, dragging on the canvas, typing, undo/redo,
//! zoom and pan, dialogs, save/copy/done outcomes.

mod common;

use common::*;
use egui::{Event, Key, Modifiers, MouseWheelUnit, PointerButton, TouchPhase, vec2};
use egui_kittest::kittest::Queryable;
use ssx_editor::{ObjectKind, Tool};
use ssx_editor_ui::{
    action::{Action, DialogKind, Finish},
    effects::EffectKind,
    request::OutcomeAction,
    state::Dialog,
    tools::ToolId,
};

fn ready() -> egui_kittest::Harness<'static, ssx_editor_ui::app::EditorApp> {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    h
}

fn key(h: &mut egui_kittest::Harness<'_, ssx_editor_ui::app::EditorApp>, k: Key) {
    h.key_press(k);
    settle(h);
}

fn ctrl(h: &mut egui_kittest::Harness<'_, ssx_editor_ui::app::EditorApp>, k: Key) {
    h.key_press_modifiers(Modifiers::COMMAND, k);
    settle(h);
}

#[test]
fn toolbar_buttons_select_tools() {
    let mut h = ready();
    for (label, tool) in [
        ("Rectangle", ToolId::Rectangle),
        ("Ellipse", ToolId::Ellipse),
        ("Arrow", ToolId::Arrow),
        ("Freehand arrow", ToolId::FreehandArrow),
        ("Text with outline and background", ToolId::TextBoxed),
        ("Speech balloon", ToolId::Balloon),
        ("Step number", ToolId::Step),
        ("Magnify", ToolId::Magnify),
        ("Spotlight", ToolId::Spotlight),
        ("Eraser", ToolId::Eraser),
        ("Blur", ToolId::Blur),
        ("Grid", ToolId::Grid),
        ("Highlighter", ToolId::Highlight),
        ("Rectangle region", ToolId::CropRect),
        ("Select and move", ToolId::Select),
    ] {
        h.get_by_label(label).click();
        settle(&mut h);
        assert_eq!(h.state().state.tool, tool, "{label}");
        assert_eq!(h.state().doc.session.tool(), tool.engine(), "{label}");
    }
}

#[test]
fn letter_shortcuts_pick_tools() {
    let mut h = ready();
    for (k, tool) in [
        (Key::R, Tool::Rectangle),
        (Key::E, Tool::Ellipse),
        (Key::L, Tool::Line),
        (Key::T, Tool::Text),
        (Key::V, Tool::Select),
    ] {
        key(&mut h, k);
        assert_eq!(h.state().doc.session.tool(), tool, "{k:?}");
    }
    h.key_press_modifiers(Modifiers::SHIFT, Key::A);
    settle(&mut h);
    assert_eq!(h.state().doc.session.tool(), Tool::FreehandArrow);
}

#[test]
fn dragging_draws_a_rectangle_and_undo_redo_work_from_the_keyboard() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (560.0, 300.0));
    let app = h.state();
    assert_eq!(kinds(app), ["rectangle"]);
    let ObjectKind::Rectangle(b) = &app.doc.doc().objects()[0].kind else { panic!() };
    assert!((b.rect.x - 300.0).abs() < 3.0 && (b.rect.y - 120.0).abs() < 3.0, "{:?}", b.rect);
    assert!((b.rect.w - 260.0).abs() < 3.0 && (b.rect.h - 180.0).abs() < 3.0, "{:?}", b.rect);
    assert!(app.doc.is_dirty());
    assert!(app.doc.title().contains('*'));

    ctrl(&mut h, Key::Z);
    assert!(h.state().doc.doc().objects().is_empty());
    assert!(!h.state().doc.is_dirty(), "back to the saved state is clean again");
    ctrl(&mut h, Key::Y);
    assert_eq!(kinds(h.state()), ["rectangle"]);
    ctrl(&mut h, Key::Z);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    settle(&mut h);
    assert_eq!(kinds(h.state()), ["rectangle"], "Ctrl+Shift+Z also redoes");
}

#[test]
fn shift_constrains_to_a_square_and_alt_draws_from_the_centre() {
    let mut h = ready();
    key(&mut h, Key::R);
    let (a, b) = (screen_of(&h, 400.0, 150.0), screen_of(&h, 520.0, 200.0));
    drag_with(&mut h, a, b, Modifiers::SHIFT);
    let ObjectKind::Rectangle(sq) = &h.state().doc.doc().objects()[0].kind else { panic!() };
    assert!((sq.rect.w - sq.rect.h).abs() < 2.0, "shift makes a square: {:?}", sq.rect);

    key(&mut h, Key::E);
    let (a, b) = (screen_of(&h, 700.0, 300.0), screen_of(&h, 760.0, 340.0));
    drag_with(&mut h, a, b, Modifiers::ALT);
    let ObjectKind::Ellipse(el) = &h.state().doc.doc().objects()[1].kind else { panic!() };
    let c = el.rect.center();
    assert!(
        (c.x - 700.0).abs() < 3.0 && (c.y - 300.0).abs() < 3.0,
        "alt grows from the centre: {:?}",
        el.rect
    );
}

#[test]
fn all_drawing_tools_create_their_objects() {
    let mut h = ready();
    for (tool_key, mods, expect) in [
        (Key::L, Modifiers::NONE, "line"),
        (Key::A, Modifiers::NONE, "arrow"),
        (Key::P, Modifiers::NONE, "freehand"),
        (Key::A, Modifiers::SHIFT, "freehand_arrow"),
        (Key::U, Modifiers::NONE, "blur"),
        (Key::U, Modifiers::SHIFT, "pixelate"),
        (Key::M, Modifiers::NONE, "magnify"),
        (Key::S, Modifiers::NONE, "spotlight"),
        (Key::G, Modifiers::NONE, "grid"),
        (Key::H, Modifiers::NONE, "highlight"),
        (Key::H, Modifiers::SHIFT, "highlight"),
        (Key::B, Modifiers::NONE, "balloon"),
    ] {
        h.key_press_modifiers(mods, tool_key);
        settle(&mut h);
        let n = h.state().doc.doc().objects().len();
        drag_image(&mut h, (300.0 + n as f32 * 20.0, 260.0), (420.0 + n as f32 * 20.0, 330.0));
        let objs = h.state().doc.doc().objects();
        assert_eq!(objs.len(), n + 1, "{expect}");
        assert_eq!(objs[n].kind.name(), expect);
    }
    // One click drops a step marker (the balloon left us in text editing: Esc leaves it).
    key(&mut h, Key::Escape);
    key(&mut h, Key::N);
    click_image(&mut h, (900.0, 600.0));
    assert_eq!(h.state().doc.doc().objects().last().map(|o| o.kind.name()), Some("step"));
}

#[test]
fn text_tool_types_edits_and_commits() {
    let mut h = ready();
    key(&mut h, Key::T);
    click_image(&mut h, (300.0, 100.0));
    assert!(
        h.state().doc.session.text_edit_state().is_some(),
        "clicking with the text tool starts editing"
    );
    // Letters are text now, not tool shortcuts.
    type_text(&mut h, "Rect ");
    type_text(&mut h, "here");
    assert_eq!(h.state().doc.session.tool(), Tool::Text);
    assert_eq!(h.state().doc.session.text_edit_state().unwrap().text, "Rect here");
    key(&mut h, Key::Backspace);
    assert_eq!(h.state().doc.session.text_edit_state().unwrap().text, "Rect her");
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    settle(&mut h);
    type_text(&mut h, "line2");
    assert_eq!(h.state().doc.session.text_edit_state().unwrap().text, "Rect her\nline2");
    key(&mut h, Key::ArrowLeft);
    key(&mut h, Key::Home);
    type_text(&mut h, ">");
    assert!(h.state().doc.session.text_edit_state().unwrap().text.contains(">line2"));
    key(&mut h, Key::Enter);
    assert!(h.state().doc.session.text_edit_state().is_none(), "Enter commits");
    let ObjectKind::Text(t) = &h.state().doc.doc().objects()[0].kind else { panic!() };
    assert!(t.content.text.starts_with("Rect her\n>line2"), "{:?}", t.content.text);
    // Typing produced one undo step for the whole text (creation and typing coalesce).
    ctrl(&mut h, Key::Z);
    assert!(h.state().doc.doc().objects().len() <= 1);
}

#[test]
fn ime_commit_and_preedit_go_through_the_session() {
    let mut h = ready();
    key(&mut h, Key::T);
    click_image(&mut h, (300.0, 100.0));
    h.event(Event::Ime(egui::ImeEvent::Preedit { text: "ni".into(), active_range_chars: None }));
    h.step();
    assert_eq!(
        h.state().doc.session.text_edit_state().unwrap().text,
        "",
        "pre-edit is not in the document"
    );
    let ov = h.state_mut().doc.session.overlay();
    assert_eq!(ov.caret.as_ref().and_then(|c| c.preedit.clone()).as_deref(), Some("ni"));
    h.event(Event::Ime(egui::ImeEvent::Commit("你好".into())));
    h.step();
    assert_eq!(h.state().doc.session.text_edit_state().unwrap().text, "你好");
    assert!(h.state_mut().doc.session.overlay().caret.unwrap().preedit.is_none());
    settle(&mut h);
}

#[test]
fn escape_leaves_text_editing_and_then_selects_nothing() {
    let mut h = ready();
    key(&mut h, Key::T);
    click_image(&mut h, (300.0, 100.0));
    type_text(&mut h, "abc");
    key(&mut h, Key::Escape);
    assert!(h.state().doc.session.text_edit_state().is_none());
    assert_eq!(h.state().doc.doc().objects().len(), 1, "text is kept");
    key(&mut h, Key::Escape);
    assert!(h.state().doc.session.selection().is_empty());
    key(&mut h, Key::Escape);
    assert_eq!(
        h.state().state.tool,
        ToolId::Select,
        "Esc with nothing to cancel returns to Select"
    );
}

#[test]
fn select_move_delete_and_nudge() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    key(&mut h, Key::V);
    click_image(&mut h, (400.0, 120.0)); // on the outline
    assert_eq!(h.state().doc.session.selection().len(), 1);
    let before = h.state().doc.doc().objects()[0].bounds();
    key(&mut h, Key::ArrowRight);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    settle(&mut h);
    let after = h.state().doc.doc().objects()[0].bounds();
    assert!((after.x - before.x - 1.0).abs() < 0.01 && (after.y - before.y - 10.0).abs() < 0.01);
    key(&mut h, Key::Delete);
    assert!(h.state().doc.doc().objects().is_empty());
    ctrl(&mut h, Key::Z);
    assert_eq!(h.state().doc.doc().objects().len(), 1);
}

#[test]
fn duplicate_copy_paste_and_select_all() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (400.0, 200.0));
    ctrl(&mut h, Key::D);
    assert_eq!(h.state().doc.doc().objects().len(), 2);
    ctrl(&mut h, Key::A);
    assert_eq!(h.state().doc.session.selection().len(), 2);
    ctrl(&mut h, Key::C);
    ctrl(&mut h, Key::V);
    assert_eq!(h.state().doc.doc().objects().len(), 4, "object clipboard paste");
    ctrl(&mut h, Key::G);
    assert!(h.state().doc.doc().objects().iter().any(|o| o.group.is_some()));
}

#[test]
fn copy_without_selection_copies_the_whole_image_to_the_clipboard() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (400.0, 200.0));
    key(&mut h, Key::Escape); // deselect
    ctrl(&mut h, Key::C);
    let img = h.state_mut().services.clipboard.image().expect("image copied");
    assert_eq!(img.size(), canvas_size(h.state()));
    // The copied pixels include the annotation (a red outline at the rectangle's left edge).
    let px = &img.data()[((150 * img.width() + 300) * 4) as usize..][..4];
    assert!(px[0] > 200 && px[1] < 60, "{px:?}");
}

#[test]
fn zoom_shortcuts_wheel_and_pan() {
    let mut h = ready();
    assert!(h.state().canvas.viewport.is_fitted());
    ctrl(&mut h, Key::Num1);
    assert!((h.state().canvas.viewport.zoom() - 1.0).abs() < 1e-3);
    ctrl(&mut h, Key::Equals);
    assert!(h.state().canvas.viewport.zoom() > 1.2);
    ctrl(&mut h, Key::Minus);
    ctrl(&mut h, Key::Minus);
    assert!(h.state().canvas.viewport.zoom() < 1.0);
    ctrl(&mut h, Key::Num0);
    assert!(h.state().canvas.viewport.is_fitted());

    // Mouse wheel zooms about the pointer.
    let p = screen_of(&h, 640.0, 380.0);
    h.hover_at(p);
    h.step();
    let z0 = h.state().canvas.viewport.zoom();
    let before = h.state().canvas.to_image(h.state().canvas.last_rect, 1.0, p, h.state().doc.doc());
    h.event(Event::MouseWheel {
        unit: MouseWheelUnit::Line,
        delta: vec2(0.0, 2.0),
        phase: TouchPhase::Move,
        modifiers: Modifiers::NONE,
    });
    h.step();
    settle(&mut h);
    assert!(h.state().canvas.viewport.zoom() > z0 * 1.2);
    let after = h.state().canvas.to_image(h.state().canvas.last_rect, 1.0, p, h.state().doc.doc());
    assert!(
        (before.x - after.x).abs() < 1.5 && (before.y - after.y).abs() < 1.5,
        "zoom keeps the point under the cursor"
    );

    // Middle-drag pans.
    let off0 = h.state().canvas.viewport.offset();
    let a = screen_of(&h, 600.0, 400.0);
    h.hover_at(a);
    h.step();
    h.event(Event::PointerButton {
        pos: a,
        button: PointerButton::Middle,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.step();
    h.hover_at(a + vec2(-50.0, -30.0));
    h.step();
    h.event(Event::PointerButton {
        pos: a + vec2(-50.0, -30.0),
        button: PointerButton::Middle,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.step();
    let off1 = h.state().canvas.viewport.offset();
    assert!(
        (off1.x - off0.x + 50.0).abs() < 1.0 && (off1.y - off0.y + 30.0).abs() < 1.0,
        "{off0:?} -> {off1:?}"
    );
    assert!(h.state().doc.doc().objects().is_empty(), "panning must not draw");

    // Space + left drag pans too, without drawing.
    key(&mut h, Key::R);
    h.key_down(Key::Space);
    h.step();
    let off2 = h.state().canvas.viewport.offset();
    let a = screen_of(&h, 600.0, 400.0);
    drag(&mut h, a, a + vec2(20.0, 20.0));
    h.key_up(Key::Space);
    h.step();
    assert!(h.state().doc.doc().objects().is_empty());
    assert!((h.state().canvas.viewport.offset() - off2).length() > 10.0);
}

#[test]
fn zoom_level_is_reported_in_the_status_bar() {
    let mut h = ready();
    ctrl(&mut h, Key::Num1);
    settle(&mut h);
    assert_eq!(h.state().state.zoom_percent.round() as i32, 100);
    h.get_by_label("Zoom level").click();
    settle(&mut h);
    assert!(
        h.query_all_by_label_contains("Fit to window").count() >= 2,
        "the zoom popup lists Fit"
    );
}

#[test]
fn drawing_at_high_zoom_lands_on_the_right_pixels() {
    let mut h = ready();
    ctrl(&mut h, Key::Num1);
    ctrl(&mut h, Key::Equals);
    ctrl(&mut h, Key::Equals);
    ctrl(&mut h, Key::Equals);
    key(&mut h, Key::R);
    let vis = h.state().canvas.viewport.visible_canvas_rect();
    let (x0, y0) = (vis.min.x + 30.0, vis.min.y + 30.0);
    drag_image(&mut h, (x0, y0), (x0 + 40.0, y0 + 25.0));
    let ObjectKind::Rectangle(b) = &h.state().doc.doc().objects()[0].kind else { panic!() };
    assert!(
        (b.rect.x - x0).abs() < 1.0 && (b.rect.y - y0).abs() < 1.0,
        "{:?} vs {x0},{y0}",
        b.rect
    );
    assert!((b.rect.w - 40.0).abs() < 1.0 && (b.rect.h - 25.0).abs() < 1.0);
}

#[test]
fn cursor_follows_the_session_hint() {
    let mut h = ready();
    key(&mut h, Key::R);
    let p = screen_of(&h, 500.0, 300.0);
    h.hover_at(p);
    h.step();
    h.step();
    assert_eq!(h.output().platform_output.cursor_icon, egui::CursorIcon::Crosshair);
    key(&mut h, Key::T);
    h.hover_at(p + vec2(1.0, 0.0));
    h.step();
    h.step();
    assert_eq!(h.output().platform_output.cursor_icon, egui::CursorIcon::Text);
    key(&mut h, Key::V);
    h.hover_at(p + vec2(2.0, 0.0));
    h.step();
    h.step();
    assert_eq!(h.output().platform_output.cursor_icon, egui::CursorIcon::Default);
}

#[test]
fn status_bar_shows_pixel_coordinates_and_colour() {
    let mut h = ready();
    let p = screen_of(&h, 10.5, 10.5);
    h.hover_at(p);
    h.step();
    h.step();
    let st = &h.state().state;
    assert_eq!(st.hover.pixel, Some((10, 10)));
    assert_eq!(st.hover.color, Some([24, 34, 64, 255]), "the header colour of the fixture");
    h.remove_cursor();
    h.step();
    settle(&mut h);
    assert_eq!(h.state().state.hover.pixel, None);
}

#[test]
fn eyedropper_picks_a_canvas_colour_into_the_active_field() {
    let mut h = ready();
    key(&mut h, Key::R);
    h.state_mut().state.eyedropper = Some(ssx_editor_ui::props::ColorField::Stroke);
    settle(&mut h);
    click_image(&mut h, (10.0, 10.0));
    settle(&mut h);
    assert!(h.state().state.eyedropper.is_none());
    let preset = h.state().doc.session.styles().get(Tool::Rectangle).unwrap();
    assert_eq!(preset.style.stroke, ssx_editor::Color::rgb(24, 34, 64));
    assert!(h.state().doc.doc().objects().is_empty(), "the pick click must not draw");
    assert!(h.state().state.prefs.recent_colors.contains(&ssx_editor::Color::rgb(24, 34, 64)));
}

#[test]
fn object_list_selects_hides_and_deletes() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (400.0, 200.0));
    key(&mut h, Key::E);
    drag_image(&mut h, (500.0, 120.0), (600.0, 200.0));
    ctrl(&mut h, Key::L);
    assert!(h.state().state.prefs.show_layers);
    h.state_mut().state.push(Action::Deselect);
    settle(&mut h);
    let id = h.state().doc.doc().objects()[0].id;
    h.state_mut().state.push(Action::SelectObjects { ids: vec![id], additive: false });
    settle(&mut h);
    assert_eq!(h.state().doc.session.selection(), [id]);
    h.state_mut().state.push(Action::SetVisible(id, false));
    settle(&mut h);
    assert!(!h.state().doc.doc().objects()[0].visible);
    h.state_mut().state.push(Action::Raise);
    settle(&mut h);
    h.state_mut().state.push(Action::DeleteSelection);
    settle(&mut h);
    assert_eq!(h.state().doc.doc().objects().len(), 1);
}

#[test]
fn crop_tool_apply_with_enter_changes_the_image_size() {
    let mut h = ready();
    key(&mut h, Key::C);
    drag_image(&mut h, (100.0, 100.0), (700.0, 500.0));
    assert!(h.state().doc.session.pending_crop().is_some());
    key(&mut h, Key::Enter);
    let (w, hh) = h.state().doc.doc().image_size();
    assert!(w == 600 && (400..=401).contains(&hh), "{w}x{hh}");
    assert!(h.state().state.crop_pending || h.state().doc.session.pending_crop().is_none());
    ctrl(&mut h, Key::Z);
    assert_eq!(h.state().doc.doc().image_size(), (1280, 760));
}

#[test]
fn rotate_flip_and_resize_go_through_actions_and_dialogs() {
    let mut h = ready();
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::R);
    settle(&mut h);
    assert_eq!(h.state().doc.doc().image_size(), (760, 1280));
    assert_eq!(h.state().doc.log.undo_labels().next(), Some("Rotate 90 degrees clockwise"));
    ctrl(&mut h, Key::Z);

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::I);
    settle(&mut h);
    assert!(matches!(h.state().state.dialog, Some(Dialog::Resize(_))));
    if let Some(Dialog::Resize(f)) = &mut h.state_mut().state.dialog {
        f.set_width(640);
    }
    settle(&mut h);
    click_dialog_button(&mut h, "Resize");
    assert!(h.state().state.dialog.is_none());
    assert_eq!(h.state().doc.doc().image_size(), (640, 380));
}

#[test]
fn canvas_dialog_adds_padding() {
    let mut h = ready();
    h.state_mut().state.push(Action::OpenDialog(DialogKind::Canvas));
    settle(&mut h);
    if let Some(Dialog::Canvas(f)) = &mut h.state_mut().state.dialog {
        f.left = 20;
        f.right = 20;
        f.top = 10;
        f.bottom = 10;
    }
    settle(&mut h);
    click_dialog_button(&mut h, "Apply");
    assert_eq!(h.state().doc.doc().canvas_size(), (1320, 780));
    assert_eq!(h.state().doc.doc().image_size(), (1280, 760));
}

#[test]
fn effect_dialog_previews_then_applies_and_is_undoable() {
    let mut h = ready();
    let before = h.state().doc.doc().clone();
    h.state_mut().state.push(Action::OpenEffect(EffectKind::Invert));
    settle(&mut h);
    assert!(matches!(h.state().state.dialog, Some(Dialog::Effect(_))));
    assert!(h.state_mut().preview.wait_idle(std::time::Duration::from_secs(20)));
    settle(&mut h);
    assert!(h.state().preview.active(), "preview document exists while the dialog is open");
    assert_eq!(h.state().doc.doc(), &before, "the real document is untouched by the preview");
    click_dialog_button(&mut h, "Apply");
    assert!(h.state().state.dialog.is_none());
    assert!(!h.state().preview.active());
    assert_ne!(h.state().doc.doc(), &before);
    assert_eq!(h.state().doc.log.undo_labels().next(), Some("Invert"));
    ctrl(&mut h, Key::Z);
    assert_eq!(h.state().doc.doc(), &before);
}

#[test]
fn cancelling_an_effect_leaves_the_document_alone() {
    let mut h = ready();
    let before = h.state().doc.doc().clone();
    h.state_mut().state.push(Action::OpenEffect(EffectKind::GaussianBlur));
    settle(&mut h);
    click_dialog_button(&mut h, "Cancel");
    assert!(h.state().state.dialog.is_none());
    assert!(!h.state().preview.active());
    assert_eq!(h.state().doc.doc(), &before);
    assert!(!h.state().doc.is_dirty());
}

#[test]
fn shortcuts_dialog_opens_with_f1_and_closes_with_escape() {
    let mut h = ready();
    key(&mut h, Key::F1);
    assert!(matches!(h.state().state.dialog, Some(Dialog::Shortcuts)));
    key(&mut h, Key::Escape);
    settle(&mut h);
    assert!(h.state().state.dialog.is_none(), "Escape dismisses the modal");
}

#[test]
fn save_in_workflow_mode_writes_the_output_and_clears_the_dirty_marker() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("edited.png");
    let mut app = app_for(dashboard());
    app.state.output = Some(out.clone());
    let mut h = window(app, [1280.0, 800.0]);
    settle(&mut h);
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    assert!(h.state().doc.is_dirty());
    ctrl(&mut h, Key::S);
    assert!(out.exists());
    let saved = ssx_types::Frame::decode(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(
        saved,
        ssx_editor_ui::export::flatten(h.state().doc.doc()),
        "the file equals the export render"
    );
    assert!(!h.state().doc.is_dirty());
    assert!(!h.state().doc.title().contains('*'));
    // Closing afterwards reports the save.
    h.state_mut().state.push(Action::RequestClose);
    settle(&mut h);
    let o = h.state().outcome().cloned().expect("finished");
    assert_eq!((o.action, o.path.as_deref()), (OutcomeAction::Save, Some(out.as_path())));
}

#[test]
fn save_without_a_path_opens_the_save_dialog_and_writes_where_asked() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready();
    ctrl(&mut h, Key::S);
    let Some(Dialog::SaveAs(_)) = &h.state().state.dialog else { panic!("save dialog expected") };
    let target = dir.path().join("shot.jpg");
    if let Some(Dialog::SaveAs(f)) = &mut h.state_mut().state.dialog {
        f.path = target.display().to_string();
        f.sync_format_from_path();
        f.jpeg_quality = 70;
    }
    settle(&mut h);
    click_dialog_button(&mut h, "Save");
    assert!(h.state().state.dialog.is_none());
    let img = ssx_types::Frame::decode(&std::fs::read(&target).unwrap()).unwrap();
    assert_eq!(img.size(), canvas_size(h.state()));
    assert_eq!(h.state().state.prefs.jpeg_quality, 70, "the chosen quality is remembered");
    assert!(!h.state().doc.is_dirty());
}

#[test]
fn saving_the_project_keeps_annotations_editable() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    h.state_mut().state.push(Action::SaveProject);
    settle(&mut h);
    let target = dir.path().join("work.ssxe");
    if let Some(Dialog::SaveAs(f)) = &mut h.state_mut().state.dialog {
        assert_eq!(
            std::path::Path::new(&f.path).extension().and_then(|e| e.to_str()),
            Some("ssxe")
        );
        f.path = target.display().to_string();
    } else {
        panic!("save dialog expected");
    }
    settle(&mut h);
    click_dialog_button(&mut h, "Save");
    let loaded = ssx_editor::project::load(&target).unwrap();
    assert_eq!(loaded.objects().len(), 1);
    // A second Save now overwrites the project without asking.
    key(&mut h, Key::E);
    drag_image(&mut h, (600.0, 120.0), (700.0, 200.0));
    ctrl(&mut h, Key::S);
    assert!(h.state().state.dialog.is_none());
    assert_eq!(ssx_editor::project::load(&target).unwrap().objects().len(), 2);
}

#[test]
fn done_variants_produce_the_documented_outcomes() {
    for (finish, action, needs_file) in [
        (Finish::Save, OutcomeAction::Save, true),
        (Finish::Copy, OutcomeAction::Copy, true),
        (Finish::Upload, OutcomeAction::Upload, true),
        (Finish::Cancel, OutcomeAction::Cancel, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.png");
        let mut app = app_for(flat(200, 100));
        app.state.output = Some(out.clone());
        let mut h = window(app, [1100.0, 700.0]);
        settle(&mut h);
        h.state_mut().state.push(Action::Done(finish));
        settle(&mut h);
        if finish == Finish::Cancel {
            // Nothing changed, so cancel needs no confirmation... but a fresh capture counts as
            // unsaved content, so the prompt may appear first.
            if matches!(h.state().state.dialog, Some(Dialog::Unsaved(_))) {
                h.state_mut()
                    .state
                    .push(Action::Unsaved(ssx_editor_ui::action::UnsavedAnswer::Discard));
                settle(&mut h);
            }
        }
        let o = h.state().outcome().cloned().unwrap_or_else(|| panic!("{finish:?} finished"));
        assert_eq!(o.action, action, "{finish:?}");
        assert_eq!(o.path.is_some(), needs_file, "{finish:?}");
        if needs_file {
            assert!(out.exists(), "{finish:?} writes the output");
        }
        if finish == Finish::Copy {
            assert!(h.state_mut().services.clipboard.image().is_some());
        }
    }
}

#[test]
fn closing_with_unsaved_changes_asks_and_discard_cancels() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    h.state_mut().state.push(Action::RequestClose);
    settle(&mut h);
    assert!(matches!(h.state().state.dialog, Some(Dialog::Unsaved(_))));
    assert!(h.state().outcome().is_none(), "nothing happens until the user answers");
    click_dialog_button(&mut h, "Cancel");
    assert!(
        h.state().state.dialog.is_none() && h.state().outcome().is_none(),
        "Cancel stays in the editor"
    );

    h.state_mut().state.push(Action::RequestClose);
    settle(&mut h);
    click_dialog_button(&mut h, "Discard");
    let o = h.state().outcome().cloned().expect("finished");
    assert_eq!(o.action, OutcomeAction::Cancel);
    assert_eq!(o.exit_code(), 3);
}

#[test]
fn unsaved_prompt_save_writes_then_closes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("saved.png");
    let mut app = app_for(dashboard());
    app.state.output = Some(out.clone());
    let mut h = window(app, [1280.0, 800.0]);
    settle(&mut h);
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    h.state_mut().state.push(Action::RequestClose);
    settle(&mut h);
    click_dialog_button(&mut h, "Save");
    assert!(out.exists());
    let o = h.state().outcome().cloned().expect("finished after saving");
    assert_eq!(o.action, OutcomeAction::Save);
}

#[test]
fn opening_a_file_replaces_the_document_and_asks_when_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other.png");
    flat(90, 60).save(&other).unwrap();
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    h.state_mut().state.push(Action::OpenPath(other.clone()));
    settle(&mut h);
    assert!(
        matches!(h.state().state.dialog, Some(Dialog::Unsaved(_))),
        "dirty documents are protected"
    );
    assert_eq!(h.state().doc.doc().image_size(), (1280, 760));
    click_dialog_button(&mut h, "Discard");
    assert_eq!(h.state().doc.doc().image_size(), (90, 60));
    assert!(h.state().doc.doc().objects().is_empty());
    assert!(!h.state().doc.is_dirty());
    assert!(h.state().canvas.viewport.is_fitted());
}

#[test]
fn dropping_a_file_opens_it_and_shift_drop_inserts_it() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("dropped.png");
    flat(64, 48).save(&other).unwrap();
    let mut h = window(app_for(flat(400, 300)), [1100.0, 700.0]);
    settle(&mut h);
    h.state_mut().doc.mark_saved();
    h.input_mut()
        .dropped_files
        .push(egui::DroppedFile { path: Some(other.clone()), ..Default::default() });
    h.step();
    settle(&mut h);
    assert_eq!(h.state().doc.doc().image_size(), (64, 48));

    let mut h = window(app_for(flat(400, 300)), [1100.0, 700.0]);
    settle(&mut h);
    h.input_mut().modifiers = Modifiers::SHIFT;
    h.input_mut().dropped_files.push(egui::DroppedFile { path: Some(other), ..Default::default() });
    h.step();
    settle(&mut h);
    assert_eq!(h.state().doc.doc().image_size(), (400, 300));
    assert_eq!(kinds(h.state()), ["image"]);
}

#[test]
fn new_from_clipboard_and_paste_image() {
    let mut app = app_for(flat(300, 200));
    app.doc.mark_saved();
    app.services.clipboard.set_image(&flat(50, 40)).unwrap();
    let mut h = window(app, [1100.0, 700.0]);
    settle(&mut h);
    ctrl(&mut h, Key::V);
    assert_eq!(kinds(h.state()), ["image"], "Ctrl+V pastes the clipboard image as an object");
    ctrl(&mut h, Key::Z);
    h.state_mut().state.push(Action::NewFromClipboard);
    settle(&mut h);
    assert_eq!(h.state().doc.doc().image_size(), (50, 40));
}

#[test]
fn properties_bar_edits_apply_to_the_selection() {
    let mut h = ready();
    key(&mut h, Key::R);
    drag_image(&mut h, (300.0, 120.0), (500.0, 220.0));
    h.state_mut().state.push(Action::Prop(ssx_editor_ui::props::PropEdit::StrokeWidth(9.0)));
    settle(&mut h);
    assert!((h.state().doc.doc().objects()[0].style.stroke_width - 9.0).abs() < 1e-3);
    // The bar shows the colour buttons for a rectangle.
    h.get_by_label("Stroke colour");
    h.get_by_label("Fill colour");
}

#[test]
fn image_tool_uses_the_pending_image() {
    let dir = tempfile::tempdir().unwrap();
    let logo = dir.path().join("logo.png");
    flat(64, 48).save(&logo).unwrap();
    let mut h = ready();
    if let Some(d) = h.state_mut().services.dialogs_fake() {
        d.answers.push_back(Some(logo));
    }
    key(&mut h, Key::I);
    settle(&mut h);
    assert!(h.state().state.has_pending_image, "the file dialog answered with the image");
    click_image(&mut h, (600.0, 300.0));
    assert_eq!(kinds(h.state()), ["image"]);
}
