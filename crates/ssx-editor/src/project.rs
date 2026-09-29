//! The `.ssxe` project format.
//!
//! A project is one JSON document:
//!
//! ```json
//! { "format": "ssxe", "version": 1, "minor": 0, "generator": "ssx-editor 0.1.0",
//!   "document": { "base": {"width": 640, "height": 480, "png": "<base64>"},
//!                 "canvas": {...}, "objects": [...], "next_id": 5, ... } }
//! ```
//!
//! * The base image is embedded as PNG (lossless, small, universally decodable).
//! * `version` is bumped only for **breaking** changes; readers refuse files with a higher
//!   `version` ([`ProjectError::TooNew`]) and run migrations for lower ones.
//! * `minor` is bumped for additive changes: every struct here tolerates unknown fields and
//!   missing fields (defaults), and unknown object kinds load as `ObjectKind::Unknown` and
//!   are written back verbatim, so a file round-trips through an older editor without losing
//!   the newer parts.

use std::{path::Path, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use ssx_types::{EncodeOptions, Frame, ImageFormat};

use crate::{
    doc::{Canvas, Document},
    object::Object,
};

/// The format version this build writes and fully understands.
pub const FORMAT_VERSION: u32 = 1;
/// The additive revision within [`FORMAT_VERSION`].
pub const FORMAT_MINOR: u32 = 0;
/// Magic value of the `format` field.
pub const FORMAT_NAME: &str = "ssxe";

/// Errors reading or writing a project.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The file is not valid JSON or does not match the schema.
    #[error("not a valid ssx project: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON is not an ssxe project.
    #[error("this is not an ssx editor project (format field is {0:?})")]
    WrongFormat(String),
    /// Written by an incompatible newer version.
    #[error(
        "this project was saved by a newer ssx (format version {found}, this build reads up to {supported}); update ssx to open it"
    )]
    TooNew {
        /// Version in the file.
        found: u32,
        /// Highest version this build understands.
        supported: u32,
    },
    /// Version 0 or otherwise unusable.
    #[error("unsupported project format version {0}")]
    Unsupported(u32),
    /// The embedded image could not be decoded/encoded.
    #[error("embedded image error: {0}")]
    Image(String),
    /// File access failed.
    #[error("project file i/o failed: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) fn to_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub(crate) fn from_base64(s: &str) -> Result<Vec<u8>, String> {
    STANDARD.decode(s).map_err(|e| format!("invalid base64: {e}"))
}

/// Encodes a frame as PNG (fast compression: project saves should not stall the UI).
pub(crate) fn encode_png(frame: &Frame) -> Result<Vec<u8>, String> {
    let opts = EncodeOptions { format: ImageFormat::Png, jpeg_quality: 90, png_fast: true };
    frame.encode(opts).map_err(|e| e.to_string())
}

pub(crate) fn decode_png(bytes: &[u8]) -> Result<Frame, String> {
    Frame::decode(bytes).map_err(|e| e.to_string())
}

#[derive(Serialize, Deserialize)]
struct BaseRepr {
    width: u32,
    height: u32,
    /// Base64 PNG; empty for a 0×0 image.
    #[serde(default)]
    png: String,
}

#[derive(Serialize, Deserialize)]
struct DocumentRepr {
    base: BaseRepr,
    #[serde(default)]
    canvas: Canvas,
    #[serde(default)]
    objects: Vec<Object>,
    #[serde(default = "one")]
    next_id: u64,
    #[serde(default = "one32")]
    step_start: u32,
}

fn one() -> u64 {
    1
}
fn one32() -> u32 {
    1
}

