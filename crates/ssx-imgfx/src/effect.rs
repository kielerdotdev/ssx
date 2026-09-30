//! A serialisable description of "an effect", so the editor can store, undo and replay
//! effects without knowing about each function.

use serde::{Deserialize, Serialize};
use ssx_types::{Frame, Point, Rect};

use crate::{
    BlurMethod, EdgeKind, EdgeSides, Placed, Result, Rgba, ShadowParams, add_border, brightness,
    contrast, drop_shadow, edge_effect, gamma, gaussian_blur, grayscale, hue_rotate, invert,
    outline, pixelate, resize::tight_copy, round_corners, saturation, sepia, sharpen, threshold,
    unsharp_mask,
};

/// An image effect with its parameters.
///
/// Pixel-level effects (blur, pixelate, sharpen, colour) honour the `region` passed to
/// [`Effect::apply`]. Effects that reshape the whole image (shadow, border, outline,
/// rounded corners, edge effects) ignore it and always work on the entire image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    /// Gaussian blur.
    GaussianBlur {
        /// Standard deviation in pixels.
        sigma: f32,
    },
    /// Mosaic.
    Pixelate {
        /// Block edge in pixels.
        block: u32,
    },
    /// Simple sharpen.
    Sharpen {
        /// Strength (0–1+).
        amount: f32,
    },
    /// Unsharp mask.
    UnsharpMask {
        /// Blur σ in pixels.
        sigma: f32,
        /// Strength.
        amount: f32,
        /// Minimum difference (0–255) that is sharpened.
        threshold: u8,
    },
    /// Brightness −1..1.
    Brightness {
        /// Amount.
        amount: f32,
    },
    /// Contrast −1..1.
    Contrast {
        /// Amount.
        amount: f32,
    },
    /// Saturation multiplier.
    Saturation {
        /// 1 = unchanged.
        amount: f32,
    },
    /// Hue rotation.
    HueRotate {
        /// Degrees.
        degrees: f32,
    },
    /// Gamma.
    Gamma {
        /// `> 1` brightens midtones.
        gamma: f32,
    },
    /// Desaturate.
    Grayscale,
    /// Sepia tone.
    Sepia,
    /// Invert colours.
    Invert,
    /// Black/white threshold.
    Threshold {
        /// Luma level 0–255.
        level: u8,
    },
    /// Soft drop shadow (grows the canvas).
    DropShadow {
        /// Shadow settings.
        params: ShadowParams,
    },
    /// Solid border (grows the canvas).
    Border {
        /// Width in pixels.
        width: u32,
        /// Border colour.
        color: Rgba,
    },
    /// Outline around the alpha shape (grows the canvas).
    Outline {
        /// Width in pixels.
        width: u32,
        /// Outline colour.
        color: Rgba,
    },
    /// Rounded corners.
    RoundedCorners {
        /// Radius in pixels.
        radius: f32,
    },
    /// Torn-paper edge.
    TornEdge {
        /// Sides to tear.
        sides: EdgeSides,
        /// Maximum depth in pixels.
        depth: f32,
        /// Average tooth width.
        period: f32,
        /// Random seed.
        seed: u64,
    },
    /// Wavy edge.
    WaveEdge {
        /// Sides to wave.
        sides: EdgeSides,
        /// Amplitude (peak-to-peak) in pixels.
        depth: f32,
        /// Wavelength.
        period: f32,
    },
}

