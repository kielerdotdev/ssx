//! Hit-testing through the session: tolerance, z-order, kinds, groups and locks.

#![allow(clippy::float_cmp)] // tests assert exact geometry values

mod common;

use common::*;
use ssx_editor::{
    Color, EditorSession, Fill, Modifiers, PointF, RectF, Style, object::*, style::DashStyle,
};

fn p(x: f32, y: f32) -> PointF {
    PointF::new(x, y)
}

fn session_with(f: impl FnOnce(&mut ssx_editor::Document)) -> EditorSession {
    let mut d = doc(300, 200);
    f(&mut d);
    EditorSession::new(d)
}

#[test]
fn topmost_object_wins() {
    let mut ids = Vec::new();
    let s = session_with(|d| {
        for i in 0..3 {
            ids.push(add(
                d,
                filled(Color::rgb(i * 60, 0, 0)),
                rect_kind(10.0 + i as f32 * 10.0, 10.0, 60.0, 60.0),
            ));
        }
    });
    assert_eq!(s.hit_test(p(65.0, 40.0)), Some(ids[2]), "all three overlap: the last is on top");
    assert_eq!(s.hit_test(p(12.0, 40.0)), Some(ids[0]));
    assert_eq!(s.hit_test(p(200.0, 150.0)), None);
}

#[test]
fn unfilled_shapes_are_hit_on_outline_only_but_with_tolerance() {
    let mut id = ObjectId(0);
    let mut s =
        session_with(|d| id = add(d, stroke(Color::RED, 2.0), rect_kind(50.0, 50.0, 100.0, 60.0)));
    assert_eq!(s.hit_test(p(50.0, 80.0)), Some(id));
    assert_eq!(
        s.hit_test(p(100.0, 80.0)),
        None,
        "interior of an unfilled rectangle passes clicks through"
    );
    assert_eq!(s.hit_test(p(46.0, 80.0)), Some(id), "slack around the outline");
    assert_eq!(s.hit_test(p(30.0, 80.0)), None);
    // Zooming out widens the slack in image pixels (constant on screen).
    s.set_view_scale(0.2);
    assert_eq!(s.hit_test(p(30.0, 80.0)), Some(id));
}