impl Serialize for Document {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let png = if self.base.width() == 0 || self.base.height() == 0 {
            String::new()
        } else {
            to_base64(&encode_png(&self.base).map_err(serde::ser::Error::custom)?)
        };
        // Serialise through a borrowed mirror to avoid cloning objects.
        #[derive(Serialize)]
        struct Out<'a> {
            base: &'a BaseRepr,
            canvas: &'a Canvas,
            objects: &'a [Object],
            next_id: u64,
            step_start: u32,
        }
        let base = BaseRepr { width: self.base.width(), height: self.base.height(), png };
        Out {
            base: &base,
            canvas: &self.canvas,
            objects: &self.objects,
            next_id: self.next_id,
            step_start: self.step_start,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = DocumentRepr::deserialize(d)?;
        let base = if r.base.png.is_empty() {
            ssx_imgfx::solid_frame(r.base.width, r.base.height, [0; 4])
        } else {
            let bytes = from_base64(&r.base.png).map_err(serde::de::Error::custom)?;
            let f = decode_png(&bytes).map_err(serde::de::Error::custom)?;
            if f.width() != r.base.width || f.height() != r.base.height {
                return Err(serde::de::Error::custom(format!(
                    "base image is {}x{} but the header says {}x{}",
                    f.width(),
                    f.height(),
                    r.base.width,
                    r.base.height
                )));
            }
            f
        };
        let max_id = r.objects.iter().map(|o| o.id.0).max().unwrap_or(0);
        let mut doc = Document::from_arc(Arc::new(base));
        doc.canvas = r.canvas;
        doc.objects = r.objects;
        doc.next_id = r.next_id.max(max_id + 1);
        doc.step_start = r.step_start;
        Ok(doc)
    }
}

#[derive(Serialize)]
struct FileOut<'a> {
    format: &'static str,
    version: u32,
    minor: u32,
    generator: String,
    document: &'a Document,
}

/// Serialises a document as `.ssxe` JSON text.
pub fn to_json(doc: &Document) -> Result<String, ProjectError> {
    Ok(serde_json::to_string(&FileOut {
        format: FORMAT_NAME,
        version: FORMAT_VERSION,
        minor: FORMAT_MINOR,
        generator: format!("ssx-editor {}", env!("CARGO_PKG_VERSION")),
        document: doc,
    })?)
}

/// Migrates a parsed project of an older `version` up to [`FORMAT_VERSION`] in place.
/// Version 1 is the first released format, so there is nothing to migrate yet; the hook
/// exists so future breaking changes have one obvious home (and a test fixture per version).
fn migrate(_value: &mut serde_json::Value, from: u32) -> Result<(), ProjectError> {
    match from {
        FORMAT_VERSION => Ok(()),
        other => Err(ProjectError::Unsupported(other)),
    }
}

/// Parses `.ssxe` JSON text.
pub fn from_json(text: &str) -> Result<Document, ProjectError> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    let format = value.get("format").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    if format != FORMAT_NAME {
        return Err(ProjectError::WrongFormat(format));
    }
    let version =
        value.get("version").and_then(serde_json::Value::as_u64).unwrap_or(0).min(u64::from(u32::MAX))
            as u32;
    if version > FORMAT_VERSION {
        return Err(ProjectError::TooNew { found: version, supported: FORMAT_VERSION });
    }
    migrate(&mut value, version)?;
    let doc = value.get_mut("document").map(serde_json::Value::take).unwrap_or_default();
    Ok(serde_json::from_value(doc)?)
}

/// Writes a project file.
pub fn save(doc: &Document, path: impl AsRef<Path>) -> Result<(), ProjectError> {
    std::fs::write(path, to_json(doc)?)?;
    Ok(())
}

/// Reads a project file.
pub fn load(path: impl AsRef<Path>) -> Result<Document, ProjectError> {
    from_json(&std::fs::read_to_string(path)?)
}

impl Document {
    /// Serialises to `.ssxe` JSON. See [`crate::project`].
    pub fn to_json(&self) -> Result<String, ProjectError> {
        to_json(self)
    }

    /// Parses `.ssxe` JSON. See [`crate::project`].
    pub fn from_json(text: &str) -> Result<Document, ProjectError> {
        from_json(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        geom::{Color, PointF, RectF},
        object::{BoxShape, ObjectId, ObjectKind, StepShape},
        style::Style,
    };

    fn sample() -> Document {
        let mut px = Vec::new();
        for i in 0..(6 * 4) {
            px.extend_from_slice(&[i as u8 * 10, 3, 200, 255]);
        }
        let mut d = Document::new(Frame::from_rgba8(6, 4, px).unwrap()).unwrap();
        let id = d.alloc_id();
        d.insert_object(
            0,
            Object::new(
                id,
                Style { stroke: Color::rgb(1, 2, 3), ..Style::default() },
                ObjectKind::Rectangle(BoxShape { rect: RectF::new(0.5, 1.25, 3.0, 2.0), rotation: 0.3 }),
            ),
        );
        let id = d.alloc_id();
        d.insert_object(
            1,
            Object::new(id, Style::default(), ObjectKind::Step(StepShape { center: PointF::new(2.0, 2.0), ..StepShape::default() })),
        );
        d
    }

    #[test]
    fn round_trip_is_exact() {
        let d = sample();
        let json = d.to_json().unwrap();
        let back = Document::from_json(&json).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.to_json().unwrap(), json, "serialisation is stable");
    }

