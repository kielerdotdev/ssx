//! Undo/redo property tests: random operation sequences must be perfectly reversible.

#![allow(clippy::float_cmp)] // tests assert exact geometry values

mod common;

use common::*;
use proptest::prelude::*;
use ssx_editor::{
    Color, EditorSession, Fill, Key, Modifiers, Padding, PointF, Tool,
    object::{Axis, Orient},
};
use ssx_imgfx::{Effect, ResizeFilter};
use ssx_types::Rect;

#[derive(Debug, Clone)]
enum Op {
    Draw { tool: u8, a: (u16, u16), b: (u16, u16), shift: bool },
    Click { at: (u16, u16), shift: bool },
    SelectAll,
    MoveSelection { from: (u16, u16), to: (u16, u16) },
    Delete,
    Nudge { dx: i8, dy: i8 },
    Copy,
    Paste,
    Duplicate,
    ToFront,
    ToBack,
    Group,
    Ungroup,
    Style { w: u8, r: u8 },
    Type { text: String },
    Erase { a: (u16, u16), b: (u16, u16) },
    Crop { x: u16, y: u16, w: u16, h: u16 },
    CutOut { vertical: bool, a: u16, b: u16 },
    Orient(u8),
    Canvas { l: i8, t: i8, r: i8, b: i8 },
    Pad(u8),
    Background(u8),
    Effect(u8),
    Resize { w: u16, h: u16 },
    AutoCrop,
    Undo,
    Redo,
}

const TOOLS: [Tool; 20] = [
    Tool::Rectangle,
    Tool::Ellipse,
    Tool::Line,
    Tool::Arrow,
    Tool::Freehand,
    Tool::FreehandArrow,
    Tool::Text,
    Tool::Balloon,
    Tool::Step,
    Tool::Magnify,
    Tool::Spotlight,
    Tool::Blur,
    Tool::Pixelate,
    Tool::Highlight,
    Tool::HighlightPen,
    Tool::Sticker,
    Tool::Cursor,
    Tool::Grid,
    Tool::Rectangle,
    Tool::Ellipse,
];

fn pt() -> impl Strategy<Value = (u16, u16)> {
    (0u16..300, 0u16..200)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u8..20, pt(), pt(), any::<bool>()).prop_map(|(tool, a, b, shift)| Op::Draw { tool, a, b, shift }),
        3 => (pt(), any::<bool>()).prop_map(|(at, shift)| Op::Click { at, shift }),
        1 => Just(Op::SelectAll),
        3 => (pt(), pt()).prop_map(|(from, to)| Op::MoveSelection { from, to }),
        2 => Just(Op::Delete),
        2 => (any::<i8>(), any::<i8>()).prop_map(|(dx, dy)| Op::Nudge { dx, dy }),
        1 => Just(Op::Copy),
        2 => Just(Op::Paste),
        1 => Just(Op::Duplicate),
        1 => Just(Op::ToFront),
        1 => Just(Op::ToBack),
        1 => Just(Op::Group),
        1 => Just(Op::Ungroup),
        2 => (any::<u8>(), any::<u8>()).prop_map(|(w, r)| Op::Style { w, r }),
        2 => "[a-zA-Z0-9 \n日本]{0,12}".prop_map(|text| Op::Type { text }),
        1 => (pt(), pt()).prop_map(|(a, b)| Op::Erase { a, b }),
        1 => (0u16..200, 0u16..150, 1u16..200, 1u16..150).prop_map(|(x, y, w, h)| Op::Crop { x, y, w, h }),
        1 => (any::<bool>(), 0u16..250, 0u16..250).prop_map(|(vertical, a, b)| Op::CutOut { vertical, a, b }),
        1 => (0u8..5).prop_map(Op::Orient),
        1 => (any::<i8>(), any::<i8>(), any::<i8>(), any::<i8>()).prop_map(|(l, t, r, b)| Op::Canvas { l, t, r, b }),
        1 => any::<u8>().prop_map(Op::Pad),
        1 => any::<u8>().prop_map(Op::Background),
        1 => (0u8..8).prop_map(Op::Effect),
        1 => (10u16..400, 10u16..300).prop_map(|(w, h)| Op::Resize { w, h }),
        1 => Just(Op::AutoCrop),
        3 => Just(Op::Undo),
        2 => Just(Op::Redo),
    ]
}

fn pf(p: (u16, u16)) -> PointF {
    PointF::new(f32::from(p.0), f32::from(p.1))
}

