//! Project-format tests: round trips, a committed v1 fixture (back-compat), and files from a
//! hypothetical newer version (forward-compat).

#![allow(clippy::float_cmp)] // tests assert exact geometry values

mod common;

use common::*;
use ssx_editor::{
    Color, Document, Fill, Padding, PointF, RectF, RenderOptions, Style,
    object::*,
    project, render,
    style::{BlendMode, DashStyle, Shadow},
};

fn checker(n: u32) -> ssx_types::Frame {
    let mut data = Vec::new();
    for y in 0..n {
        for x in 0..n {
            data.extend_from_slice(&if (x + y) % 2 == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 128]
            });
        }
    }
    ssx_types::Frame::from_rgba8(n, n, data).unwrap()
}

/// A document exercising every object kind and most style features.
fn kitchen_sink() -> Document {
    let mut d = doc(96, 64);
    d.set_padding(Padding { left: 4, top: 3, right: 2, bottom: 1 });
    d.set_background(Fill::Gradient {
        from: Color::rgb(255, 0, 0),
        to: Color::rgb(0, 0, 255),
        angle: 30.0,
    });
    d.set_step_start(3);
    let st = Style {
        stroke: Color::rgba(10, 20, 30, 200),
        stroke_width: 2.5,
        dash: DashStyle::DashDot,
        fill: Fill::Gradient { from: Color::WHITE, to: Color::BLACK, angle: 90.0 },
        opacity: 0.75,
        shadow: Some(Shadow { dx: 2.0, dy: 3.0, blur: 1.5, color: Color::rgba(0, 0, 0, 90) }),
        corner_radius: 4.0,
        blend: BlendMode::Multiply,
    };
    let text = TextContent {
        text: "Grüße 日本\nline two".into(),
        font: FontSpec { family: "Liberation Sans".into(), size: 13.0, bold: true, italic: true },
        color: Color::rgb(1, 2, 3),
        align: TextAlign::Center,
        line_spacing: 1.4,
        outline: Some(TextOutline { color: Color::WHITE, width: 1.5 }),
        background: Some(Color::rgba(255, 255, 0, 128)),
        padding: 3.0,
    };
    let kinds = vec![
        ObjectKind::Rectangle(BoxShape { rect: RectF::new(1.5, 2.5, 30.0, 20.0), rotation: 0.25 }),
        ObjectKind::Ellipse(BoxShape { rect: RectF::new(10.0, 10.0, 30.0, 20.0), rotation: -0.5 }),
        line_kind((1.0, 2.0), (30.0, 40.0)),
        ObjectKind::Arrow(ArrowShape {
            a: PointF::new(1.0, 1.0),
            b: PointF::new(50.0, 20.0),
            heads: ArrowHeads {
                start: HeadStyle::Diamond,
                end: HeadStyle::Open,
                size: 5.0,
                angle: 30.0,
            },
        }),
        ObjectKind::Freehand(FreehandShape {
            points: vec![PointF::new(1.0, 1.0), PointF::new(5.5, 9.25), PointF::new(20.0, 3.0)],
            smooth: true,
            arrow: Some(ArrowHeads::default()),
        }),
        ObjectKind::Text(TextShape {
            rect: RectF::new(5.0, 5.0, 60.0, 30.0),
            rotation: 0.1,
            auto_width: false,
            content: text.clone(),
        }),
        ObjectKind::Balloon(BalloonShape {
            rect: RectF::new(5.0, 5.0, 60.0, 30.0),
            tail: PointF::new(10.0, 60.0),
            tail_width: 12.0,
            content: text,
        }),
        ObjectKind::Step(StepShape {
            center: PointF::new(20.0, 20.0),
            diameter: 22.0,
            manual: Some(9),
            text_color: Color::BLACK,
        }),
        ObjectKind::Magnify(MagnifyShape {
            rect: RectF::new(20.0, 10.0, 40.0, 40.0),
            circular: false,
            source: PointF::new(10.0, 10.0),
            zoom: 2.5,
        }),
        ObjectKind::Spotlight(SpotlightShape {
            rect: RectF::new(5.0, 5.0, 40.0, 30.0),
            ellipse: true,
            dim: Color::rgba(0, 0, 0, 99),
            feather: 3.0,
        }),
        ObjectKind::Blur(EffectBox { rect: RectF::new(5.0, 5.0, 20.0, 10.0), amount: 4.0 }),
        ObjectKind::Pixelate(EffectBox { rect: RectF::new(30.0, 5.0, 20.0, 10.0), amount: 5.0 }),
        ObjectKind::Highlight(HighlightShape {
            rect: RectF::new(5.0, 40.0, 40.0, 8.0),
            points: vec![],
        }),
        ObjectKind::Highlight(HighlightShape {
            rect: RectF::default(),
            points: vec![PointF::new(5.0, 50.0), PointF::new(40.0, 52.0)],
        }),
        ObjectKind::Image(ImageShape {
            rect: RectF::new(50.0, 30.0, 16.0, 16.0),
            rotation: 0.2,
            image: ImageData::new(checker(4)),
        }),
        ObjectKind::Sticker(StickerShape {
            rect: RectF::new(50.0, 10.0, 16.0, 16.0),
            rotation: 0.0,
            source: StickerSource::Builtin { which: BuiltinSticker::Heart },
        }),
        ObjectKind::Sticker(StickerShape {
            rect: RectF::new(50.0, 10.0, 16.0, 16.0),
            rotation: 0.3,
            source: StickerSource::Glyph { text: "★".into() },
        }),
        ObjectKind::Sticker(StickerShape {
            rect: RectF::new(50.0, 10.0, 16.0, 16.0),
            rotation: 0.0,
            source: StickerSource::Bitmap { image: ImageData::new(checker(2)) },
        }),
        ObjectKind::Cursor(CursorShape {
            pos: PointF::new(60.0, 40.0),
            kind: CursorKind::IBeam,
            scale: 1.25,
        }),
        ObjectKind::Grid(GridShape {
            rect: RectF::new(2.0, 2.0, 40.0, 30.0),
            pattern: GridPattern::CrossHatch,
            spacing: 7.0,
        }),
    ];
    for k in kinds {
        let id = add(&mut d, st.clone(), k);
        if id.0.is_multiple_of(3) {
            d.object_mut(id).unwrap().group = Some(2);
        }
        if id.0.is_multiple_of(7) {
            d.object_mut(id).unwrap().locked = true;
        }
    }
    d
}

