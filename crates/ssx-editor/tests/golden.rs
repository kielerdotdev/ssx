//! Golden-image tests: every object kind, blend mode and effect rendered and compared with the
//! committed PNGs in `tests/golden/` (tolerance based; regenerate with `UPDATE_GOLDEN=1`).

mod common;

use common::*;
use ssx_editor::{
    Color, Fill, Padding, PointF, RectF, RenderOptions, Style, render,
    object::*,
    style::{BlendMode, DashStyle, Shadow},
};
use ssx_imgfx::{Effect, ShadowParams};
use ssx_types::Rect;

const W: u32 = 240;
const H: u32 = 160;

fn shot(d: &ssx_editor::Document) -> ssx_types::Frame {
    render(d, &RenderOptions::default())
}

#[test]
fn base_only() {
    assert_golden("base_only", &shot(&doc(W, H)));
}

#[test]
fn rectangle_variants() {
    let mut d = doc(W, H);
    add(&mut d, Style { fill: Fill::solid(Color::rgba(255, 220, 0, 160)), stroke_width: 3.0, corner_radius: 12.0, ..Style::default() }, rect_kind(20.0, 20.0, 90.0, 50.0));
    add(&mut d, Style { dash: DashStyle::Dash, stroke: Color::rgb(0, 200, 0), stroke_width: 3.0, ..Style::default() }, ObjectKind::Rectangle(BoxShape { rect: RectF::new(130.0, 30.0, 80.0, 40.0), rotation: 0.35 }));
    add(&mut d, Style { dash: DashStyle::Dot, stroke: Color::rgb(255, 255, 255), stroke_width: 4.0, ..Style::default() }, rect_kind(30.0, 100.0, 80.0, 40.0));
    add(&mut d, Style { dash: DashStyle::DashDot, stroke: Color::BLACK, stroke_width: 2.0, ..Style::default() }, rect_kind(130.0, 100.0, 80.0, 40.0));
    assert_golden("rectangle_variants", &shot(&d));
}

#[test]
fn ellipse_gradient_and_stroke() {
    let mut d = doc(W, H);
    add(&mut d, Style { fill: Fill::Gradient { from: Color::rgb(255, 0, 100), to: Color::rgb(255, 230, 0), angle: 45.0 }, stroke: Color::WHITE, stroke_width: 4.0, ..Style::default() }, ObjectKind::Ellipse(BoxShape { rect: RectF::new(30.0, 30.0, 110.0, 80.0), rotation: 0.0 }));
    add(&mut d, stroke(Color::RED, 3.0), ObjectKind::Ellipse(BoxShape { rect: RectF::new(150.0, 40.0, 60.0, 90.0), rotation: 0.5 }));
    assert_golden("ellipse_gradient", &shot(&d));
}

#[test]
fn lines_and_arrows() {
    let mut d = doc(W, H);
    add(&mut d, stroke(Color::RED, 4.0), line_kind((20.0, 20.0), (120.0, 40.0)));
    let heads = |s, e| ArrowHeads { start: s, end: e, size: 4.0, angle: 25.0 };
    let arrow = |a: (f32, f32), b: (f32, f32), h| ObjectKind::Arrow(ArrowShape { a: PointF::new(a.0, a.1), b: PointF::new(b.0, b.1), heads: h });
    add(&mut d, stroke(Color::rgb(255, 255, 0), 4.0), arrow((20.0, 60.0), (120.0, 60.0), heads(HeadStyle::None, HeadStyle::Filled)));
    add(&mut d, stroke(Color::rgb(0, 255, 255), 3.0), arrow((20.0, 85.0), (120.0, 85.0), heads(HeadStyle::Open, HeadStyle::Open)));
    add(&mut d, stroke(Color::BLACK, 3.0), arrow((20.0, 110.0), (120.0, 110.0), heads(HeadStyle::Diamond, HeadStyle::Round)));
    add(&mut d, stroke(Color::WHITE, 3.0), arrow((20.0, 135.0), (120.0, 135.0), heads(HeadStyle::Bar, HeadStyle::Bar)));
    add(&mut d, stroke(Color::RED, 6.0), arrow((220.0, 20.0), (150.0, 140.0), heads(HeadStyle::Filled, HeadStyle::Filled)));
    add(&mut d, Style { dash: DashStyle::Dash, ..stroke(Color::rgb(0, 200, 0), 3.0) }, arrow((140.0, 20.0), (200.0, 20.0), heads(HeadStyle::None, HeadStyle::Open)));
    assert_golden("lines_and_arrows", &shot(&d));
}

