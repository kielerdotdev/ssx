//! Edge effects: torn paper, wavy edge and rounded corners.
//!
//! All three only touch the alpha channel. They are analytic (distance based) so edges
//! are anti-aliased with exact 1-px coverage ramps and the result does not depend on
//! image size other than through the edge length. Randomness is a seeded `splitmix64`
//! hash of `(seed, side, tooth index)`, so output is deterministic and independent of
//! thread scheduling — tests and golden images rely on that.

use serde::{Deserialize, Serialize};
use ssx_types::Frame;

use crate::{Result, check, finite, region::for_rows_mut};

/// Which image sides an edge effect applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSides {
    /// Top edge.
    pub top: bool,
    /// Right edge.
    pub right: bool,
    /// Bottom edge.
    pub bottom: bool,
    /// Left edge.
    pub left: bool,
}

impl EdgeSides {
    /// All four sides.
    pub const ALL: EdgeSides = EdgeSides { top: true, right: true, bottom: true, left: true };
}

impl Default for EdgeSides {
    fn default() -> Self {
        Self::ALL
    }
}

/// Shape of the edge profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Random jagged teeth of average width `period`, like torn paper.
    Torn,
    /// A regular sine wave of wavelength `period`.
    Wave,
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(seed: u64, side: u64, i: i64) -> f32 {
    let h = splitmix(seed ^ splitmix(side.wrapping_mul(0x1234_5678_9ABC_DEF1) ^ (i as u64)));
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Depth of the cut (0..=depth) at position `t` along an edge.
fn profile(kind: EdgeKind, depth: f32, period: f32, seed: u64, side: u64, t: f32) -> f32 {
    match kind {
        EdgeKind::Wave => {
            let phase = unit(seed, side, -1) * std::f32::consts::TAU;
            depth * (0.5 + 0.5 * (std::f32::consts::TAU * t / period + phase).sin())
        }
        EdgeKind::Torn => {
            let u = t / period;
            let i = u.floor();
            let f = u - i;
            let a = 0.15 + 0.85 * unit(seed, side, i as i64);
            let b = 0.15 + 0.85 * unit(seed, side, i as i64 + 1);
            depth * (a + (b - a) * f)
        }
    }
}

/// Cuts a torn or wavy edge into `sides` of the image by making pixels transparent.
/// `depth` is the maximum cut depth in pixels, `period` the tooth/wave length.
pub fn edge_effect(
    frame: &mut Frame,
    sides: EdgeSides,
    kind: EdgeKind,
    depth: f32,
    period: f32,
    seed: u64,
) -> Result<()> {
    check(frame)?;
    let depth = finite(depth, "depth")?;
    let period = finite(period, "period")?.max(1.0);
    let (w, h) = (frame.width(), frame.height());
    if depth <= 0.0 || w == 0 || h == 0 {
        return Ok(());
    }
    let (wf, hf) = (w as f32, h as f32);
    let full = frame_rect(w, h);
    // Precompute the profile along each axis once.
    let prof = |side: u64, len: u32| -> Vec<f32> {
        (0..len).map(|i| profile(kind, depth, period, seed, side, i as f32 + 0.5)).collect()
    };
    let top = prof(0, w);
    let bottom = prof(2, w);
    let left = prof(3, h);
    let right = prof(1, h);
    for_rows_mut(frame, full, |y, row| {
        let yc = y as f32 + 0.5;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let xc = x as f32 + 0.5;
            let mut keep = 1.0f32;
            if sides.top {
                keep *= (yc - top[x] + 0.5).clamp(0.0, 1.0);
            }
            if sides.bottom {
                keep *= ((hf - yc) - bottom[x] + 0.5).clamp(0.0, 1.0);
            }
            if sides.left {
                keep *= (xc - left[y as usize] + 0.5).clamp(0.0, 1.0);
            }
            if sides.right {
                keep *= ((wf - xc) - right[y as usize] + 0.5).clamp(0.0, 1.0);
            }
            if keep < 1.0 {
                px[3] = (f32::from(px[3]) * keep).round() as u8;
            }
        }
    });
    Ok(())
}

fn frame_rect(w: u32, h: u32) -> ssx_types::Rect {
    ssx_types::Rect::new(0, 0, w, h)
}