impl Effect {
    /// Human-readable name for undo menus.
    pub fn label(&self) -> &'static str {
        match self {
            Effect::GaussianBlur { .. } => "Blur",
            Effect::Pixelate { .. } => "Pixelate",
            Effect::Sharpen { .. } | Effect::UnsharpMask { .. } => "Sharpen",
            Effect::Brightness { .. } => "Brightness",
            Effect::Contrast { .. } => "Contrast",
            Effect::Saturation { .. } => "Saturation",
            Effect::HueRotate { .. } => "Hue",
            Effect::Gamma { .. } => "Gamma",
            Effect::Grayscale => "Grayscale",
            Effect::Sepia => "Sepia",
            Effect::Invert => "Invert",
            Effect::Threshold { .. } => "Threshold",
            Effect::DropShadow { .. } => "Drop shadow",
            Effect::Border { .. } => "Border",
            Effect::Outline { .. } => "Outline",
            Effect::RoundedCorners { .. } => "Rounded corners",
            Effect::TornEdge { .. } => "Torn edge",
            Effect::WaveEdge { .. } => "Wave edge",
        }
    }

    /// `true` when the output size can differ from the input (so annotations must move).
    pub fn changes_size(&self) -> bool {
        matches!(self, Effect::DropShadow { .. } | Effect::Border { .. } | Effect::Outline { .. })
    }

    /// Applies the effect to a copy of `frame`.
    pub fn apply(&self, frame: &Frame, region: Option<Rect>) -> Result<Placed> {
        let origin = Point::new(0, 0);
        let mut f = tight_copy(frame);
        crate::check(&f)?;
        match self {
            Effect::GaussianBlur { sigma } => {
                gaussian_blur(&mut f, region, *sigma, BlurMethod::Auto)?;
            }
            Effect::Pixelate { block } => pixelate(&mut f, region, *block)?,
            Effect::Sharpen { amount } => sharpen(&mut f, region, *amount)?,
            Effect::UnsharpMask { sigma, amount, threshold } => {
                unsharp_mask(&mut f, region, *sigma, *amount, *threshold)?;
            }
            Effect::Brightness { amount } => brightness(&mut f, region, *amount)?,
            Effect::Contrast { amount } => contrast(&mut f, region, *amount)?,
            Effect::Saturation { amount } => saturation(&mut f, region, *amount)?,
            Effect::HueRotate { degrees } => hue_rotate(&mut f, region, *degrees)?,
            Effect::Gamma { gamma: g } => gamma(&mut f, region, *g)?,
            Effect::Grayscale => grayscale(&mut f, region)?,
            Effect::Sepia => sepia(&mut f, region)?,
            Effect::Invert => invert(&mut f, region)?,
            Effect::Threshold { level } => threshold(&mut f, region, *level)?,
            Effect::DropShadow { params } => return drop_shadow(&f, params),
            Effect::Border { width, color } => return add_border(&f, *width, *color),
            Effect::Outline { width, color } => return outline(&f, *width, *color),
            Effect::RoundedCorners { radius } => round_corners(&mut f, *radius, [true; 4])?,
            Effect::TornEdge { sides, depth, period, seed } => {
                edge_effect(&mut f, *sides, EdgeKind::Torn, *depth, *period, *seed)?;
            }
            Effect::WaveEdge { sides, depth, period } => {
                edge_effect(&mut f, *sides, EdgeKind::Wave, *depth, *period, 0)?;
            }
        }
        Ok(Placed { frame: f, origin })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        solid_frame,
        testutil::{noise, px},
    };

    #[test]
    fn serde_round_trip_every_variant() {
        let all = vec![
            Effect::GaussianBlur { sigma: 3.5 },
            Effect::Pixelate { block: 8 },
            Effect::Sharpen { amount: 0.5 },
            Effect::UnsharpMask { sigma: 2.0, amount: 1.0, threshold: 3 },
            Effect::Brightness { amount: 0.1 },
            Effect::Contrast { amount: -0.2 },
            Effect::Saturation { amount: 1.5 },
            Effect::HueRotate { degrees: 90.0 },
            Effect::Gamma { gamma: 2.2 },
            Effect::Grayscale,
            Effect::Sepia,
            Effect::Invert,
            Effect::Threshold { level: 100 },
            Effect::DropShadow { params: ShadowParams::default() },
            Effect::Border { width: 3, color: [1, 2, 3, 4] },
            Effect::Outline { width: 2, color: [1, 2, 3, 4] },
            Effect::RoundedCorners { radius: 9.0 },
            Effect::TornEdge { sides: EdgeSides::ALL, depth: 5.0, period: 8.0, seed: 7 },
            Effect::WaveEdge { sides: EdgeSides::ALL, depth: 5.0, period: 8.0 },
        ];
        let src = noise(30, 20, true, 1);
        for e in all {
            let json = serde_json::to_string(&e).unwrap();
            let back: Effect = serde_json::from_str(&json).unwrap();
            assert_eq!(back, e);
            // Applies without panic on odd inputs too.
            for f in [src.clone(), solid_frame(1, 1, [1; 4]), solid_frame(0, 0, [0; 4])] {
                let out = e.apply(&f, Some(Rect::new(-3, -3, 10, 10))).unwrap();
                if !e.changes_size() {
                    assert_eq!(out.frame.size(), f.size(), "{}", e.label());
                }
            }
        }
    }

    #[test]
    fn apply_does_not_touch_input_and_honours_region() {
        let f = noise(8, 8, false, 1);
        let orig = f.clone();
        let out = Effect::Invert.apply(&f, Some(Rect::new(0, 0, 4, 8))).unwrap();
        assert_eq!(f, orig);
        assert_ne!(px(&out.frame, 0, 0), px(&f, 0, 0));
        assert_eq!(px(&out.frame, 6, 0), px(&f, 6, 0));
        assert_eq!(out.origin, Point::new(0, 0));
    }
}
