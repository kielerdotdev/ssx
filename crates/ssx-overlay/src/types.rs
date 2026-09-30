//! Public request/response types of the overlay.
//!
//! Everything here is serialisable because the same types travel over the helper-process
//! protocol (`protocol.rs`); only the frozen desktop [`Frame`] is passed out of band.

use serde::{Deserialize, Serialize};
use ssx_types::{Frame, Monitor, Point, Rect, WindowInfo};

/// What the user is asked to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SelectMode {
    /// Drag a rectangle (the default).
    #[default]
    Rect,
    /// Drag the bounding box of an ellipse.
    Ellipse,
    /// Draw a free-hand outline; the outcome carries the polygon and its bounding rect.
    Freeform,
    /// Click a monitor.
    Monitor,
    /// Click a window (hover-highlighted).
    Window,
}

/// How UI furniture (handles, text, loupe) is sized.
///
/// UI sizes are expressed in *desktop pixels*. Where the desktop is the compositor's
/// logical layout multiplied by one global scale (Wayland, see the capture crate's
/// coordinate model) a single global factor is right; on Windows every monitor has its own
/// DPI and desktop pixels are true physical pixels, so the factor must follow the monitor
/// under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum UiScale {
    /// Use `Frame::scale_factor` of the desktop frame (at least 1.0).
    #[default]
    Auto,
    /// Use `Monitor::scale_factor` of the monitor under the pointer.
    PerMonitor,
    /// A fixed factor.
    Fixed(f32),
}

/// Which windowing backend to use. `Auto` probes the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BackendPreference {
    /// Choose from the environment and the advertised Wayland globals.
    #[default]
    Auto,
    /// X11 override-redirect window.
    X11,
    /// Wayland `wlr-layer-shell` (wlroots, KDE).
    WaylandLayerShell,
    /// One fullscreen `xdg_toplevel` per output (GNOME/Mutter and anything without
    /// layer-shell).
    WaylandFullscreen,
    /// Win32 topmost window.
    Windows,
}

/// Behaviour switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayOptions {
    /// Show the magnifier loupe next to the pointer.
    pub show_loupe: bool,
    /// Show the `W x H` / position label.
    pub show_dimensions: bool,
    /// Highlight the window under the pointer and let a click select its exact rectangle.
    pub snap_to_windows: bool,
    /// How dark the area outside the selection is, 0.0 (not at all) to 1.0 (black).
    pub dim: f32,
    /// A previously used region to start with (`ShareX` "last region").
    pub initial: Option<Rect>,
    /// What is being selected.
    pub mode: SelectMode,
    /// Loupe magnification (screen pixels per source pixel); the mouse wheel changes it.
    pub loupe_zoom: u32,
    /// Pressing `C` ends the overlay with [`OverlayOutcome::ColorPicked`].
    pub allow_color_pick: bool,
    /// UI scale policy.
    pub ui_scale: UiScale,
    /// Give up (and report [`OverlayOutcome::Cancelled`]) after this long. `None` waits
    /// forever in-process; the helper client applies its own default.
    pub timeout_ms: Option<u64>,
    /// Backend override.
    pub backend: BackendPreference,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            show_loupe: true,
            show_dimensions: true,
            snap_to_windows: true,
            dim: 0.5,
            initial: None,
            mode: SelectMode::Rect,
            loupe_zoom: 8,
            allow_color_pick: true,
            ui_scale: UiScale::Auto,
            timeout_ms: None,
            backend: BackendPreference::Auto,
        }
    }
}

/// Everything the overlay needs.
#[derive(Debug, Clone)]
pub struct OverlayInput {
    /// The whole virtual desktop, 8-bit sRGB (already tone-mapped). `Frame::origin` is the
    /// desktop position of its top-left pixel.
    pub desktop: Frame,
    /// Monitor layout in desktop pixels. May be empty: the frame is then one monitor.
    pub monitors: Vec<Monitor>,
    /// Windows for hover-snap, front to back. May be empty.
    pub windows: Vec<WindowInfo>,
    /// Options.
    pub options: OverlayOptions,
}

/// The shape a selection has beyond its bounding rectangle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectionShape {
    /// Plain rectangle.
    Rect,
    /// Ellipse inscribed in the rectangle.
    Ellipse,
    /// Closed polygon (desktop pixels); the rectangle is its bounding box.
    Freeform(Vec<Point>),
}

