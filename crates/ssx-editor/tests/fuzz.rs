//! No-panic "fuzz-ish" tests: thousands of random (and hostile) events must never panic and
//! must leave the document in a valid, renderable, serialisable state.

mod common;

use common::*;
use ssx_editor::{
    EditorSession, Key, Modifiers, PointF, RenderOptions, Tool,
    object::{Axis, Orient},
};
use ssx_types::Rect;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn coord(&mut self) -> f32 {
        match self.below(40) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => 1e30,
            4 => -1e30,
            5 => 1e-30,
            6 => 0.0,
            7 => -0.0,
            8 => f32::MAX,
            _ => -60.0 + self.unit() * 420.0,
        }
    }
    fn mods(&mut self) -> Modifiers {
        let b = self.below(16);
        Modifiers { shift: b & 1 != 0, ctrl: b & 2 != 0, alt: b & 4 != 0 }
    }
}

const KEYS: [Key; 12] = [
    Key::Left,
    Key::Right,
    Key::Up,
    Key::Down,
    Key::Home,
    Key::End,
    Key::Backspace,
    Key::Delete,
    Key::Enter,
    Key::Escape,
    Key::Tab,
    Key::Char('z'),
];

const TEXTS: [&str; 8] = ["a", "hello world", "日本語", "\n", "😀", "e\u{301}", "\r\n", "   "];

/// Optional fields legitimately serialise as `null`; anything else means a NaN/inf leaked in.
fn find_bad_null(v: &serde_json::Value, path: String) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => m.iter().find_map(|(k, x)| {
            if x.is_null() && !["shadow", "outline", "background", "arrow", "manual"].contains(&k.as_str()) {
                Some(format!("{path}/{k}"))
            } else {
                find_bad_null(x, format!("{path}/{k}"))
            }
        }),
        serde_json::Value::Array(a) => a.iter().enumerate().find_map(|(i, x)| find_bad_null(x, format!("{path}/{i}"))),
        _ => None,
    }
}

fn run(seed: u64, events: usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut s = EditorSession::new(doc(160, 100));
    let mut down = false;
    for i in 0..events {
        let r = rng.below(100);
        match r {
            0..=4 => s.set_tool(Tool::ALL[rng.below(Tool::ALL.len() as u64) as usize]),
            5..=24 if !down => {
                s.pointer_down(PointF::new(rng.coord(), rng.coord()), rng.mods(), Some(rng.unit()));
                down = true;
            }
            25..=54 => {
                s.pointer_move(PointF::new(rng.coord(), rng.coord()), rng.mods(), None);
            }
            55..=69 if down => {
                s.pointer_up(PointF::new(rng.coord(), rng.coord()), rng.mods());
                down = false;
            }
            70..=79 => {
                let k = KEYS[rng.below(KEYS.len() as u64) as usize];
                s.key_down(k, rng.mods());
            }
            80..=84 => s.text_insert(TEXTS[rng.below(TEXTS.len() as u64) as usize]),
            85 => s.double_click(PointF::new(rng.coord(), rng.coord()), rng.mods()),
            86 => s.set_view_scale(rng.coord()),
            87 => match rng.below(9) {
                0 => {
                    let _ = s.crop(Rect::new(rng.below(200) as i32 - 50, rng.below(120) as i32 - 30, rng.below(200) as u32, rng.below(150) as u32));
                }
                1 => {
                    let _ = s.cut_out(Axis::X, rng.below(200) as i32 - 20, rng.below(200) as i32 - 20);
                }
                2 => {
                    let _ = s.cut_out(Axis::Y, rng.below(150) as i32 - 20, rng.below(150) as i32 - 20);
                }
                3 => {
                    let _ = s.orient([Orient::Rotate90, Orient::Rotate180, Orient::Rotate270, Orient::FlipH, Orient::FlipV][rng.below(5) as usize]);
                }
                4 => {
                    let _ = s.resize_canvas(rng.below(40) as i32 - 20, rng.below(40) as i32 - 20, rng.below(40) as i32 - 20, rng.below(40) as i32 - 20);
                }
                5 => {
                    let _ = s.resize(rng.below(300) as u32, rng.below(200) as u32, ssx_imgfx::ResizeFilter::Lanczos3);
                }
                6 => {
                    let _ = s.apply_effect(&ssx_imgfx::Effect::GaussianBlur { sigma: rng.coord() }, Some(Rect::new(rng.below(100) as i32 - 50, 0, 80, 80)));
                }
                7 => {
                    let _ = s.apply_crop();
                }
                _ => {
                    let _ = s.flatten();
                }
            },
            88 => match rng.below(6) {
                0 => s.undo(),
                1 => s.redo(),
                2 => s.copy(),
                3 => s.paste(),
                4 => s.group_selection(),
                _ => s.bring_to_front(),
            },
            89 => {
                let w = rng.coord();
                s.set_style(move |st| {
                    st.stroke_width = w;
                    st.opacity = w;
                    st.corner_radius = w;
                });
            }
            90 => s.set_kind_props(|k| {
                if let Some(c) = k.text_content_mut() {
                    c.font.size = f32::NAN;
                }
            }),
            91 => s.delete_selection(),
            92 => s.nudge(rng.coord(), rng.coord()),
            93 => s.ime_preedit(Some(("x".into(), Some((5, 9))))),
            _ => {}
        }
        // Query APIs must always be safe too.
        let _ = s.cursor_hint();
        let _ = s.overlay();
        let _ = s.handles();
        let _ = s.take_events();
        for sel in s.selection() {
            assert!(
                s.document().object(*sel).is_some(),
                "seed {seed} step {i} (draw {r}): dangling selection {sel:?}; tool {:?}; objects {:?}; history {:?}",
                s.tool(),
                s.document().objects().iter().map(|o| (o.id, o.kind.name())).collect::<Vec<_>>(),
                s.history()
            );
        }
        if i % 40 == 0 {
            let d = s.document();
            let scale = [0.25, 1.0, 2.5][rng.below(3) as usize];
            let (cw, ch) = d.canvas_size();
            let vp = Rect::new(rng.below(60) as i32 - 30, rng.below(40) as i32 - 20, rng.below(200) as u32, rng.below(200) as u32);
            let f = s.render(&RenderOptions { scale, viewport: Some(vp), ..RenderOptions::default() });
            assert_eq!((f.width(), f.height()), (vp.width, vp.height));
            let _ = (cw, ch);
        }
    }
    if down {
        s.pointer_up(PointF::new(10.0, 10.0), Modifiers::NONE);
    }
    s.commit_text_edit();
    // Final state: renders, serialises, round-trips.
    let f = s.render(&RenderOptions::default());
    assert_eq!(f.size().width, s.document().canvas_size().0);
    let json = s.document().to_json().expect("serialises");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    if let Some(path) = find_bad_null(&v, String::new()) {
        panic!("seed {seed}: non-finite number serialised as null at {path}");
    }
    let back = ssx_editor::Document::from_json(&json).expect("re-parses");
    assert_eq!(back.to_json().unwrap(), json, "seed {seed}");
    // Undo everything: must not panic and must reach a valid document.
    let mut guard = 0;
    while s.can_undo() && guard < 10_000 {
        s.undo();
        guard += 1;
    }
    let _ = s.render(&RenderOptions::default());
}

