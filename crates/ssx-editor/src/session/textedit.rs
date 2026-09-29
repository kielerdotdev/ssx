//! Text editing model: caret, selection, IME-ready insert/commit, keyboard handling.
//!
//! The model is byte-offset based (always on char boundaries) and lives in the session while a
//! text object is being edited. Every edit is a `Modify` command with a `typing` coalescing
//! key, so a burst of typing is one undo step; moving the caret seals the run, so the next
//! keystroke starts a new step (matching what editors do).

use std::sync::Arc;

use super::{Drag, EditorSession};
use crate::{
    geom::{PointF, RectF},
    history::{CoalesceKey, Command},
    input::{CaretOverlay, Key, Modifiers, SessionEvent},
    object::{Object, ObjectId, ObjectKind},
    text::{LayoutRequest, TextEngine, TextLayout, balloon_text_origin},
};

/// Live text-editing state.
#[derive(Debug, Clone)]
pub(crate) struct TextEdit {
    pub id: ObjectId,
    pub caret: usize,
    pub anchor: usize,
    pub preferred_x: Option<f32>,
    pub preedit: Option<(String, Option<(usize, usize)>)>,
}

/// Public snapshot of the text-editing state (for the GUI's IME / status bar).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditState {
    /// The object being edited.
    pub id: ObjectId,
    /// Caret byte offset.
    pub caret: usize,
    /// Selection anchor byte offset (equals `caret` when nothing is selected).
    pub anchor: usize,
    /// Current text.
    pub text: String,
}

/// Recomputes the stored box of a text-bearing object from its content (auto width/height).
pub fn sync_text_rect(o: &mut Object, engine: &mut TextEngine) {
    match &mut o.kind {
        ObjectKind::Text(t) => {
            let pad = t.content.padding.max(0.0);
            let wrap = (!t.auto_width).then(|| (t.rect.w - 2.0 * pad).max(1.0));
            let l = engine.layout(&LayoutRequest::from_content(&t.content, wrap));
            t.rect.h = l.height + 2.0 * pad;
            if t.auto_width {
                t.rect.w = l.width.max(t.content.font.size * 0.3) + 2.0 * pad;
            }
        }
        ObjectKind::Balloon(b) => {
            let pad = b.content.padding.max(0.0);
            let wrap = Some((b.rect.w - 2.0 * pad).max(1.0));
            let l = engine.layout(&LayoutRequest::from_content(&b.content, wrap));
            let need = l.height + 2.0 * pad;
            if b.rect.h < need {
                b.rect.h = need;
            }
        }
        _ => {}
    }
}

struct Geom {
    layout: Arc<TextLayout>,
    origin: PointF,
    rotation: f32,
    pivot: PointF,
}

fn prev_char(s: &str, i: usize) -> usize {
    s[..i.min(s.len())].char_indices().next_back().map_or(0, |(p, _)| p)
}