/// A confirmed region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    /// Bounding rectangle in virtual-desktop physical pixels; never empty.
    pub rect: Rect,
    /// Mask shape.
    pub shape: SelectionShape,
    /// Set when the rectangle came from clicking a hover-highlighted window.
    pub snapped_window: Option<WindowInfo>,
}

impl Selection {
    /// Whether a desktop pixel is inside the selection (honouring ellipse/polygon masks).
    pub fn contains(&self, p: Point) -> bool {
        if !self.rect.contains(p) {
            return false;
        }
        match &self.shape {
            SelectionShape::Rect => true,
            SelectionShape::Ellipse => crate::model::geometry::in_ellipse(self.rect, p),
            SelectionShape::Freeform(pts) => crate::model::geometry::in_polygon(pts, p),
        }
    }

    /// Row-major 8-bit coverage mask of `rect.width * rect.height` (255 inside, 0 outside),
    /// for cropping a frame to the shape.
    pub fn mask(&self) -> Vec<u8> {
        use crate::model::geometry::{ellipse_span, polygon_spans};
        let (w, h) = (self.rect.width as usize, self.rect.height as usize);
        let mut out = vec![0u8; w * h];
        let x0 = i64::from(self.rect.x);
        for row in 0..h {
            let y = i64::from(self.rect.y) + row as i64;
            let spans = match &self.shape {
                SelectionShape::Rect => vec![(x0, x0 + w as i64)],
                SelectionShape::Ellipse => ellipse_span(self.rect, y).into_iter().collect(),
                SelectionShape::Freeform(pts) => polygon_spans(pts, y),
            };
            for (a, b) in spans {
                let (a, b) =
                    ((a - x0).clamp(0, w as i64) as usize, (b - x0).clamp(0, w as i64) as usize);
                out[row * w + a..row * w + b].fill(255);
            }
        }
        out
    }
}

/// A pixel picked with `C`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedColor {
    /// Desktop position of the pixel.
    pub point: Point,
    /// sRGB value.
    pub rgb: [u8; 3],
}

impl PickedColor {
    /// `#rrggbb`.
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.rgb[0], self.rgb[1], self.rgb[2])
    }
}

/// How the overlay ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OverlayOutcome {
    /// A region was confirmed.
    Selected(Selection),
    /// A window was picked (window mode).
    Window(WindowInfo),
    /// A monitor was picked (monitor mode).
    Monitor(Monitor),
    /// `C` was pressed over a pixel.
    ColorPicked(PickedColor),
    /// Escape, right-click on an empty overlay, or timeout.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(shape: SelectionShape, rect: Rect) -> Selection {
        Selection { rect, shape, snapped_window: None }
    }

    #[test]
    fn mask_agrees_with_contains_for_every_shape() {
        let rect = Rect::new(-5, 10, 31, 17);
        let tri = vec![Point::new(-5, 10), Point::new(26, 12), Point::new(3, 27)];
        for shape in [SelectionShape::Rect, SelectionShape::Ellipse, SelectionShape::Freeform(tri)]
        {
            let s = sel(shape, rect);
            let mask = s.mask();
            assert_eq!(mask.len(), 31 * 17);
            for y in 0..17 {
                for x in 0..31 {
                    let p = Point::new(rect.x + x, rect.y + y);
                    assert_eq!(
                        mask[(y * 31 + x) as usize] == 255,
                        s.contains(p),
                        "{:?} {p:?}",
                        s.shape
                    );
                }
            }
            assert!(
                !s.contains(Point::new(rect.x - 1, rect.y)),
                "outside the rectangle is never inside"
            );
        }
        let all = sel(SelectionShape::Rect, rect).mask();
        assert!(all.iter().all(|&m| m == 255));
        let ell = sel(SelectionShape::Ellipse, rect).mask();
        let inside = ell.iter().map(|&m| usize::from(m == 255)).sum::<usize>() as f64;
        let area = std::f64::consts::PI * 31.0 * 17.0 / 4.0;
        assert!((inside - area).abs() < area * 0.08, "ellipse covers ~pi*a*b: {inside} vs {area}");
    }

    #[test]
    fn colour_hex_and_options_round_trip() {
        let c = PickedColor { point: Point::new(1, 2), rgb: [0x0a, 0xff, 0x00] };
        assert_eq!(c.hex(), "#0aff00");
        let o = OverlayOptions {
            mode: SelectMode::Freeform,
            ui_scale: UiScale::Fixed(1.5),
            ..OverlayOptions::default()
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<OverlayOptions>(&json).unwrap(), o);
    }
}
