//! State and validation of the modal dialogs, separate from how they are drawn.
//!
//! Each form owns the raw field values the widgets edit and knows how to turn them into an
//! engine operation, or into a human-readable reason why it cannot. Tests cover the arithmetic
//! (aspect lock, percent scaling, clamping) once here instead of once per widget.

use std::path::PathBuf;

use ssx_editor::{Color, Document, object::Axis};
use ssx_imgfx::ResizeFilter;

use crate::export::{ExportSettings, SaveFormat};

/// Largest side length the editor accepts for resize/canvas operations.
pub const MAX_SIDE: u32 = 32_768;

/// "Save as" form.
#[derive(Debug, Clone, PartialEq)]
pub struct SaveForm {
    /// Target path being edited as text.
    pub path: String,
    /// Chosen format (kept in step with the path's extension).
    pub format: SaveFormat,
    /// JPEG quality 1-100.
    pub jpeg_quality: u8,
    /// Fast PNG compression.
    pub png_fast: bool,
}

impl SaveForm {
    /// A form pre-filled with a suggested path.
    pub fn new(path: PathBuf, settings: ExportSettings) -> Self {
        let format =
            SaveFormat::from_path(&path).unwrap_or(SaveFormat::Image(ssx_types::ImageFormat::Png));
        Self {
            path: path.display().to_string(),
            format,
            jpeg_quality: settings.jpeg_quality,
            png_fast: settings.png_fast,
        }
    }

    /// Picks a format and rewrites the path's extension to match.
    pub fn set_format(&mut self, f: SaveFormat) {
        self.format = f;
        let p = PathBuf::from(self.path.trim());
        if !self.path.trim().is_empty() {
            self.path = crate::export::with_format(&p, f).display().to_string();
        }
    }

    /// Re-derives the format from the typed path (when the extension is recognised).
    pub fn sync_format_from_path(&mut self) {
        if let Some(f) = SaveFormat::from_path(&PathBuf::from(self.path.trim())) {
            self.format = f;
        }
    }

    /// The encoder settings.
    pub fn settings(&self) -> ExportSettings {
        ExportSettings { jpeg_quality: self.jpeg_quality.clamp(1, 100), png_fast: self.png_fast }
    }

    /// The validated target path.
    pub fn target(&self) -> Result<PathBuf, String> {
        let t = self.path.trim();
        if t.is_empty() {
            return Err("Enter a file name.".into());
        }
        let p = PathBuf::from(t);
        if SaveFormat::from_path(&p).is_none() {
            return Err("Use a .png, .jpg, .webp, .bmp or .ssxe file name.".into());
        }
        if p.file_name().is_none() || p.is_dir() {
            return Err("That is a folder, not a file name.".into());
        }
        Ok(p)
    }
}

/// "Open by path" form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenForm {
    /// Path being typed.
    pub path: String,
}

impl OpenForm {
    /// The validated path.
    pub fn target(&self) -> Result<PathBuf, String> {
        let t = self.path.trim().trim_matches('"');
        if t.is_empty() {
            return Err("Enter a file path.".into());
        }
        let p = PathBuf::from(t);
        if !p.is_file() {
            return Err(format!("{} is not a file.", p.display()));
        }
        Ok(p)
    }
}

/// "Resize image" form.
#[derive(Debug, Clone, PartialEq)]
pub struct ResizeForm {
    /// Original width.
    pub orig_w: u32,
    /// Original height.
    pub orig_h: u32,
    /// Target width.
    pub width: u32,
    /// Target height.
    pub height: u32,
    /// Keep the aspect ratio while editing one side.
    pub lock_aspect: bool,
    /// Resampling filter.
    pub filter: ResizeFilter,
}

