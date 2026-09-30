//! Visual style shared by all object kinds.
//!
//! One flat struct (like `ShareX`'s shape options) rather than per-kind styles: a GUI can bind
//! a single "colour / width / fill / shadow" panel to whatever is selected, and per-tool
//! "last used style" memory stays trivial. Fields that make no sense for a kind (a text
//! object has no corner radius) are simply ignored by that kind.

use serde::{Deserialize, Serialize};
pub use ssx_imgfx::BlendMode;

use crate::geom::Color;

/// Dash pattern of strokes. Lengths are in multiples of the stroke width so the pattern
/// scales with thickness like GDI+ presets do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashStyle {
    /// Continuous line.
    #[default]
    Solid,
    /// Long dashes.
    Dash,
    /// Round dots.
    Dot,
    /// Dash, dot, dash, dot.
    DashDot,
}

impl DashStyle {
    /// The on/off pattern in units of stroke width, `None` for solid.
    pub fn pattern(self) -> Option<&'static [f32]> {
        match self {
            DashStyle::Solid => None,
            DashStyle::Dash => Some(&[4.0, 2.0]),
            DashStyle::Dot => Some(&[0.1, 2.0]),
            DashStyle::DashDot => Some(&[4.0, 2.0, 0.1, 2.0]),
        }
    }
}

/// Interior fill.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fill {
    /// No fill.
    #[default]
    None,
    /// A flat colour.
    Solid {
        /// Fill colour.
        color: Color,
    },
    /// A two-stop linear gradient across the object's bounding box.
    Gradient {
        /// Colour at the start.
        from: Color,
        /// Colour at the end.
        to: Color,
        /// Direction in degrees (0 = left→right, 90 = top→bottom).
        angle: f32,
    },
}

impl Fill {
    /// `true` when nothing would be painted.
    pub fn is_none(&self) -> bool {
        match self {
            Fill::None => true,
            Fill::Solid { color } => color.is_transparent(),
            Fill::Gradient { from, to, .. } => from.is_transparent() && to.is_transparent(),
        }
    }

    /// A solid fill helper.
    pub const fn solid(color: Color) -> Fill {
        Fill::Solid { color }
    }
}

/// A soft drop shadow behind an object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Shadow {
    /// Horizontal offset in image pixels.
    pub dx: f32,
    /// Vertical offset in image pixels.
    pub dy: f32,
    /// Gaussian blur σ in image pixels.
    pub blur: f32,
    /// Shadow colour (its alpha is the shadow strength).
    pub color: Color,
}

impl Default for Shadow {
    fn default() -> Self {
        Self { dx: 3.0, dy: 3.0, blur: 4.0, color: Color::rgba(0, 0, 0, 140) }
    }
}

/// Style properties common to every object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    /// Outline / line / arrow colour.
    pub stroke: Color,
    /// Outline thickness in image pixels (0 = no outline).
    pub stroke_width: f32,
    /// Stroke dash pattern.
    pub dash: DashStyle,
    /// Interior fill.
    pub fill: Fill,
    /// Overall opacity 0–1 applied to the whole object.
    pub opacity: f32,
    /// Optional drop shadow.
    pub shadow: Option<Shadow>,
    /// Corner radius for rectangles / balloons (image pixels).
    pub corner_radius: f32,
    /// How the object is composited onto what is below it.
    pub blend: BlendMode,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            stroke: Color::RED,
            stroke_width: 4.0,
            dash: DashStyle::Solid,
            fill: Fill::None,
            opacity: 1.0,
            shadow: None,
            corner_radius: 0.0,
            blend: BlendMode::Normal,
        }
    }
}

impl Style {
    /// `true` when a stroke would be visible.
    pub fn has_stroke(&self) -> bool {
        self.stroke_width > 0.0 && !self.stroke.is_transparent()
    }

    /// The fill colour when the fill is solid.
    pub fn solid_fill(&self) -> Option<Color> {
        match self.fill {
            Fill::Solid { color } => Some(color),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact geometry values are what these tests assert
mod tests {
    use super::*;

    #[test]
    fn style_defaults_and_partial_json() {
        let s: Style = serde_json::from_str(r##"{"stroke":"#00ff00ff"}"##).unwrap();
        assert_eq!(s.stroke, Color::rgb(0, 255, 0));
        assert_eq!(s.stroke_width, 4.0, "missing fields fall back to defaults");
        assert!(s.has_stroke());
        let round: Style = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round, s);
    }

    #[test]
    fn fill_emptiness() {
        assert!(Fill::None.is_none());
        assert!(Fill::solid(Color::TRANSPARENT).is_none());
        assert!(!Fill::solid(Color::RED).is_none());
    }
}
