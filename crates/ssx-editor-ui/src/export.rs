//! Turning the edited document into files: PNG/JPEG/WebP/BMP images and `.ssxe` projects.
//!
//! Two details worth knowing: JPEG has no alpha, so transparent pixels are composited on white
//! first (encoding straight from RGBA would turn them black); and every write goes through a
//! temporary file plus rename so a crash or full disk never leaves a half-written file where a
//! good one used to be.

use std::path::{Path, PathBuf};

use ssx_editor::{Document, project};
use ssx_types::{EncodeOptions, Frame, ImageFormat};

/// What a file save produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFormat {
    /// A flattened raster image.
    Image(ImageFormat),
    /// The editable `.ssxe` project.
    Project,
}

impl SaveFormat {
    /// File extension without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            SaveFormat::Image(f) => f.extension(),
            SaveFormat::Project => "ssxe",
        }
    }

    /// Menu name.
    pub fn label(self) -> &'static str {
        match self {
            SaveFormat::Image(ImageFormat::Png) => "PNG",
            SaveFormat::Image(ImageFormat::Jpeg) => "JPEG",
            SaveFormat::Image(ImageFormat::WebP) => "WebP (lossless)",
            SaveFormat::Image(ImageFormat::Bmp) => "BMP",
            SaveFormat::Project => "ssx project (editable)",
        }
    }

    /// Every format the Save dialog offers.
    pub const ALL: [SaveFormat; 5] = [
        SaveFormat::Image(ImageFormat::Png),
        SaveFormat::Image(ImageFormat::Jpeg),
        SaveFormat::Image(ImageFormat::WebP),
        SaveFormat::Image(ImageFormat::Bmp),
        SaveFormat::Project,
    ];

    /// The format implied by a path's extension.
    pub fn from_path(path: &Path) -> Option<SaveFormat> {
        let ext = path.extension()?.to_str()?;
        if ext.eq_ignore_ascii_case("ssxe") {
            return Some(SaveFormat::Project);
        }
        ImageFormat::from_extension(ext).map(SaveFormat::Image)
    }
}

/// Errors while writing or reading files.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Filesystem trouble.
    #[error("cannot write {path}: {source}")]
    Io {
        /// Target file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The image could not be encoded.
    #[error("cannot encode the image: {0}")]
    Encode(#[from] ssx_types::FrameError),
    /// The project could not be written.
    #[error("cannot write the project: {0}")]
    Project(#[from] project::ProjectError),
    /// The extension is not one we can write.
    #[error("do not know how to save a .{0} file; use png, jpg, webp, bmp or ssxe")]
    UnknownFormat(String),
}

/// Encoder options chosen in the Save dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSettings {
    /// JPEG quality 1-100.
    pub jpeg_quality: u8,
    /// Fast PNG compression.
    pub png_fast: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self { jpeg_quality: 90, png_fast: false }
    }
}

/// Composites transparent pixels over `bg` (opaque), for formats without alpha.
pub fn flatten_on(frame: &Frame, bg: [u8; 3]) -> Frame {
    let mut out = frame.clone();
    for px in out.data_mut().chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a == 255 {
            continue;
        }
        for (c, b) in px.iter_mut().zip(bg) {
            *c = ((u32::from(*c) * a + u32::from(b) * (255 - a) + 127) / 255) as u8;
        }
        px[3] = 255;
    }
    out
}

/// Encodes `frame` for `format`.
pub fn encode(
    frame: &Frame,
    format: ImageFormat,
    settings: ExportSettings,
) -> Result<Vec<u8>, ExportError> {
    let opts = EncodeOptions {
        format,
        jpeg_quality: settings.jpeg_quality.clamp(1, 100),
        png_fast: settings.png_fast,
    };
    if format == ImageFormat::Jpeg {
        return Ok(flatten_on(frame, [255, 255, 255]).encode(opts)?);
    }
    Ok(frame.encode(opts)?)
}

/// Writes `bytes` to `path` via a temporary sibling file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    let io = |source| ExportError::Io { path: path.to_path_buf(), source };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".ssx-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

/// Writes the flattened `frame` as an image, choosing the format from the extension.
pub fn write_image(
    frame: &Frame,
    path: &Path,
    settings: ExportSettings,
) -> Result<(), ExportError> {
    match SaveFormat::from_path(path) {
        Some(SaveFormat::Image(f)) => atomic_write(path, &encode(frame, f, settings)?),
        Some(SaveFormat::Project) | None => Err(ExportError::UnknownFormat(
            path.extension().and_then(|e| e.to_str()).unwrap_or("").to_owned(),
        )),
    }
}

/// Writes the editable project.
pub fn write_project(doc: &Document, path: &Path) -> Result<(), ExportError> {
    atomic_write(path, project::to_json(doc)?.as_bytes())
}

/// Renders the document exactly as exported (1x, everything included).
pub fn flatten(doc: &Document) -> Frame {
    ssx_editor::render(doc, &ssx_editor::RenderOptions::default())
}

/// A default file name for "Save as": `<stem>-edited.png` next to the source, or a timestamped
/// `screenshot.png` in `dir` when the image has no file.
pub fn suggest_save_path(source: Option<&Path>, dir: Option<&Path>) -> PathBuf {
    if let Some(src) = source {
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let stem = stem.strip_suffix("-edited").unwrap_or(stem);
        let parent = src.parent().map(Path::to_path_buf).unwrap_or_default();
        return parent.join(format!("{stem}-edited.png"));
    }
    dir.map(Path::to_path_buf).unwrap_or_default().join("screenshot.png")
}

