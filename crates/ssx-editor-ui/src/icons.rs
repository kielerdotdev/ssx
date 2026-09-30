//! Toolbar and UI icons drawn as vector geometry in code.
//!
//! No image assets are shipped: every icon is a short list of strokes and filled polygons on a
//! 24 x 24 grid ([`Icon::prims`]), painted through egui's tessellator by [`paint`]. That keeps
//! them crisp at any scale factor, lets them follow the theme colours, and avoids any licensing
//! question about third-party icon sets (these shapes are original, drawn for this editor).
//!
//! Design rules, so the set reads as one family: 24 px grid with 3 px padding, 1.6 px strokes
//! with round caps and joins, one *accent* element at most per icon (the yellow of the
//! highlighter, the dimmed area of the spotlight), and outlines rather than solid blobs
//! except for small details.
//!
//! Concave fills (cursor arrow, sparkles) are triangulated with a small ear-clipping routine,
//! because egui only fills convex paths correctly.

use std::f32::consts::{PI, TAU};

use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, pos2};

type P = [f32; 2];

/// Which theme colour a primitive uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The normal icon colour.
    Ink,
    /// The one highlighted detail (marker yellow, spotlight beam).
    Accent,
    /// A muted secondary detail.
    Dim,
}

/// One drawing primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    /// A polyline (or closed outline) with round caps and joins.
    Stroke {
        /// Vertices on the 24 x 24 grid.
        pts: Vec<P>,
        /// Connect the last vertex to the first.
        closed: bool,
        /// Width in grid units.
        width: f32,
        /// Colour role.
        tone: Tone,
    },
    /// A filled simple polygon (may be concave).
    Fill {
        /// Vertices on the grid.
        pts: Vec<P>,
        /// Colour role.
        tone: Tone,
    },
}

/// Colours an icon is painted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconColors {
    /// [`Tone::Ink`].
    pub ink: Color32,
    /// [`Tone::Accent`].
    pub accent: Color32,
    /// [`Tone::Dim`].
    pub dim: Color32,
}

impl IconColors {
    /// The dark toolbar's colours with `ink` as the main colour.
    pub fn with_ink(ink: Color32) -> Self {
        Self { ink, accent: Color32::from_rgb(255, 214, 51), dim: ink.gamma_multiply(0.45) }
    }
}

/// Every icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // the variant names are the documentation
pub enum Icon {
    // tools
    RegionRect,
    RegionEllipse,
    RegionFree,
    Select,
    Rectangle,
    Ellipse,
    Freehand,
    Line,
    Arrow,
    FreehandArrow,
    Text,
    TextBoxed,
    Balloon,
    Step,
    Magnify,
    Spotlight,
    Image,
    Sticker,
    CursorStamp,
    Eraser,
    Blur,
    Pixelate,
    Grid,
    Highlighter,
    CutOut,
    Effects,
    Canvas,
    Gear,
    // actions
    Undo,
    Redo,
    Open,
    Save,
    Copy,
    Upload,
    Clipboard,
    Layers,
    Eye,
    EyeOff,
    Lock,
    Unlock,
    Trash,
    ChevronDown,
    ChevronUp,
    Eyedropper,
    Check,
    Close,
    RotateCw,
    RotateCcw,
    FlipH,
    FlipV,
    Resize,
    ZoomIn,
    ZoomOut,
    Fit,
    Help,
    Keyboard,
    AlignLeft,
    AlignCenter,
    AlignRight,
    Done,
    Menu,
    Plus,
    ToFront,
    ToBack,
}

impl Icon {
    /// Every icon, for tests and the icon sheet.
    pub const ALL: [Icon; 64] = [
        Icon::RegionRect,
        Icon::RegionEllipse,
        Icon::RegionFree,
        Icon::Select,
        Icon::Rectangle,
        Icon::Ellipse,
        Icon::Freehand,
        Icon::Line,
        Icon::Arrow,
        Icon::FreehandArrow,
        Icon::Text,
        Icon::TextBoxed,
        Icon::Balloon,
        Icon::Step,
        Icon::Magnify,
        Icon::Spotlight,
        Icon::Image,
        Icon::Sticker,
        Icon::CursorStamp,
        Icon::Eraser,
        Icon::Blur,
        Icon::Pixelate,
        Icon::Grid,
        Icon::Highlighter,
        Icon::CutOut,
        Icon::Effects,
        Icon::Canvas,
        Icon::Gear,
        Icon::Undo,
        Icon::Redo,
        Icon::Open,
        Icon::Save,
        Icon::Copy,
        Icon::Upload,
        Icon::Clipboard,
        Icon::Layers,
        Icon::Eye,
        Icon::EyeOff,
        Icon::Lock,
        Icon::Unlock,
        Icon::Trash,
        Icon::ChevronDown,
        Icon::ChevronUp,
        Icon::Eyedropper,
        Icon::Check,
        Icon::Close,
        Icon::RotateCw,
        Icon::RotateCcw,
        Icon::FlipH,
        Icon::FlipV,
        Icon::Resize,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::Fit,
        Icon::Help,
        Icon::Keyboard,
        Icon::AlignLeft,
        Icon::AlignCenter,
        Icon::AlignRight,
        Icon::Done,
        Icon::Menu,
        Icon::Plus,
        Icon::ToFront,
        Icon::ToBack,
    ];
}

