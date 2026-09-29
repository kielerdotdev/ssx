//! ssx-editor: a UI-toolkit-agnostic image editor engine.
//!
//! See the crate README for the architecture and GUI integration notes.

#![forbid(unsafe_code)]

pub mod doc;
pub mod geom;
pub mod history;
pub mod input;
pub mod object;
pub mod project;
pub mod render;
pub mod session;
pub mod shapes;
pub mod style;
pub mod text;
pub mod tool;

pub use doc::{Canvas, DocError, Document, Padding};
pub use geom::{Color, PointF, RectF};
pub use object::{Object, ObjectId, ObjectKind};
pub use style::{DashStyle, Fill, Style};
pub use render::{BaseFilter, RenderOptions, Renderer, render};
pub use tool::{Preset, StyleMemory, Tool};
pub use history::{Command, History, HistoryLimits};
pub use input::{CursorHint, Handle, HandleKind, Key, Modifiers, Overlay, SessionEvent};
pub use session::EditorSession;
