//! [`EditorDoc`]: one open document = an `EditorSession` plus everything the window needs to
//! know about it that the engine does not (file names, the dirty marker, the labelled history).
//!
//! **Dirty tracking.** The engine has no "saved" concept. A plain "history changed since save"
//! counter would still call the document dirty after undoing back to the saved state, which is
//! irritating in the unsaved-changes prompt, so we keep a cheap clone of the document at save
//! time (`Document` is `Arc`-based, so the clone shares the pixels) and compare on demand,
//! caching the answer per revision.

use std::{cell::Cell, path::{Path, PathBuf}};

use ssx_editor::{
    CursorHint, Document, EditorSession, SessionEvent, Tool, project,
};
use ssx_types::{Frame, Rect};

use crate::{
    history_log::HistoryLog,
    request::RunError,
};

/// What happened since the last [`EditorDoc::pump`], reduced to what the window reacts to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pumped {
    /// Image-space rectangles whose pixels changed.
    pub dirty: Vec<Rect>,
    /// The canvas size or base image changed.
    pub canvas_changed: bool,
    /// The engine switched tool (e.g. after cancelling).
    pub tool_changed: Option<Tool>,
    /// Text editing started (`true`) or ended (`false`).
    pub text_editing: Option<bool>,
    /// Text the engine wants on the system clipboard.
    pub clipboard_text: Option<String>,
    /// The engine wants clipboard text inserted.
    pub paste_text_requested: bool,
    /// The selection changed.
    pub selection_changed: bool,
    /// The history changed (a document edit happened).
    pub history_changed: bool,
    /// The cursor hint changed.
    pub cursor: Option<CursorHint>,
}

/// An open document.
pub struct EditorDoc {
    /// The engine session.
    pub session: EditorSession,
    /// The file the document was opened from (image or project).
    pub source: Option<PathBuf>,
    /// The `.ssxe` project this document was last saved to or loaded from.
    pub project_path: Option<PathBuf>,
    /// The image file this document was last exported to.
    pub image_path: Option<PathBuf>,
    /// Labelled undo history.
    pub log: HistoryLog,
    /// Name for the next global operation's history entry.
    pub pending_label: Option<String>,
    revision: u64,
    saved_revision: u64,
    saved_doc: Document,
    dirty_cache: Cell<Option<(u64, bool)>>,
}