#[test]
fn freehand_and_freehand_arrow() {
    let mut d = doc(W, H);
    let wave: Vec<PointF> = (0..40).map(|i| PointF::new(20.0 + i as f32 * 5.0, 50.0 + (i as f32 * 0.4).sin() * 25.0)).collect();
    add(&mut d, stroke(Color::RED, 4.0), ObjectKind::Freehand(FreehandShape { points: wave.clone(), smooth: true, arrow: None }));
    let poly: Vec<PointF> = wave.iter().map(|p| PointF::new(p.x, p.y + 60.0)).collect();
    add(&mut d, stroke(Color::rgb(255, 255, 0), 3.0), ObjectKind::Freehand(FreehandShape { points: poly, smooth: false, arrow: None }));
    let curve: Vec<PointF> = (0..30).map(|i| { let t = i as f32 / 29.0; PointF::new(40.0 + t * 150.0, 140.0 - (t * std::f32::consts::PI).sin() * 30.0) }).collect();
    add(&mut d, stroke(Color::rgb(255, 0, 200), 4.0), ObjectKind::Freehand(FreehandShape { points: curve, smooth: true, arrow: Some(ArrowHeads::default()) }));
    add(&mut d, stroke(Color::WHITE, 6.0), ObjectKind::Freehand(FreehandShape { points: vec![PointF::new(220.0, 20.0)], smooth: true, arrow: None }));
    assert_golden("freehand", &shot(&d));
}

fn text_content(text: &str, size: f32) -> TextContent {
    TextContent { text: text.into(), font: FontSpec { size, ..FontSpec::default() }, ..TextContent::default() }
}

fn text_obj(d: &mut ssx_editor::Document, x: f32, y: f32, auto: bool, w: f32, c: TextContent) {
    let mut o = Object::new(d.alloc_id(), Style { stroke_width: 0.0, ..Style::default() }, ObjectKind::Text(TextShape { rect: RectF::new(x, y, w, 0.0), rotation: 0.0, auto_width: auto, content: c }));
    let mut engine = ssx_editor::text::TextEngine::new();
    ssx_editor::session::sync_text_rect(&mut o, &mut engine);
    d.insert_object(usize::MAX, o);
}

#[test]
fn text_styles() {
    let mut d = doc(W, H);
    text_obj(&mut d, 12.0, 8.0, true, 0.0, text_content("Hello, ssx!", 28.0));
    text_obj(&mut d, 12.0, 46.0, true, 0.0, TextContent { font: FontSpec { size: 22.0, bold: true, italic: true, ..FontSpec::default() }, color: Color::WHITE, outline: Some(TextOutline { color: Color::BLACK, width: 2.0 }), ..text_content("Bold italic outline", 22.0) });
    text_obj(&mut d, 12.0, 80.0, true, 0.0, TextContent { color: Color::BLACK, background: Some(Color::rgb(255, 235, 120)), padding: 6.0, align: TextAlign::Center, ..text_content("Two lines\ncentred", 18.0) });
    text_obj(&mut d, 130.0, 84.0, false, 100.0, TextContent { color: Color::rgb(0, 40, 120), align: TextAlign::Right, ..text_content("A longer sentence that wraps inside a fixed box", 14.0) });
    assert_golden("text_styles", &shot(&d));
}

#[test]
fn text_rotated() {
    let mut d = doc(W, H);
    let mut o = Object::new(d.alloc_id(), Style { stroke_width: 0.0, ..Style::default() }, ObjectKind::Text(TextShape { rect: RectF::new(50.0, 60.0, 0.0, 0.0), rotation: -0.4, auto_width: true, content: TextContent { color: Color::rgb(255, 255, 0), outline: Some(TextOutline { color: Color::BLACK, width: 2.0 }), ..text_content("Rotated label", 30.0) } }));
    ssx_editor::session::sync_text_rect(&mut o, &mut ssx_editor::text::TextEngine::new());
    d.insert_object(usize::MAX, o);
    assert_golden("text_rotated", &shot(&d));
}

