//! Names for the undo history, so the toolbar can offer "Undo: Add rectangle".
//!
//! `ssx_editor::History` deliberately exposes only counts and the newest command; it does not
//! keep human-readable names. The UI therefore keeps a parallel list of labels and reconciles
//! it with the history's lengths after every interaction ([`HistoryLog::sync`]). The
//! reconciliation is heuristic only in one case: when the history hits its entry limit and
//! silently drops its oldest step the labels shift by one until the next undo. That costs a
//! wrong label in a dropdown, never a wrong undo, because undo itself is the engine's.

use ssx_editor::{Command, EditorSession, Object};

/// Human-readable labels of the undo and redo stacks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryLog {
    /// Oldest first.
    undo: Vec<String>,
    /// Next-to-redo first.
    redo: Vec<String>,
}

fn nice(name: &str) -> String {
    let mut s = name.replace('_', " ");
    if let Some(c) = s.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    s
}

fn describe_objects(objs: &[&Object]) -> String {
    match objs {
        [one] => nice(one.kind.name()).to_lowercase(),
        many => format!("{} objects", many.len()),
    }
}

/// A short label for a command.
pub fn label_of(cmd: &Command) -> String {
    match cmd {
        Command::Add { items, .. } => {
            let objs: Vec<&Object> = items.iter().map(|(_, o)| o).collect();
            format!("Add {}", describe_objects(&objs))
        }
        Command::Remove { items } => {
            let objs: Vec<&Object> = items.iter().map(|(_, o)| o).collect();
            format!("Delete {}", describe_objects(&objs))
        }
        Command::Modify { before, after } => {
            let style = before.iter().zip(after).any(|(b, a)| b.style != a.style);
            let text = before.iter().zip(after).any(|(b, a)| {
                b.kind.text_content().map(|c| &c.text) != a.kind.text_content().map(|c| &c.text)
            });
            let visibility = before.iter().zip(after).any(|(b, a)| {
                b.visible != a.visible || b.locked != a.locked || b.group != a.group
            });
            if text {
                "Edit text".into()
            } else if style {
                "Change style".into()
            } else if visibility {
                "Change visibility".into()
            } else {
                "Move or resize".into()
            }
        }
        Command::Reorder { .. } => "Change order".into(),
        Command::SetDocument { before, after } => {
            if before.image_size() != after.image_size() {
                "Change image size".into()
            } else {
                "Change image".into()
            }
        }
        Command::Batch(_) => "Several changes".into(),
    }
}

impl HistoryLog {
    /// An empty log (fresh document).
    pub fn new() -> Self {
        Self::default()
    }

    /// Labels of undoable steps, newest first (the order the dropdown lists them).
    pub fn undo_labels(&self) -> impl Iterator<Item = &str> {
        self.undo.iter().rev().map(String::as_str)
    }

    /// Labels of redoable steps, next-to-redo first.
    pub fn redo_labels(&self) -> impl Iterator<Item = &str> {
        self.redo.iter().map(String::as_str)
    }

    /// Number of undo labels.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of redo labels.
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Call after a successful `undo()`: the newest undo label becomes the next redo label.
    pub fn note_undo(&mut self) {
        if let Some(l) = self.undo.pop() {
            self.redo.insert(0, l);
        }
    }

    /// Call after a successful `redo()`.
    pub fn note_redo(&mut self) {
        if !self.redo.is_empty() {
            let l = self.redo.remove(0);
            self.undo.push(l);
        }
    }

    /// Reconciles with the engine after any *other* change. `top` names the newest command
    /// (`None` when the stack is empty); `override_label` beats it for new entries (used for
    /// global operations, whose command alone cannot say "Rotate").
    pub fn sync(
        &mut self,
        undo_len: usize,
        redo_len: usize,
        top: Option<String>,
        override_label: Option<String>,
    ) {
        if redo_len == 0 {
            self.redo.clear();
        }
        let label = override_label.or(top);
        match undo_len.cmp(&self.undo.len()) {
            std::cmp::Ordering::Greater => {
                while self.undo.len() + 1 < undo_len {
                    self.undo.push("Edit".into());
                }
                self.undo.push(label.unwrap_or_else(|| "Edit".into()));
            }
            std::cmp::Ordering::Equal => {
                // A coalesced edit (typing, dragging, a slider) keeps its step but its
                // description may have changed, e.g. "Add text" -> "Edit text".
                if let (Some(l), Some(last)) = (label, self.undo.last_mut()) {
                    if !last.starts_with("Add ") || l.starts_with("Add ") {
                        *last = l;
                    }
                }
            }
            std::cmp::Ordering::Less => self.undo.truncate(undo_len),
        }
        self.redo.truncate(redo_len);
    }