impl ResizeForm {
    /// A form for an image of the given size.
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            orig_w: w,
            orig_h: h,
            width: w,
            height: h,
            lock_aspect: true,
            filter: ResizeFilter::Lanczos3,
        }
    }

    /// Sets the width, adjusting the height when the aspect is locked.
    pub fn set_width(&mut self, w: u32) {
        self.width = w.clamp(1, MAX_SIDE);
        if self.lock_aspect && self.orig_w > 0 {
            let h =
                (f64::from(self.width) * f64::from(self.orig_h) / f64::from(self.orig_w)).round();
            self.height = (h as u32).clamp(1, MAX_SIDE);
        }
    }

    /// Sets the height, adjusting the width when the aspect is locked.
    pub fn set_height(&mut self, h: u32) {
        self.height = h.clamp(1, MAX_SIDE);
        if self.lock_aspect && self.orig_h > 0 {
            let w =
                (f64::from(self.height) * f64::from(self.orig_w) / f64::from(self.orig_h)).round();
            self.width = (w as u32).clamp(1, MAX_SIDE);
        }
    }

    /// Scales both sides to `percent` of the original.
    pub fn set_percent(&mut self, percent: f32) {
        let k = f64::from(percent.clamp(0.1, 1000.0)) / 100.0;
        self.width = ((f64::from(self.orig_w) * k).round() as u32).clamp(1, MAX_SIDE);
        self.height = ((f64::from(self.orig_h) * k).round() as u32).clamp(1, MAX_SIDE);
    }

    /// The current width as a percentage of the original.
    pub fn percent(&self) -> f32 {
        if self.orig_w == 0 {
            100.0
        } else {
            (f64::from(self.width) / f64::from(self.orig_w) * 100.0) as f32
        }
    }

    /// Validates the target size.
    pub fn target(&self) -> Result<(u32, u32), String> {
        if self.width == 0 || self.height == 0 {
            return Err("Width and height must be at least 1 pixel.".into());
        }
        if self.width > MAX_SIDE || self.height > MAX_SIDE {
            return Err(format!("The largest supported side is {MAX_SIDE} pixels."));
        }
        Ok((self.width, self.height))
    }
}

/// "Canvas size" form: grow or shrink on each side.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasForm {
    /// Pixels added (positive) or removed (negative) on the left.
    pub left: i32,
    /// Top.
    pub top: i32,
    /// Right.
    pub right: i32,
    /// Bottom.
    pub bottom: i32,
    /// Fill of the added area; `None` = transparent.
    pub background: Option<Color>,
    /// Current image size, to validate shrinking.
    pub image: (u32, u32),
}

impl CanvasForm {
    /// A form with no change, white background.
    pub fn new(image: (u32, u32)) -> Self {
        Self { left: 0, top: 0, right: 0, bottom: 0, background: Some(Color::WHITE), image }
    }

    /// The resulting size, or why it is invalid.
    pub fn result_size(&self) -> Result<(u32, u32), String> {
        let w = i64::from(self.image.0) + i64::from(self.left) + i64::from(self.right);
        let h = i64::from(self.image.1) + i64::from(self.top) + i64::from(self.bottom);
        if w < 1 || h < 1 {
            return Err("That would remove the whole image.".into());
        }
        if w > i64::from(MAX_SIDE) || h > i64::from(MAX_SIDE) {
            return Err(format!("The largest supported side is {MAX_SIDE} pixels."));
        }
        Ok((w as u32, h as u32))
    }

    /// `true` when the form changes nothing.
    pub fn is_noop(&self) -> bool {
        self.left == 0 && self.top == 0 && self.right == 0 && self.bottom == 0
    }
}

/// Numeric crop form (image coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct CropForm {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Image size.
    pub image: (u32, u32),
    /// Auto-crop tolerance 0-255.
    pub tolerance: u8,
}

impl CropForm {
    /// A form covering the whole image.
    pub fn new(image: (u32, u32)) -> Self {
        Self { x: 0, y: 0, width: image.0, height: image.1, image, tolerance: 8 }
    }

    /// Validates the rectangle against the image.
    pub fn rect(&self) -> Result<ssx_types::Rect, String> {
        if self.width == 0 || self.height == 0 {
            return Err("The crop must be at least 1 pixel wide and tall.".into());
        }
        let r = ssx_types::Rect::new(self.x, self.y, self.width, self.height);
        let img = ssx_types::Rect::new(0, 0, self.image.0, self.image.1);
        if r.intersect(img).is_none() {
            return Err("The crop rectangle is outside the image.".into());
        }
        Ok(r)
    }
}

/// Numeric cut-out form.
#[derive(Debug, Clone, PartialEq)]
pub struct CutForm {
    /// Cut a vertical strip (`X`) or a horizontal one (`Y`).
    pub axis: Axis,
    /// First removed pixel.
    pub start: u32,
    /// One past the last removed pixel.
    pub end: u32,
    /// Image size.
    pub image: (u32, u32),
}

impl CutForm {
    /// A form removing the middle tenth of the width.
    pub fn new(image: (u32, u32)) -> Self {
        let w = image.0;
        Self { axis: Axis::X, start: w * 45 / 100, end: w * 55 / 100, image }
    }

    /// Length of the image along the chosen axis.
    pub fn len(&self) -> u32 {
        match self.axis {
            Axis::X => self.image.0,
            Axis::Y => self.image.1,
        }
    }

    /// Validates the strip.
    pub fn strip(&self) -> Result<(Axis, i32, i32), String> {
        let len = self.len();
        if self.end <= self.start {
            return Err("The end must be after the start.".into());
        }
        if self.end > len {
            return Err(format!("The strip runs past the image edge ({len} px)."));
        }
        if self.start == 0 && self.end == len {
            return Err("That would remove the whole image.".into());
        }
        Ok((self.axis, self.start as i32, self.end as i32))
    }
}

