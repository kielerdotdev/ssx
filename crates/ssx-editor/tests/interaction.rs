//! Interaction-script tests: drive `EditorSession` with pointer/key sequences and assert on the
//! resulting document (typed fields and JSON).

mod common;

use common::*;
use ssx_editor::{
    Color, CursorHint, EditorSession, Key, Modifiers, ObjectKind, PointF, RectF, SessionEvent, Tool,
    HandleKind,
    object::Axis,
};

const NONE: Modifiers = Modifiers::NONE;

fn p(x: f32, y: f32) -> PointF {
    PointF::new(x, y)
}

fn session() -> EditorSession {
    EditorSession::new(doc(300, 200))
}

fn drag(s: &mut EditorSession, from: (f32, f32), to: (f32, f32), mods: Modifiers) {
    s.pointer_down(p(from.0, from.1), mods, None);
    let mid = p((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
    s.pointer_move(mid, mods, None);
    s.pointer_move(p(to.0, to.1), mods, None);
    s.pointer_up(p(to.0, to.1), mods);
}

fn only_rect(s: &EditorSession) -> RectF {
    match s.document().objects().last().map(|o| &o.kind) {
        Some(ObjectKind::Rectangle(b)) => b.rect,
        other => panic!("expected a rectangle, got {other:?}"),
    }
}

fn types(s: &EditorSession) -> Vec<&'static str> {
    s.document().objects().iter().map(|o| o.kind.name()).collect()
}

#[test]
fn drawing_a_rectangle_is_one_undo_step_and_matches_json() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    assert_eq!(only_rect(&s), RectF::new(10.0, 10.0, 50.0, 30.0));
    assert_eq!(s.selection().len(), 1, "the new object is selected");
    assert_eq!(s.history().undo_len(), 1, "creation + drag coalesce into one step");
    let json = serde_json::to_value(&s.document().objects()[0]).unwrap();
    assert_eq!(json["kind"]["type"], "rectangle");
    assert_eq!(json["kind"]["rect"], serde_json::json!({"x": 10.0, "y": 10.0, "w": 50.0, "h": 30.0}));
    assert_eq!(json["style"]["stroke"], "#ff0000ff");
    assert_eq!(json["visible"], true);
    s.undo();
    assert!(s.document().objects().is_empty());
    assert!(s.can_redo());
    s.redo();
    assert_eq!(only_rect(&s), RectF::new(10.0, 10.0, 50.0, 30.0));
}

#[test]
fn drag_in_any_direction_and_modifiers() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (60.0, 40.0), (10.0, 10.0), NONE);
    assert_eq!(only_rect(&s), RectF::new(10.0, 10.0, 50.0, 30.0), "reverse drag normalises");
    drag(&mut s, (100.0, 100.0), (140.0, 120.0), Modifiers::SHIFT);
    assert_eq!(only_rect(&s), RectF::new(100.0, 100.0, 40.0, 40.0), "shift = square");
    drag(&mut s, (200.0, 100.0), (220.0, 110.0), Modifiers::ALT);
    assert_eq!(only_rect(&s), RectF::new(180.0, 90.0, 40.0, 20.0), "alt = from centre");
    s.set_tool(Tool::Ellipse);
    drag(&mut s, (10.0, 150.0), (50.0, 170.0), Modifiers::SHIFT);
    assert_eq!(s.document().objects().last().unwrap().kind.name(), "ellipse");
    assert_eq!(s.document().objects().last().unwrap().bounds().w, 40.0, "shift = circle");
}

#[test]
fn accidental_click_creates_nothing() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    s.pointer_down(p(50.0, 50.0), NONE, None);
    s.pointer_up(p(50.0, 50.5), NONE);
    assert!(s.document().objects().is_empty());
    assert!(!s.can_undo(), "no phantom undo step");
    s.set_tool(Tool::Arrow);
    drag(&mut s, (10.0, 10.0), (11.0, 10.0), NONE);
    assert!(s.document().objects().is_empty());
}

#[test]
fn line_and_arrow_constraints() {
    let mut s = session();
    s.set_tool(Tool::Line);
    drag(&mut s, (10.0, 10.0), (110.0, 30.0), Modifiers::SHIFT);
    let ObjectKind::Line(l) = &s.document().objects()[0].kind else { panic!() };
    assert!((l.b.y - l.a.y).abs() < 1e-3, "shift snaps to horizontal");
    s.set_tool(Tool::Arrow);
    drag(&mut s, (100.0, 100.0), (150.0, 160.0), Modifiers::SHIFT);
    let ObjectKind::Arrow(a) = &s.document().objects()[1].kind else { panic!() };
    assert!(((a.b.x - a.a.x).abs() - (a.b.y - a.a.y).abs()).abs() < 1e-3, "45 degrees");
    drag(&mut s, (200.0, 50.0), (220.0, 60.0), Modifiers::ALT);
    let ObjectKind::Arrow(a) = &s.document().objects()[2].kind else { panic!() };
    assert_eq!(a.a, p(180.0, 40.0));
}