/// Rounds the image corners with an anti-aliased quarter-circle mask of `radius` pixels
/// (clamped to half the shorter side). `corners` = top-left, top-right, bottom-right,
/// bottom-left.
pub fn round_corners(frame: &mut Frame, radius: f32, corners: [bool; 4]) -> Result<()> {
    check(frame)?;
    let radius = finite(radius, "radius")?;
    let (w, h) = (frame.width(), frame.height());
    if w == 0 || h == 0 || radius <= 0.0 {
        return Ok(());
    }
    let (wf, hf) = (w as f32, h as f32);
    let r = radius.min(wf / 2.0).min(hf / 2.0);
    let full = frame_rect(w, h);
    for_rows_mut(frame, full, |y, row| {
        let yc = y as f32 + 0.5;
        let near_top = yc < r;
        let near_bottom = yc > hf - r;
        if !near_top && !near_bottom {
            return;
        }
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let xc = x as f32 + 0.5;
            let (cx, cy, on) = match (xc < r, xc > wf - r, near_top, near_bottom) {
                (true, _, true, _) => (r, r, corners[0]),
                (_, true, true, _) => (wf - r, r, corners[1]),
                (_, true, _, true) => (wf - r, hf - r, corners[2]),
                (true, _, _, true) => (r, hf - r, corners[3]),
                _ => continue,
            };
            if !on {
                continue;
            }
            let d = ((xc - cx).powi(2) + (yc - cy).powi(2)).sqrt();
            let keep = (r - d + 0.5).clamp(0.0, 1.0);
            if keep < 1.0 {
                px[3] = (f32::from(px[3]) * keep).round() as u8;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{solid_frame, testutil::px};

    #[test]
    fn rounded_corners_pixels() {
        let mut f = solid_frame(20, 20, [255, 0, 0, 255]);
        round_corners(&mut f, 6.0, [true; 4]).unwrap();
        assert_eq!(px(&f, 0, 0)[3], 0, "extreme corner removed");
        assert_eq!(px(&f, 19, 19)[3], 0);
        assert_eq!(px(&f, 19, 0)[3], 0);
        assert_eq!(px(&f, 0, 19)[3], 0);
        assert_eq!(px(&f, 10, 0)[3], 255, "edge midpoints untouched");
        assert_eq!(px(&f, 0, 10)[3], 255);
        assert_eq!(px(&f, 6, 6)[3], 255);
        // Anti-aliased ramp exists somewhere along the arc.
        assert!((0..6).any(|i| {
            let a = px(&f, i, 1)[3];
            a > 0 && a < 255
        }));
        // Symmetric.
        assert_eq!(px(&f, 1, 3)[3], px(&f, 3, 1)[3]);
        assert_eq!(px(&f, 1, 3)[3], px(&f, 18, 3)[3]);
        assert_eq!(px(&f, 1, 3)[3], px(&f, 1, 16)[3]);
    }

    #[test]
    fn rounded_corner_selection_and_clamp() {
        let mut f = solid_frame(10, 10, [1, 1, 1, 255]);
        round_corners(&mut f, 100.0, [true, false, false, false]).unwrap();
        assert_eq!(px(&f, 0, 0)[3], 0);
        assert_eq!(px(&f, 9, 0)[3], 255);
        assert_eq!(px(&f, 9, 9)[3], 255);
        let mut one = solid_frame(1, 1, [1, 1, 1, 255]);
        round_corners(&mut one, 5.0, [true; 4]).unwrap();
        let mut e = solid_frame(0, 3, [0; 4]);
        round_corners(&mut e, 5.0, [true; 4]).unwrap();
        let mut ok = solid_frame(4, 4, [1, 1, 1, 255]);
        round_corners(&mut ok, 0.0, [true; 4]).unwrap();
        assert_eq!(ok, solid_frame(4, 4, [1, 1, 1, 255]));
        assert!(round_corners(&mut ok, f32::NAN, [true; 4]).is_err());
    }

    #[test]
    fn wave_and_torn_deterministic_and_bounded() {
        for kind in [EdgeKind::Wave, EdgeKind::Torn] {
            let mut a = solid_frame(64, 48, [10, 20, 30, 255]);
            let mut b = a.clone();
            edge_effect(&mut a, EdgeSides::ALL, kind, 6.0, 10.0, 42).unwrap();
            edge_effect(&mut b, EdgeSides::ALL, kind, 6.0, 10.0, 42).unwrap();
            assert_eq!(a, b, "deterministic {kind:?}");
            // Interior untouched beyond the depth, alpha removed somewhere near edges.
            assert_eq!(px(&a, 32, 24)[3], 255);
            assert!((0..64).any(|x| px(&a, x, 0)[3] < 255));
            // Never cuts deeper than depth + 1.
            for x in 8..56 {
                assert_eq!(px(&a, x, 8)[3], 255);
                assert_eq!(px(&a, x, 47 - 8)[3], 255);
            }
            for y in 8..40 {
                assert_eq!(px(&a, 8, y)[3], 255);
                assert_eq!(px(&a, 63 - 8, y)[3], 255);
            }
            // Different seeds differ for torn.
            if kind == EdgeKind::Torn {
                let mut c = solid_frame(64, 48, [10, 20, 30, 255]);
                edge_effect(&mut c, EdgeSides::ALL, kind, 6.0, 10.0, 43).unwrap();
                assert_ne!(a, c);
            }
        }
    }

    #[test]
    fn sides_are_selective() {
        let mut f = solid_frame(40, 40, [1, 2, 3, 255]);
        let sides = EdgeSides { top: true, right: false, bottom: false, left: false };
        edge_effect(&mut f, sides, EdgeKind::Torn, 8.0, 6.0, 1).unwrap();
        assert!((0..40).any(|x| px(&f, x, 0)[3] < 255));
        for i in 0..40 {
            assert_eq!(px(&f, i, 39)[3], 255);
            assert_eq!(px(&f, 0, i.max(9))[3], 255);
            assert_eq!(px(&f, 39, i.max(9))[3], 255);
        }
    }

    #[test]
    fn degenerate() {
        let mut e = solid_frame(0, 0, [0; 4]);
        edge_effect(&mut e, EdgeSides::ALL, EdgeKind::Torn, 4.0, 4.0, 0).unwrap();
        let mut one = solid_frame(1, 1, [9, 9, 9, 255]);
        edge_effect(&mut one, EdgeSides::ALL, EdgeKind::Wave, 4.0, 4.0, 0).unwrap();
        let mut f = solid_frame(3, 3, [9, 9, 9, 255]);
        edge_effect(&mut f, EdgeSides::ALL, EdgeKind::Torn, 0.0, 4.0, 0).unwrap();
        assert_eq!(f, solid_frame(3, 3, [9, 9, 9, 255]));
        assert!(edge_effect(&mut f, EdgeSides::ALL, EdgeKind::Torn, f32::NAN, 4.0, 0).is_err());
    }
}
