//! Catalogue of the whole-image effects offered in the Effects menu.
//!
//! `ssx_imgfx::Effect` is a serialisable enum with named fields; a dialog wants *uniform*
//! sliders. This module describes each effect as a list of [`ParamSpec`]s plus a builder that
//! turns slider values back into an `Effect`, so one generic dialog (with live preview) serves
//! them all and a new effect only needs a row here.

use ssx_imgfx::{Effect, EdgeSides, Rgba, ShadowParams};

/// The effects the menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    /// Gaussian blur.
    GaussianBlur,
    /// Mosaic.
    Pixelate,
    /// Simple sharpen.
    Sharpen,
    /// Unsharp mask.
    UnsharpMask,
    /// Brightness.
    Brightness,
    /// Contrast.
    Contrast,
    /// Saturation.
    Saturation,
    /// Hue rotation.
    HueRotate,
    /// Gamma.
    Gamma,
    /// Threshold.
    Threshold,
    /// Grayscale.
    Grayscale,
    /// Sepia.
    Sepia,
    /// Invert.
    Invert,
    /// Drop shadow.
    DropShadow,
    /// Border.
    Border,
    /// Outline.
    Outline,
    /// Rounded corners.
    RoundedCorners,
    /// Torn edge.
    TornEdge,
    /// Wave edge.
    WaveEdge,
}

/// One numeric parameter of an effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    /// Slider label.
    pub label: &'static str,
    /// Lowest value.
    pub min: f32,
    /// Highest value.
    pub max: f32,
    /// Starting value.
    pub default: f32,
    /// Whole numbers only.
    pub integer: bool,
    /// Unit shown after the value.
    pub suffix: &'static str,
}

const fn pf(
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    suffix: &'static str,
) -> ParamSpec {
    ParamSpec { label, min, max, default, integer: false, suffix }
}

const fn pi(label: &'static str, min: f32, max: f32, default: f32, suffix: &'static str) -> ParamSpec {
    ParamSpec { label, min, max, default, integer: true, suffix }
}

impl EffectKind {
    /// Menu order, grouped by [`EffectKind::category`].
    pub const ALL: [EffectKind; 19] = [
        EffectKind::GaussianBlur,
        EffectKind::Pixelate,
        EffectKind::Sharpen,
        EffectKind::UnsharpMask,
        EffectKind::Brightness,
        EffectKind::Contrast,
        EffectKind::Saturation,
        EffectKind::HueRotate,
        EffectKind::Gamma,
        EffectKind::Threshold,
        EffectKind::Grayscale,
        EffectKind::Sepia,
        EffectKind::Invert,
        EffectKind::DropShadow,
        EffectKind::Border,
        EffectKind::Outline,
        EffectKind::RoundedCorners,
        EffectKind::TornEdge,
        EffectKind::WaveEdge,
    ];