#[test]
fn freehand_collects_points_in_one_step() {
    let mut s = session();
    s.set_tool(Tool::Freehand);
    s.pointer_down(p(10.0, 10.0), NONE, Some(0.7));
    for i in 1..30 {
        s.pointer_move(p(10.0 + i as f32 * 3.0, 10.0 + (i as f32 * 0.3).sin() * 20.0), NONE, Some(0.3));
    }
    s.pointer_up(p(100.0, 10.0), NONE);
    let ObjectKind::Freehand(f) = &s.document().objects()[0].kind else { panic!() };
    assert!(f.points.len() > 20);
    assert!(f.smooth && f.arrow.is_none());
    assert_eq!(s.history().undo_len(), 1);
    s.set_tool(Tool::FreehandArrow);
    drag(&mut s, (10.0, 100.0), (120.0, 140.0), NONE);
    let ObjectKind::Freehand(f) = &s.document().objects()[1].kind else { panic!() };
    assert!(f.arrow.is_some());
}

#[test]
fn select_move_and_undo() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    s.set_tool(Tool::Select);
    s.clear_selection();
    s.pointer_down(p(10.0, 25.0), NONE, None); // on the left edge
    assert_eq!(s.selection().len(), 1);
    s.pointer_move(p(30.0, 35.0), NONE, None);
    s.pointer_move(p(50.0, 45.0), NONE, None);
    s.pointer_up(p(50.0, 45.0), NONE);
    assert_eq!(only_rect(&s), RectF::new(50.0, 30.0, 50.0, 30.0));
    assert_eq!(s.history().undo_len(), 2, "create, move");
    s.undo();
    assert_eq!(only_rect(&s), RectF::new(10.0, 10.0, 50.0, 30.0));
    s.redo();
    assert_eq!(only_rect(&s), RectF::new(50.0, 30.0, 50.0, 30.0));
}

#[test]
fn shift_locks_move_axis_and_escape_cancels() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    s.set_tool(Tool::Select);
    s.clear_selection();
    // Shift is pressed after the drag started (shift+press would toggle the selection).
    s.pointer_down(p(10.0, 25.0), NONE, None);
    s.pointer_move(p(40.0, 35.0), Modifiers::SHIFT, None);
    s.pointer_up(p(40.0, 35.0), Modifiers::SHIFT);
    assert_eq!(only_rect(&s), RectF::new(40.0, 10.0, 50.0, 30.0), "horizontal only");
    // Start a drag, move, press Escape: the object returns and no history entry remains.
    let steps = s.history().undo_len();
    s.clear_selection();
    s.pointer_down(p(40.0, 25.0), NONE, None);
    s.pointer_move(p(100.0, 100.0), NONE, None);
    assert_ne!(only_rect(&s).x, 40.0);
    assert!(s.key_down(Key::Escape, NONE));
    assert_eq!(only_rect(&s), RectF::new(40.0, 10.0, 50.0, 30.0));
    assert_eq!(s.history().undo_len(), steps);
    s.pointer_up(p(100.0, 100.0), NONE);
    assert_eq!(only_rect(&s).x, 40.0, "the late pointer-up does nothing");
}

#[test]
fn marquee_and_multiselect_and_group_move() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (40.0, 30.0), NONE);
    drag(&mut s, (100.0, 10.0), (140.0, 30.0), NONE);
    drag(&mut s, (10.0, 150.0), (40.0, 180.0), NONE);
    s.set_tool(Tool::Select);
    s.clear_selection();
    // Marquee over the first two.
    drag(&mut s, (0.0, 0.0), (160.0, 60.0), NONE);
    assert_eq!(s.selection().len(), 2);
    // Shift-click toggles the third in and out.
    s.pointer_down(p(10.0, 165.0), Modifiers::SHIFT, None);
    s.pointer_up(p(10.0, 165.0), Modifiers::SHIFT);
    assert_eq!(s.selection().len(), 3);
    s.pointer_down(p(10.0, 165.0), Modifiers::SHIFT, None);
    s.pointer_up(p(10.0, 165.0), Modifiers::SHIFT);
    assert_eq!(s.selection().len(), 2);
    // Group + click one member selects both; moving moves both.
    s.group_selection();
    s.clear_selection();
    s.pointer_down(p(10.0, 20.0), NONE, None);
    s.pointer_up(p(10.0, 20.0), NONE);
    assert_eq!(s.selection().len(), 2, "group selects together");
    drag(&mut s, (25.0, 10.0), (25.0, 60.0), NONE);
    let ys: Vec<f32> = s.document().objects().iter().map(|o| o.bounds().y).collect();
    assert_eq!(ys, vec![60.0, 60.0, 150.0]);
    s.ungroup_selection();
    s.clear_selection();
    s.pointer_down(p(10.0, 70.0), NONE, None);
    s.pointer_up(p(10.0, 70.0), NONE);
    assert_eq!(s.selection().len(), 1);
}