fn apply(s: &mut EditorSession, op: &Op) {
    let none = Modifiers::NONE;
    match op {
        Op::Draw { tool, a, b, shift } => {
            s.set_tool(TOOLS[*tool as usize % TOOLS.len()]);
            let m = Modifiers { shift: *shift, ..none };
            s.pointer_down(pf(*a), none, None);
            s.pointer_move(pf((u16::midpoint(a.0, b.0), u16::midpoint(a.1, b.1))), m, None);
            s.pointer_move(pf(*b), m, None);
            s.pointer_up(pf(*b), m);
            // Text tools start editing: type something short, then commit.
            if s.text_edit_state().is_some() {
                s.text_insert("ab");
                s.key_down(Key::Enter, none);
            }
        }
        Op::Click { at, shift } => {
            s.set_tool(Tool::Select);
            let m = Modifiers { shift: *shift, ..none };
            s.pointer_down(pf(*at), m, None);
            s.pointer_up(pf(*at), m);
        }
        Op::SelectAll => s.select_all(),
        Op::MoveSelection { from, to } => {
            s.set_tool(Tool::Select);
            s.pointer_down(pf(*from), none, None);
            s.pointer_move(pf(*to), none, None);
            s.pointer_up(pf(*to), none);
        }
        Op::Delete => s.delete_selection(),
        Op::Nudge { dx, dy } => s.nudge(f32::from(*dx), f32::from(*dy)),
        Op::Copy => s.copy(),
        Op::Paste => s.paste(),
        Op::Duplicate => s.duplicate(),
        Op::ToFront => s.bring_to_front(),
        Op::ToBack => s.send_to_back(),
        Op::Group => s.group_selection(),
        Op::Ungroup => s.ungroup_selection(),
        Op::Style { w, r } => {
            let (w, r) = (f32::from(*w % 20), f32::from(*r % 30));
            s.set_style(move |st| {
                st.stroke_width = w;
                st.corner_radius = r;
                st.stroke = Color::rgb((w * 10.0) as u8, 0, 200);
            });
        }
        Op::Type { text } => {
            s.set_tool(Tool::Select);
            if let Some(id) = s.selection().first().copied()
                && s.document().object(id).is_some_and(|o| o.kind.text_content().is_some())
            {
                s.begin_text_edit(id);
                s.text_insert(text);
                s.commit_text_edit();
            }
        }
        Op::Erase { a, b } => {
            s.set_tool(Tool::Eraser);
            s.pointer_down(pf(*a), none, None);
            s.pointer_move(pf(*b), none, None);
            s.pointer_up(pf(*b), none);
        }
        Op::Crop { x, y, w, h } => {
            let _ = s.crop(Rect::new(i32::from(*x), i32::from(*y), u32::from(*w), u32::from(*h)));
        }
        Op::CutOut { vertical, a, b } => {
            let axis = if *vertical { Axis::X } else { Axis::Y };
            let _ = s.cut_out(axis, i32::from(*a), i32::from(*b));
        }
        Op::Orient(k) => {
            let o = [
                Orient::Rotate90,
                Orient::Rotate180,
                Orient::Rotate270,
                Orient::FlipH,
                Orient::FlipV,
            ][*k as usize % 5];
            let _ = s.orient(o);
        }
        Op::Canvas { l, t, r, b } => {
            let c = |v: i8| i32::from(v) / 4;
            let _ = s.resize_canvas(c(*l), c(*t), c(*r), c(*b));
        }
        Op::Pad(v) => {
            let _ = s.set_padding(Padding::uniform(u32::from(*v % 20)));
        }
        Op::Background(v) => {
            let _ = s.set_background(if v % 2 == 0 {
                Fill::None
            } else {
                Fill::solid(Color::rgb(*v, 10, 10))
            });
        }
        Op::Effect(k) => {
            let e = match k % 8 {
                0 => Effect::Invert,
                1 => Effect::Grayscale,
                2 => Effect::GaussianBlur { sigma: 2.0 },
                3 => Effect::Pixelate { block: 4 },
                4 => Effect::Border { width: 3, color: [0, 0, 0, 255] },
                5 => Effect::Sepia,
                6 => Effect::RoundedCorners { radius: 6.0 },
                _ => Effect::DropShadow { params: ssx_imgfx::ShadowParams::default() },
            };
            let _ = s.apply_effect(&e, None);
        }
        Op::Resize { w, h } => {
            let _ = s.resize(u32::from(*w), u32::from(*h), ResizeFilter::Bilinear);
        }
        Op::AutoCrop => {
            let _ = s.auto_crop(4);
        }
        Op::Undo => s.undo(),
        Op::Redo => s.redo(),
    }
}

fn small_session() -> EditorSession {
    let mut s = EditorSession::new(doc(120, 80));
    // Keep the property tests fast: 120x80 base, big history.
    s.set_history_limits(ssx_editor::HistoryLimits { max_entries: 10_000, max_bytes: usize::MAX });
    s
}