    /// Menu title.
    pub fn label(self) -> &'static str {
        match self {
            EffectKind::GaussianBlur => "Blur",
            EffectKind::Pixelate => "Pixelate",
            EffectKind::Sharpen => "Sharpen",
            EffectKind::UnsharpMask => "Unsharp mask",
            EffectKind::Brightness => "Brightness",
            EffectKind::Contrast => "Contrast",
            EffectKind::Saturation => "Saturation",
            EffectKind::HueRotate => "Hue",
            EffectKind::Gamma => "Gamma",
            EffectKind::Threshold => "Threshold",
            EffectKind::Grayscale => "Grayscale",
            EffectKind::Sepia => "Sepia",
            EffectKind::Invert => "Invert colours",
            EffectKind::DropShadow => "Drop shadow",
            EffectKind::Border => "Border",
            EffectKind::Outline => "Outline",
            EffectKind::RoundedCorners => "Rounded corners",
            EffectKind::TornEdge => "Torn edge",
            EffectKind::WaveEdge => "Wave edge",
        }
    }

    /// Menu section.
    pub fn category(self) -> &'static str {
        match self {
            EffectKind::GaussianBlur
            | EffectKind::Pixelate
            | EffectKind::Sharpen
            | EffectKind::UnsharpMask => "Filters",
            EffectKind::Brightness
            | EffectKind::Contrast
            | EffectKind::Saturation
            | EffectKind::HueRotate
            | EffectKind::Gamma
            | EffectKind::Threshold
            | EffectKind::Grayscale
            | EffectKind::Sepia
            | EffectKind::Invert => "Adjust",
            _ => "Decorate",
        }
    }

    /// Sliders of the effect.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            EffectKind::GaussianBlur => const { &[pf("Radius", 0.5, 50.0, 4.0, " px")] },
            EffectKind::Pixelate => const { &[pi("Block size", 2.0, 96.0, 8.0, " px")] },
            EffectKind::Sharpen => const { &[pf("Amount", 0.0, 3.0, 0.6, "")] },
            EffectKind::UnsharpMask => const { &[
                pf("Radius", 0.5, 20.0, 2.0, " px"),
                pf("Amount", 0.0, 5.0, 1.0, ""),
                pi("Threshold", 0.0, 255.0, 0.0, ""),
            ] },
            EffectKind::Brightness => const { &[pf("Amount", -1.0, 1.0, 0.15, "")] },
            EffectKind::Contrast => const { &[pf("Amount", -1.0, 1.0, 0.2, "")] },
            EffectKind::Saturation => const { &[pf("Amount", 0.0, 3.0, 1.4, "")] },
            EffectKind::HueRotate => const { &[pf("Rotation", -180.0, 180.0, 30.0, "°")] },
            EffectKind::Gamma => const { &[pf("Gamma", 0.2, 4.0, 1.4, "")] },
            EffectKind::Threshold => const { &[pi("Level", 0.0, 255.0, 128.0, "")] },
            EffectKind::Grayscale | EffectKind::Sepia | EffectKind::Invert => const { &[] },
            EffectKind::DropShadow => const { &[
                pi("Offset X", -60.0, 60.0, 6.0, " px"),
                pi("Offset Y", -60.0, 60.0, 6.0, " px"),
                pf("Blur", 0.0, 40.0, 8.0, " px"),
            ] },
            EffectKind::Border => const { &[pi("Width", 1.0, 100.0, 8.0, " px")] },
            EffectKind::Outline => const { &[pi("Width", 1.0, 50.0, 4.0, " px")] },
            EffectKind::RoundedCorners => const { &[pf("Radius", 0.0, 300.0, 16.0, " px")] },
            EffectKind::TornEdge => const { &[
                pf("Depth", 1.0, 60.0, 8.0, " px"),
                pf("Tooth width", 2.0, 100.0, 12.0, " px"),
                pi("Pattern", 0.0, 999.0, 7.0, ""),
            ] },
            EffectKind::WaveEdge => const { &[
                pf("Depth", 1.0, 60.0, 8.0, " px"),
                pf("Wavelength", 4.0, 200.0, 24.0, " px"),
            ] },
        }
    }

    /// Does the effect take a colour?
    pub fn has_color(self) -> bool {
        matches!(self, EffectKind::DropShadow | EffectKind::Border | EffectKind::Outline)
    }

    /// Does the effect take a set of sides?
    pub fn has_sides(self) -> bool {
        matches!(self, EffectKind::TornEdge | EffectKind::WaveEdge)
    }

    /// The colour the dialog starts with.
    pub fn default_color(self) -> Rgba {
        match self {
            EffectKind::DropShadow => [0, 0, 0, 160],
            EffectKind::Outline => [255, 255, 255, 255],
            _ => [30, 30, 30, 255],
        }
    }

    /// Can the effect be restricted to a region? (`false` for canvas-reshaping effects.)
    pub fn honours_region(self) -> bool {
        !matches!(
            self,
            EffectKind::DropShadow
                | EffectKind::Border
                | EffectKind::Outline
                | EffectKind::RoundedCorners
                | EffectKind::TornEdge
                | EffectKind::WaveEdge
        )
    }

    /// Builds the effect from slider `values` (missing values fall back to defaults).
    pub fn build(self, values: &[f32], color: Rgba, sides: EdgeSides) -> Effect {
        let specs = self.params();
        let v = |i: usize| {
            let spec = specs.get(i).copied().unwrap_or(pf("", 0.0, 1.0, 0.0, ""));
            let raw = values.get(i).copied().filter(|x| x.is_finite()).unwrap_or(spec.default);
            let raw = raw.clamp(spec.min, spec.max);
            if spec.integer { raw.round() } else { raw }
        };
        match self {
            EffectKind::GaussianBlur => Effect::GaussianBlur { sigma: v(0) },
            EffectKind::Pixelate => Effect::Pixelate { block: v(0) as u32 },
            EffectKind::Sharpen => Effect::Sharpen { amount: v(0) },
            EffectKind::UnsharpMask => {
                Effect::UnsharpMask { sigma: v(0), amount: v(1), threshold: v(2) as u8 }
            }
            EffectKind::Brightness => Effect::Brightness { amount: v(0) },
            EffectKind::Contrast => Effect::Contrast { amount: v(0) },
            EffectKind::Saturation => Effect::Saturation { amount: v(0) },
            EffectKind::HueRotate => Effect::HueRotate { degrees: v(0) },
            EffectKind::Gamma => Effect::Gamma { gamma: v(0) },
            EffectKind::Threshold => Effect::Threshold { level: v(0) as u8 },
            EffectKind::Grayscale => Effect::Grayscale,
            EffectKind::Sepia => Effect::Sepia,
            EffectKind::Invert => Effect::Invert,
            EffectKind::DropShadow => Effect::DropShadow {
                params: ShadowParams {
                    offset_x: v(0) as i32,
                    offset_y: v(1) as i32,
                    sigma: v(2),
                    color,
                },
            },
            EffectKind::Border => Effect::Border { width: v(0) as u32, color },
            EffectKind::Outline => Effect::Outline { width: v(0) as u32, color },
            EffectKind::RoundedCorners => Effect::RoundedCorners { radius: v(0) },
            EffectKind::TornEdge => {
                Effect::TornEdge { sides, depth: v(0), period: v(1), seed: v(2) as u64 }
            }
            EffectKind::WaveEdge => Effect::WaveEdge { sides, depth: v(0), period: v(1) },
        }
    }
}