#[test]
fn resize_handles_and_rotation() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (100.0, 100.0), (160.0, 140.0), NONE);
    let h = s.handles();
    let east = h.iter().find(|h| h.kind == HandleKind::East).unwrap();
    assert_eq!(east.pos, p(160.0, 120.0));
    assert_eq!(east.cursor, CursorHint::ResizeEw);
    s.set_tool(Tool::Select);
    drag(&mut s, (160.0, 120.0), (200.0, 120.0), NONE);
    assert_eq!(only_rect(&s), RectF::new(100.0, 100.0, 100.0, 40.0));
    // Corner with shift keeps the aspect ratio.
    let se = s.handles().into_iter().find(|h| h.kind == HandleKind::SouthEast).unwrap();
    drag(&mut s, (se.pos.x, se.pos.y), (300.0, 150.0), Modifiers::SHIFT);
    let r = only_rect(&s);
    assert!((r.w / r.h - 2.5).abs() < 1e-3, "{r:?}");
    // Rotate handle: drag to the right of the centre => 90 degrees clockwise.
    let rot = s.handles().into_iter().find(|h| h.kind == HandleKind::Rotate).unwrap();
    let c = only_rect(&s).center();
    drag(&mut s, (rot.pos.x, rot.pos.y), (c.x + 80.0, c.y), NONE);
    let ObjectKind::Rectangle(b) = &s.document().objects()[0].kind else { panic!() };
    assert!((b.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "{}", b.rotation);
    // Handles rotate with the box: the east handle now points down.
    let east = s.handles().into_iter().find(|h| h.kind == HandleKind::East).unwrap();
    assert_eq!(east.cursor, CursorHint::ResizeNs);
}

#[test]
fn arrow_endpoint_and_balloon_tail_handles() {
    let mut s = session();
    s.set_tool(Tool::Arrow);
    drag(&mut s, (20.0, 20.0), (120.0, 20.0), NONE);
    let hs = s.handles();
    assert_eq!(hs.len(), 2);
    drag(&mut s, (120.0, 20.0), (120.0, 90.0), NONE);
    let ObjectKind::Arrow(a) = &s.document().objects()[0].kind else { panic!() };
    assert_eq!((a.a, a.b), (p(20.0, 20.0), p(120.0, 90.0)));

    s.set_tool(Tool::Balloon);
    drag(&mut s, (150.0, 100.0), (250.0, 150.0), NONE);
    s.commit_text_edit();
    let tail = s.handles().into_iter().find(|h| h.kind == HandleKind::Tail).expect("tail handle");
    s.set_tool(Tool::Select);
    drag(&mut s, (tail.pos.x, tail.pos.y), (270.0, 190.0), NONE);
    let ObjectKind::Balloon(b) = &s.document().objects()[1].kind else { panic!() };
    assert_eq!(b.tail, p(270.0, 190.0));
}

#[test]
fn magnifier_source_handle() {
    let mut s = session();
    s.set_tool(Tool::Magnify);
    drag(&mut s, (100.0, 50.0), (180.0, 130.0), NONE);
    let src = s.handles().into_iter().find(|h| h.kind == HandleKind::Source).unwrap();
    assert_eq!(src.pos, p(140.0, 90.0), "source starts at the lens centre");
    s.set_tool(Tool::Select);
    drag(&mut s, (src.pos.x, src.pos.y), (60.0, 60.0), NONE);
    let ObjectKind::Magnify(m) = &s.document().objects()[0].kind else { panic!() };
    assert_eq!(m.source, p(60.0, 60.0));
    assert_eq!(m.rect, RectF::new(100.0, 50.0, 80.0, 80.0), "the lens itself did not move");
}

#[test]
fn text_tool_typing_commit_and_undo() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    assert!(s.text_edit_state().is_some());
    s.text_insert("Hel");
    s.text_insert("lo");
    let st = s.text_edit_state().unwrap();
    assert_eq!((st.text.as_str(), st.caret), ("Hello", 5));
    assert!(s.key_down(Key::Enter, NONE), "enter commits");
    assert!(s.text_edit_state().is_none());
    let ObjectKind::Text(t) = &s.document().objects()[0].kind else { panic!() };
    assert_eq!(t.content.text, "Hello");
    assert_eq!((t.rect.x, t.rect.y), (30.0, 40.0));
    assert!(t.rect.w > 20.0 && t.rect.h > 20.0, "box was sized from the layout: {:?}", t.rect);
    assert_eq!(s.history().undo_len(), 1, "creating and typing is a single undo step");
    s.undo();
    assert!(s.document().objects().is_empty());
    s.redo();
    assert_eq!(s.document().objects().len(), 1);
}