fn next_char(s: &str, i: usize) -> usize {
    let i = i.min(s.len());
    s[i..].chars().next().map_or(s.len(), |c| i + c.len_utf8())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn prev_word(s: &str, i: usize) -> usize {
    let mut chars: Vec<(usize, char)> = s[..i.min(s.len())].char_indices().collect();
    while chars.last().is_some_and(|(_, c)| !is_word(*c) && *c != '\n') {
        chars.pop();
    }
    while chars.last().is_some_and(|(_, c)| is_word(*c)) {
        chars.pop();
    }
    chars.last().map_or(0, |(p, c)| p + c.len_utf8())
}

fn next_word(s: &str, i: usize) -> usize {
    let mut pos = i.min(s.len());
    let mut it = s[pos..].chars().peekable();
    while let Some(c) = it.peek().copied() {
        if is_word(c) {
            break;
        }
        pos += c.len_utf8();
        it.next();
    }
    while let Some(c) = it.peek().copied() {
        if !is_word(c) {
            break;
        }
        pos += c.len_utf8();
        it.next();
    }
    pos
}

impl EditorSession {
    /// Recomputes the stored size of a text-bearing object.
    pub(crate) fn sync_text(&mut self, o: &mut Object) {
        sync_text_rect(o, self.renderer.text_mut());
    }

    fn edit_geom(&mut self, id: ObjectId) -> Option<Geom> {
        let o = self.doc.object(id)?.clone();
        let engine = self.renderer.text_mut();
        match &o.kind {
            ObjectKind::Text(t) => {
                let pad = t.content.padding.max(0.0);
                let wrap = (!t.auto_width).then(|| (t.rect.w - 2.0 * pad).max(1.0));
                let layout = engine.layout(&LayoutRequest::from_content(&t.content, wrap));
                Some(Geom {
                    layout,
                    origin: PointF::new(t.rect.x + pad, t.rect.y + pad),
                    rotation: t.rotation,
                    pivot: t.rect.center(),
                })
            }
            ObjectKind::Balloon(b) => {
                let pad = b.content.padding.max(0.0);
                let layout = engine
                    .layout(&LayoutRequest::from_content(&b.content, Some((b.rect.w - 2.0 * pad).max(1.0))));
                let origin = balloon_text_origin(b, layout.height);
                Some(Geom { layout, origin, rotation: 0.0, pivot: b.rect.center() })
            }
            _ => None,
        }
    }

    /// Starts editing the text of `id` (a text or balloon object), caret at the end.
    pub fn begin_text_edit(&mut self, id: ObjectId) {
        self.begin_text_edit_inner(id, true);
    }

    pub(crate) fn begin_text_edit_inner(&mut self, id: ObjectId, seal: bool) {
        if self.text.as_ref().is_some_and(|t| t.id != id) {
            self.commit_text_edit();
        }
        let Some(o) = self.doc.object(id) else { return };
        let Some(c) = o.kind.text_content() else { return };
        let end = c.text.len();
        if seal {
            self.history.seal();
        }
        let was_editing = self.text.is_some();
        self.text = Some(TextEdit { id, caret: end, anchor: end, preferred_x: None, preedit: None });
        self.set_selection(vec![id]);
        if !was_editing {
            self.events.push(SessionEvent::TextEditing(true));
        }
        self.events.push(SessionEvent::OverlayChanged);
    }

    /// Finishes text editing. An object left empty is removed again.
    pub fn commit_text_edit(&mut self) {
        let Some(te) = self.text.take() else { return };
        self.events.push(SessionEvent::TextEditing(false));
        let empty = self
            .doc
            .object(te.id)
            .and_then(|o| o.kind.text_content())
            .is_some_and(|c| c.text.trim().is_empty());
        let is_text_tool_object = matches!(self.doc.object(te.id).map(|o| &o.kind), Some(ObjectKind::Text(_)));
        if empty && is_text_tool_object {
            let id = te.id;
            let discarded = self
                .history
                .discard_open(&mut self.doc, |c| matches!(c, Command::Add { items, .. } if items.iter().any(|(_, o)| o.id == id)));
            if discarded {
                self.after_change(None, false);
            } else if let Some(i) = self.doc.index_of(id) {
                let item = (i, self.doc.objects()[i].clone());
                self.exec(Command::Remove { items: vec![item] }, None);
            }
        }
        self.history.seal();
        self.events.push(SessionEvent::OverlayChanged);
    }

    /// Snapshot of the text-editing state, `None` when not editing.
    pub fn text_edit_state(&self) -> Option<TextEditState> {
        let te = self.text.as_ref()?;
        let text = self.doc.object(te.id)?.kind.text_content()?.text.clone();
        Some(TextEditState { id: te.id, caret: te.caret, anchor: te.anchor, text })
    }

    /// Tells the session what the system clipboard holds, for ctrl+V while editing text.
    pub fn set_text_clipboard(&mut self, text: impl Into<String>) {
        self.text_clip = text.into();
    }

    /// Inserts (IME commit or typed) text at the caret, replacing the selection. Ignored when
    /// no text is being edited. `\r\n` and `\r` are normalised to `\n`.
    pub fn text_insert(&mut self, s: &str) {
        let Some(te) = self.text.as_mut() else { return };
        te.preedit = None;
        if s.is_empty() {
            return;
        }
        let ins = s.replace("\r\n", "\n").replace('\r', "\n");
        let (id, caret, anchor) = (te.id, te.caret, te.anchor);
        let Some(cur) = self.doc.object(id).and_then(|o| o.kind.text_content()).map(|c| c.text.clone()) else {
            return;
        };
        let (a, b) = (caret.min(anchor).min(cur.len()), caret.max(anchor).min(cur.len()));
        let mut new = String::with_capacity(cur.len() + ins.len());
        new.push_str(&cur[..a]);
        new.push_str(&ins);
        new.push_str(&cur[b..]);
        self.apply_text(new, a + ins.len(), a + ins.len());
    }

    /// Sets (or clears, with `None`) the IME pre-edit string. It is drawn by the GUI at the
    /// caret and is *not* part of the document until committed via [`Self::text_insert`].
    pub fn ime_preedit(&mut self, preedit: Option<(String, Option<(usize, usize)>)>) {
        if let Some(te) = self.text.as_mut() {
            te.preedit = preedit.filter(|(s, _)| !s.is_empty());
            self.events.push(SessionEvent::OverlayChanged);
        }
    }

    fn apply_text(&mut self, text: String, caret: usize, anchor: usize) {
        let Some(id) = self.text.as_ref().map(|t| t.id) else { return };
        let Some(before) = self.doc.object(id).cloned() else { return };
        let mut after = before.clone();
        if let Some(c) = after.kind.text_content_mut() {
            c.text = text;
        }
        self.sync_text(&mut after);
        let key = CoalesceKey { kind: "typing", tag: id.0 };
        self.modify(vec![before], vec![after], Some(key));
        if let Some(te) = self.text.as_mut() {
            te.caret = caret;
            te.anchor = anchor;
            te.preferred_x = None;
        }
        self.events.push(SessionEvent::OverlayChanged);
    }

    fn set_caret(&mut self, caret: usize, extend: bool, keep_x: Option<f32>) {
        self.history.seal();
        if let Some(te) = self.text.as_mut() {
            te.caret = caret;
            if !extend {
                te.anchor = caret;
            }
            te.preferred_x = keep_x;
        }
        self.events.push(SessionEvent::OverlayChanged);
    }

    /// Clamps caret/anchor after an undo/redo changed the text under the editor.
    pub(crate) fn clamp_text_edit(&mut self) {
        let Some(te) = self.text.as_ref() else { return };
        let len = self
            .doc
            .object(te.id)
            .and_then(|o| o.kind.text_content())
            .map_or(0, |c| c.text.len());
        let text = self.doc.object(te.id).and_then(|o| o.kind.text_content()).map(|c| c.text.clone());
        if let (Some(te), Some(text)) = (self.text.as_mut(), text) {
            let snap = |mut i: usize| {
                i = i.min(len);
                while i > 0 && !text.is_char_boundary(i) {
                    i -= 1;
                }
                i
            };
            te.caret = snap(te.caret);
            te.anchor = snap(te.anchor);
        }
    }

    /// Handles a key while editing. Returns `true` if consumed.
    pub(crate) fn text_key(&mut self, key: Key, mods: Modifiers) -> bool {
        let Some(te) = self.text.clone() else { return false };
        let Some(cur) = self.doc.object(te.id).and_then(|o| o.kind.text_content()).map(|c| c.text.clone()) else {
            return false;
        };
        let (lo, hi) = (te.caret.min(te.anchor), te.caret.max(te.anchor));
        let has_sel = lo != hi;
        match key {
            Key::Escape => {
                self.commit_text_edit();
                true
            }
            Key::Enter => {
                if mods.shift {
                    self.text_insert("\n");
                } else {
                    self.commit_text_edit();
                }
                true
            }
            Key::Left | Key::Right => {
                let left = key == Key::Left;
                let target = if has_sel && !mods.shift && !mods.ctrl {
                    if left { lo } else { hi }
                } else if left {
                    if mods.ctrl { prev_word(&cur, te.caret) } else { prev_char(&cur, te.caret) }
                } else if mods.ctrl {
                    next_word(&cur, te.caret)
                } else {
                    next_char(&cur, te.caret)
                };
                self.set_caret(target, mods.shift, None);
                true
            }
            Key::Up | Key::Down => {
                let Some(g) = self.edit_geom(te.id) else { return true };
                let dir = if key == Key::Up { -1 } else { 1 };
                let (pos, x) = g.layout.move_vertical(te.caret, dir, te.preferred_x);
                self.set_caret(pos, mods.shift, Some(x));
                true
            }
            Key::Home | Key::End => {
                let Some(g) = self.edit_geom(te.id) else { return true };
                let target = match (key, mods.ctrl) {
                    (Key::Home, true) => 0,
                    (Key::End, true) => cur.len(),
                    (Key::Home, false) => g.layout.line_start(te.caret),
                    _ => g.layout.line_end(te.caret),
                };
                self.set_caret(target, mods.shift, None);
                true
            }
            Key::Backspace | Key::Delete => {
                let (a, b) = if has_sel {
                    (lo, hi)
                } else if key == Key::Backspace {
                    let p = if mods.ctrl { prev_word(&cur, te.caret) } else { prev_char(&cur, te.caret) };
                    (p, te.caret)
                } else {
                    let n = if mods.ctrl { next_word(&cur, te.caret) } else { next_char(&cur, te.caret) };
                    (te.caret, n)
                };
                if a < b {
                    let mut new = String::with_capacity(cur.len());
                    new.push_str(&cur[..a]);
                    new.push_str(&cur[b..]);
                    self.apply_text(new, a, a);
                }
                true
            }
            Key::Char(c) if mods.ctrl => match c.to_ascii_lowercase() {
                'a' => {
                    self.set_caret(cur.len(), false, None);
                    if let Some(t) = self.text.as_mut() {
                        t.anchor = 0;
                    }
                    true
                }
                'c' | 'x' => {
                    if has_sel {
                        let s = cur[lo..hi].to_owned();
                        self.text_clip.clone_from(&s);
                        self.events.push(SessionEvent::SetClipboardText(s));
                        if c.eq_ignore_ascii_case(&'x') {
                            let mut new = String::new();
                            new.push_str(&cur[..lo]);
                            new.push_str(&cur[hi..]);
                            self.apply_text(new, lo, lo);
                        }
                    }
                    true
                }
                'v' => {
                    if self.text_clip.is_empty() {
                        self.events.push(SessionEvent::PasteTextRequested);
                    } else {
                        let clip = self.text_clip.clone();
                        self.text_insert(&clip);
                    }
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Pointer press while editing: places the caret when inside the edited box. Returns
    /// `false` when the press is elsewhere (the caller then commits and handles it normally).
    pub(crate) fn text_pointer_down(&mut self, pos: PointF) -> bool {
        let Some(id) = self.text.as_ref().map(|t| t.id) else { return false };
        let inside = self.doc.object(id).is_some_and(|o| o.hit_test_box(pos, self.hit_slack()));
        if !inside {
            return false;
        }
        self.text_drag_to_inner(id, pos, true);
        self.history.seal();
        self.drag = Some(Drag::TextSelect);
        true
    }

    pub(crate) fn text_drag_to(&mut self, pos: PointF) {
        if let Some(id) = self.text.as_ref().map(|t| t.id) {
            self.text_drag_to_inner(id, pos, false);
        }
    }

    fn text_drag_to_inner(&mut self, id: ObjectId, pos: PointF, reset_anchor: bool) {
        let Some(g) = self.edit_geom(id) else { return };
        let local = pos.rotate_about(g.pivot, -g.rotation) - g.origin;
        let byte = g.layout.hit(local.x, local.y);
        if let Some(te) = self.text.as_mut() {
            te.caret = byte;
            if reset_anchor {
                te.anchor = byte;
            }
            te.preferred_x = None;
        }
        self.events.push(SessionEvent::OverlayChanged);
    }

    /// Caret / selection geometry for the overlay.
    pub(crate) fn caret_overlay(&mut self) -> Option<CaretOverlay> {
        let te = self.text.clone()?;
        let g = self.edit_geom(te.id)?;
        let c = g.layout.caret(te.caret);
        let (lo, hi) = (te.caret.min(te.anchor), te.caret.max(te.anchor));
        let selection = g
            .layout
            .selection_rects(lo, hi)
            .into_iter()
            .map(|r| r.translate(g.origin.x, g.origin.y))
            .collect();
        Some(CaretOverlay {
            caret: RectF::new(g.origin.x + c.x, g.origin.y + c.y, 0.0, c.height),
            selection,
            rotation: g.rotation,
            pivot: g.pivot,
            preedit: te.preedit.map(|(s, _)| s),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_and_word_navigation() {
        let s = "héllo wörld  foo";
        assert_eq!(next_char(s, 0), 1);
        assert_eq!(next_char(s, 1), 3, "é is two bytes");
        assert_eq!(prev_char(s, 3), 1);
        assert_eq!(prev_char(s, 0), 0);
        assert_eq!(next_char(s, s.len()), s.len());
        assert_eq!(next_word(s, 0), 6);
        assert_eq!(prev_word(s, s.len()), 15);
        assert_eq!(prev_word(s, 0), 0);
        assert_eq!(next_word(s, s.len()), s.len());
        assert_eq!(prev_word("ab\ncd", 5), 3);
    }
}
