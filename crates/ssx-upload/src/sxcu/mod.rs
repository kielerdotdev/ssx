//! ShareX `.sxcu` custom uploader support.
//!
//! * [`model`]: parsing, migration, validation, serialisation of the file format.
//! * [`template`]: the `{function:arg}` template language (+ [`jsonpath`] for `{json:}`).
//! * [`engine`]: executes a definition as an [`crate::Uploader`].
//!
//! Behaviour is specified by ShareX's own source (`ShareX.UploadersLib/CustomUploader`);
//! `README.md` has the compatibility table including intentional deviations.

pub mod engine;
pub mod jsonpath;
pub mod model;
pub mod template;

pub use engine::SxcuUploader;
pub use model::{
    BodyType, CustomUploader, DestinationType, HttpMethod, SxcuError, ValidationError,
    ValidationReport,
};
pub use template::{Interaction, NonInteractive, Template, TemplateError, TemplateResponse};