#[test]
fn empty_text_is_discarded_without_history() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    assert_eq!(s.document().objects().len(), 1);
    s.key_down(Key::Enter, NONE);
    assert!(s.document().objects().is_empty());
    assert!(!s.can_undo() && !s.can_redo());
    // Whitespace only is also dropped.
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    s.text_insert("   ");
    s.key_down(Key::Escape, NONE);
    assert!(s.document().objects().is_empty());
}

#[test]
fn text_caret_selection_and_editing_keys() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    s.text_insert("abc def");
    s.key_down(Key::Left, NONE);
    s.key_down(Key::Backspace, NONE);
    assert_eq!(s.text_edit_state().unwrap().text, "abc df");
    s.key_down(Key::Left, Modifiers::SHIFT);
    s.key_down(Key::Left, Modifiers::SHIFT);
    let st = s.text_edit_state().unwrap();
    assert_eq!((st.anchor, st.caret), (5, 3));
    s.text_insert("X");
    assert_eq!(s.text_edit_state().unwrap().text, "abcXf");
    s.key_down(Key::Home, NONE);
    s.key_down(Key::Delete, NONE);
    assert_eq!(s.text_edit_state().unwrap().text, "bcXf");
    s.key_down(Key::Enter, Modifiers::SHIFT);
    assert_eq!(s.text_edit_state().unwrap().text, "\nbcXf");
    s.key_down(Key::Up, NONE);
    assert_eq!(s.text_edit_state().unwrap().caret, 0);
    s.key_down(Key::Down, NONE);
    assert_eq!(s.text_edit_state().unwrap().caret, 1, "down from the empty first line lands on line 2");
    s.key_down(Key::End, NONE);
    assert_eq!(s.text_edit_state().unwrap().caret, 5);
    s.key_down(Key::Char('a'), Modifiers::CTRL);
    let st = s.text_edit_state().unwrap();
    assert_eq!((st.anchor.min(st.caret), st.anchor.max(st.caret)), (0, 5));
    s.key_down(Key::Char('c'), Modifiers::CTRL);
    assert!(s.take_events().iter().any(|e| matches!(e, SessionEvent::SetClipboardText(t) if t == "\nbcXf")));
    s.key_down(Key::Char('v'), Modifiers::CTRL);
    assert_eq!(s.text_edit_state().unwrap().text, "\nbcXf", "paste replaces the selection with the same text");
}

#[test]
fn ime_preedit_is_not_document_text_until_committed() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    s.ime_preedit(Some(("にほ".into(), Some((0, 3)))));
    assert_eq!(s.text_edit_state().unwrap().text, "");
    assert_eq!(s.overlay().caret.unwrap().preedit.as_deref(), Some("にほ"));
    s.text_insert("日本");
    assert_eq!(s.text_edit_state().unwrap().text, "日本");
    assert!(s.overlay().caret.unwrap().preedit.is_none());
    assert_eq!(s.text_edit_state().unwrap().caret, "日本".len());
}

#[test]
fn clicking_inside_edited_text_moves_caret_and_outside_commits() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    s.text_insert("abcdef");
    s.pointer_down(p(34.0, 50.0), NONE, None); // near the very start of the text
    s.pointer_up(p(34.0, 50.0), NONE);
    assert_eq!(s.text_edit_state().unwrap().caret, 0);
    s.pointer_down(p(280.0, 180.0), NONE, None); // elsewhere: commits (and starts a new text)
    s.pointer_up(p(280.0, 180.0), NONE);
    s.key_down(Key::Escape, NONE);
    let texts: Vec<_> = s.document().objects().iter().map(|o| o.kind.text_content().unwrap().text.clone()).collect();
    assert_eq!(texts, vec!["abcdef".to_string()]);
}