#[test]
fn every_kind_round_trips_exactly() {
    let d = kitchen_sink();
    let json = d.to_json().unwrap();
    let back = Document::from_json(&json).unwrap();
    assert_eq!(back, d);
    assert_eq!(back.to_json().unwrap(), json, "byte-stable re-serialisation");
    assert_eq!(render(&back, &RenderOptions::default()), render(&d, &RenderOptions::default()));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["format"], "ssxe");
    assert_eq!(v["version"], 1);
    let names: std::collections::BTreeSet<_> = v["document"]["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"]["type"].as_str().unwrap().to_owned())
        .collect();
    for want in [
        "rectangle",
        "ellipse",
        "line",
        "arrow",
        "freehand",
        "text",
        "balloon",
        "step",
        "magnify",
        "spotlight",
        "blur",
        "pixelate",
        "highlight",
        "image",
        "sticker",
        "cursor",
        "grid",
    ] {
        assert!(names.contains(want), "{want} missing from the kitchen sink");
    }
}

#[test]
fn committed_v1_fixture_still_loads_and_renders_identically() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/kitchen_sink_v1.ssxe");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        project::save(&kitchen_sink(), &path).unwrap();
    }
    let loaded = project::load(&path).expect("the v1 fixture must stay loadable forever");
    assert_eq!(loaded, kitchen_sink(), "fixture content matches what this build would write");
    assert_golden("fixture_v1_render", &render(&loaded, &RenderOptions::default()));
}