/// The values a dialog edits: sliders, colour and sides.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectForm {
    /// Which effect.
    pub kind: EffectKind,
    /// Slider values, parallel to [`EffectKind::params`].
    pub values: Vec<f32>,
    /// Colour parameter.
    pub color: Rgba,
    /// Sides parameter.
    pub sides: EdgeSides,
}

impl EffectForm {
    /// A form with the effect's default values.
    pub fn new(kind: EffectKind) -> Self {
        Self {
            kind,
            values: kind.params().iter().map(|s| s.default).collect(),
            color: kind.default_color(),
            sides: EdgeSides::ALL,
        }
    }

    /// The effect the form currently describes.
    pub fn effect(&self) -> Effect {
        self.kind.build(&self.values, self.color, self.sides)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssx_imgfx::solid_frame;

    #[test]
    fn every_default_effect_applies_and_serialises() {
        let f = solid_frame(40, 30, [90, 120, 200, 255]);
        for k in EffectKind::ALL {
            let e = EffectForm::new(k).effect();
            let json = serde_json::to_string(&e).unwrap();
            let back: Effect = serde_json::from_str(&json).unwrap();
            assert_eq!(back, e, "{k:?}");
            let out = e.apply(&f, None).unwrap_or_else(|err| panic!("{k:?}: {err}"));
            assert!(out.frame.width() >= 40, "{k:?}");
            assert!(!k.label().is_empty() && !k.category().is_empty());
        }
    }

    #[test]
    fn build_clamps_and_rounds() {
        let e = EffectKind::Pixelate.build(&[1e9], [0; 4], EdgeSides::ALL);
        assert_eq!(e, Effect::Pixelate { block: 96 });
        let e = EffectKind::Pixelate.build(&[f32::NAN], [0; 4], EdgeSides::ALL);
        assert_eq!(e, Effect::Pixelate { block: 8 }, "NaN falls back to the default");
        let e = EffectKind::Brightness.build(&[-9.0], [0; 4], EdgeSides::ALL);
        assert_eq!(e, Effect::Brightness { amount: -1.0 });
        let e = EffectKind::GaussianBlur.build(&[], [0; 4], EdgeSides::ALL);
        assert_eq!(e, Effect::GaussianBlur { sigma: 4.0 });
    }

    #[test]
    fn specs_are_sane() {
        for k in EffectKind::ALL {
            for s in k.params() {
                assert!(s.min < s.max, "{k:?} {}", s.label);
                assert!(s.default >= s.min && s.default <= s.max, "{k:?} {}", s.label);
            }
        }
    }

    #[test]
    fn region_and_colour_flags_match_the_engine() {
        // Effects that ignore the region in ssx-imgfx must not claim to honour it.
        let f = ssx_imgfx::solid_frame(20, 20, [10, 200, 30, 255]);
        let region = ssx_types::Rect::new(0, 0, 5, 5);
        for k in EffectKind::ALL {
            if k == EffectKind::Invert || k.honours_region() {
                continue;
            }
            let e = EffectForm::new(k).effect();
            let with = e.apply(&f, Some(region)).unwrap().frame;
            let without = e.apply(&f, None).unwrap().frame;
            assert_eq!(with, without, "{k:?} should ignore the region");
        }
        assert!(EffectKind::Border.has_color() && !EffectKind::Sepia.has_color());
        assert!(EffectKind::TornEdge.has_sides() && !EffectKind::Border.has_sides());
    }
}