#[test]
fn balloons() {
    let mut d = doc(W, H);
    let st = ssx_editor::Tool::Balloon.preset().unwrap().style;
    add(&mut d, st.clone(), ObjectKind::Balloon(BalloonShape { rect: RectF::new(15.0, 12.0, 110.0, 45.0), tail: PointF::new(40.0, 85.0), tail_width: 22.0, content: TextContent { color: Color::BLACK, align: TextAlign::Center, padding: 8.0, ..text_content("Look here!", 16.0) } }));
    add(&mut d, st.clone(), ObjectKind::Balloon(BalloonShape { rect: RectF::new(130.0, 20.0, 95.0, 50.0), tail: PointF::new(235.0, 100.0), tail_width: 20.0, content: TextContent { color: Color::BLACK, align: TextAlign::Center, padding: 8.0, ..text_content("Right tail", 14.0) } }));
    add(&mut d, Style { fill: Fill::solid(Color::rgb(200, 255, 200)), ..st }, ObjectKind::Balloon(BalloonShape { rect: RectF::new(80.0, 105.0, 120.0, 40.0), tail: PointF::new(20.0, 120.0), tail_width: 18.0, content: TextContent { color: Color::BLACK, align: TextAlign::Center, padding: 8.0, ..text_content("Left tail", 14.0) } }));
    assert_golden("balloons", &shot(&d));
}

#[test]
fn steps_renumber_after_delete() {
    let mut d = doc(W, H);
    let st = ssx_editor::Tool::Step.preset().unwrap().style;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(add(&mut d, st.clone(), ObjectKind::Step(StepShape { center: PointF::new(35.0 + i as f32 * 55.0, 40.0), ..StepShape::default() })));
    }
    d.remove_object(ids[1]);
    for i in 0..3 {
        add(&mut d, st.clone(), ObjectKind::Step(StepShape { center: PointF::new(35.0 + i as f32 * 55.0, 110.0), diameter: 24.0 + i as f32 * 10.0, manual: (i == 2).then_some(42), ..StepShape::default() }));
    }
    assert_golden("steps", &shot(&d));
}

#[test]
fn magnifier_lenses() {
    let mut d = doc(W, H);
    let st = ssx_editor::Tool::Magnify.preset().unwrap().style;
    add(&mut d, st.clone(), ObjectKind::Magnify(MagnifyShape { rect: RectF::new(120.0, 10.0, 100.0, 100.0), circular: true, source: PointF::new(120.0, 70.0), zoom: 3.0 }));
    add(&mut d, Style { shadow: None, ..st }, ObjectKind::Magnify(MagnifyShape { rect: RectF::new(15.0, 90.0, 90.0, 55.0), circular: false, source: PointF::new(60.0, 40.0), zoom: 2.0 }));
    assert_golden("magnifiers", &shot(&d));
}

#[test]
fn spotlights_combine() {
    let mut d = doc(W, H);
    add(&mut d, Style::default(), ObjectKind::Spotlight(SpotlightShape { rect: RectF::new(20.0, 30.0, 90.0, 60.0), ellipse: false, dim: Color::rgba(0, 0, 0, 170), feather: 0.0 }));
    add(&mut d, Style::default(), ObjectKind::Spotlight(SpotlightShape { rect: RectF::new(130.0, 70.0, 80.0, 70.0), ellipse: true, dim: Color::rgba(0, 0, 0, 170), feather: 0.0 }));
    add(&mut d, stroke(Color::RED, 3.0), rect_kind(40.0, 50.0, 40.0, 20.0));
    assert_golden("spotlights", &shot(&d));
}

#[test]
fn spotlight_feathered() {
    let mut d = doc(W, H);
    add(&mut d, Style::default(), ObjectKind::Spotlight(SpotlightShape { rect: RectF::new(60.0, 40.0, 120.0, 80.0), ellipse: true, dim: Color::rgba(10, 10, 40, 200), feather: 10.0 }));
    assert_golden("spotlight_feathered", &shot(&d));
}

