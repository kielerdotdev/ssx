//! Per-pixel colour adjustments.
//!
//! All operate on straight-alpha sRGB values, leave alpha alone and are region-limited.
//! Luma uses Rec. 709 weights. Adjustments that are 1-D curves (brightness, contrast,
//! gamma, invert, threshold) go through a 256-entry lookup table; the matrix ones
//! (saturation, hue, sepia, grayscale) round once at the end.

use ssx_types::{Frame, Rect};

use crate::{
    Result, check, finite,
    region::{clip_region, for_rows_mut},
};

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn apply_lut(frame: &mut Frame, region: Option<Rect>, lut: &[u8; 256]) -> Result<()> {
    check(frame)?;
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    for_rows_mut(frame, r, |_, row| {
        for px in row.chunks_exact_mut(4) {
            px[0] = lut[px[0] as usize];
            px[1] = lut[px[1] as usize];
            px[2] = lut[px[2] as usize];
        }
    });
    Ok(())
}

fn lut_from(f: impl Fn(f32) -> f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (f(i as f32 / 255.0) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

fn apply_matrix(frame: &mut Frame, region: Option<Rect>, m: [[f32; 3]; 3]) -> Result<()> {
    check(frame)?;
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    for_rows_mut(frame, r, |_, row| {
        for px in row.chunks_exact_mut(4) {
            let (r, g, b) = (f32::from(px[0]), f32::from(px[1]), f32::from(px[2]));
            for (c, out) in m.iter().zip(px.iter_mut()) {
                *out = (c[0] * r + c[1] * g + c[2] * b).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    Ok(())
}

/// Adds `amount` (−1..1, where 1 = +255 levels) to every colour channel.
pub fn brightness(frame: &mut Frame, region: Option<Rect>, amount: f32) -> Result<()> {
    let amount = finite(amount, "amount")?.clamp(-1.0, 1.0);
    apply_lut(frame, region, &lut_from(|v| v + amount))
}

/// Scales contrast about mid-grey. `amount` −1 = flat grey, 0 = unchanged, →1 = extreme.
pub fn contrast(frame: &mut Frame, region: Option<Rect>, amount: f32) -> Result<()> {
    let amount = finite(amount, "amount")?.clamp(-1.0, 0.99);
    let f = ((amount + 1.0) * std::f32::consts::FRAC_PI_4).tan();
    apply_lut(frame, region, &lut_from(|v| (v - 0.5) * f + 0.5))
}

/// Gamma correction: `out = in^(1/gamma)`; `gamma > 1` brightens midtones. `gamma <= 0`
/// is ignored.
pub fn gamma(frame: &mut Frame, region: Option<Rect>, gamma: f32) -> Result<()> {
    let g = finite(gamma, "gamma")?;
    if g <= 0.0 {
        return check(frame);
    }
    apply_lut(frame, region, &lut_from(|v| v.powf(1.0 / g)))
}

/// Inverts colour channels (alpha untouched).
pub fn invert(frame: &mut Frame, region: Option<Rect>) -> Result<()> {
    apply_lut(frame, region, &lut_from(|v| 1.0 - v))
}

/// Saturation multiplier: 0 = grayscale, 1 = unchanged, 2 = double saturation.
pub fn saturation(frame: &mut Frame, region: Option<Rect>, amount: f32) -> Result<()> {
    let s = finite(amount, "amount")?.max(0.0);
    let m = [
        [LUMA[0] * (1.0 - s) + s, LUMA[1] * (1.0 - s), LUMA[2] * (1.0 - s)],
        [LUMA[0] * (1.0 - s), LUMA[1] * (1.0 - s) + s, LUMA[2] * (1.0 - s)],
        [LUMA[0] * (1.0 - s), LUMA[1] * (1.0 - s), LUMA[2] * (1.0 - s) + s],
    ];
    apply_matrix(frame, region, m)
}

/// Rotates hue by `degrees` (CSS `hue-rotate` matrix).
pub fn hue_rotate(frame: &mut Frame, region: Option<Rect>, degrees: f32) -> Result<()> {
    let (s, c) = finite(degrees, "degrees")?.to_radians().sin_cos();
    let m = [
        [
            0.213 + c * 0.787 - s * 0.213,
            0.715 - c * 0.715 - s * 0.715,
            0.072 - c * 0.072 + s * 0.928,
        ],
        [
            0.213 - c * 0.213 + s * 0.143,
            0.715 + c * 0.285 + s * 0.140,
            0.072 - c * 0.072 - s * 0.283,
        ],
        [
            0.213 - c * 0.213 - s * 0.787,
            0.715 - c * 0.715 + s * 0.715,
            0.072 + c * 0.928 + s * 0.072,
        ],
    ];
    apply_matrix(frame, region, m)
}

/// Converts to grayscale using Rec. 709 luma.
pub fn grayscale(frame: &mut Frame, region: Option<Rect>) -> Result<()> {
    saturation(frame, region, 0.0)
}

/// Classic sepia tone matrix.
pub fn sepia(frame: &mut Frame, region: Option<Rect>) -> Result<()> {
    apply_matrix(
        frame,
        region,
        [[0.393, 0.769, 0.189], [0.349, 0.686, 0.168], [0.272, 0.534, 0.131]],
    )
}

/// Black/white threshold: pixels whose luma is `>= level` (0–255) become white.
pub fn threshold(frame: &mut Frame, region: Option<Rect>, level: u8) -> Result<()> {
    check(frame)?;
    let Some(r) = clip_region(frame.width(), frame.height(), region) else { return Ok(()) };
    for_rows_mut(frame, r, |_, row| {
        for px in row.chunks_exact_mut(4) {
            let l = LUMA[0] * f32::from(px[0])
                + LUMA[1] * f32::from(px[1])
                + LUMA[2] * f32::from(px[2]);
            let v = if l.round() >= f32::from(level) { 255 } else { 0 };
            px[0] = v;
            px[1] = v;
            px[2] = v;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        solid_frame,
        testutil::{noise, px},
    };

    #[test]
    fn invert_is_involution_and_keeps_alpha() {
        let base = noise(13, 9, true, 4);
        let mut f = base.clone();
        invert(&mut f, None).unwrap();
        assert_ne!(f, base);
        assert_eq!(px(&f, 0, 0)[3], px(&base, 0, 0)[3]);
        invert(&mut f, None).unwrap();
        assert_eq!(f, base);
    }

    #[test]
    fn brightness_exact() {
        let mut f = solid_frame(2, 1, [100, 200, 250, 77]);
        brightness(&mut f, None, 0.2).unwrap();
        assert_eq!(px(&f, 0, 0), [151, 251, 255, 77]);
        brightness(&mut f, None, -1.0).unwrap();
        assert_eq!(px(&f, 1, 0), [0, 0, 0, 77]);
    }

    #[test]
    fn identity_parameters_do_nothing() {
        let base = noise(11, 7, true, 8);
        let mut f = base.clone();
        brightness(&mut f, None, 0.0).unwrap();
        contrast(&mut f, None, 0.0).unwrap();
        gamma(&mut f, None, 1.0).unwrap();
        saturation(&mut f, None, 1.0).unwrap();
        assert_eq!(f, base);
        hue_rotate(&mut f, None, 0.0).unwrap();
        // The CSS hue matrix at 0° is the identity up to its 3-digit coefficients.
        for (a, b) in f.data().iter().zip(base.data()) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 1);
        }
    }

    #[test]
    fn grayscale_and_sepia() {
        let mut f = solid_frame(1, 1, [255, 0, 0, 255]);
        grayscale(&mut f, None).unwrap();
        assert_eq!(px(&f, 0, 0), [54, 54, 54, 255]); // 0.2126 * 255
        let mut w = solid_frame(1, 1, [255, 255, 255, 255]);
        sepia(&mut w, None).unwrap();
        assert_eq!(px(&w, 0, 0), [255, 255, 239, 255]);
        let mut g = solid_frame(1, 1, [40, 90, 200, 255]);
        grayscale(&mut g, None).unwrap();
        let p = px(&g, 0, 0);
        assert!(p[0] == p[1] && p[1] == p[2]);
    }

    #[test]
    fn threshold_binary() {
        let mut f =
            Frame::from_rgba8(3, 1, vec![10, 10, 10, 255, 128, 128, 128, 255, 250, 250, 250, 9])
                .unwrap();
        threshold(&mut f, None, 128).unwrap();
        assert_eq!(px(&f, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&f, 1, 0), [255, 255, 255, 255]);
        assert_eq!(px(&f, 2, 0), [255, 255, 255, 9]);
    }

    #[test]
    fn contrast_and_gamma_direction() {
        let mut f = solid_frame(1, 1, [200, 100, 128, 255]);
        contrast(&mut f, None, 0.5).unwrap();
        let p = px(&f, 0, 0);
        assert!(p[0] > 200 && p[1] < 100);
        let mut g = solid_frame(1, 1, [64, 64, 64, 255]);
        gamma(&mut g, None, 2.0).unwrap();
        assert_eq!(px(&g, 0, 0)[0], 128);
        let mut flat = solid_frame(1, 1, [10, 200, 90, 255]);
        contrast(&mut flat, None, -1.0).unwrap();
        assert_eq!(px(&flat, 0, 0), [128, 128, 128, 255]);
    }

    #[test]
    fn hue_rotate_120_moves_red_to_green_ish() {
        let mut f = solid_frame(1, 1, [255, 0, 0, 255]);
        hue_rotate(&mut f, None, 120.0).unwrap();
        let p = px(&f, 0, 0);
        assert!(p[1] > p[0] && p[1] > p[2], "{p:?}");
    }

    #[test]
    fn region_and_degenerate() {
        let base = solid_frame(4, 4, [10, 20, 30, 255]);
        let mut f = base.clone();
        invert(&mut f, Some(Rect::new(-2, -2, 4, 4))).unwrap();
        assert_eq!(px(&f, 1, 1), [245, 235, 225, 255]);
        assert_eq!(px(&f, 2, 2), [10, 20, 30, 255]);
        let mut e = solid_frame(0, 0, [0; 4]);
        invert(&mut e, None).unwrap();
        sepia(&mut e, None).unwrap();
        let mut one = solid_frame(1, 1, [1, 2, 3, 4]);
        grayscale(&mut one, Some(Rect::new(0, 0, 1, 1))).unwrap();
        assert!(brightness(&mut one, None, f32::INFINITY).is_err());
    }
}