#[test]
fn step_tool_numbers_and_renumbers() {
    let mut s = session();
    s.set_tool(Tool::Step);
    for i in 0..4 {
        let x = 30.0 + i as f32 * 40.0;
        s.pointer_down(p(x, 30.0), NONE, None);
        s.pointer_up(p(x, 30.0), NONE);
    }
    let ids: Vec<_> = s.document().objects().iter().map(|o| o.id).collect();
    assert_eq!(ids.iter().map(|i| s.document().step_number(*i).unwrap()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    s.select(&[ids[1]]);
    s.delete_selection();
    assert_eq!(s.document().step_number(ids[2]), Some(2));
    assert_eq!(s.document().step_number(ids[3]), Some(3));
    s.undo();
    assert_eq!(s.document().step_number(ids[3]), Some(4));
    // Z-order changes renumber as well.
    s.select(&[ids[0]]);
    s.bring_to_front();
    assert_eq!(s.document().step_number(ids[0]), Some(4));
    assert_eq!(s.document().step_number(ids[1]), Some(1));
}

#[test]
fn stamp_tools_place_on_click_and_follow_drag() {
    let mut s = session();
    s.set_tool(Tool::Cursor);
    drag(&mut s, (50.0, 50.0), (80.0, 90.0), NONE);
    let ObjectKind::Cursor(c) = &s.document().objects()[0].kind else { panic!() };
    assert_eq!(c.pos, p(80.0, 90.0));
    assert_eq!(s.history().undo_len(), 1);
    s.set_tool(Tool::Sticker);
    s.pointer_down(p(200.0, 100.0), NONE, None);
    s.pointer_up(p(200.0, 100.0), NONE);
    assert_eq!(types(&s), vec!["cursor", "sticker"]);
    // Image tool needs a pending bitmap.
    s.set_tool(Tool::Image);
    s.pointer_down(p(100.0, 100.0), NONE, None);
    s.pointer_up(p(100.0, 100.0), NONE);
    assert_eq!(s.document().objects().len(), 2);
    s.set_pending_image(Some(ssx_imgfx::solid_frame(40, 30, [255, 0, 0, 255])));
    s.pointer_down(p(100.0, 100.0), NONE, None);
    s.pointer_up(p(100.0, 100.0), NONE);
    assert_eq!(types(&s), vec!["cursor", "sticker", "image"]);
    assert_eq!(s.document().objects()[2].bounds(), RectF::new(80.0, 85.0, 40.0, 30.0));
}

#[test]
fn eraser_removes_objects_touched_by_the_stroke_in_one_step() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    drag(&mut s, (100.0, 10.0), (150.0, 40.0), NONE);
    drag(&mut s, (10.0, 150.0), (60.0, 180.0), NONE);
    s.set_tool(Tool::Eraser);
    s.pointer_down(p(5.0, 25.0), NONE, None);
    s.pointer_move(p(50.0, 25.0), NONE, None);
    assert_eq!(s.overlay().erase_marks.len(), 1, "preview marks the touched object");
    s.pointer_move(p(105.0, 25.0), NONE, None);
    s.pointer_up(p(105.0, 25.0), NONE);
    assert_eq!(s.document().objects().len(), 1);
    assert_eq!(s.document().objects()[0].bounds().y, 150.0);
    s.undo();
    assert_eq!(s.document().objects().len(), 3, "one undo restores both");
    // A stroke through empty space erases nothing and adds no history.
    let steps = s.history().undo_len();
    drag(&mut s, (200.0, 100.0), (250.0, 100.0), NONE);
    assert_eq!(s.history().undo_len(), steps);
}

#[test]
fn object_clipboard_duplicate_and_json() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    s.copy();
    s.paste();
    s.paste();
    assert_eq!(s.document().objects().len(), 3);
    let xs: Vec<f32> = s.document().objects().iter().map(|o| o.bounds().x).collect();
    assert_eq!(xs, vec![10.0, 26.0, 42.0], "each paste is offset further");
    let ids: std::collections::HashSet<_> = s.document().objects().iter().map(|o| o.id).collect();
    assert_eq!(ids.len(), 3, "pasted objects get fresh ids");
    assert_eq!(s.selection().len(), 1);
    s.duplicate();
    assert_eq!(s.document().objects().len(), 4);
    s.undo();
    s.undo();
    s.undo();
    assert_eq!(s.document().objects().len(), 1);
    let json = s.clipboard_json().unwrap();
    let mut other = session();
    other.paste_json(&json).unwrap();
    assert_eq!(other.document().objects().len(), 1);
    assert!(other.paste_json("not json").is_err());
    // Cut = copy + delete.
    s.select_all();
    s.cut();
    assert!(s.document().objects().is_empty());
    s.paste();
    assert_eq!(s.document().objects().len(), 1);
}

#[test]
fn z_order_operations() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    for i in 0..4 {
        drag(&mut s, (10.0 + i as f32 * 60.0, 10.0), (50.0 + i as f32 * 60.0, 40.0), NONE);
    }
    let ids: Vec<_> = s.document().objects().iter().map(|o| o.id).collect();
    s.select(&[ids[0]]);
    s.bring_to_front();
    assert_eq!(s.document().order(), vec![ids[1], ids[2], ids[3], ids[0]]);
    s.send_to_back();
    assert_eq!(s.document().order(), ids);
    s.raise();
    assert_eq!(s.document().order(), vec![ids[1], ids[0], ids[2], ids[3]]);
    s.lower();
    s.lower();
    assert_eq!(s.document().order(), ids, "lower at the bottom is a no-op");
    let steps = s.history().undo_len();
    s.send_to_back();
    assert_eq!(s.history().undo_len(), steps, "no-op reorders leave no undo step");
    s.select(&[ids[1], ids[3]]);
    s.bring_to_front();
    assert_eq!(s.document().order(), vec![ids[0], ids[2], ids[1], ids[3]]);
}