#[test]
fn every_kind_is_hittable_where_it_is_drawn() {
    let mut s = session_with(|d| {
        add(d, stroke(Color::RED, 4.0), line_kind((10.0, 10.0), (90.0, 10.0)));
        add(
            d,
            stroke(Color::RED, 4.0),
            ObjectKind::Arrow(ArrowShape {
                a: PointF::new(10.0, 30.0),
                b: PointF::new(90.0, 30.0),
                heads: ArrowHeads::default(),
            }),
        );
        add(
            d,
            stroke(Color::RED, 4.0),
            ObjectKind::Freehand(FreehandShape {
                points: vec![p(10.0, 50.0), p(50.0, 70.0), p(90.0, 50.0)],
                smooth: true,
                arrow: None,
            }),
        );
        add(
            d,
            filled(Color::rgb(0, 0, 200)),
            ObjectKind::Ellipse(BoxShape {
                rect: RectF::new(110.0, 10.0, 60.0, 40.0),
                rotation: 0.0,
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Step(StepShape {
                center: p(200.0, 30.0),
                diameter: 30.0,
                ..StepShape::default()
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Blur(EffectBox { rect: RectF::new(110.0, 60.0, 60.0, 40.0), amount: 5.0 }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Pixelate(EffectBox {
                rect: RectF::new(180.0, 60.0, 60.0, 40.0),
                amount: 5.0,
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Magnify(MagnifyShape {
                rect: RectF::new(10.0, 100.0, 60.0, 60.0),
                circular: true,
                source: p(0.0, 0.0),
                zoom: 2.0,
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Highlight(HighlightShape {
                rect: RectF::new(80.0, 110.0, 60.0, 14.0),
                points: vec![],
            }),
        );
        add(
            d,
            Style { stroke_width: 16.0, ..Style::default() },
            ObjectKind::Highlight(HighlightShape {
                rect: RectF::default(),
                points: vec![p(150.0, 150.0), p(220.0, 150.0)],
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Cursor(CursorShape {
                pos: p(250.0, 20.0),
                kind: CursorKind::Arrow,
                scale: 1.0,
            }),
        );
        add(
            d,
            Style::default(),
            ObjectKind::Sticker(StickerShape {
                rect: RectF::new(250.0, 100.0, 30.0, 30.0),
                rotation: 0.0,
                source: StickerSource::Builtin { which: BuiltinSticker::Star },
            }),
        );
        add(
            d,
            Style { fill: Fill::None, dash: DashStyle::Solid, ..Style::default() },
            ObjectKind::Grid(GridShape {
                rect: RectF::new(180.0, 150.0, 60.0, 40.0),
                pattern: GridPattern::Grid,
                spacing: 10.0,
            }),
        );
    });
    let n = s.document().objects().len();
    let probes = [
        p(50.0, 10.0),
        p(50.0, 30.0),
        p(50.0, 68.0),
        p(140.0, 30.0),
        p(200.0, 30.0),
        p(140.0, 80.0),
        p(210.0, 80.0),
        p(40.0, 130.0),
        p(100.0, 117.0),
        p(160.0, 150.0),
        p(255.0, 30.0),
        p(265.0, 115.0),
        p(200.0, 170.0),
    ];
    assert_eq!(probes.len(), n);
    for (i, pr) in probes.iter().enumerate() {
        let id = s.hit_test(*pr).unwrap_or_else(|| panic!("probe {i} at {pr:?} hit nothing"));
        assert_eq!(id, s.document().objects()[i].id, "probe {i}");
    }
    // Clicking selects; a click on empty canvas deselects.
    s.set_tool(ssx_editor::Tool::Select);
    s.pointer_down(probes[3], Modifiers::NONE, None);
    s.pointer_up(probes[3], Modifiers::NONE);
    assert_eq!(s.selection().len(), 1);
    s.pointer_down(p(290.0, 190.0), Modifiers::NONE, None);
    s.pointer_up(p(290.0, 190.0), Modifiers::NONE);
    assert!(s.selection().is_empty());
}

#[test]
fn spotlights_yield_to_other_objects_and_locked_or_hidden_are_skipped() {
    let mut ids = Vec::new();
    let mut s = session_with(|d| {
        ids.push(add(d, filled(Color::RED), rect_kind(50.0, 50.0, 40.0, 40.0)));
        ids.push(add(
            d,
            Style::default(),
            ObjectKind::Spotlight(SpotlightShape {
                rect: RectF::new(0.0, 0.0, 200.0, 200.0),
                ellipse: false,
                dim: Color::rgba(0, 0, 0, 100),
                feather: 0.0,
            }),
        ));
        ids.push(add(d, filled(Color::BLACK), rect_kind(120.0, 120.0, 20.0, 20.0)));
    });
    assert_eq!(
        s.hit_test(p(60.0, 60.0)),
        Some(ids[0]),
        "spotlight is below in priority even though it is above in z-order"
    );
    assert_eq!(s.hit_test(p(20.0, 20.0)), Some(ids[1]));
    s.set_locked(ids[2], true);
    assert_eq!(s.hit_test(p(130.0, 130.0)), Some(ids[1]), "locked object is skipped");
    s.set_visible(ids[1], false);
    assert_eq!(s.hit_test(p(130.0, 130.0)), None);
}

#[test]
fn rotated_boxes_are_hit_in_their_rotated_shape() {
    let mut id = ObjectId(0);
    let s = session_with(|d| {
        id = add(
            d,
            filled(Color::RED),
            ObjectKind::Rectangle(BoxShape {
                rect: RectF::new(100.0, 90.0, 100.0, 20.0),
                rotation: std::f32::consts::FRAC_PI_2,
            }),
        );
    });
    // Rotated a quarter turn about (150,100): a vertical bar x in [140,160], y in [50,150].
    assert_eq!(s.hit_test(p(150.0, 60.0)), Some(id));
    assert_eq!(s.hit_test(p(105.0, 100.0)), None, "the old horizontal extent is empty now");
}

#[test]
fn text_and_balloon_hit_their_boxes_and_the_tail() {
    let mut ids = Vec::new();
    let s = session_with(|d| {
        let mut o = Object::new(
            d.alloc_id(),
            Style::default(),
            ObjectKind::Text(TextShape {
                rect: RectF::new(20.0, 20.0, 0.0, 0.0),
                rotation: 0.0,
                auto_width: true,
                content: TextContent { text: "Hello".into(), ..TextContent::default() },
            }),
        );
        ssx_editor::session::sync_text_rect(&mut o, &mut ssx_editor::text::TextEngine::new());
        ids.push(o.id);
        d.insert_object(usize::MAX, o);
        ids.push(add(
            d,
            Style::default(),
            ObjectKind::Balloon(BalloonShape {
                rect: RectF::new(120.0, 20.0, 100.0, 40.0),
                tail: p(140.0, 100.0),
                ..BalloonShape::default()
            }),
        ));
    });
    assert_eq!(
        s.hit_test(p(30.0, 30.0)),
        Some(ids[0]),
        "clicking inside the text box, not only on glyphs"
    );
    assert_eq!(s.hit_test(p(160.0, 40.0)), Some(ids[1]));
    assert_eq!(s.hit_test(p(140.0, 85.0)), Some(ids[1]), "tail");
    assert_eq!(s.hit_test(p(200.0, 85.0)), None);
}

#[test]
fn handles_are_absent_for_locked_or_empty_selection() {
    let mut id = ObjectId(0);
    let mut s =
        session_with(|d| id = add(d, stroke(Color::RED, 2.0), rect_kind(50.0, 50.0, 100.0, 60.0)));
    assert!(s.handles().is_empty());
    s.select(&[id]);
    assert_eq!(s.handles().len(), 9);
    s.set_locked(id, true);
    assert!(s.handles().is_empty(), "locked objects cannot be transformed");
}