/// Changes a path's extension to match `format`.
pub fn with_format(path: &Path, format: SaveFormat) -> PathBuf {
    path.with_extension(format.extension())
}

#[cfg(test)]
mod tests {
    use ssx_editor::{EditorSession, Modifiers, PointF, Tool};
    use ssx_imgfx::solid_frame;

    use super::*;

    #[test]
    fn formats_from_extensions() {
        for (p, f) in [
            ("a.png", Some(SaveFormat::Image(ImageFormat::Png))),
            ("a.JPG", Some(SaveFormat::Image(ImageFormat::Jpeg))),
            ("a.jpeg", Some(SaveFormat::Image(ImageFormat::Jpeg))),
            ("a.webp", Some(SaveFormat::Image(ImageFormat::WebP))),
            ("a.bmp", Some(SaveFormat::Image(ImageFormat::Bmp))),
            ("a.ssxe", Some(SaveFormat::Project)),
            ("a.txt", None),
            ("noext", None),
        ] {
            assert_eq!(SaveFormat::from_path(Path::new(p)), f, "{p}");
        }
        for f in SaveFormat::ALL {
            assert!(!f.label().is_empty());
            assert_eq!(SaveFormat::from_path(&Path::new("x").with_extension(f.extension())), Some(f));
        }
    }

    #[test]
    fn jpeg_flattens_transparency_onto_white() {
        let f = solid_frame(8, 8, [0, 0, 0, 0]);
        let bytes = encode(&f, ImageFormat::Jpeg, ExportSettings::default()).unwrap();
        let back = Frame::decode(&bytes).unwrap();
        let px = &back.data()[0..4];
        assert!(px[0] > 250 && px[1] > 250 && px[2] > 250, "{px:?}");
        // Half transparent black over white is mid grey.
        let half = flatten_on(&solid_frame(1, 1, [0, 0, 0, 128]), [255; 3]);
        assert!((i32::from(half.data()[0]) - 127).abs() <= 1);
        assert_eq!(half.data()[3], 255);
    }

    #[test]
    fn png_round_trips_pixels_exactly() {
        let mut s = EditorSession::from_frame(solid_frame(64, 48, [10, 20, 30, 255])).unwrap();
        s.set_tool(Tool::Rectangle);
        s.pointer_down(PointF::new(5.0, 5.0), Modifiers::NONE, None);
        s.pointer_up(PointF::new(40.0, 30.0), Modifiers::NONE);
        let frame = flatten(s.document());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.png");
        write_image(&frame, &p, ExportSettings::default()).unwrap();
        let back = Frame::decode(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back, frame);
        assert!(!dir.path().join("out.png.ssx-tmp").exists());
    }

    #[test]
    fn jpeg_quality_changes_size() {
        let mut data = Vec::new();
        for i in 0..(128 * 128u32) {
            data.extend_from_slice(&[(i * 7) as u8, (i * 13 >> 3) as u8, (i % 251) as u8, 255]);
        }
        let f = Frame::from_rgba8(128, 128, data).unwrap();
        let lo = encode(&f, ImageFormat::Jpeg, ExportSettings { jpeg_quality: 10, png_fast: false }).unwrap();
        let hi = encode(&f, ImageFormat::Jpeg, ExportSettings { jpeg_quality: 95, png_fast: false }).unwrap();
        assert!(lo.len() < hi.len());
    }

    #[test]
    fn unknown_extension_is_a_clear_error() {
        let f = solid_frame(2, 2, [1; 4]);
        let dir = tempfile::tempdir().unwrap();
        let err = write_image(&f, &dir.path().join("x.tiff"), ExportSettings::default()).unwrap_err();
        assert!(matches!(err, ExportError::UnknownFormat(ref e) if e == "tiff"));
        assert!(err.to_string().contains("png"));
        let err = write_image(&f, &dir.path().join("x.ssxe"), ExportSettings::default()).unwrap_err();
        assert!(matches!(err, ExportError::UnknownFormat(_)));
    }

    #[test]
    fn io_errors_name_the_path() {
        let f = solid_frame(2, 2, [1; 4]);
        let err = write_image(&f, Path::new("/nonexistent-dir-xyz/a.png"), ExportSettings::default()).unwrap_err();
        assert!(err.to_string().contains("/nonexistent-dir-xyz/a.png"), "{err}");
    }

    #[test]
    fn project_round_trip_keeps_objects() {
        let mut s = EditorSession::from_frame(solid_frame(64, 48, [10, 20, 30, 255])).unwrap();
        s.set_tool(Tool::Ellipse);
        s.pointer_down(PointF::new(5.0, 5.0), Modifiers::NONE, None);
        s.pointer_up(PointF::new(40.0, 30.0), Modifiers::NONE);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p.ssxe");
        write_project(s.document(), &p).unwrap();
        let back = project::load(&p).unwrap();
        assert_eq!(&back, s.document());
    }

    #[test]
    fn suggested_names() {
        assert_eq!(
            suggest_save_path(Some(Path::new("/a/b/shot.jpg")), None),
            PathBuf::from("/a/b/shot-edited.png")
        );
        assert_eq!(
            suggest_save_path(Some(Path::new("/a/b/shot-edited.png")), None),
            PathBuf::from("/a/b/shot-edited.png"),
            "no -edited-edited"
        );
        assert_eq!(
            suggest_save_path(None, Some(Path::new("/pics"))),
            PathBuf::from("/pics/screenshot.png")
        );
        assert_eq!(
            with_format(Path::new("/a/b.png"), SaveFormat::Image(ImageFormat::Jpeg)),
            PathBuf::from("/a/b.jpg")
        );
    }
}