    #[test]
    fn rejects_wrong_format_and_future_versions() {
        assert!(matches!(Document::from_json("{}"), Err(ProjectError::WrongFormat(_))));
        assert!(matches!(Document::from_json("[1,2"), Err(ProjectError::Json(_))));
        let mut v: serde_json::Value = serde_json::from_str(&sample().to_json().unwrap()).unwrap();
        v["version"] = 99.into();
        let err = Document::from_json(&v.to_string()).unwrap_err();
        assert!(matches!(err, ProjectError::TooNew { found: 99, .. }), "{err}");
        assert!(err.to_string().contains("newer"));
        v["version"] = 0.into();
        assert!(matches!(Document::from_json(&v.to_string()), Err(ProjectError::Unsupported(0))));
    }

    #[test]
    fn newer_minor_with_unknown_fields_and_kinds_loads_and_preserves() {
        let mut v: serde_json::Value = serde_json::from_str(&sample().to_json().unwrap()).unwrap();
        v["minor"] = 7.into();
        v["from_the_future"] = serde_json::json!({"x": 1});
        v["document"]["shiny_new_setting"] = true.into();
        v["document"]["objects"][0]["style"]["glow"] = 5.into();
        v["document"]["objects"][0]["kind"]["sparkle"] = 1.into();
        v["document"]["objects"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": 50, "kind": {"type": "hologram", "depth": 3}}));
        let d = Document::from_json(&v.to_string()).unwrap();
        assert_eq!(d.objects().len(), 3);
        assert!(matches!(d.objects()[2].kind, ObjectKind::Unknown(_)));
        assert!(d.peek_next_id() > ObjectId(50), "ids of unknown objects are never reused");
        let again: serde_json::Value = serde_json::from_str(&d.to_json().unwrap()).unwrap();
        assert_eq!(again["document"]["objects"][2]["kind"]["type"], "hologram");
        assert_eq!(again["document"]["objects"][2]["kind"]["depth"], 3);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let d = sample();
        let mut v: serde_json::Value = serde_json::from_str(&d.to_json().unwrap()).unwrap();
        let doc = v["document"].as_object_mut().unwrap();
        doc.remove("canvas");
        doc.remove("step_start");
        v["document"]["objects"][0]["style"] = serde_json::json!({});
        let back = Document::from_json(&v.to_string()).unwrap();
        assert_eq!(back.step_start(), 1);
        assert_eq!(back.canvas().padding.left, 0);
        assert_eq!(back.objects()[0].style, Style::default());
    }

    #[test]
    fn corrupted_image_reports_error() {
        let mut v: serde_json::Value = serde_json::from_str(&sample().to_json().unwrap()).unwrap();
        v["document"]["base"]["png"] = "AAAA".into();
        assert!(Document::from_json(&v.to_string()).is_err());
        v["document"]["base"]["png"] = "!!!".into();
        assert!(Document::from_json(&v.to_string()).is_err());
        let mut v: serde_json::Value = serde_json::from_str(&sample().to_json().unwrap()).unwrap();
        v["document"]["base"]["width"] = 99.into();
        let e = Document::from_json(&v.to_string()).unwrap_err();
        assert!(e.to_string().contains("header says"), "{e}");
    }

    #[test]
    fn empty_base_round_trips() {
        let d = Document::new(ssx_imgfx::solid_frame(0, 0, [0; 4])).unwrap();
        let back = Document::from_json(&d.to_json().unwrap()).unwrap();
        assert_eq!(back.image_size(), (0, 0));
    }

    #[test]
    fn save_and_load_file() {
        let d = sample();
        let dir = std::env::temp_dir().join(format!("ssxe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ssxe");
        save(&d, &path).unwrap();
        assert_eq!(load(&path).unwrap(), d);
        assert!(load(dir.join("missing.ssxe")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