impl std::fmt::Debug for EditorDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorDoc")
            .field("source", &self.source)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl EditorDoc {
    /// Wraps a session around `doc`, considered saved.
    pub fn new(doc: Document) -> Self {
        Self {
            saved_doc: doc.clone(),
            session: EditorSession::new(doc),
            source: None,
            project_path: None,
            image_path: None,
            log: HistoryLog::new(),
            pending_label: None,
            revision: 0,
            saved_revision: 0,
            dirty_cache: Cell::new(None),
        }
    }

    /// A document around captured pixels (unsaved: a fresh capture is worth keeping).
    pub fn from_frame(frame: Frame) -> Result<Self, RunError> {
        let doc = Document::new(frame).map_err(|e| RunError::BadImage(e.to_string()))?;
        let mut d = Self::new(doc);
        // A capture that was never written anywhere counts as having unsaved content.
        d.saved_revision = u64::MAX;
        Ok(d)
    }

    /// Opens an image file or an `.ssxe` project.
    pub fn open(path: &Path) -> Result<Self, RunError> {
        let open_err = |reason: String| RunError::Open { path: path.to_path_buf(), reason };
        let is_project =
            path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ssxe"));
        if is_project {
            let doc = project::load(path).map_err(|e| open_err(e.to_string()))?;
            let mut d = Self::new(doc);
            d.source = Some(path.to_path_buf());
            d.project_path = Some(path.to_path_buf());
            return Ok(d);
        }
        let bytes = std::fs::read(path).map_err(|e| open_err(e.to_string()))?;
        let frame = Frame::decode(&bytes).map_err(|e| open_err(e.to_string()))?;
        let mut d = Self::from_frame(frame).map_err(|e| open_err(e.to_string()))?;
        // An untouched, just-opened file has nothing to lose.
        d.saved_revision = 0;
        d.source = Some(path.to_path_buf());
        d.image_path = Some(path.to_path_buf());
        Ok(d)
    }

    /// The document.
    pub fn doc(&self) -> &Document {
        self.session.document()
    }

    /// Bumped on every document edit.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// `true` when the document differs from what was last saved.
    pub fn is_dirty(&self) -> bool {
        if self.revision == self.saved_revision {
            return false;
        }
        if let Some((rev, d)) = self.dirty_cache.get() {
            if rev == self.revision {
                return d;
            }
        }
        let d = self.saved_revision == u64::MAX || *self.session.document() != self.saved_doc;
        self.dirty_cache.set(Some((self.revision, d)));
        d
    }

    /// Records the current state as saved.
    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
        self.saved_doc = self.session.document().clone();
        self.dirty_cache.set(None);
    }

    /// The file name shown in the title bar.
    pub fn display_name(&self) -> String {
        self.project_path
            .as_ref()
            .or(self.image_path.as_ref())
            .or(self.source.as_ref())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map_or_else(|| "Untitled".to_owned(), str::to_owned)
    }

    /// Window title: `name* - ssx editor`.
    pub fn title(&self) -> String {
        format!("{}{} - ssx editor", self.display_name(), if self.is_dirty() { "*" } else { "" })
    }

    /// Drains the session's events into a [`Pumped`] summary, keeps the dirty marker and the
    /// history labels in step. Call after anything that may have touched the session.
    pub fn pump(&mut self) -> Pumped {
        let mut out = Pumped::default();
        for ev in self.session.take_events() {
            match ev {
                SessionEvent::Dirty(r) => out.dirty.push(r),
                SessionEvent::CanvasChanged => out.canvas_changed = true,
                SessionEvent::SelectionChanged => out.selection_changed = true,
                SessionEvent::ToolChanged(t) => out.tool_changed = Some(t),
                SessionEvent::CursorChanged(c) => out.cursor = Some(c),
                SessionEvent::HistoryChanged { .. } => {
                    out.history_changed = true;
                    self.revision += 1;
                }
                SessionEvent::TextEditing(on) => out.text_editing = Some(on),
                SessionEvent::SetClipboardText(t) => out.clipboard_text = Some(t),
                SessionEvent::PasteTextRequested => out.paste_text_requested = true,
                SessionEvent::OverlayChanged => {}
            }
        }
        if out.history_changed {
            let label = self.pending_label.take();
            self.log.sync_session(&self.session, label);
        }
        out
    }

    /// Undoes one step, keeping the labels aligned.
    pub fn undo(&mut self) -> Pumped {
        if self.session.can_undo() {
            self.session.undo();
            self.log.note_undo();
        }
        self.pump()
    }

    /// Redoes one step.
    pub fn redo(&mut self) -> Pumped {
        if self.session.can_redo() {
            self.session.redo();
            self.log.note_redo();
        }
        self.pump()
    }

    /// Undoes `n` steps in one go (history dropdown), merging the summaries.
    pub fn undo_steps(&mut self, n: usize) -> Pumped {
        let mut all = Pumped::default();
        for _ in 0..n {
            merge(&mut all, self.undo());
        }
        all
    }

    /// Redoes `n` steps.
    pub fn redo_steps(&mut self, n: usize) -> Pumped {
        let mut all = Pumped::default();
        for _ in 0..n {
            merge(&mut all, self.redo());
        }
        all
    }
}

/// Folds `b` into `a`.
pub fn merge(a: &mut Pumped, b: Pumped) {
    a.dirty.extend(b.dirty);
    a.canvas_changed |= b.canvas_changed;
    a.tool_changed = b.tool_changed.or(a.tool_changed);
    a.text_editing = b.text_editing.or(a.text_editing);
    a.clipboard_text = b.clipboard_text.or_else(|| a.clipboard_text.take());
    a.paste_text_requested |= b.paste_text_requested;
    a.selection_changed |= b.selection_changed;
    a.history_changed |= b.history_changed;
    a.cursor = b.cursor.or(a.cursor);
}

#[cfg(test)]
mod tests {
    use ssx_editor::{Modifiers, PointF};
    use ssx_imgfx::solid_frame;

    use super::*;

    fn doc() -> EditorDoc {
        let mut d = EditorDoc::from_frame(solid_frame(120, 80, [200, 200, 200, 255])).unwrap();
        d.mark_saved();
        d
    }