    /// Forgets everything (new document).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Reconciles against a live session (see [`HistoryLog::sync`]).
    pub fn sync_session(&mut self, s: &EditorSession, override_label: Option<String>) {
        let h = s.history();
        let top = h.peek_undo().map(label_of);
        self.sync(h.undo_len(), h.redo_len(), top, override_label);
    }
}

#[cfg(test)]
mod tests {
    use ssx_editor::{Modifiers, PointF, Tool};
    use ssx_imgfx::solid_frame;

    use super::*;

    fn session() -> EditorSession {
        EditorSession::from_frame(solid_frame(200, 150, [200, 200, 200, 255])).unwrap()
    }

    fn drag(s: &mut EditorSession, tool: Tool, a: (f32, f32), b: (f32, f32)) {
        s.set_tool(tool);
        s.pointer_down(PointF::new(a.0, a.1), Modifiers::NONE, None);
        s.pointer_move(PointF::new(b.0, b.1), Modifiers::NONE, None);
        s.pointer_up(PointF::new(b.0, b.1), Modifiers::NONE);
    }

    #[test]
    fn tracks_a_session_through_edits_undo_and_redo() {
        let mut s = session();
        let mut log = HistoryLog::new();
        drag(&mut s, Tool::Rectangle, (10.0, 10.0), (80.0, 60.0));
        log.sync_session(&s, None);
        drag(&mut s, Tool::Ellipse, (90.0, 10.0), (150.0, 60.0));
        log.sync_session(&s, None);
        assert_eq!(log.undo_labels().collect::<Vec<_>>(), ["Add ellipse", "Add rectangle"]);

        s.undo();
        log.note_undo();
        assert_eq!(log.undo_len(), 1);
        assert_eq!(log.redo_labels().collect::<Vec<_>>(), ["Add ellipse"]);
        s.redo();
        log.note_redo();
        assert_eq!(log.redo_len(), 0);
        assert_eq!(log.undo_len(), 2);

        // A new edit after an undo clears redo.
        s.undo();
        log.note_undo();
        drag(&mut s, Tool::Line, (5.0, 5.0), (100.0, 100.0));
        log.sync_session(&s, None);
        assert_eq!(log.redo_len(), 0);
        assert_eq!(log.undo_labels().next(), Some("Add line"));
        assert_eq!(log.undo_len(), s.history().undo_len());
    }

    #[test]
    fn override_names_global_operations() {
        let mut s = session();
        let mut log = HistoryLog::new();
        s.orient(ssx_editor::object::Orient::Rotate90).unwrap();
        log.sync_session(&s, Some("Rotate 90 degrees".into()));
        assert_eq!(log.undo_labels().next(), Some("Rotate 90 degrees"));
        // Without an override the command still gets a sensible generic name.
        s.resize(50, 50, ssx_imgfx::ResizeFilter::Bilinear).unwrap();
        log.sync_session(&s, None);
        assert_eq!(log.undo_labels().next(), Some("Change image size"));
    }

    #[test]
    fn discarded_drag_leaves_no_label() {
        let mut s = session();
        let mut log = HistoryLog::new();
        s.set_tool(Tool::Rectangle);
        s.pointer_down(PointF::new(10.0, 10.0), Modifiers::NONE, None);
        s.pointer_move(PointF::new(60.0, 60.0), Modifiers::NONE, None);
        log.sync_session(&s, None);
        assert_eq!(log.undo_len(), 1);
        s.cancel_drag();
        log.sync_session(&s, None);
        assert_eq!(log.undo_len(), 0);
    }

    #[test]
    fn coalesced_edits_keep_one_entry() {
        let mut s = session();
        let mut log = HistoryLog::new();
        drag(&mut s, Tool::Rectangle, (10.0, 10.0), (80.0, 60.0));
        log.sync_session(&s, None);
        for w in [5.0, 6.0, 7.0, 8.0] {
            crate::props::apply(&mut s, &crate::props::PropEdit::StrokeWidth(w));
            log.sync_session(&s, None);
        }
        assert_eq!(log.undo_len(), s.history().undo_len());
        assert!(log.undo_len() <= 2);
    }

    #[test]
    fn deleting_and_ordering_are_named() {
        let mut s = session();
        let mut log = HistoryLog::new();
        drag(&mut s, Tool::Rectangle, (10.0, 10.0), (80.0, 60.0));
        log.sync_session(&s, None);
        s.delete_selection();
        log.sync_session(&s, None);
        assert_eq!(log.undo_labels().next(), Some("Delete rectangle"));
    }

    #[test]
    fn labels_are_nice() {
        assert_eq!(nice("freehand_arrow"), "Freehand arrow");
        assert_eq!(nice(""), "");
    }
}
