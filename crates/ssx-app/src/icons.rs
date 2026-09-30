//! The tray icons, drawn in code.
//!
//! No image files ship with the daemon: the icon is a handful of shapes in unit coordinates
//! (a dark rounded square, viewfinder corner brackets, a centre dot; a red disc while
//! recording; a red "!" badge after an error), rasterised at whatever size the desktop
//! asks for with 4x4 supersampling. That keeps the icon crisp at 16 to 64 px and at HiDPI
//! scales (the StatusNotifier protocol lets us offer several sizes and the host picks the closest;
//! Windows takes one image at the size we choose), and there is nothing to lose or mis-package.
//!
//! Shapes are described once ([`layers`]) and shared by every size, so the picture cannot drift
//! between sizes. The unit tests check what matters for a tray icon: it is never empty, the
//! corners are transparent (so it does not look like a tile on a dark panel), the recording
//! state is unmistakably red, and the error badge only exists in the error state.

use crate::menu::IconKind;

/// Sizes offered to StatusNotifier hosts (they pick the closest and scale).
pub const SNI_SIZES: [u32; 5] = [16, 24, 32, 48, 64];

const SAMPLES: u32 = 4;

/// An RGBA image (straight alpha, row-major, no padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, R G B A.
    pub rgba: Vec<u8>,
}

impl IconImage {
    /// The RGBA of pixel `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    /// The share of the image that is covered (sum of alpha over the area, 0..=1).
    pub fn coverage(&self) -> f32 {
        let sum: u64 = self.rgba.chunks_exact(4).map(|p| u64::from(p[3])).sum();
        sum as f32 / (255.0 * (self.width * self.height) as f32)
    }