    fn draw(d: &mut EditorDoc) {
        d.session.set_tool(Tool::Rectangle);
        d.session.pointer_down(PointF::new(10.0, 10.0), Modifiers::NONE, None);
        d.session.pointer_move(PointF::new(60.0, 50.0), Modifiers::NONE, None);
        d.session.pointer_up(PointF::new(60.0, 50.0), Modifiers::NONE);
    }

    #[test]
    fn dirty_follows_edits_and_returns_clean_after_undo() {
        let mut d = doc();
        assert!(!d.is_dirty());
        draw(&mut d);
        let p = d.pump();
        assert!(p.history_changed && !p.dirty.is_empty());
        assert!(d.is_dirty());
        assert!(d.title().contains("Untitled*"));
        d.undo();
        assert!(!d.is_dirty(), "undoing back to the saved state is clean again");
        d.redo();
        assert!(d.is_dirty());
        d.mark_saved();
        assert!(!d.is_dirty());
        assert!(!d.title().contains('*'));
    }

    #[test]
    fn a_fresh_capture_is_unsaved_until_written() {
        let d = EditorDoc::from_frame(solid_frame(10, 10, [1; 4])).unwrap();
        assert!(d.is_dirty());
        let mut d = d;
        d.mark_saved();
        assert!(!d.is_dirty());
    }

    #[test]
    fn history_labels_follow_undo_redo_and_steps() {
        let mut d = doc();
        draw(&mut d);
        d.pump();
        draw(&mut d);
        d.pump();
        assert_eq!(d.log.undo_len(), 2);
        d.undo_steps(2);
        assert_eq!(d.log.undo_len(), 0);
        assert_eq!(d.log.redo_len(), 2);
        assert!(d.doc().objects().is_empty());
        d.redo_steps(1);
        assert_eq!(d.doc().objects().len(), 1);
        assert_eq!(d.log.redo_len(), 1);
    }

    #[test]
    fn pending_label_names_the_next_global_op() {
        let mut d = doc();
        d.pending_label = Some("Rotate".into());
        d.session.orient(ssx_editor::object::Orient::Rotate90).unwrap();
        let p = d.pump();
        assert!(p.canvas_changed);
        assert_eq!(d.log.undo_labels().next(), Some("Rotate"));
        assert!(d.pending_label.is_none());
    }

    #[test]
    fn open_reports_missing_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let err = EditorDoc::open(&dir.path().join("nope.png")).unwrap_err();
        assert!(matches!(err, RunError::Open { .. }));
        let bad = dir.path().join("bad.png");
        std::fs::write(&bad, b"not an image").unwrap();
        assert!(matches!(EditorDoc::open(&bad).unwrap_err(), RunError::Open { .. }));
        let badp = dir.path().join("bad.ssxe");
        std::fs::write(&badp, b"{}").unwrap();
        let e = EditorDoc::open(&badp).unwrap_err().to_string();
        assert!(e.contains("bad.ssxe"), "{e}");
    }

    #[test]
    fn open_image_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        solid_frame(30, 20, [9, 9, 9, 255]).save(&png).unwrap();
        let mut d = EditorDoc::open(&png).unwrap();
        assert!(!d.is_dirty(), "an untouched opened file is clean");
        assert_eq!(d.display_name(), "a.png");
        draw(&mut d);
        d.pump();
        assert!(d.is_dirty());
        let proj = dir.path().join("a.ssxe");
        crate::export::write_project(d.doc(), &proj).unwrap();
        let p = EditorDoc::open(&proj).unwrap();
        assert_eq!(p.doc().objects().len(), 1);
        assert_eq!(p.project_path.as_deref(), Some(proj.as_path()));
        assert!(!p.is_dirty());
    }

    #[test]
    fn merge_combines_summaries() {
        let mut a = Pumped { dirty: vec![Rect::new(0, 0, 1, 1)], ..Pumped::default() };
        let b = Pumped {
            dirty: vec![Rect::new(1, 1, 1, 1)],
            canvas_changed: true,
            tool_changed: Some(Tool::Line),
            clipboard_text: Some("x".into()),
            ..Pumped::default()
        };
        merge(&mut a, b);
        assert_eq!(a.dirty.len(), 2);
        assert!(a.canvas_changed);
        assert_eq!(a.tool_changed, Some(Tool::Line));
        assert_eq!(a.clipboard_text.as_deref(), Some("x"));
    }
}