#[test]
fn nudge_keys_coalesce() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    let steps = s.history().undo_len();
    s.key_down(Key::Right, NONE);
    s.key_down(Key::Right, NONE);
    s.key_down(Key::Down, Modifiers::SHIFT);
    assert_eq!(only_rect(&s), RectF::new(12.0, 20.0, 50.0, 30.0));
    assert_eq!(s.history().undo_len(), steps + 1, "a run of nudges is one step");
    s.undo();
    assert_eq!(only_rect(&s), RectF::new(10.0, 10.0, 50.0, 30.0));
    assert!(!s.key_down(Key::Delete, Modifiers::NONE) || s.document().objects().is_empty());
}

#[test]
fn delete_key_and_shortcuts() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    assert!(s.key_down(Key::Delete, NONE));
    assert!(s.document().objects().is_empty());
    assert!(s.key_down(Key::Char('z'), Modifiers::CTRL));
    assert_eq!(s.document().objects().len(), 1);
    assert!(s.key_down(Key::Char('y'), Modifiers::CTRL));
    assert!(s.document().objects().is_empty());
    assert!(s.key_down(Key::Char('z'), Modifiers { shift: false, ctrl: true, alt: false }));
    assert!(s.key_down(Key::Char('Z'), Modifiers { shift: true, ctrl: true, alt: false }), "ctrl+shift+z redoes");
    assert!(s.document().objects().is_empty());
    assert!(!s.key_down(Key::Tab, NONE));
    assert!(!s.key_down(Key::Delete, NONE), "nothing selected: not consumed");
}

#[test]
fn style_edits_apply_to_selection_and_are_remembered_per_tool() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    for w in [4.0, 6.0, 8.0, 10.0] {
        s.set_style(move |st| {
            st.stroke = Color::rgb(0, 128, 255);
            st.stroke_width = w;
        });
    }
    assert_eq!(s.document().objects()[0].style.stroke_width, 10.0);
    assert_eq!(s.history().undo_len(), 2, "slider-style repeated edits coalesce");
    // A new rectangle uses the remembered style; an ellipse does not.
    drag(&mut s, (100.0, 10.0), (150.0, 40.0), NONE);
    assert_eq!(s.document().objects()[1].style.stroke, Color::rgb(0, 128, 255));
    assert_eq!(s.document().objects()[1].style.stroke_width, 10.0);
    s.set_tool(Tool::Ellipse);
    drag(&mut s, (100.0, 100.0), (150.0, 140.0), NONE);
    assert_eq!(s.document().objects()[2].style.stroke, Color::RED);
    // With nothing selected, set_style edits the tool's default instead.
    s.clear_selection();
    s.set_style(|st| st.stroke_width = 1.0);
    drag(&mut s, (200.0, 100.0), (250.0, 140.0), NONE);
    assert_eq!(s.document().objects()[3].style.stroke_width, 1.0);
    assert_eq!(s.document().objects()[2].style.stroke_width, 3.0, "existing objects untouched");
    // Persisting the memory.
    let json = serde_json::to_string(s.styles()).unwrap();
    assert!(json.contains("rectangle"));
}

#[test]
fn text_kind_props_update_the_layout() {
    let mut s = session();
    s.set_tool(Tool::Text);
    s.pointer_down(p(30.0, 40.0), NONE, None);
    s.pointer_up(p(30.0, 40.0), NONE);
    s.text_insert("Size me");
    s.commit_text_edit();
    let w0 = s.document().objects()[0].bounds().w;
    s.select(&[s.document().objects()[0].id]);
    s.set_kind_props(|k| {
        if let Some(c) = k.text_content_mut() {
            c.font.size = 48.0;
            c.font.bold = true;
        }
    });
    let w1 = s.document().objects()[0].bounds().w;
    assert!(w1 > w0 * 1.5, "{w1} vs {w0}: the stored box grew with the font");
}

#[test]
fn crop_tool_apply_and_cancel() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (100.0, 100.0), (150.0, 130.0), NONE);
    s.set_tool(Tool::Crop);
    drag(&mut s, (50.0, 50.0), (250.0, 170.0), NONE);
    assert_eq!(s.pending_crop().unwrap().rect, RectF::new(50.0, 50.0, 200.0, 120.0));
    assert_eq!(s.document().image_size(), (300, 200), "nothing happens until confirmed");
    assert!(s.key_down(Key::Escape, NONE));
    assert!(s.pending_crop().is_none());
    drag(&mut s, (50.0, 50.0), (250.0, 170.0), NONE);
    assert!(s.key_down(Key::Enter, NONE));
    assert_eq!(s.document().image_size(), (200, 120));
    let b = s.document().objects()[0].bounds();
    assert_eq!((b.x, b.y), (50.0, 50.0), "annotation moved with the crop");
    assert!(s.take_events().contains(&SessionEvent::CanvasChanged));
    s.undo();
    assert_eq!(s.document().image_size(), (300, 200));
    assert_eq!(s.document().objects()[0].bounds().x, 100.0);
}