    /// How many pixels are not fully transparent.
    pub fn visible_pixels(&self) -> usize {
        self.rgba.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    /// The pixels as ARGB32 in network byte order (A, R, G, B per pixel): the format of the
    /// StatusNotifierItem `IconPixmap` property.
    pub fn to_argb32(&self) -> Vec<u8> {
        self.rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// x0, y0, x1, y1, corner radius.
    RoundRect(f32, f32, f32, f32, f32),
    /// x0, y0, x1, y1.
    Rect(f32, f32, f32, f32),
    /// cx, cy, r.
    Circle(f32, f32, f32),
}

impl Shape {
    fn contains(self, x: f32, y: f32) -> bool {
        match self {
            Self::Rect(x0, y0, x1, y1) => x >= x0 && x < x1 && y >= y0 && y < y1,
            Self::Circle(cx, cy, r) => (x - cx).powi(2) + (y - cy).powi(2) <= r * r,
            Self::RoundRect(x0, y0, x1, y1, r) => {
                if !(x >= x0 && x < x1 && y >= y0 && y < y1) {
                    return false;
                }
                // Inside the rectangle: outside only if in a corner square and beyond its arc.
                let cx = x.clamp(x0 + r, x1 - r);
                let cy = y.clamp(y0 + r, y1 - r);
                (x - cx).powi(2) + (y - cy).powi(2) <= r * r
            }
        }
    }
}

type Color = [f32; 4];

const SLATE: Color = [0.149, 0.204, 0.271, 1.0];
const WHITE: Color = [1.0, 1.0, 1.0, 1.0];
const RED: Color = [0.898, 0.224, 0.208, 1.0];
const REC_RED: Color = [1.0, 0.231, 0.188, 1.0];

/// The shapes of an icon, back to front, in unit coordinates.
fn layers(kind: IconKind) -> Vec<(Shape, Color)> {
    let mut v = vec![(Shape::RoundRect(0.04, 0.04, 0.96, 0.96, 0.22), SLATE)];
    // Viewfinder brackets: an L in each corner of a square inset from the edge.
    let (lo, hi, arm, t) = (0.20_f32, 0.80_f32, 0.17_f32, 0.075_f32);
    for (x, y, dx, dy) in
        [(lo, lo, 1.0, 1.0), (hi, lo, -1.0, 1.0), (lo, hi, 1.0, -1.0), (hi, hi, -1.0, -1.0)]
    {
        let horizontal =
            (x.min(x + dx * arm), (y.min(y + dy * t)), x.max(x + dx * arm), y.max(y + dy * t));
        let vertical =
            (x.min(x + dx * t), y.min(y + dy * arm), x.max(x + dx * t), y.max(y + dy * arm));
        v.push((Shape::Rect(horizontal.0, horizontal.1, horizontal.2, horizontal.3), WHITE));
        v.push((Shape::Rect(vertical.0, vertical.1, vertical.2, vertical.3), WHITE));
    }
    match kind {
        IconKind::Idle => v.push((Shape::Circle(0.5, 0.5, 0.09), WHITE)),
        IconKind::Recording => {
            v.push((Shape::Circle(0.5, 0.5, 0.235), WHITE));
            v.push((Shape::Circle(0.5, 0.5, 0.19), REC_RED));
        }
        IconKind::Error => {
            v.push((Shape::Circle(0.5, 0.5, 0.09), WHITE));
            v.push((Shape::Circle(0.72, 0.72, 0.26), WHITE));
            v.push((Shape::Circle(0.72, 0.72, 0.22), RED));
            v.push((Shape::Rect(0.695, 0.60, 0.745, 0.75), WHITE));
            v.push((Shape::Circle(0.72, 0.82, 0.035), WHITE));
        }
    }
    v
}

/// Draws the icon at `size` x `size` pixels (at least 8).
pub fn render(kind: IconKind, size: u32) -> IconImage {
    let size = size.max(8);
    let layers = layers(kind);
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let n = (SAMPLES * SAMPLES) as f32;
    for py in 0..size {
        for px in 0..size {
            // Premultiplied accumulation over the sub-samples.
            let mut acc = [0.0_f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    // Composite the layers "over" each other for this sample.
                    let mut s = [0.0_f32; 4]; // premultiplied
                    for (shape, c) in &layers {
                        if shape.contains(x, y) {
                            let a = c[3];
                            for i in 0..3 {
                                s[i] = c[i] * a + s[i] * (1.0 - a);
                            }
                            s[3] = a + s[3] * (1.0 - a);
                        }
                    }
                    for i in 0..4 {
                        acc[i] += s[i];
                    }
                }
            }
            let a = acc[3] / n;
            let unpremultiply = |c: f32| if a > 0.0 { (c / n / a).clamp(0.0, 1.0) } else { 0.0 };
            rgba.extend_from_slice(&[
                (unpremultiply(acc[0]) * 255.0).round() as u8,
                (unpremultiply(acc[1]) * 255.0).round() as u8,
                (unpremultiply(acc[2]) * 255.0).round() as u8,
                (a * 255.0).round() as u8,
            ]);
        }
    }
    IconImage { width: size, height: size, rgba }
}

/// The icon at every size in [`SNI_SIZES`].
pub fn render_set(kind: IconKind) -> Vec<IconImage> {
    SNI_SIZES.iter().map(|s| render(kind, *s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [IconKind; 3] = [IconKind::Idle, IconKind::Recording, IconKind::Error];

    #[test]
    fn every_kind_at_every_size_draws_something_of_the_right_dimensions() {
        for kind in KINDS {
            for size in [16, 18, 20, 22, 24, 32, 40, 48, 64, 96, 128] {
                let img = render(kind, size);
                assert_eq!((img.width, img.height), (size, size));
                assert_eq!(img.rgba.len(), (size * size * 4) as usize);
                let visible = img.visible_pixels();
                let total = (size * size) as usize;
                assert!(
                    visible > total / 2 && visible < total,
                    "{kind:?} at {size}: {visible} of {total} pixels visible"
                );
            }
        }
    }

    #[test]
    fn corners_are_transparent_and_the_body_is_opaque() {
        for kind in KINDS {
            for size in [16, 32, 48] {
                let img = render(kind, size);
                for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                    assert_eq!(
                        img.pixel(x, y)[3],
                        0,
                        "{kind:?}@{size} corner ({x},{y}) must be transparent"
                    );
                }
                // Inside the rounded square, away from the glyph: fully opaque slate.
                let p = img.pixel(size / 2, (size as f32 * 0.14) as u32);
                assert_eq!(p[3], 255, "{kind:?}@{size}");
            }
        }
    }

    #[test]
    fn recording_is_red_in_the_middle_and_idle_is_not() {
        for size in [16, 24, 32, 48] {
            let c = size / 2;
            let rec = render(IconKind::Recording, size).pixel(c, c);
            assert!(
                rec[0] > 200 && rec[1] < 110 && rec[2] < 100,
                "recording centre {rec:?} @ {size}"
            );
            let idle = render(IconKind::Idle, size).pixel(c, c);
            assert!(
                idle[0] > 200 && idle[1] > 200 && idle[2] > 200,
                "idle centre {idle:?} @ {size}"
            );
        }
    }

    #[test]
    fn the_error_badge_exists_only_in_the_error_state() {
        for size in [16, 24, 32, 48] {
            let (x, y) = ((size as f32 * 0.87) as u32, (size as f32 * 0.72) as u32);
            let err = render(IconKind::Error, size).pixel(x, y);
            let idle = render(IconKind::Idle, size).pixel(x, y);
            assert!(err[0] > 180 && err[1] < 120, "error badge {err:?} @ {size}");
            assert!(idle[0] < 120, "idle has no badge: {idle:?} @ {size}");
        }
    }

    #[test]
    fn the_three_states_are_visibly_different() {
        let a = render(IconKind::Idle, 32);
        let b = render(IconKind::Recording, 32);
        let c = render(IconKind::Error, 32);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render(IconKind::Error, 24), render(IconKind::Error, 24));
    }

    #[test]
    fn the_idle_icon_is_left_right_symmetric() {
        let img = render(IconKind::Idle, 32);
        for y in 0..32 {
            for x in 0..16 {
                let (l, r) = (img.pixel(x, y), img.pixel(31 - x, y));
                for i in 0..4 {
                    assert!(l[i].abs_diff(r[i]) <= 2, "({x},{y}) {l:?} vs {r:?}");
                }
            }
        }
    }

    #[test]
    fn hidpi_sizes_are_the_same_picture_scaled() {
        // The share of the image covered must not depend on the size (no size-specific art).
        for kind in KINDS {
            let share = |s: u32| render(kind, s).coverage();
            let base = share(16);
            for s in [32, 48, 64] {
                assert!(
                    (share(s) - base).abs() < 0.02,
                    "{kind:?}: {base} at 16 vs {} at {s}",
                    share(s)
                );
            }
        }
    }

    #[test]
    fn tiny_requests_are_clamped_and_the_set_covers_the_standard_sizes() {
        assert_eq!(render(IconKind::Idle, 0).width, 8);
        let set = render_set(IconKind::Recording);
        assert_eq!(set.iter().map(|i| i.width).collect::<Vec<_>>(), SNI_SIZES);
    }

    #[test]
    fn argb32_is_network_order() {
        let img = IconImage { width: 2, height: 1, rgba: vec![1, 2, 3, 4, 10, 20, 30, 40] };
        assert_eq!(img.to_argb32(), [4, 1, 2, 3, 40, 10, 20, 30]);
        let real = render(IconKind::Idle, 16);
        let argb = real.to_argb32();
        assert_eq!(argb.len(), real.rgba.len());
        assert_eq!(argb[0], real.rgba[3], "alpha comes first");
    }
}