#[test]
fn blur_and_pixelate() {
    let mut d = doc(W, H);
    add(&mut d, Style::default(), ObjectKind::Blur(EffectBox { rect: RectF::new(20.0, 40.0, 100.0, 50.0), amount: 6.0 }));
    add(&mut d, Style::default(), ObjectKind::Pixelate(EffectBox { rect: RectF::new(130.0, 40.0, 90.0, 60.0), amount: 9.0 }));
    add(&mut d, Style::default(), ObjectKind::Blur(EffectBox { rect: RectF::new(-30.0, 120.0, 90.0, 60.0), amount: 3.0 }));
    assert_golden("blur_pixelate", &shot(&d));
}

#[test]
fn highlighter_multiplies() {
    let mut d = doc(W, H);
    add(&mut d, ssx_editor::Tool::Highlight.preset().unwrap().style, ObjectKind::Highlight(HighlightShape { rect: RectF::new(24.0, 60.0, 130.0, 14.0), points: vec![] }));
    let pen = ssx_editor::Tool::HighlightPen.preset().unwrap().style;
    let pts: Vec<PointF> = (0..20).map(|i| PointF::new(30.0 + i as f32 * 8.0, 100.0 + (i as f32 * 0.6).sin() * 6.0)).collect();
    add(&mut d, Style { stroke: Color::rgb(255, 120, 200), stroke_width: 16.0, ..pen }, ObjectKind::Highlight(HighlightShape { rect: RectF::default(), points: pts }));
    add(&mut d, Style { fill: Fill::solid(Color::rgb(0, 255, 255)), stroke_width: 0.0, blend: BlendMode::Screen, ..Style::default() }, rect_kind(170.0, 60.0, 50.0, 60.0));
    assert_golden("highlighter", &shot(&d));
}

fn checker(w: u32, h: u32) -> ssx_types::Frame {
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            data.extend_from_slice(&if on { [255, 128, 0, 255] } else { [30, 30, 30, 200] });
        }
    }
    ssx_types::Frame::from_rgba8(w, h, data).unwrap()
}

#[test]
fn inserted_images() {
    let mut d = doc(W, H);
    add(&mut d, Style::default(), ObjectKind::Image(ImageShape { rect: RectF::new(20.0, 20.0, 64.0, 64.0), rotation: 0.0, image: ImageData::new(checker(32, 32)) }));
    add(&mut d, Style { opacity: 0.6, ..Style::default() }, ObjectKind::Image(ImageShape { rect: RectF::new(120.0, 30.0, 80.0, 50.0), rotation: 0.3, image: ImageData::new(checker(16, 16)) }));
    assert_golden("images", &shot(&d));
}

#[test]
fn stickers() {
    let mut d = doc(W, H);
    let st = |c| Style { fill: Fill::solid(c), stroke_width: 0.0, ..Style::default() };
    let all = [BuiltinSticker::Check, BuiltinSticker::Cross, BuiltinSticker::Star, BuiltinSticker::Heart, BuiltinSticker::Exclamation, BuiltinSticker::Plus, BuiltinSticker::Minus, BuiltinSticker::ArrowRight, BuiltinSticker::Bolt];
    for (i, which) in all.into_iter().enumerate() {
        let (x, y) = (12.0 + (i % 5) as f32 * 45.0, 14.0 + (i / 5) as f32 * 50.0);
        add(&mut d, st(Color::rgb(255, 255 - (i as u8 * 25), 0)), ObjectKind::Sticker(StickerShape { rect: RectF::new(x, y, 38.0, 38.0), rotation: 0.0, source: StickerSource::Builtin { which } }));
    }
    add(&mut d, st(Color::rgb(200, 0, 0)), ObjectKind::Sticker(StickerShape { rect: RectF::new(20.0, 110.0, 40.0, 40.0), rotation: 0.4, source: StickerSource::Glyph { text: "A".into() } }));
    add(&mut d, Style::default(), ObjectKind::Sticker(StickerShape { rect: RectF::new(90.0, 110.0, 40.0, 40.0), rotation: 0.0, source: StickerSource::Bitmap { image: ImageData::new(checker(32, 32)) } }));
    assert_golden("stickers", &shot(&d));
}