// ---------------------------------------------------------------------------------------------
// Geometry builders
// ---------------------------------------------------------------------------------------------

const W: f32 = 1.6;

fn stroke(pts: &[P]) -> Prim {
    Prim::Stroke { pts: pts.to_vec(), closed: false, width: W, tone: Tone::Ink }
}

fn closed(pts: &[P]) -> Prim {
    Prim::Stroke { pts: pts.to_vec(), closed: true, width: W, tone: Tone::Ink }
}

fn fill(pts: &[P]) -> Prim {
    Prim::Fill { pts: pts.to_vec(), tone: Tone::Ink }
}

fn toned(p: Prim, tone: Tone) -> Prim {
    match p {
        Prim::Stroke { pts, closed, width, .. } => Prim::Stroke { pts, closed, width, tone },
        Prim::Fill { pts, .. } => Prim::Fill { pts, tone },
    }
}

fn wide(p: Prim, width: f32) -> Prim {
    match p {
        Prim::Stroke { pts, closed, tone, .. } => Prim::Stroke { pts, closed, width, tone },
        other => other,
    }
}

fn arc_pts(cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32, n: usize) -> Vec<P> {
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            [cx + rx * a.cos(), cy + ry * a.sin()]
        })
        .collect()
}

fn ellipse_pts(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<P> {
    let mut v = arc_pts(cx, cy, rx, ry, 0.0, TAU, 48);
    v.pop();
    v
}

fn circle(cx: f32, cy: f32, r: f32) -> Prim {
    closed(&ellipse_pts(cx, cy, r, r))
}

fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Prim {
    closed(&ellipse_pts(cx, cy, rx, ry))
}

fn disc(cx: f32, cy: f32, r: f32) -> Prim {
    fill(&ellipse_pts(cx, cy, r, r))
}

fn bez(p0: P, p1: P, p2: P, p3: P, n: usize) -> Vec<P> {
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let f = |k: usize| {
                u * u * u * p0[k]
                    + 3.0 * u * u * t * p1[k]
                    + 3.0 * u * t * t * p2[k]
                    + t * t * t * p3[k]
            };
            [f(0), f(1)]
        })
        .collect()
}

/// A rounded rectangle outline, clockwise from the top-left corner's end. `bottom_extra` lets a
/// caller splice points into the bottom edge (right to left), used for the balloon's tail.
fn rrect_pts(x: f32, y: f32, w: f32, h: f32, r: f32, bottom_extra: &[P]) -> Vec<P> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut v = Vec::new();
    v.extend(arc_pts(x + w - r, y + r, r, r, -PI / 2.0, 0.0, 6));
    v.extend(arc_pts(x + w - r, y + h - r, r, r, 0.0, PI / 2.0, 6));
    v.extend_from_slice(bottom_extra);
    v.extend(arc_pts(x + r, y + h - r, r, r, PI / 2.0, PI, 6));
    v.extend(arc_pts(x + r, y + r, r, r, PI, PI * 1.5, 6));
    v
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Prim {
    closed(&rrect_pts(x, y, w, h, r, &[]))
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Prim {
    closed(&[[x, y], [x + w, y], [x + w, y + h], [x, y + h]])
}

fn rect_fill(x: f32, y: f32, w: f32, h: f32) -> Prim {
    fill(&[[x, y], [x + w, y], [x + w, y + h], [x, y + h]])
}

fn rotated_rect(cx: f32, cy: f32, w: f32, h: f32, deg: f32) -> Vec<P> {
    let (s, c) = deg.to_radians().sin_cos();
    [[-w, -h], [w, -h], [w, h], [-w, h]]
        .iter()
        .map(|p| {
            let (x, y) = (p[0] / 2.0, p[1] / 2.0);
            [cx + x * c - y * s, cy + x * s + y * c]
        })
        .collect()
}

/// A filled triangular arrow head at `tip`, pointing away from `from`.
fn head(tip: P, from: P, len: f32, half_w: f32) -> Prim {
    let (dx, dy) = (tip[0] - from[0], tip[1] - from[1]);
    let l = dx.hypot(dy).max(1e-6);
    let (ux, uy) = (dx / l, dy / l);
    let base = [tip[0] - ux * len, tip[1] - uy * len];
    fill(&[
        tip,
        [base[0] - uy * half_w, base[1] + ux * half_w],
        [base[0] + uy * half_w, base[1] - ux * half_w],
    ])
}

/// An open "V" head (two strokes) at `tip`.
fn v_head(tip: P, from: P, len: f32, half_w: f32) -> Prim {
    let (dx, dy) = (tip[0] - from[0], tip[1] - from[1]);
    let l = dx.hypot(dy).max(1e-6);
    let (ux, uy) = (dx / l, dy / l);
    let base = [tip[0] - ux * len, tip[1] - uy * len];
    stroke(&[
        [base[0] - uy * half_w, base[1] + ux * half_w],
        tip,
        [base[0] + uy * half_w, base[1] - ux * half_w],
    ])
}

fn dashed(pts: &[P], is_closed: bool, on: f32, off: f32) -> Vec<Prim> {
    let mut path: Vec<P> = pts.to_vec();
    if is_closed {
        path.push(pts[0]);
    }
    let mut out = Vec::new();
    let mut cur: Vec<P> = Vec::new();
    let mut drawing = true;
    let mut left = on;
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = (b[0] - a[0]).hypot(b[1] - a[1]);
        let mut t = 0.0;
        while t < seg {
            let step = left.min(seg - t);
            let p0 = [a[0] + (b[0] - a[0]) * t / seg, a[1] + (b[1] - a[1]) * t / seg];
            let p1 =
                [a[0] + (b[0] - a[0]) * (t + step) / seg, a[1] + (b[1] - a[1]) * (t + step) / seg];
            if drawing {
                if cur.is_empty() {
                    cur.push(p0);
                }
                cur.push(p1);
            }
            t += step;
            left -= step;
            if left <= 1e-4 {
                if drawing && cur.len() >= 2 {
                    out.push(stroke(&cur));
                }
                cur.clear();
                drawing = !drawing;
                left = if drawing { on } else { off };
            }
        }
    }
    if drawing && cur.len() >= 2 {
        out.push(stroke(&cur));
    }
    out
}