/// Convenience: the image size of a document.
pub fn size_of(doc: &Document) -> (u32, u32) {
    doc.image_size()
}

#[cfg(test)]
mod tests {
    use ssx_types::ImageFormat;

    use super::*;

    #[test]
    fn resize_keeps_the_aspect_ratio() {
        let mut f = ResizeForm::new(1920, 1080);
        f.set_width(960);
        assert_eq!((f.width, f.height), (960, 540));
        f.set_height(270);
        assert_eq!((f.width, f.height), (480, 270));
        f.lock_aspect = false;
        f.set_width(100);
        assert_eq!((f.width, f.height), (100, 270));
        f.set_percent(50.0);
        assert_eq!((f.width, f.height), (960, 540));
        assert!((f.percent() - 50.0).abs() < 0.01);
        assert_eq!(f.target(), Ok((960, 540)));
    }

    #[test]
    fn resize_clamps_and_validates() {
        let mut f = ResizeForm::new(100, 50);
        f.set_width(0);
        assert_eq!(f.width, 1);
        f.set_width(u32::MAX);
        assert_eq!(f.width, MAX_SIDE);
        f.width = 0;
        assert!(f.target().is_err());
        f.width = MAX_SIDE + 1;
        f.height = 1;
        assert!(f.target().unwrap_err().contains("largest"));
        let mut f = ResizeForm::new(3, 1000);
        f.set_height(1);
        assert_eq!(f.width, 1, "never rounds down to zero");
        f.set_percent(f32::NAN.max(0.0));
        assert!(f.width >= 1 && f.height >= 1);
    }

    #[test]
    fn canvas_validation() {
        let mut c = CanvasForm::new((100, 80));
        assert!(c.is_noop());
        c.left = 20;
        c.right = -10;
        assert_eq!(c.result_size(), Ok((110, 80)));
        c.top = -80;
        assert!(c.result_size().is_err());
        c.top = 0;
        c.bottom = i32::MAX;
        assert!(c.result_size().unwrap_err().contains("largest"));
        c.bottom = i32::MIN;
        assert!(c.result_size().is_err(), "no overflow panic");
    }

    #[test]
    fn crop_validation() {
        let mut c = CropForm::new((200, 100));
        assert_eq!(c.rect().unwrap(), ssx_types::Rect::new(0, 0, 200, 100));
        c.x = 500;
        assert!(c.rect().unwrap_err().contains("outside"));
        c.x = -20;
        c.width = 50;
        assert!(c.rect().is_ok(), "partially overlapping rectangles are clipped by the engine");
        c.width = 0;
        assert!(c.rect().is_err());
    }

    #[test]
    fn cut_validation() {
        let mut c = CutForm::new((200, 100));
        assert_eq!(c.strip(), Ok((Axis::X, 90, 110)));
        c.axis = Axis::Y;
        c.end = 500;
        assert!(c.strip().unwrap_err().contains("past"));
        c.start = 0;
        c.end = 100;
        assert!(c.strip().unwrap_err().contains("whole"));
        c.start = 60;
        c.end = 40;
        assert!(c.strip().is_err());
        assert_eq!(c.len(), 100);
    }

    #[test]
    fn save_form_tracks_extension_and_validates() {
        let mut f = SaveForm::new(PathBuf::from("/tmp/a.png"), ExportSettings::default());
        assert_eq!(f.format, SaveFormat::Image(ImageFormat::Png));
        f.set_format(SaveFormat::Image(ImageFormat::Jpeg));
        assert_eq!(f.path, "/tmp/a.jpg");
        f.path = "/tmp/b.webp".into();
        f.sync_format_from_path();
        assert_eq!(f.format, SaveFormat::Image(ImageFormat::WebP));
        assert_eq!(f.target().unwrap(), PathBuf::from("/tmp/b.webp"));
        f.path = "  ".into();
        assert!(f.target().is_err());
        f.path = "/tmp/noext".into();
        assert!(f.target().unwrap_err().contains(".png"));
        f.jpeg_quality = 250;
        assert_eq!(f.settings().jpeg_quality, 100);
    }

    #[test]
    fn open_form_requires_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.png");
        std::fs::write(&file, b"x").unwrap();
        let mut f = OpenForm { path: format!("\"{}\"", file.display()) };
        assert_eq!(f.target().unwrap(), file, "surrounding quotes from a copied path are stripped");
        f.path = dir.path().display().to_string();
        assert!(f.target().is_err());
        f.path = String::new();
        assert!(f.target().is_err());
    }
}
