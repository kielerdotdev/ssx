//! ssx-editor: a UI-toolkit-agnostic image editor engine.
//!
//! See the crate README for the architecture and GUI integration notes.

#![forbid(unsafe_code)]

pub mod doc;
pub mod geom;
pub mod object;
pub mod project;
pub mod shapes;
pub mod style;
pub mod text;

pub use doc::{Canvas, DocError, Document, Padding};
pub use geom::{Color, PointF, RectF};
pub use object::{Object, ObjectId, ObjectKind};
pub use style::{DashStyle, Fill, Style};