fn sparkle(cx: f32, cy: f32, r: f32) -> Prim {
    let k = r * 0.28;
    fill(&[
        [cx, cy - r],
        [cx + k, cy - k],
        [cx + r, cy],
        [cx + k, cy + k],
        [cx, cy + r],
        [cx - k, cy + k],
        [cx - r, cy],
        [cx - k, cy - k],
    ])
}

fn mirror_x(prims: Vec<Prim>) -> Vec<Prim> {
    prims
        .into_iter()
        .map(|p| match p {
            Prim::Stroke { pts, closed, width, tone } => Prim::Stroke {
                pts: pts.iter().map(|q| [24.0 - q[0], q[1]]).collect(),
                closed,
                width,
                tone,
            },
            Prim::Fill { pts, tone } => {
                Prim::Fill { pts: pts.iter().map(|q| [24.0 - q[0], q[1]]).collect(), tone }
            }
        })
        .collect()
}

fn cursor_arrow(dx: f32, dy: f32, s: f32) -> Vec<P> {
    [[6.0, 3.0], [6.0, 19.0], [10.2, 15.0], [12.8, 21.0], [15.2, 19.9], [12.6, 14.0], [18.0, 14.0]]
        .iter()
        .map(|p| [dx + (p[0] - 6.0) * s + 6.0 * s, dy + (p[1] - 3.0) * s + 3.0 * s])
        .collect()
}