#[test]
fn cursors() {
    let mut d = doc(W, H);
    let st = ssx_editor::Tool::Cursor.preset().unwrap().style;
    add(&mut d, st.clone(), ObjectKind::Cursor(CursorShape { pos: PointF::new(30.0, 30.0), kind: CursorKind::Arrow, scale: 1.0 }));
    add(&mut d, st.clone(), ObjectKind::Cursor(CursorShape { pos: PointF::new(100.0, 30.0), kind: CursorKind::Arrow, scale: 2.0 }));
    add(&mut d, Style { stroke: Color::WHITE, stroke_width: 2.0, fill: Fill::None, ..st.clone() }, ObjectKind::Cursor(CursorShape { pos: PointF::new(60.0, 120.0), kind: CursorKind::IBeam, scale: 1.5 }));
    add(&mut d, Style { stroke: Color::RED, stroke_width: 2.0, fill: Fill::None, ..st }, ObjectKind::Cursor(CursorShape { pos: PointF::new(150.0, 120.0), kind: CursorKind::Crosshair, scale: 1.5 }));
    assert_golden("cursors", &shot(&d));
}

#[test]
fn grid_patterns() {
    let mut d = doc(W, H);
    let mk = |pattern| ObjectKind::Grid(GridShape { rect: RectF::new(0.0, 0.0, 70.0, 60.0), pattern, spacing: 10.0 });
    for (i, p) in [GridPattern::Grid, GridPattern::HatchForward, GridPattern::HatchBackward, GridPattern::CrossHatch, GridPattern::Dots].into_iter().enumerate() {
        let (x, y) = ((i % 3) as f32 * 80.0 + 5.0, (i / 3) as f32 * 80.0 + 10.0);
        let mut k = mk(p);
        if let ObjectKind::Grid(g) = &mut k {
            g.rect = RectF::new(x, y, 70.0, 60.0);
        }
        let style = Style { stroke: Color::rgb(255, 255, 255), stroke_width: if p == GridPattern::Dots { 3.0 } else { 1.5 }, fill: if i == 0 { Fill::solid(Color::rgba(0, 0, 0, 120)) } else { Fill::None }, ..Style::default() };
        add(&mut d, style, k);
    }
    assert_golden("grid_patterns", &shot(&d));
}

#[test]
fn shadow_opacity_and_hidden() {
    let mut d = doc(W, H);
    add(&mut d, Style { fill: Fill::solid(Color::rgb(30, 144, 255)), stroke: Color::WHITE, stroke_width: 3.0, shadow: Some(Shadow { dx: 6.0, dy: 6.0, blur: 5.0, color: Color::rgba(0, 0, 0, 200) }), ..Style::default() }, rect_kind(20.0, 20.0, 90.0, 60.0));
    add(&mut d, Style { fill: Fill::solid(Color::RED), stroke_width: 0.0, opacity: 0.5, ..Style::default() }, ObjectKind::Ellipse(BoxShape { rect: RectF::new(80.0, 50.0, 100.0, 80.0), rotation: 0.0 }));
    let hidden = add(&mut d, filled(Color::rgb(0, 255, 0)), rect_kind(180.0, 10.0, 50.0, 50.0));
    d.object_mut(hidden).unwrap().visible = false;
    let mut o = Object::new(d.alloc_id(), Style { stroke_width: 0.0, shadow: Some(Shadow::default()), ..Style::default() }, ObjectKind::Text(TextShape { rect: RectF::new(120.0, 120.0, 0.0, 0.0), rotation: 0.0, auto_width: true, content: TextContent { color: Color::WHITE, ..text_content("Shadow text", 22.0) } }));
    ssx_editor::session::sync_text_rect(&mut o, &mut ssx_editor::text::TextEngine::new());
    d.insert_object(usize::MAX, o);
    assert_golden("shadow_opacity_hidden", &shot(&d));
}