#[test]
fn random_events_never_panic() {
    let seeds: u64 = std::env::var("FUZZ_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    for seed in 1..=seeds {
        run(seed, 250);
    }
}

#[test]
fn hostile_pointer_positions_on_every_tool() {
    let hostile = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30, f32::MAX, f32::MIN, 0.0];
    for tool in Tool::ALL {
        eprintln!("tool {tool:?}");
        let mut s = EditorSession::new(doc(160, 100));
        s.set_tool(tool);
        for &x in &hostile {
            for &y in &hostile {
                s.pointer_down(PointF::new(x, y), Modifiers::NONE, None);
                s.pointer_move(PointF::new(y, x), Modifiers::SHIFT, None);
                s.pointer_up(PointF::new(x, x), Modifiers::ALT);
                s.text_insert("x");
                s.commit_text_edit();
            }
        }
        // Finite but enormous drags.
        s.pointer_down(PointF::new(-1e9, -1e9), Modifiers::NONE, None);
        s.pointer_move(PointF::new(1e9, 1e9), Modifiers::NONE, None);
        s.pointer_up(PointF::new(1e9, 1e9), Modifiers::NONE);
        let _ = s.render(&RenderOptions::default());
        let _ = s.document().to_json().unwrap();
    }
}

#[test]
fn many_objects_and_long_freehand_stay_responsive() {
    let mut s = EditorSession::new(doc(400, 300));
    s.set_tool(Tool::Freehand);
    s.pointer_down(PointF::new(0.0, 0.0), Modifiers::NONE, None);
    for i in 0..5000 {
        s.pointer_move(PointF::new(i as f32 * 0.07, (i as f32 * 0.01).sin() * 100.0 + 150.0), Modifiers::NONE, None);
    }
    s.pointer_up(PointF::new(350.0, 150.0), Modifiers::NONE);
    let pts = match &s.document().objects()[0].kind {
        ssx_editor::ObjectKind::Freehand(f) => f.points.len(),
        _ => unreachable!(),
    };
    assert!(pts < 5000, "points are thinned while drawing ({pts})");
    s.set_tool(Tool::Rectangle);
    for i in 0..300 {
        let x = (i % 20) as f32 * 18.0;
        let y = (i / 20) as f32 * 18.0;
        s.clear_selection(); // otherwise the previous rectangle's handles would grab the press
        s.pointer_down(PointF::new(x + 1.0, y + 1.0), Modifiers::NONE, None);
        s.pointer_move(PointF::new(x + 12.0, y + 12.0), Modifiers::NONE, None);
        s.pointer_up(PointF::new(x + 12.0, y + 12.0), Modifiers::NONE);
    }
    assert_eq!(s.document().objects().len(), 301);
    let _ = s.render(&RenderOptions::default());
    s.select_all();
    s.nudge(3.0, 3.0);
    s.undo();
    s.undo();
}