#[test]
fn newer_minor_version_with_unknown_content_loads_and_survives_resave() {
    let mut v: serde_json::Value =
        serde_json::from_str(&kitchen_sink().to_json().unwrap()).unwrap();
    v["minor"] = 9.into();
    v["document"]["future_canvas_feature"] = serde_json::json!({"mode": "spiral"});
    v["document"]["objects"][0]["style"]["future_glow"] = 3.into();
    v["document"]["objects"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"id": 500, "kind": {"type": "particle_cloud", "seed": 7, "layers": [1, 2, 3]}}));
    let d = Document::from_json(&v.to_string()).unwrap();
    assert!(matches!(d.objects().last().unwrap().kind, ObjectKind::Unknown(_)));
    // Unknown objects are inert: not rendered, not hit, not selectable.
    let _ = render(&d, &RenderOptions::default());
    assert!(!d.objects().last().unwrap().hit_test(PointF::new(0.0, 0.0), 1000.0));
    // Saving again keeps them so a round trip through an old version is lossless for them.
    let resaved: serde_json::Value = serde_json::from_str(&d.to_json().unwrap()).unwrap();
    let last = resaved["document"]["objects"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["kind"]["type"], "particle_cloud");
    assert_eq!(last["kind"]["layers"], serde_json::json!([1, 2, 3]));
    // Editing around an unknown object works and never panics.
    let mut s = ssx_editor::EditorSession::new(d);
    s.select_all();
    s.nudge(3.0, 3.0);
    s.delete_selection();
    s.undo();
    assert!(s.document().objects().iter().any(|o| matches!(o.kind, ObjectKind::Unknown(_))));
}

#[test]
fn incompatible_and_broken_files_give_actionable_errors() {
    let good = kitchen_sink().to_json().unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&good).unwrap();
    v["version"] = 2.into();
    let e = Document::from_json(&v.to_string()).unwrap_err().to_string();
    assert!(e.contains("newer") && e.contains("update"), "{e}");
    assert!(Document::from_json("").is_err());
    assert!(
        Document::from_json("{\"format\":\"png\"}").unwrap_err().to_string().contains("not an ssx")
    );
    let mut v: serde_json::Value = serde_json::from_str(&good).unwrap();
    v["document"]["objects"][0]["kind"]["rect"] = "oops".into();
    // A malformed object does not make the whole project unreadable: it loads as an inert
    // `Unknown` object and is written back verbatim (nothing is lost, nothing crashes).
    let d = Document::from_json(&v.to_string()).unwrap();
    assert!(matches!(d.objects()[0].kind, ObjectKind::Unknown(_)));
    let again: serde_json::Value = serde_json::from_str(&d.to_json().unwrap()).unwrap();
    assert_eq!(again["document"]["objects"][0]["kind"]["rect"], "oops");
    // Truncated file.
    assert!(Document::from_json(&good[..good.len() / 2]).is_err());
}

#[test]
fn project_file_helpers_round_trip_through_disk() {
    let dir = std::env::temp_dir().join(format!("ssxe-int-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sink.ssxe");
    let d = kitchen_sink();
    project::save(&d, &path).unwrap();
    assert_eq!(project::load(&path).unwrap(), d);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn colours_serialise_as_hex_and_accept_short_forms() {
    let s: Style =
        serde_json::from_str(r##"{"stroke":"#f00","fill":{"type":"solid","color":"00ff0080"}}"##)
            .unwrap();
    assert_eq!(s.stroke, Color::RED);
    assert_eq!(s.solid_fill(), Some(Color::rgba(0, 255, 0, 128)));
    assert!(serde_json::from_str::<Style>(r#"{"stroke":"red"}"#).is_err());
}
