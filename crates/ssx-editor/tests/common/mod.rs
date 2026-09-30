//! Shared helpers for the integration tests.
#![allow(dead_code)] // each test binary uses a different subset

use ssx_editor::{
    Color, Document, Fill, Object, ObjectId, ObjectKind, PointF, RectF, Style,
    object::{BoxShape, LineShape},
};
use ssx_types::Frame;

/// A deterministic "screenshot": gradient background, coloured window blocks and grey text-like
/// stripes. Busy enough that blur/pixelate/magnify/multiply are clearly visible.
pub fn sample_frame(w: u32, h: u32) -> Frame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let mut px = [
                (40 + x * 90 / w) as u8,
                (60 + y * 90 / h) as u8,
                (120 + (x + y) * 60 / (w + h)) as u8,
                255,
            ];
            // A white "window" with a title bar and text stripes.
            if (16..w - 16).contains(&x) && (16..h - 16).contains(&y) {
                px = [245, 245, 245, 255];
                if y < 34 {
                    px = [60, 90, 160, 255];
                } else if (y / 6) % 2 == 0 && x > 26 && x < w - 60 {
                    px = [70, 70, 70, 255];
                }
                if x > w - 56 && y > 44 && y < 90 {
                    px = [220, 80, 60, 255];
                }
            }
            data.extend_from_slice(&px);
        }
    }
    Frame::from_rgba8(w, h, data).expect("exact size")
}

pub fn doc(w: u32, h: u32) -> Document {
    Document::new(sample_frame(w, h)).expect("rgba8")
}

pub fn add(d: &mut Document, style: Style, kind: ObjectKind) -> ObjectId {
    let id = d.alloc_id();
    d.insert_object(usize::MAX, Object::new(id, style, kind));
    id
}

pub fn rect_kind(x: f32, y: f32, w: f32, h: f32) -> ObjectKind {
    ObjectKind::Rectangle(BoxShape { rect: RectF::new(x, y, w, h), rotation: 0.0 })
}

pub fn line_kind(a: (f32, f32), b: (f32, f32)) -> ObjectKind {
    ObjectKind::Line(LineShape { a: PointF::new(a.0, a.1), b: PointF::new(b.0, b.1) })
}

pub fn stroke(c: Color, w: f32) -> Style {
    Style { stroke: c, stroke_width: w, ..Style::default() }
}

pub fn filled(c: Color) -> Style {
    Style { fill: Fill::solid(c), stroke_width: 0.0, ..Style::default() }
}

/// Golden-image comparison. Set `UPDATE_GOLDEN=1` to (re)write the reference PNGs.
pub fn assert_golden(name: &str, frame: &Frame) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let path = dir.join(format!("{name}.png"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        frame.save(&path).unwrap();
        return;
    }
    let bytes = std::fs::read(&path).unwrap_or_else(|_| {
        panic!("missing golden {}; run with UPDATE_GOLDEN=1 to create it", path.display())
    });
    let want = Frame::decode(&bytes).expect("golden decodes");
    assert_eq!(
        (frame.width(), frame.height()),
        (want.width(), want.height()),
        "golden {name}: size differs"
    );
    let (mut max, mut sum, mut bad) = (0i32, 0u64, 0u64);
    let mut n = 0u64;
    for y in 0..want.height() {
        for (a, b) in frame.row(y).iter().zip(want.row(y)) {
            let d = (i32::from(*a) - i32::from(*b)).abs();
            max = max.max(d);
            sum += d as u64;
            bad += u64::from(d > 4);
            n += 1;
        }
    }
    let mean = sum as f64 / n.max(1) as f64;
    // Tolerance covers rasteriser float differences across CPUs/OSes (±1–2 levels on edges).
    let ok = max <= 24 && mean < 0.08 && (bad as f64) < n as f64 * 0.0015;
    if !ok {
        let out = std::env::temp_dir().join(format!("golden-actual-{name}.png"));
        let _ = frame.save(&out);
    }
    assert!(
        ok,
        "golden {name}: max diff {max}, mean {mean:.4}, {bad} channels differ by >4 (actual saved to temp dir)"
    );
}