#[test]
fn canvas_padding_and_gradient_background() {
    let mut d = doc(W, H);
    d.set_padding(Padding { left: 20, top: 16, right: 30, bottom: 24 });
    d.set_background(Fill::Gradient { from: Color::rgb(255, 120, 0), to: Color::rgb(120, 0, 200), angle: 60.0 });
    add(&mut d, stroke(Color::WHITE, 3.0), rect_kind(-15.0, -12.0, 60.0, 40.0));
    add(&mut d, Style { shadow: Some(Shadow::default()), ..stroke(Color::RED, 3.0) }, rect_kind(200.0, 130.0, 60.0, 40.0));
    assert_golden("canvas_padding_background", &shot(&d));
}

#[test]
fn scaled_render_2x_and_half() {
    let mut d = doc(W, H);
    add(&mut d, stroke(Color::RED, 3.0), ObjectKind::Ellipse(BoxShape { rect: RectF::new(40.0, 30.0, 100.0, 70.0), rotation: 0.0 }));
    let mut o = Object::new(d.alloc_id(), Style { stroke_width: 0.0, ..Style::default() }, ObjectKind::Text(TextShape { rect: RectF::new(60.0, 100.0, 0.0, 0.0), rotation: 0.0, auto_width: true, content: TextContent { color: Color::WHITE, ..text_content("Zoomed", 20.0) } }));
    ssx_editor::session::sync_text_rect(&mut o, &mut ssx_editor::text::TextEngine::new());
    d.insert_object(usize::MAX, o);
    assert_golden("scaled_2x", &render(&d, &RenderOptions { scale: 2.0, ..RenderOptions::default() }));
    assert_golden("scaled_half", &render(&d, &RenderOptions { scale: 0.5, ..RenderOptions::default() }));
}

#[test]
fn ops_crop_cutout_rotate_effects() {
    let mut d = doc(W, H);
    add(&mut d, stroke(Color::RED, 3.0), rect_kind(60.0, 40.0, 80.0, 50.0));
    let mut c = d.clone();
    c.crop(Rect::new(40, 20, 140, 100)).unwrap();
    assert_golden("op_crop", &shot(&c));
    let mut cut = d.clone();
    cut.cut_out(ssx_editor::object::Axis::X, 80, 120).unwrap();
    cut.cut_out(ssx_editor::object::Axis::Y, 30, 50).unwrap();
    assert_golden("op_cutout", &shot(&cut));
    let mut rot = d.clone();
    rot.orient(Orient::Rotate90).unwrap();
    assert_golden("op_rotate90", &shot(&rot));
    let mut fx = d.clone();
    fx.apply_effect(&Effect::Sepia, Some(Rect::new(0, 0, 120, 160))).unwrap();
    fx.apply_effect(&Effect::DropShadow { params: ShadowParams::default() }, None).unwrap();
    assert_golden("op_effects", &shot(&fx));
}

#[test]
fn viewport_render_equals_crop_of_full_render() {
    let mut d = doc(W, H);
    d.set_padding(Padding::uniform(10));
    add(&mut d, stroke(Color::RED, 3.0), ObjectKind::Ellipse(BoxShape { rect: RectF::new(40.0, 30.0, 100.0, 70.0), rotation: 0.2 }));
    add(&mut d, Style::default(), ObjectKind::Blur(EffectBox { rect: RectF::new(20.0, 40.0, 100.0, 50.0), amount: 5.0 }));
    add(&mut d, Style::default(), ObjectKind::Magnify(MagnifyShape { rect: RectF::new(120.0, 20.0, 80.0, 80.0), circular: true, source: PointF::new(60.0, 60.0), zoom: 2.0 }));
    for scale in [1.0f32, 2.0] {
        let full = render(&d, &RenderOptions { scale, ..RenderOptions::default() });
        for vp in [Rect::new(30, 25, 100, 80), Rect::new(0, 0, 50, 50), Rect::new(200, 100, 500, 500), Rect::new(-20, -20, 60, 60)] {
            let part = render(&d, &RenderOptions { scale, viewport: Some(vp), ..RenderOptions::default() });
            assert_eq!((part.width(), part.height()), (vp.width, vp.height));
            // Different translations round path coordinates slightly differently, so anti-aliased
            // edges may differ by a few levels; everything else must match exactly.
            let mut off_by_more_than_one = 0u32;
            for y in 0..vp.height as i32 {
                for x in 0..vp.width as i32 {
                    let (fx, fy) = (vp.x + x, vp.y + y);
                    let want = if fx < 0 || fy < 0 || fx >= full.width() as i32 || fy >= full.height() as i32 { [0; 4] } else { ssx_imgfx::get_pixel(&full, fx, fy) };
                    let got = ssx_imgfx::get_pixel(&part, x, y);
                    let diff = (0..4).map(|c| (i32::from(want[c]) - i32::from(got[c])).abs()).max().unwrap();
                    assert!(diff <= 16, "scale {scale} viewport {vp:?} at ({x},{y}): {got:?} vs {want:?}");
                    off_by_more_than_one += u32::from(diff > 1);
                }
            }
            assert!(
                f64::from(off_by_more_than_one) < f64::from(vp.width * vp.height) * 0.01,
                "scale {scale} viewport {vp:?}: {off_by_more_than_one} pixels differ"
            );
        }
    }
}

