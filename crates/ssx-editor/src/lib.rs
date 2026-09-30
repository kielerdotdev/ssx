//! ssx-editor: a UI-toolkit-agnostic image editor engine.
//!
//! | Module | What it is |
//! |---|---|
//! | [`doc`] | [`Document`]: base image + canvas + ordered objects, and the global operations (crop, cut-out, rotate...) |
//! | [`object`] | The annotation kinds, their geometry, hit-testing and structural transforms |
//! | [`style`], [`tool`] | Shared style, tools, and the per-tool "last used style" memory |
//! | [`render`] | `tiny-skia` renderer: pixel-exact 1× export, zoomed viewport rendering |
//! | [`text`] | Bundled-font text shaping, layout, caret geometry |
//! | [`history`] | Command-pattern undo/redo with coalescing and bounded memory |
//! | [`session`] | [`EditorSession`]: the interactive state machine a GUI drives |
//! | [`project`] | The versioned `.ssxe` JSON project format |
//!
//! ```
//! use ssx_editor::{EditorSession, Modifiers, PointF, RenderOptions, Tool};
//!
//! let frame = ssx_imgfx::solid_frame(200, 120, [240, 240, 240, 255]);
//! let mut s = EditorSession::from_frame(frame).unwrap();
//! s.set_tool(Tool::Rectangle);
//! s.pointer_down(PointF::new(20.0, 20.0), Modifiers::NONE, None);
//! s.pointer_move(PointF::new(120.0, 80.0), Modifiers::NONE, None);
//! s.pointer_up(PointF::new(120.0, 80.0), Modifiers::NONE);
//! let png_ready = s.render(&RenderOptions::default());
//! assert_eq!(png_ready.width(), 200);
//! s.undo();
//! assert!(s.document().objects().is_empty());
//! ```

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
pub use history::{Command, History, HistoryLimits};
pub use input::{CursorHint, Handle, HandleKind, Key, Modifiers, Overlay, SessionEvent};
pub use object::{Object, ObjectId, ObjectKind};
pub use render::{BaseFilter, RenderOptions, Renderer, render};
pub use session::EditorSession;
pub use style::{DashStyle, Fill, Style};
pub use tool::{Preset, StyleMemory, Tool};