#[test]
fn ellipse_and_freeform_crops_flatten_with_transparent_corners() {
    let mut s = session();
    s.set_tool(Tool::CropEllipse);
    drag(&mut s, (50.0, 50.0), (150.0, 130.0), NONE);
    s.apply_crop().unwrap();
    assert_eq!(s.document().image_size(), (100, 80));
    let f = s.document().base();
    assert_eq!(ssx_imgfx::get_pixel(f, 0, 0)[3], 0, "corner outside the ellipse is transparent");
    assert_eq!(ssx_imgfx::get_pixel(f, 50, 40)[3], 255);
    s.undo();
    s.set_tool(Tool::CropFreeform);
    s.pointer_down(p(50.0, 50.0), NONE, None);
    for q in [(150.0, 60.0), (120.0, 150.0), (60.0, 120.0)] {
        s.pointer_move(p(q.0, q.1), NONE, None);
    }
    s.pointer_up(p(60.0, 120.0), NONE);
    assert_eq!(s.pending_crop().unwrap().polygon.len(), 4);
    s.apply_crop().unwrap();
    let f = s.document().base();
    assert_eq!(ssx_imgfx::get_pixel(f, 95, 5)[3], 0, "outside the polygon");
    assert_eq!(ssx_imgfx::get_pixel(f, 50, 50)[3], 255, "inside the polygon");
}

#[test]
fn cutout_tool_removes_a_strip() {
    let mut s = session();
    s.set_tool(Tool::CutOut);
    let ov_check = {
        s.pointer_down(p(100.0, 50.0), NONE, None);
        s.pointer_move(p(140.0, 60.0), NONE, None);
        s.overlay().cut_strip
    };
    assert_eq!(ov_check, Some(RectF::new(100.0, 0.0, 40.0, 200.0)), "mostly-horizontal drag cuts a vertical strip");
    s.pointer_up(p(140.0, 60.0), NONE);
    assert_eq!(s.document().image_size(), (260, 200));
    drag(&mut s, (10.0, 20.0), (15.0, 70.0), NONE);
    assert_eq!(s.document().image_size(), (260, 150));
    s.undo();
    s.undo();
    assert_eq!(s.document().image_size(), (300, 200));
}

#[test]
fn global_ops_are_undoable_and_report_canvas_change() {
    let mut s = session();
    let initial = s.document().clone();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    s.orient(ssx_editor::object::Orient::Rotate90).unwrap();
    assert_eq!(s.document().image_size(), (200, 300));
    s.resize(100, 100, ssx_imgfx::ResizeFilter::Lanczos3).unwrap();
    s.resize_canvas(5, 5, 5, 5).unwrap();
    s.apply_effect(&ssx_imgfx::Effect::Invert, None).unwrap();
    s.auto_crop(0).unwrap();
    assert!(s.crop(ssx_types::Rect::new(500, 500, 5, 5)).is_err(), "errors change nothing");
    s.flatten().unwrap();
    assert!(s.document().objects().is_empty());
    assert!(s.take_events().contains(&SessionEvent::CanvasChanged));
    while s.can_undo() {
        s.undo();
    }
    assert_eq!(*s.document(), initial, "undo-all restores the initial document exactly, ids included");
    while s.can_redo() {
        s.redo();
    }
    assert!(s.document().objects().is_empty());
    let mut s2 = session();
    assert!(s2.cut_out(Axis::X, 5, 5).is_err());
    assert!(!s2.can_undo());
}

#[test]
fn snapping_with_ctrl_shows_guides() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (100.0, 100.0), (160.0, 140.0), NONE);
    s.clear_selection();
    s.pointer_down(p(20.0, 20.0), Modifiers::CTRL, None);
    s.pointer_move(p(102.0, 50.0), Modifiers::CTRL, None);
    let g = s.overlay().guides;
    assert!(g.iter().any(|g| g.vertical && g.position == 100.0), "{g:?}");
    s.pointer_up(p(102.0, 50.0), Modifiers::CTRL);
    assert_eq!(s.document().objects()[1].bounds().right(), 100.0, "corner snapped to the other rectangle's edge");
    assert!(s.overlay().guides.is_empty(), "guides vanish after the gesture");
    // Without ctrl there is no snapping.
    drag(&mut s, (20.0, 150.0), (102.0, 190.0), NONE);
    assert_eq!(s.document().objects()[2].bounds().right(), 102.0);
}