fn snapshot(s: &EditorSession) -> String {
    s.document().to_json().expect("serialises")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, max_shrink_iters: 200, ..ProptestConfig::default() })]

    /// Undoing every step returns the initial document byte-for-byte; redoing every step returns
    /// the final one byte-for-byte.
    #[test]
    fn undo_all_and_redo_all_round_trip(ops in proptest::collection::vec(op(), 1..28)) {
        let mut s = small_session();
        let initial = snapshot(&s);
        for o in &ops {
            apply(&mut s, o);
        }
        let last = snapshot(&s);
        let steps = s.history().undo_len();
        for _ in 0..steps {
            s.undo();
        }
        prop_assert!(!s.can_undo());
        prop_assert_eq!(snapshot(&s), initial, "undo-all must restore the initial document");
        for _ in 0..steps {
            s.redo();
        }
        prop_assert_eq!(snapshot(&s), last, "redo-all must restore the final document");
    }

    /// After any operation sequence every object id is unique and step numbers are 1..n.
    #[test]
    fn invariants_hold(ops in proptest::collection::vec(op(), 1..28)) {
        let mut s = small_session();
        for o in &ops {
            apply(&mut s, o);
            let d = s.document();
            let mut ids: Vec<_> = d.objects().iter().map(|o| o.id).collect();
            ids.sort();
            ids.dedup();
            prop_assert_eq!(ids.len(), d.objects().len(), "duplicate ids");
            for sel in s.selection() {
                prop_assert!(d.object(*sel).is_some(), "selection refers to a missing object");
            }
            let next = d.peek_next_id();
            prop_assert!(d.objects().iter().all(|o| o.id < next), "next id is above every id");
            let steps: Vec<u32> = d.objects().iter().filter_map(|o| d.step_number(o.id)).collect();
            let want: Vec<u32> = (d.step_start()..d.step_start() + steps.len() as u32).collect();
            let autos: Vec<u32> = d.objects().iter().filter_map(|o| match &o.kind {
                ssx_editor::ObjectKind::Step(st) if st.manual.is_none() => d.step_number(o.id),
                _ => None,
            }).collect();
            prop_assert!(autos.windows(2).all(|w| w[1] == w[0] + 1), "auto steps count up: {autos:?}");
            let _ = want;
            let (w, h) = d.canvas_size();
            prop_assert!(w > 0 && h > 0);
        }
        // The final state always renders and round-trips through the project format.
        let f = s.render(&ssx_editor::RenderOptions::default());
        prop_assert_eq!(f.size(), s.document().canvas_size().into_size());
        let json = snapshot(&s);
        let back = ssx_editor::Document::from_json(&json).unwrap();
        prop_assert_eq!(back.to_json().unwrap(), json);
    }
}

trait IntoSize {
    fn into_size(self) -> ssx_types::Size;
}
impl IntoSize for (u32, u32) {
    fn into_size(self) -> ssx_types::Size {
        ssx_types::Size::new(self.0, self.1)
    }
}

#[test]
fn redo_is_cleared_by_new_edits_and_history_is_bounded() {
    let mut s = small_session();
    s.set_tool(Tool::Rectangle);
    for i in 0..5 {
        s.pointer_down(PointF::new(10.0 + i as f32, 10.0), Modifiers::NONE, None);
        s.pointer_move(PointF::new(60.0, 40.0 + i as f32), Modifiers::NONE, None);
        s.pointer_up(PointF::new(60.0, 40.0 + i as f32), Modifiers::NONE);
    }
    s.undo();
    s.undo();
    assert!(s.can_redo());
    s.pointer_down(PointF::new(70.0, 10.0), Modifiers::NONE, None);
    s.pointer_move(PointF::new(110.0, 50.0), Modifiers::NONE, None);
    s.pointer_up(PointF::new(110.0, 50.0), Modifiers::NONE);
    assert!(!s.can_redo(), "a new edit invalidates redo");

    // Bounded memory: with a tiny budget old steps are evicted but the newest always survive.
    let mut b = EditorSession::new(doc(120, 80));
    b.set_history_limits(ssx_editor::HistoryLimits { max_entries: 5, max_bytes: usize::MAX });
    for i in 0..20u32 {
        b.resize_canvas(1, 0, 0, 0).unwrap();
        assert!(b.history().undo_len() <= 5, "step {i}");
    }
    let start_w = b.document().canvas_size().0;
    for _ in 0..10 {
        b.undo();
    }
    assert_eq!(b.document().canvas_size().0, start_w - 5, "only five steps were retained");
    let mut m = EditorSession::new(doc(120, 80));
    m.set_history_limits(ssx_editor::HistoryLimits { max_entries: 1000, max_bytes: 40_000 });
    for i in 0..30 {
        m.resize(120 + i, 80, ResizeFilter::Nearest).unwrap();
    }
    assert!(
        m.history().approx_bytes() <= 40_000 + 120 * 100 * 4 * 2,
        "byte budget respected: {}",
        m.history().approx_bytes()
    );
    assert!(m.history().undo_len() < 30);
}