#[test]
fn export_at_1x_is_pixel_exact_for_untouched_areas() {
    let mut d = doc(W, H);
    add(&mut d, stroke(Color::RED, 3.0), rect_kind(60.0, 40.0, 40.0, 30.0));
    let out = shot(&d);
    let base = d.base();
    for (x, y) in [(0, 0), (5, 5), (230, 150), (200, 20), (120, 120)] {
        assert_eq!(ssx_imgfx::get_pixel(&out, x, y), ssx_imgfx::get_pixel(base, x, y));
    }
    assert_ne!(ssx_imgfx::get_pixel(&out, 60, 55), ssx_imgfx::get_pixel(base, 60, 55));
}

#[test]
fn rendering_is_deterministic() {
    let mut d = doc(W, H);
    let mut o = Object::new(d.alloc_id(), Style { stroke_width: 0.0, ..Style::default() }, ObjectKind::Text(TextShape { rect: RectF::new(10.0, 10.0, 0.0, 0.0), rotation: 0.1, auto_width: true, content: text_content("Determinism", 30.0) }));
    ssx_editor::session::sync_text_rect(&mut o, &mut ssx_editor::text::TextEngine::new());
    d.insert_object(usize::MAX, o);
    add(&mut d, Style { shadow: Some(Shadow::default()), ..stroke(Color::RED, 4.0) }, ObjectKind::Ellipse(BoxShape { rect: RectF::new(40.0, 30.0, 100.0, 70.0), rotation: 0.2 }));
    let a = shot(&d);
    let b = shot(&d);
    let mut fresh = ssx_editor::Renderer::new();
    let c = fresh.render(&d, &RenderOptions::default());
    assert_eq!(a, b);
    assert_eq!(a, c, "a cold renderer produces identical pixels");
}

#[test]
fn zero_and_tiny_documents_render() {
    let d = ssx_editor::Document::new(ssx_imgfx::solid_frame(1, 1, [9, 9, 9, 255])).unwrap();
    let f = shot(&d);
    assert_eq!(ssx_imgfx::get_pixel(&f, 0, 0), [9, 9, 9, 255]);
    let mut z = ssx_editor::Document::new(ssx_imgfx::solid_frame(0, 0, [0; 4])).unwrap();
    add(&mut z, stroke(Color::RED, 3.0), rect_kind(0.0, 0.0, 5.0, 5.0));
    let f = shot(&z);
    assert_eq!((f.width(), f.height()), (0, 0));
    for scale in [0.0f32, -1.0, f32::NAN, f32::INFINITY, 1e9] {
        let _ = render(&d, &RenderOptions { scale, ..RenderOptions::default() });
    }
    let _ = render(&d, &RenderOptions { viewport: Some(Rect::new(5, 5, 3, 3)), ..RenderOptions::default() });
}

#[test]
fn guides_option_draws_something() {
    let mut d = doc(W, H);
    add(&mut d, stroke(Color::RED, 3.0), rect_kind(60.0, 40.0, 40.0, 30.0));
    let plain = shot(&d);
    let guided = render(&d, &RenderOptions { include_guides: true, ..RenderOptions::default() });
    assert_ne!(plain, guided);
}