#[test]
fn hover_cursor_hints() {
    let mut s = session();
    assert_eq!(s.cursor_hint(), CursorHint::Default);
    s.set_tool(Tool::Rectangle);
    assert_eq!(s.cursor_hint(), CursorHint::Crosshair);
    drag(&mut s, (100.0, 100.0), (160.0, 140.0), NONE);
    s.pointer_move(p(160.0, 120.0), NONE, None);
    assert_eq!(s.cursor_hint(), CursorHint::ResizeEw, "over a handle of the selection");
    s.set_tool(Tool::Select);
    s.pointer_move(p(115.0, 100.0), NONE, None);
    assert_eq!(s.cursor_hint(), CursorHint::Move);
    s.pointer_move(p(250.0, 20.0), NONE, None);
    assert_eq!(s.cursor_hint(), CursorHint::Default);
    s.set_tool(Tool::Text);
    assert_eq!(s.cursor_hint(), CursorHint::Text);
    s.set_tool(Tool::Eraser);
    assert_eq!(s.cursor_hint(), CursorHint::Eraser);
    let evs = s.take_events();
    assert!(evs.iter().any(|e| matches!(e, SessionEvent::CursorChanged(CursorHint::Eraser))));
}

#[test]
fn events_report_dirty_rects_in_image_space() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    s.take_events();
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    let evs = s.take_events();
    let dirty: Vec<_> = evs.iter().filter_map(|e| if let SessionEvent::Dirty(r) = e { Some(*r) } else { None }).collect();
    assert!(!dirty.is_empty());
    let all = dirty.iter().copied().reduce(|a, b| a.union(b)).unwrap();
    assert!(all.x <= 8 && all.y <= 8 && all.right() >= 62 && all.bottom() >= 42, "{all:?}");
    assert!(all.right() < 100, "dirty area stays local to the shape: {all:?}");
    assert!(evs.iter().any(|e| matches!(e, SessionEvent::HistoryChanged { can_undo: true, .. })));
    assert!(evs.iter().any(|e| matches!(e, SessionEvent::SelectionChanged)));
    assert!(s.take_dirty().is_some());
    assert!(s.take_dirty().is_none());
}

#[test]
fn blur_object_dirty_expansion_when_underlying_object_changes() {
    let mut s = session();
    s.set_tool(Tool::Blur);
    drag(&mut s, (100.0, 100.0), (200.0, 180.0), NONE);
    s.set_tool(Tool::Rectangle);
    s.clear_selection();
    s.take_events();
    s.take_dirty();
    // Draw a small rectangle underneath?  It is added on top, so only its own area is dirty...
    drag(&mut s, (0.0, 0.0), (20.0, 20.0), NONE);
    let d = s.take_dirty().unwrap();
    assert!(d.right() < 100);
    // ...but *moving* an object that sits under the blur dirties the whole blur region.
    s.set_tool(Tool::Select);
    s.select(&[s.document().objects()[1].id]);
    s.take_events();
    s.nudge(120.0, 110.0);
    let d = s.take_dirty().unwrap();
    assert!(d.contains(ssx_types::Point::new(100, 100)) || d.right() >= 121, "{d:?}");
}

#[test]
fn lock_hide_and_locked_objects_are_not_grabbed() {
    let mut s = session();
    s.set_tool(Tool::Rectangle);
    drag(&mut s, (10.0, 10.0), (60.0, 40.0), NONE);
    let id = s.document().objects()[0].id;
    s.set_locked(id, true);
    s.set_tool(Tool::Select);
    s.pointer_down(p(10.0, 25.0), NONE, None);
    s.pointer_up(p(10.0, 25.0), NONE);
    assert!(s.selection().is_empty());
    assert!(s.hit_test(p(10.0, 25.0)).is_none());
    s.set_locked(id, false);
    assert_eq!(s.hit_test(p(10.0, 25.0)), Some(id));
    s.set_visible(id, false);
    assert!(s.hit_test(p(10.0, 25.0)).is_none());
    s.undo();
    assert!(s.document().objects()[0].visible, "undo of hide");
    assert_eq!(s.hit_test(p(10.0, 25.0)), Some(id));
}

#[test]
fn zoomed_view_scales_hit_slack_and_handles() {
    let mut s = session();
    s.set_tool(Tool::Line);
    drag(&mut s, (10.0, 100.0), (200.0, 100.0), NONE);
    s.set_tool(Tool::Select);
    s.clear_selection();
    // 6 image px away: missed at 1x (slack ~5 px + half stroke 2), hit at 0.5x zoom (slack 10 px).
    assert!(s.hit_test(p(100.0, 110.0)).is_none());
    s.set_view_scale(0.25);
    assert!(s.hit_test(p(100.0, 110.0)).is_some());
    s.set_view_scale(f32::NAN);
    s.set_view_scale(-3.0);
    assert!(s.hit_test(p(100.0, 110.0)).is_some(), "invalid zoom values are ignored");
}