impl Icon {
    /// The geometry of the icon on a 24 x 24 grid.
    pub fn prims(self) -> Vec<Prim> {
        use Icon as I;
        match self {
            I::RegionRect => {
                let mut v =
                    dashed(&[[3.5, 5.5], [20.5, 5.5], [20.5, 18.5], [3.5, 18.5]], true, 2.6, 2.0);
                v.push(toned(rect_fill(11.0, 10.5, 2.0, 2.0), Tone::Dim));
                v
            }
            I::RegionEllipse => {
                let mut v = dashed(&ellipse_pts(12.0, 12.0, 8.8, 6.6), true, 2.6, 2.0);
                v.push(toned(rect_fill(11.0, 11.0, 2.0, 2.0), Tone::Dim));
                v
            }
            I::RegionFree => {
                let pts = [
                    [4.5, 14.0],
                    [3.8, 9.0],
                    [8.0, 5.0],
                    [12.5, 7.0],
                    [17.5, 4.5],
                    [20.5, 9.5],
                    [17.0, 13.0],
                    [19.5, 18.0],
                    [13.0, 19.5],
                    [8.5, 17.0],
                ];
                let mut v = dashed(&pts, true, 2.4, 1.9);
                v.push(toned(rect_fill(11.0, 11.0, 2.0, 2.0), Tone::Dim));
                v
            }
            I::Select => vec![
                fill(&cursor_arrow(0.0, 0.0, 1.0)),
                toned(closed(&cursor_arrow(0.0, 0.0, 1.0)), Tone::Ink),
            ],
            I::Rectangle => vec![rrect(3.5, 5.5, 17.0, 13.0, 1.5)],
            I::Ellipse => vec![ellipse(12.0, 12.0, 8.8, 6.6)],
            I::Freehand => {
                vec![stroke(&bez([3.5, 16.5], [7.0, 4.5], [11.0, 21.0], [20.5, 7.5], 24))]
            }
            I::Line => vec![stroke(&[[4.5, 19.5], [19.5, 4.5]])],
            I::Arrow => {
                vec![stroke(&[[4.5, 19.5], [16.5, 7.5]]), head([20.0, 4.0], [4.5, 19.5], 8.0, 4.4)]
            }
            I::FreehandArrow => {
                let c = bez([3.5, 17.5], [6.0, 8.0], [12.0, 20.0], [17.0, 8.5], 20);
                vec![stroke(&c), head([20.5, 4.5], [15.5, 9.5], 7.5, 4.2)]
            }
            I::Text => vec![
                stroke(&[[5.0, 6.5], [19.0, 6.5]]),
                stroke(&[[12.0, 6.5], [12.0, 19.0]]),
                stroke(&[[9.5, 19.0], [14.5, 19.0]]),
                stroke(&[[5.0, 6.5], [5.0, 9.0]]),
                stroke(&[[19.0, 6.5], [19.0, 9.0]]),
            ],
            I::TextBoxed => vec![
                rrect(3.0, 4.0, 18.0, 16.0, 2.0),
                stroke(&[[7.5, 8.5], [16.5, 8.5]]),
                stroke(&[[12.0, 8.5], [12.0, 16.5]]),
            ],
            I::Balloon => {
                let tail = [[13.5, 16.5], [7.0, 21.0], [8.5, 16.5]];
                vec![closed(&rrect_pts(3.0, 3.5, 18.0, 13.0, 3.5, &tail))]
            }
            I::Step => vec![
                circle(12.0, 12.0, 8.6),
                stroke(&[[9.6, 9.6], [12.6, 7.6], [12.6, 16.4]]),
                stroke(&[[10.2, 16.4], [15.0, 16.4]]),
            ],
            I::Magnify => {
                vec![circle(10.0, 10.0, 6.2), wide(stroke(&[[14.6, 14.6], [20.5, 20.5]]), 2.4)]
            }
            I::Spotlight => vec![
                toned(rect_fill(3.0, 4.5, 18.0, 15.0), Tone::Dim),
                toned(disc(12.0, 12.0, 5.6), Tone::Accent),
                rect(3.0, 4.5, 18.0, 15.0),
            ],
            I::Image => vec![
                rrect(3.0, 4.5, 18.0, 15.0, 1.5),
                disc(8.5, 9.5, 1.7),
                stroke(&[[3.5, 17.5], [9.0, 12.5], [13.0, 16.0], [15.5, 13.5], [20.5, 18.0]]),
            ],
            I::Sticker => {
                let mut smile = arc_pts(12.0, 12.6, 4.6, 3.6, 0.45, PI - 0.45, 10);
                smile.shrink_to_fit();
                vec![
                    circle(12.0, 12.0, 8.6),
                    disc(9.2, 9.8, 1.0),
                    disc(14.8, 9.8, 1.0),
                    stroke(&smile),
                ]
            }
            I::CursorStamp => {
                let mut v = vec![toned(fill(&cursor_arrow(2.5, 2.0, 0.85)), Tone::Dim)];
                v.push(closed(&cursor_arrow(0.0, 0.0, 0.85)));
                v
            }
            I::Eraser => vec![
                closed(&rotated_rect(12.5, 12.0, 15.0, 7.5, -45.0)),
                stroke(&{
                    let r = rotated_rect(12.5, 12.0, 15.0, 7.5, -45.0);
                    [
                        [
                            (r[0][0] + r[3][0]) / 2.0 + (r[1][0] - r[0][0]) * 0.4,
                            (r[0][1] + r[3][1]) / 2.0 + (r[1][1] - r[0][1]) * 0.4,
                        ],
                        [
                            (r[1][0] + r[2][0]) / 2.0 - (r[1][0] - r[0][0]) * 0.6,
                            (r[1][1] + r[2][1]) / 2.0 - (r[1][1] - r[0][1]) * 0.6,
                        ],
                    ]
                }),
                stroke(&[[13.0, 20.5], [21.0, 20.5]]),
            ],
            I::Blur => {
                let mut v = vec![rrect(4.0, 4.0, 16.0, 16.0, 1.5)];
                for l in [
                    [[4.0, 12.0], [12.0, 4.0]],
                    [[4.0, 18.0], [18.0, 4.0]],
                    [[6.0, 20.0], [20.0, 6.0]],
                    [[12.0, 20.0], [20.0, 12.0]],
                ] {
                    v.push(toned(stroke(&l), Tone::Dim));
                }
                v
            }
            I::Pixelate => {
                let mut v = vec![rrect(4.0, 4.0, 16.0, 16.0, 1.0)];
                for (i, j) in [(0, 0), (2, 0), (1, 1), (0, 2), (2, 2)] {
                    v.push(rect_fill(
                        4.0 + i as f32 * 16.0 / 3.0 + 0.6,
                        4.0 + j as f32 * 16.0 / 3.0 + 0.6,
                        16.0 / 3.0 - 1.2,
                        16.0 / 3.0 - 1.2,
                    ));
                }
                v
            }
            I::Grid => vec![
                rrect(4.0, 4.0, 16.0, 16.0, 1.0),
                stroke(&[[9.33, 4.0], [9.33, 20.0]]),
                stroke(&[[14.66, 4.0], [14.66, 20.0]]),
                stroke(&[[4.0, 9.33], [20.0, 9.33]]),
                stroke(&[[4.0, 14.66], [20.0, 14.66]]),
            ],
            I::Highlighter => vec![
                wide(toned(stroke(&[[3.5, 20.5], [20.5, 20.5]]), Tone::Accent), 3.0),
                closed(&[[15.0, 3.5], [20.0, 8.5], [11.5, 17.0], [6.5, 12.0]]),
                closed(&[[6.5, 12.0], [11.5, 17.0], [7.0, 17.6], [5.0, 15.6]]),
            ],
            I::CutOut => vec![
                circle(7.0, 17.5, 2.6),
                circle(17.0, 17.5, 2.6),
                stroke(&[[8.6, 15.4], [17.5, 3.5]]),
                stroke(&[[15.4, 15.4], [6.5, 3.5]]),
            ],
            I::Effects => vec![
                wide(stroke(&[[4.5, 19.5], [15.0, 9.0]]), 2.2),
                sparkle(17.5, 6.5, 4.4),
                toned(sparkle(6.5, 6.0, 2.4), Tone::Dim),
                toned(sparkle(19.0, 16.5, 2.2), Tone::Dim),
            ],
            I::Canvas => vec![
                stroke(&[[7.0, 3.0], [7.0, 17.0], [21.0, 17.0]]),
                stroke(&[[3.0, 7.0], [17.0, 7.0], [17.0, 21.0]]),
            ],
            I::Gear => {
                let mut pts = Vec::new();
                for i in 0..8 {
                    let a = i as f32 * TAU / 8.0;
                    for (da, r) in [(-0.20, 7.4), (-0.13, 9.6), (0.13, 9.6), (0.20, 7.4)] {
                        pts.push([12.0 + r * (a + da).cos(), 12.0 + r * (a + da).sin()]);
                    }
                }
                vec![closed(&pts), circle(12.0, 12.0, 3.2)]
            }
            I::Undo => vec![
                stroke(&bez([19.5, 18.0], [19.5, 11.0], [15.5, 8.5], [9.5, 8.5], 16)),
                head([4.0, 8.5], [10.0, 8.5], 6.0, 4.4),
            ],
            I::Redo => mirror_x(Icon::Undo.prims()),
            I::Open => vec![closed(&[
                [3.0, 6.0],
                [9.0, 6.0],
                [11.0, 8.5],
                [20.0, 8.5],
                [20.0, 19.0],
                [3.0, 19.0],
            ])],
            I::Save => vec![
                closed(&[[4.0, 4.0], [17.0, 4.0], [20.0, 7.0], [20.0, 20.0], [4.0, 20.0]]),
                rect(8.0, 4.0, 7.0, 4.5),
                rect(7.0, 13.0, 10.0, 7.0),
            ],
            I::Copy => vec![
                stroke(&[[8.0, 6.5], [8.0, 3.5], [20.0, 3.5], [20.0, 16.5], [16.5, 16.5]]),
                rrect(4.0, 7.5, 12.5, 13.0, 1.5),
            ],
            I::Upload => vec![
                stroke(&[[4.0, 15.0], [4.0, 20.0], [20.0, 20.0], [20.0, 15.0]]),
                stroke(&[[12.0, 16.0], [12.0, 4.5]]),
                v_head([12.0, 3.5], [12.0, 9.0], 5.0, 5.0),
            ],
            I::Clipboard => vec![
                rrect(5.0, 5.0, 14.0, 16.0, 2.0),
                rrect(8.5, 3.0, 7.0, 4.0, 1.2),
                stroke(&[[8.5, 12.0], [15.5, 12.0]]),
                stroke(&[[8.5, 16.0], [13.0, 16.0]]),
            ],
            I::Layers => vec![
                closed(&[[12.0, 3.5], [21.0, 8.5], [12.0, 13.5], [3.0, 8.5]]),
                stroke(&[[3.0, 12.5], [12.0, 17.5], [21.0, 12.5]]),
                toned(stroke(&[[3.0, 16.5], [12.0, 21.5], [21.0, 16.5]]), Tone::Dim),
            ],
            I::Eye => {
                let mut top = arc_pts(12.0, 16.0, 10.0, 8.5, -2.4, -0.74, 14);
                let bottom: Vec<P> = arc_pts(12.0, 8.0, 10.0, 8.5, 0.74, 2.4, 14);
                top.extend(bottom);
                vec![closed(&top), circle(12.0, 12.0, 3.0)]
            }
            I::EyeOff => {
                let mut v = Icon::Eye.prims();
                v.push(wide(stroke(&[[4.0, 20.0], [20.0, 4.0]]), 1.8));
                v
            }
            I::Lock => vec![
                rrect(5.5, 11.0, 13.0, 9.5, 1.8),
                stroke(&{
                    let mut a = vec![[8.5, 11.0], [8.5, 8.0]];
                    a.extend(arc_pts(12.0, 8.0, 3.5, 3.5, PI, TAU, 10));
                    a.push([15.5, 11.0]);
                    a
                }),
                disc(12.0, 15.5, 1.3),
            ],
            I::Unlock => vec![
                rrect(5.5, 11.0, 13.0, 9.5, 1.8),
                stroke(&{
                    let mut a = vec![[8.5, 11.0], [8.5, 8.0]];
                    a.extend(arc_pts(12.0, 8.0, 3.5, 3.5, PI, TAU - 0.4, 10));
                    a
                }),
                disc(12.0, 15.5, 1.3),
            ],
            I::Trash => vec![
                stroke(&[[4.0, 7.0], [20.0, 7.0]]),
                stroke(&[[9.0, 7.0], [9.0, 4.0], [15.0, 4.0], [15.0, 7.0]]),
                closed(&[[6.0, 7.0], [7.0, 20.0], [17.0, 20.0], [18.0, 7.0]]),
                toned(stroke(&[[10.0, 10.5], [10.3, 17.0]]), Tone::Dim),
                toned(stroke(&[[14.0, 10.5], [13.7, 17.0]]), Tone::Dim),
            ],
            I::ChevronDown => vec![stroke(&[[6.5, 9.5], [12.0, 15.0], [17.5, 9.5]])],
            I::ChevronUp => vec![stroke(&[[6.5, 14.5], [12.0, 9.0], [17.5, 14.5]])],
            I::Eyedropper => vec![
                closed(&rotated_rect(13.5, 10.5, 12.0, 4.6, -45.0)),
                stroke(&[[8.6, 15.4], [4.5, 19.5]]),
                wide(stroke(&[[17.0, 4.5], [19.5, 7.0]]), 2.6),
            ],
            I::Check => vec![stroke(&[[5.0, 12.5], [10.0, 17.5], [19.0, 7.0]])],
            I::Close => {
                vec![stroke(&[[6.0, 6.0], [18.0, 18.0]]), stroke(&[[18.0, 6.0], [6.0, 18.0]])]
            }
            I::RotateCw => vec![
                stroke(&arc_pts(12.0, 12.5, 7.0, 7.0, -PI * 0.9, PI * 0.45, 22)),
                head([16.0, 3.8], [10.0, 5.0], 5.2, 3.8),
            ],
            I::RotateCcw => mirror_x(Icon::RotateCw.prims()),
            I::FlipH => {
                let mut v = dashed(&[[12.0, 3.0], [12.0, 21.0]], false, 2.0, 1.6);
                v.push(closed(&[[9.5, 7.0], [9.5, 17.0], [3.5, 17.0]]));
                v.push(toned(closed(&[[14.5, 7.0], [14.5, 17.0], [20.5, 17.0]]), Tone::Dim));
                v
            }
            I::FlipV => {
                let mut v = dashed(&[[3.0, 12.0], [21.0, 12.0]], false, 2.0, 1.6);
                v.push(closed(&[[7.0, 9.5], [17.0, 9.5], [17.0, 3.5]]));
                v.push(toned(closed(&[[7.0, 14.5], [17.0, 14.5], [17.0, 20.5]]), Tone::Dim));
                v
            }
            I::Resize => vec![
                rrect(3.5, 10.5, 10.0, 10.0, 1.2),
                stroke(&[[11.0, 13.0], [19.5, 4.5]]),
                v_head([20.5, 3.5], [14.0, 10.0], 5.0, 5.0),
            ],
            I::ZoomIn => vec![
                circle(10.0, 10.0, 6.2),
                wide(stroke(&[[14.6, 14.6], [20.5, 20.5]]), 2.4),
                stroke(&[[7.2, 10.0], [12.8, 10.0]]),
                stroke(&[[10.0, 7.2], [10.0, 12.8]]),
            ],
            I::ZoomOut => vec![
                circle(10.0, 10.0, 6.2),
                wide(stroke(&[[14.6, 14.6], [20.5, 20.5]]), 2.4),
                stroke(&[[7.2, 10.0], [12.8, 10.0]]),
            ],
            I::Fit => vec![
                stroke(&[[3.5, 8.5], [3.5, 3.5], [8.5, 3.5]]),
                stroke(&[[15.5, 3.5], [20.5, 3.5], [20.5, 8.5]]),
                stroke(&[[20.5, 15.5], [20.5, 20.5], [15.5, 20.5]]),
                stroke(&[[8.5, 20.5], [3.5, 20.5], [3.5, 15.5]]),
                toned(rect(8.5, 8.5, 7.0, 7.0), Tone::Dim),
            ],
            I::Help => vec![
                circle(12.0, 12.0, 8.6),
                stroke(&{
                    let mut a = arc_pts(12.0, 9.6, 2.8, 2.6, PI * 1.05, TAU + 0.5, 10);
                    a.push([12.0, 13.6]);
                    a.push([12.0, 14.4]);
                    a
                }),
                disc(12.0, 17.0, 1.0),
            ],
            I::Keyboard => {
                let mut v = vec![rrect(2.5, 6.0, 19.0, 12.0, 2.0)];
                for x in [6.0, 9.5, 13.0, 16.5] {
                    v.push(toned(rect_fill(x - 0.6, 9.0, 1.4, 1.4), Tone::Ink));
                }
                for x in [7.6, 11.1, 14.6] {
                    v.push(toned(rect_fill(x - 0.6, 12.0, 1.4, 1.4), Tone::Ink));
                }
                v.push(stroke(&[[7.5, 15.4], [16.5, 15.4]]));
                v
            }
            I::AlignLeft => vec![
                stroke(&[[4.0, 6.0], [20.0, 6.0]]),
                stroke(&[[4.0, 10.5], [14.0, 10.5]]),
                stroke(&[[4.0, 15.0], [20.0, 15.0]]),
                stroke(&[[4.0, 19.5], [12.0, 19.5]]),
            ],
            I::AlignCenter => vec![
                stroke(&[[4.0, 6.0], [20.0, 6.0]]),
                stroke(&[[7.0, 10.5], [17.0, 10.5]]),
                stroke(&[[4.0, 15.0], [20.0, 15.0]]),
                stroke(&[[8.0, 19.5], [16.0, 19.5]]),
            ],
            I::AlignRight => vec![
                stroke(&[[4.0, 6.0], [20.0, 6.0]]),
                stroke(&[[10.0, 10.5], [20.0, 10.5]]),
                stroke(&[[4.0, 15.0], [20.0, 15.0]]),
                stroke(&[[12.0, 19.5], [20.0, 19.5]]),
            ],
            I::Done => {
                vec![circle(12.0, 12.0, 8.6), stroke(&[[7.8, 12.4], [10.8, 15.4], [16.4, 9.2]])]
            }
            I::Menu => vec![
                stroke(&[[4.0, 7.0], [20.0, 7.0]]),
                stroke(&[[4.0, 12.0], [20.0, 12.0]]),
                stroke(&[[4.0, 17.0], [20.0, 17.0]]),
            ],
            I::Plus => {
                vec![stroke(&[[12.0, 5.0], [12.0, 19.0]]), stroke(&[[5.0, 12.0], [19.0, 12.0]])]
            }
            I::ToFront => vec![
                stroke(&[[6.5, 11.0], [12.0, 5.5], [17.5, 11.0]]),
                stroke(&[[6.5, 18.0], [12.0, 12.5], [17.5, 18.0]]),
            ],
            I::ToBack => vec![
                stroke(&[[6.5, 13.0], [12.0, 18.5], [17.5, 13.0]]),
                stroke(&[[6.5, 6.0], [12.0, 11.5], [17.5, 6.0]]),
            ],
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Triangulation of concave polygons
// ---------------------------------------------------------------------------------------------

fn area2(p: &[P]) -> f32 {
    let mut a = 0.0;
    for i in 0..p.len() {
        let (u, v) = (p[i], p[(i + 1) % p.len()]);
        a += u[0] * v[1] - v[0] * u[1];
    }
    a
}

fn cross(o: P, a: P, b: P) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

fn in_triangle(p: P, a: P, b: P, c: P) -> bool {
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// Ear-clipping triangulation of a simple polygon. Returns index triples into `pts`.
/// Degenerate input falls back to a fan so something is always drawn.
pub fn triangulate(pts: &[P]) -> Vec<[usize; 3]> {
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }
    let ccw = area2(pts) > 0.0;
    let mut idx: Vec<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n - 2);
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (pts[ia], pts[ib], pts[ic]);
            let convex = if ccw { cross(a, b, c) > 1e-6 } else { cross(a, b, c) < -1e-6 };
            if !convex {
                continue;
            }
            let ear =
                idx.iter().all(|&j| j == ia || j == ib || j == ic || !in_triangle(pts[j], a, b, c));
            if ear {
                out.push([ia, ib, ic]);
                idx.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            break;
        }
    }
    if idx.len() == 3 {
        out.push([idx[0], idx[1], idx[2]]);
    } else if idx.len() > 3 {
        for i in 1..idx.len() - 1 {
            out.push([idx[0], idx[i], idx[i + 1]]);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------------------------

/// Builds the egui shapes of `icon` inside `rect`.
pub fn shapes(icon: Icon, rect: Rect, colors: IconColors) -> Vec<Shape> {
    let scale = rect.width().min(rect.height()) / 24.0;
    let origin = rect.center() - egui::vec2(12.0, 12.0) * scale;
    let map = |p: P| pos2(origin.x + p[0] * scale, origin.y + p[1] * scale);
    let color = |t: Tone| match t {
        Tone::Ink => colors.ink,
        Tone::Accent => colors.accent,
        Tone::Dim => colors.dim,
    };
    let mut out = Vec::new();
    for prim in icon.prims() {
        match prim {
            Prim::Stroke { pts, closed, width, tone } => {
                let c = color(tone);
                let w = (width * scale).max(1.0);
                let pos: Vec<Pos2> = pts.iter().copied().map(map).collect();
                if pos.len() < 2 {
                    continue;
                }
                let st = Stroke::new(w, c);
                if closed {
                    out.push(Shape::closed_line(pos.clone(), st));
                    // Round joins: a disc at every vertex hides the tessellator's mitre spikes.
                    for p in &pos {
                        out.push(Shape::circle_filled(*p, w / 2.0, c));
                    }
                } else {
                    out.push(Shape::line(pos.clone(), st));
                    for p in [pos[0], pos[pos.len() - 1]] {
                        out.push(Shape::circle_filled(p, w / 2.0, c));
                    }
                    if pos.len() > 2 {
                        for p in &pos[1..pos.len() - 1] {
                            out.push(Shape::circle_filled(*p, w / 2.0, c));
                        }
                    }
                }
            }
            Prim::Fill { pts, tone } => {
                let c = color(tone);
                let mut mesh = Mesh::default();
                for p in &pts {
                    mesh.colored_vertex(map(*p), c);
                }
                for [a, b, cc] in triangulate(&pts) {
                    mesh.add_triangle(a as u32, b as u32, cc as u32);
                }
                if !mesh.is_empty() {
                    out.push(Shape::mesh(mesh));
                }
            }
        }
    }
    out
}

/// Paints `icon` into `rect` with `painter`.
pub fn paint(painter: &Painter, icon: Icon, rect: Rect, colors: IconColors) {
    painter.extend(shapes(icon, rect, colors));
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn colors() -> IconColors {
        IconColors::with_ink(Color32::WHITE)
    }

    #[test]
    fn every_icon_has_geometry_inside_the_grid() {
        let all: HashSet<Icon> = Icon::ALL.iter().copied().collect();
        assert_eq!(all.len(), Icon::ALL.len(), "no icon is listed twice");
        for icon in &all {
            let prims = icon.prims();
            assert!(!prims.is_empty(), "{icon:?}");
            for p in prims {
                let pts = match &p {
                    Prim::Stroke { pts, .. } | Prim::Fill { pts, .. } => pts,
                };
                assert!(pts.len() >= 2, "{icon:?} has a degenerate primitive");
                for q in pts {
                    assert!(q[0].is_finite() && q[1].is_finite(), "{icon:?}");
                    assert!(
                        (0.0..=24.0).contains(&q[0]) && (0.0..=24.0).contains(&q[1]),
                        "{icon:?} draws outside the grid: {q:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn icons_are_all_different() {
        let mut seen: Vec<(String, Icon)> = Vec::new();
        for icon in Icon::ALL {
            let key = format!("{:?}", icon.prims());
            if let Some((_, other)) = seen.iter().find(|(k, _)| *k == key) {
                assert_eq!(*other, icon, "{icon:?} draws the same as {other:?}");
            } else {
                seen.push((key, icon));
            }
        }
    }

    #[test]
    fn every_icon_renders_non_empty_and_stays_in_its_box() {
        let ctx = egui::Context::default();
        // Fonts are installed at the start of the first pass; tessellating needs them.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let rect = Rect::from_min_size(pos2(40.0, 40.0), egui::vec2(48.0, 48.0));
        for icon in Icon::ALL {
            let shapes = shapes(icon, rect, colors());
            assert!(!shapes.is_empty(), "{icon:?}");
            // Tessellate for real: vertices must exist, lie in the box (plus antialiasing
            // fringe) and cover a meaningful area.
            let clipped = ctx.tessellate(
                shapes
                    .into_iter()
                    .map(|s| egui::epaint::ClippedShape { clip_rect: Rect::EVERYTHING, shape: s })
                    .collect(),
                1.0,
            );
            let mut bounds = Rect::NOTHING;
            let mut verts = 0;
            for c in &clipped {
                if let egui::epaint::Primitive::Mesh(m) = &c.primitive {
                    for v in &m.vertices {
                        bounds.extend_with(v.pos);
                        verts += 1;
                    }
                }
            }
            assert!(verts >= 6, "{icon:?} tessellated to {verts} vertices");
            assert!(
                rect.expand(3.0).contains_rect(bounds),
                "{icon:?} {bounds:?} leaks out of {rect:?}"
            );
            assert!(
                bounds.width() > 6.0 || bounds.height() > 6.0,
                "{icon:?} is nearly invisible: {bounds:?}"
            );
        }
    }

    #[test]
    fn strokes_survive_tiny_sizes() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(12.0, 12.0));
        for icon in Icon::ALL {
            assert!(!shapes(icon, rect, colors()).is_empty());
        }
    }

    #[test]
    fn triangulation_covers_concave_polygons() {
        // The cursor arrow is concave; area of the triangles must equal the polygon area.
        let poly = cursor_arrow(0.0, 0.0, 1.0);
        let tris = triangulate(&poly);
        assert_eq!(tris.len(), poly.len() - 2);
        let sum: f32 =
            tris.iter().map(|t| (cross(poly[t[0]], poly[t[1]], poly[t[2]]) / 2.0).abs()).sum();
        assert!((sum - area2(&poly).abs() / 2.0).abs() < 1e-3, "{sum}");
        // Clockwise input works too.
        let mut rev = poly.clone();
        rev.reverse();
        let sum2: f32 = triangulate(&rev)
            .iter()
            .map(|t| (cross(rev[t[0]], rev[t[1]], rev[t[2]]) / 2.0).abs())
            .sum();
        assert!((sum2 - sum).abs() < 1e-3);
        assert!(triangulate(&[[0.0, 0.0], [1.0, 1.0]]).is_empty());
        // A sparkle (8 vertices, concave) also works.
        if let Prim::Fill { pts, .. } = sparkle(10.0, 10.0, 5.0) {
            assert_eq!(triangulate(&pts).len(), 6);
        }
    }

    #[test]
    fn dashes_split_a_path() {
        let d = dashed(&[[0.0, 0.0], [10.0, 0.0]], false, 2.0, 1.0);
        assert!(d.len() >= 3, "{}", d.len());
        let closed_dashes =
            dashed(&[[0.0, 0.0], [5.0, 0.0], [5.0, 5.0], [0.0, 5.0]], true, 2.0, 1.0);
        assert!(closed_dashes.len() > 4);
    }

    #[test]
    fn accent_and_dim_tones_are_used_where_designed() {
        let has = |i: Icon, t: Tone| {
            i.prims().iter().any(|p| match p {
                Prim::Stroke { tone, .. } | Prim::Fill { tone, .. } => *tone == t,
            })
        };
        assert!(has(Icon::Highlighter, Tone::Accent));
        assert!(has(Icon::Spotlight, Tone::Accent) && has(Icon::Spotlight, Tone::Dim));
        assert!(!has(Icon::Rectangle, Tone::Accent));
    }

    #[test]
    fn mirrored_icons_are_mirrors() {
        let (a, b) = (Icon::Undo.prims(), Icon::Redo.prims());
        assert_eq!(a.len(), b.len());
        if let (Prim::Stroke { pts: p, .. }, Prim::Stroke { pts: q, .. }) = (&a[0], &b[0]) {
            assert!((p[0][0] + q[0][0] - 24.0).abs() < 1e-4);
        }
    }
}
