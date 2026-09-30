//! Timing reports (ignored by default so CI stays deterministic).
//! Run: `cargo test -p ssx-editor --release --test perf -- --ignored --nocapture`

#![allow(clippy::float_cmp)] // tests assert exact geometry values

mod common;

use std::time::Instant;

use common::*;
use ssx_editor::{
    Color, Document, EditorSession, Modifiers, PointF, RectF, RenderOptions, Renderer, Style, Tool,
    object::*, style::Shadow,
};
use ssx_types::Rect;

fn big_doc(w: u32, h: u32, objects: usize) -> Document {
    let mut d = Document::new(sample_frame(w, h)).unwrap();
    let mut engine = ssx_editor::text::TextEngine::new();
    for i in 0..objects {
        let x = (i * 97 % (w as usize - 400)) as f32;
        let y = (i * 53 % (h as usize - 300)) as f32;
        let st = Style {
            stroke: Color::rgb((i * 40) as u8, 80, 200),
            stroke_width: 3.0,
            ..Style::default()
        };
        match i % 10 {
            0 => {
                add(&mut d, st, rect_kind(x, y, 300.0, 150.0));
            }
            1 => {
                add(
                    &mut d,
                    st,
                    ObjectKind::Ellipse(BoxShape {
                        rect: RectF::new(x, y, 200.0, 120.0),
                        rotation: 0.3,
                    }),
                );
            }
            2 => {
                add(
                    &mut d,
                    st,
                    ObjectKind::Arrow(ArrowShape {
                        a: PointF::new(x, y),
                        b: PointF::new(x + 250.0, y + 100.0),
                        heads: ArrowHeads::default(),
                    }),
                );
            }
            3 => {
                let mut o = Object::new(
                    d.alloc_id(),
                    Style { stroke_width: 0.0, ..Style::default() },
                    ObjectKind::Text(TextShape {
                        rect: RectF::new(x, y, 0.0, 0.0),
                        rotation: 0.0,
                        auto_width: true,
                        content: TextContent {
                            text: format!("Annotation number {i}\nsecond line"),
                            font: FontSpec { size: 32.0, ..FontSpec::default() },
                            ..TextContent::default()
                        },
                    }),
                );
                ssx_editor::session::sync_text_rect(&mut o, &mut engine);
                d.insert_object(usize::MAX, o);
            }
            4 => {
                add(
                    &mut d,
                    Style { shadow: Some(Shadow::default()), ..st },
                    ObjectKind::Balloon(BalloonShape {
                        rect: RectF::new(x, y, 220.0, 90.0),
                        tail: PointF::new(x + 40.0, y + 150.0),
                        ..BalloonShape::default()
                    }),
                );
            }
            5 => {
                add(
                    &mut d,
                    Style::default(),
                    ObjectKind::Blur(EffectBox {
                        rect: RectF::new(x, y, 300.0, 200.0),
                        amount: 12.0,
                    }),
                );
            }
            6 => {
                add(
                    &mut d,
                    Style::default(),
                    ObjectKind::Pixelate(EffectBox {
                        rect: RectF::new(x, y, 250.0, 150.0),
                        amount: 14.0,
                    }),
                );
            }
            7 => {
                add(
                    &mut d,
                    ssx_editor::Tool::Highlight.preset().unwrap().style,
                    ObjectKind::Highlight(HighlightShape {
                        rect: RectF::new(x, y, 400.0, 30.0),
                        points: vec![],
                    }),
                );
            }
            8 => {
                add(
                    &mut d,
                    Style { opacity: 0.7, shadow: Some(Shadow::default()), ..st },
                    ObjectKind::Ellipse(BoxShape {
                        rect: RectF::new(x, y, 150.0, 150.0),
                        rotation: 0.0,
                    }),
                );
            }
            _ => {
                let pts: Vec<PointF> = (0..60)
                    .map(|k| PointF::new(x + k as f32 * 4.0, y + (k as f32 * 0.3).sin() * 40.0))
                    .collect();
                add(
                    &mut d,
                    st,
                    ObjectKind::Freehand(FreehandShape { points: pts, smooth: true, arrow: None }),
                );
            }
        }
    }
    d
}

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    eprintln!("{label:<58} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    out
}

#[test]
#[ignore = "timing report; run with --release --ignored --nocapture"]
fn report_render_timings() {
    for (w, h, n, label) in [
        (3840, 2160, 50, "4K / 50 objects"),
        (3840, 2160, 300, "4K / 300 objects"),
        (7680, 4320, 100, "8K / 100 objects"),
    ] {
        let d = big_doc(w, h, n);
        let mut r = Renderer::new();
        let cold = time(&format!("{label}: full 1x render (cold caches)"), || {
            r.render(&d, &RenderOptions::default())
        });
        assert_eq!(cold.width(), w);
        time(&format!("{label}: full 1x render (warm)"), || {
            r.render(&d, &RenderOptions::default())
        });
        time(&format!("{label}: 1920x1080 viewport at 1x"), || {
            r.render(
                &d,
                &RenderOptions {
                    viewport: Some(Rect::new(500, 400, 1920, 1080)),
                    ..RenderOptions::default()
                },
            )
        });
        time(&format!("{label}: fit-to-screen (scale 0.25) full canvas"), || {
            r.render(&d, &RenderOptions { scale: 0.25, ..RenderOptions::default() })
        });
        time(&format!("{label}: 2x zoom, 1600x900 viewport"), || {
            r.render(
                &d,
                &RenderOptions {
                    scale: 2.0,
                    viewport: Some(Rect::new(1000, 800, 1600, 900)),
                    ..RenderOptions::default()
                },
            )
        });
        time(&format!("{label}: dirty-rect repaint 256x256"), || {
            r.render(
                &d,
                &RenderOptions {
                    viewport: Some(Rect::new(700, 500, 256, 256)),
                    ..RenderOptions::default()
                },
            )
        });
    }
}

#[test]
#[ignore = "timing report; run with --release --ignored --nocapture"]
fn report_interaction_timings() {
    let d = big_doc(3840, 2160, 50);
    let mut s = EditorSession::new(d);
    s.set_tool(Tool::Select);
    time("session: 200 pointer-move events dragging a selection", || {
        s.pointer_down(PointF::new(10.0, 10.0), Modifiers::NONE, None);
        for i in 0..200 {
            s.pointer_move(
                PointF::new(10.0 + i as f32, 10.0 + i as f32 * 0.5),
                Modifiers::NONE,
                None,
            );
        }
        s.pointer_up(PointF::new(210.0, 110.0), Modifiers::NONE);
    });
    time("session: hit-test at 1000 random points", || {
        for i in 0..1000 {
            let _ = s.hit_test(PointF::new((i * 37 % 3800) as f32, (i * 91 % 2100) as f32));
        }
    });
    time("session: select all + nudge + undo", || {
        s.select_all();
        s.nudge(5.0, 5.0);
        s.undo();
    });
    time("document JSON serialise (4K, fast PNG)", || s.document().to_json().unwrap().len());
}
