//! Undo/redo: the command pattern with coalescing and bounded memory.
//!
//! Every document mutation made by the interactive session is a [`Command`]. Commands store
//! *absolute* before/after states (whole objects, or whole documents for global operations)
//! instead of deltas. That makes undo trivially exact — the property tests demand
//! byte-for-byte identical documents — and makes coalescing a matter of keeping the first
//! `before` and the latest `after`.
//!
//! Global operations snapshot the whole [`Document`]; that is cheap because the base image is
//! an `Arc<Frame>` shared between snapshots. The history charges each snapshot only for base
//! images that are *new* (different from the previous state) and evicts the oldest entries
//! when either the entry count or the byte budget is exceeded.

use crate::{
    doc::Document,
    geom::RectF,
    object::{Object, ObjectId},
};

/// Identifies a run of commands that should merge into one undo step (a drag, a burst of
/// typing, repeated nudges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceKey {
    /// What kind of gesture (`"drag"`, `"typing"`, `"nudge"`, `"style"`).
    pub kind: &'static str,
    /// Distinguishes gestures of the same kind (drag id, object id, ...).
    pub tag: u64,
}

/// One undoable mutation.
#[derive(Debug, Clone)]
pub enum Command {
    /// Insert objects at the given z-order indices (ascending).
    Add {
        /// `(index, object)` pairs, ascending by index.
        items: Vec<(usize, Object)>,
        /// The id counter before the add, restored on undo so ids are not burned.
        prev_next_id: u64,
    },
    /// Remove objects; remembers where they were.
    Remove {
        /// `(index, object)` pairs, ascending by index.
        items: Vec<(usize, Object)>,
    },
    /// Replace objects (matched by id) with new versions.
    Modify {
        /// States before.
        before: Vec<Object>,
        /// States after (same ids, same order).
        after: Vec<Object>,
    },
    /// Change the z-order.
    Reorder {
        /// Order before.
        before: Vec<ObjectId>,
        /// Order after.
        after: Vec<ObjectId>,
    },
    /// Replace the whole document (crop, rotate, effects...).
    SetDocument {
        /// Document before.
        before: Box<Document>,
        /// Document after.
        after: Box<Document>,
    },
    /// Several commands undone/redone together.
    Batch(Vec<Command>),
}

impl Command {
    /// Applies the command.
    pub fn apply(&self, doc: &mut Document) {
        match self {
            Command::Add { items, .. } => {
                for (i, o) in items {
                    doc.insert_object(*i, o.clone());
                }
            }
            Command::Remove { items } => {
                for (_, o) in items {
                    doc.remove_object(o.id);
                }
            }
            Command::Modify { after, .. } => replace_objects(doc, after),
            Command::Reorder { after, .. } => doc.set_order(after),
            Command::SetDocument { after, .. } => *doc = (**after).clone(),
            Command::Batch(cmds) => cmds.iter().for_each(|c| c.apply(doc)),
        }
    }

    /// Reverses the command.
    pub fn revert(&self, doc: &mut Document) {
        match self {
            Command::Add { items, prev_next_id } => {
                for (_, o) in items.iter().rev() {
                    doc.remove_object(o.id);
                }
                doc.next_id = *prev_next_id;
            }
            Command::Remove { items } => {
                for (i, o) in items {
                    doc.insert_object(*i, o.clone());
                }
            }
            Command::Modify { before, .. } => replace_objects(doc, before),
            Command::Reorder { before, .. } => doc.set_order(before),
            Command::SetDocument { before, .. } => *doc = (**before).clone(),
            Command::Batch(cmds) => cmds.iter().rev().for_each(|c| c.revert(doc)),
        }
    }

    /// Image-space rectangle whose pixels this command may change (before expansion through
    /// effect objects). `None` means "everything" (global operations).
    pub fn affected(&self) -> Option<RectF> {
        let union = |it: &mut dyn Iterator<Item = RectF>| it.reduce(|a, b| a.union(&b));
        match self {
            Command::Add { items, .. } | Command::Remove { items } => {
                Some(union(&mut items.iter().map(|(_, o)| o.render_bounds())).unwrap_or_default())
            }
            Command::Modify { before, after } => Some(
                union(&mut before.iter().chain(after).map(Object::render_bounds))
                    .unwrap_or_default(),
            ),
            Command::Reorder { .. } | Command::SetDocument { .. } => None,
            Command::Batch(cmds) => {
                let mut acc: Option<RectF> = None;
                for c in cmds {
                    acc = Some(match (acc, c.affected()) {
                        (_, None) => return None,
                        (Some(a), Some(b)) => a.union(&b),
                        (None, Some(b)) => b,
                    });
                }
                acc
            }
        }
    }

    /// Rough memory footprint, for the history budget.
    pub fn approx_bytes(&self) -> usize {
        fn objs(v: &[Object]) -> usize {
            v.iter().map(object_bytes).sum()
        }
        match self {
            Command::Add { items, .. } | Command::Remove { items } => {
                items.iter().map(|(_, o)| object_bytes(o)).sum::<usize>() + 64
            }
            Command::Modify { before, after } => objs(before) + objs(after) + 64,
            Command::Reorder { before, after } => (before.len() + after.len()) * 8 + 64,
            Command::SetDocument { before, after } => {
                let new_base = if std::sync::Arc::ptr_eq(before.base(), after.base()) {
                    0
                } else {
                    after.base().data().len()
                };
                new_base + objs(before.objects()) + objs(after.objects()) + 256
            }
            Command::Batch(cmds) => cmds.iter().map(Command::approx_bytes).sum(),
        }
    }

    /// Merges `next` (already applied to the document) into `self` when both describe the
    /// same gesture. Returns `false` when they cannot be combined.
    fn merge(&mut self, next: &Command) -> bool {
        match (&mut *self, next) {
            (Command::Modify { before, after }, Command::Modify { before: nb, after: na })
                if ids(before) == ids(nb) && ids(after) == ids(na) =>
            {
                after.clone_from(na);
                true
            }
            (Command::Add { items, .. }, Command::Modify { after, .. }) => {
                if after.iter().all(|a| items.iter().any(|(_, o)| o.id == a.id)) {
                    for a in after {
                        if let Some((_, o)) = items.iter_mut().find(|(_, o)| o.id == a.id) {
                            *o = a.clone();
                        }
                    }
                    true
                } else {
                    false
                }
            }
            (Command::Reorder { after, .. }, Command::Reorder { after: na, .. }) => {
                after.clone_from(na);
                true
            }
            _ => false,
        }
    }
}

fn ids(v: &[Object]) -> Vec<ObjectId> {
    v.iter().map(|o| o.id).collect()
}

fn object_bytes(o: &Object) -> usize {
    use crate::object::{ObjectKind, StickerSource};
    let extra = match &o.kind {
        ObjectKind::Image(i) => i.image.0.data().len(),
        ObjectKind::Sticker(s) => match &s.source {
            StickerSource::Bitmap { image } => image.0.data().len(),
            _ => 0,
        },
        ObjectKind::Freehand(f) => f.points.len() * 8,
        ObjectKind::Highlight(h) => h.points.len() * 8,
        ObjectKind::Text(t) => t.content.text.len(),
        ObjectKind::Balloon(b) => b.content.text.len(),
        _ => 0,
    };
    extra + 400
}

fn replace_objects(doc: &mut Document, objs: &[Object]) {
    for o in objs {
        if let Some(slot) = doc.object_mut(o.id) {
            *slot = o.clone();
        }
    }
}

struct Entry {
    cmd: Command,
    key: Option<CoalesceKey>,
    bytes: usize,
}

/// Bounds on the history.
#[derive(Debug, Clone, Copy)]
pub struct HistoryLimits {
    /// Maximum number of undo steps kept.
    pub max_entries: usize,
    /// Maximum approximate bytes kept.
    pub max_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self { max_entries: 200, max_bytes: 512 * 1024 * 1024 }
    }
}

/// Undo/redo stacks.
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// The top entry no longer accepts merges.
    sealed: bool,
    limits: HistoryLimits,
    bytes: usize,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("undo", &self.undo.len())
            .field("redo", &self.redo.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new(HistoryLimits::default())
    }
}

impl History {
    /// Creates an empty history.
    pub fn new(limits: HistoryLimits) -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), sealed: true, limits, bytes: 0 }
    }

    /// Applies `cmd` to `doc` and records it. With a `key`, consecutive commands with the same
    /// key merge into a single undo step until [`History::seal`] is called. Clears the redo
    /// stack.
    pub fn execute(&mut self, doc: &mut Document, cmd: Command, key: Option<CoalesceKey>) {
        cmd.apply(doc);
        self.redo.clear();
        if let (Some(k), false) = (key, self.sealed)
            && let Some(top) = self.undo.last_mut()
            && top.key == Some(k)
            && top.cmd.merge(&cmd)
        {
            let nb = top.cmd.approx_bytes();
            self.bytes = self.bytes + nb - top.bytes;
            top.bytes = nb;
            return;
        }
        let bytes = cmd.approx_bytes();
        self.bytes += bytes;
        self.undo.push(Entry { cmd, key, bytes });
        self.sealed = key.is_none();
        self.enforce_limits();
    }

    fn enforce_limits(&mut self) {
        while self.undo.len() > 1
            && (self.undo.len() > self.limits.max_entries || self.bytes > self.limits.max_bytes)
        {
            let e = self.undo.remove(0);
            self.bytes -= e.bytes;
        }
    }

    /// Ends the current coalescing run: the next command starts a new undo step.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// Undoes the last step. Returns the command that was reverted.
    pub fn undo(&mut self, doc: &mut Document) -> Option<&Command> {
        let e = self.undo.pop()?;
        self.bytes -= e.bytes;
        e.cmd.revert(doc);
        self.sealed = true;
        self.redo.push(e);
        self.redo.last().map(|e| &e.cmd)
    }

    /// Re-applies the last undone step.
    pub fn redo(&mut self, doc: &mut Document) -> Option<&Command> {
        let e = self.redo.pop()?;
        e.cmd.apply(doc);
        self.bytes += e.bytes;
        self.sealed = true;
        self.undo.push(e);
        self.undo.last().map(|e| &e.cmd)
    }

    /// If the top entry is still open (not sealed) and `pred` accepts its command, reverts
    /// and removes it without touching the redo stack. Used to drop a just-created object
    /// that turned out to be an accidental click. Returns whether it did.
    pub fn discard_open(&mut self, doc: &mut Document, pred: impl Fn(&Command) -> bool) -> bool {
        if self.sealed || !self.undo.last().is_some_and(|e| pred(&e.cmd)) {
            return false;
        }
        if let Some(e) = self.undo.pop() {
            self.bytes -= e.bytes;
            e.cmd.revert(doc);
        }
        self.sealed = true;
        true
    }

    /// Can something be undone?
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Can something be redone?
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of undo steps.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of redo steps.
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Approximate bytes held.
    pub fn approx_bytes(&self) -> usize {
        self.bytes
    }

    /// Forgets everything (e.g. after saving with "clear history").
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.bytes = 0;
        self.sealed = true;
    }

    /// The command that `undo` would revert.
    pub fn peek_undo(&self) -> Option<&Command> {
        self.undo.last().map(|e| &e.cmd)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact geometry values are what these tests assert
mod tests {
    use super::*;
    use crate::{
        geom::RectF,
        object::{BoxShape, ObjectKind},
        style::Style,
    };
    use ssx_imgfx::solid_frame;

    fn doc() -> Document {
        Document::new(solid_frame(10, 10, [1, 2, 3, 255])).unwrap()
    }

    fn rect(id: u64, x: f32) -> Object {
        Object::new(
            ObjectId(id),
            Style::default(),
            ObjectKind::Rectangle(BoxShape { rect: RectF::new(x, 0.0, 5.0, 5.0), rotation: 0.0 }),
        )
    }

    fn add(d: &Document, o: Object) -> Command {
        Command::Add { items: vec![(d.objects().len(), o)], prev_next_id: d.peek_next_id().0 }
    }

    #[test]
    fn add_undo_redo_restores_id_counter() {
        let mut d = doc();
        let mut h = History::default();
        let before = d.clone();
        let id = d.alloc_id();
        // Simulate what the session does: allocate, then record an add with the old counter.
        let prev = id.0;
        h.execute(
            &mut d,
            Command::Add { items: vec![(0, rect(id.0, 1.0))], prev_next_id: prev },
            None,
        );
        assert_eq!(d.objects().len(), 1);
        h.undo(&mut d);
        assert_eq!(d, before);
        h.redo(&mut d);
        assert_eq!(d.objects().len(), 1);
    }

    #[test]
    fn coalescing_merges_until_sealed() {
        let mut d = doc();
        let mut h = History::default();
        let key = Some(CoalesceKey { kind: "drag", tag: 1 });
        {
            let c = add(&d, rect(1, 0.0));
            h.execute(&mut d, c, key);
        }
        for x in 1..=5 {
            let before = vec![d.object(ObjectId(1)).unwrap().clone()];
            let after = vec![rect(1, x as f32)];
            h.execute(&mut d, Command::Modify { before, after }, key);
        }
        assert_eq!(h.undo_len(), 1, "creation + drag is one step");
        h.seal();
        let before = vec![d.object(ObjectId(1)).unwrap().clone()];
        h.execute(&mut d, Command::Modify { before, after: vec![rect(1, 50.0)] }, key);
        assert_eq!(h.undo_len(), 2, "sealed: a new step starts");
        h.undo(&mut d);
        assert_eq!(d.object(ObjectId(1)).unwrap().bounds().x, 5.0);
        h.undo(&mut d);
        assert!(d.objects().is_empty());
    }

    #[test]
    fn different_keys_do_not_merge() {
        let mut d = doc();
        let mut h = History::default();
        {
            let c = add(&d, rect(1, 0.0));
            h.execute(&mut d, c, None);
        }
        let mk = |x: f32, d: &Document| Command::Modify {
            before: vec![d.object(ObjectId(1)).unwrap().clone()],
            after: vec![rect(1, x)],
        };
        let c = mk(1.0, &d);
        h.execute(&mut d, c, Some(CoalesceKey { kind: "drag", tag: 1 }));
        let c = mk(2.0, &d);
        h.execute(&mut d, c, Some(CoalesceKey { kind: "drag", tag: 2 }));
        assert_eq!(h.undo_len(), 3);
    }

    #[test]
    fn redo_is_invalidated_by_new_command() {
        let mut d = doc();
        let mut h = History::default();
        {
            let c = add(&d, rect(1, 0.0));
            h.execute(&mut d, c, None);
        }
        h.undo(&mut d);
        assert!(h.can_redo());
        {
            let c = add(&d, rect(2, 0.0));
            h.execute(&mut d, c, None);
        }
        assert!(!h.can_redo());
        assert!(h.redo(&mut d).is_none());
    }

    #[test]
    fn limits_evict_oldest_but_keep_newest() {
        let mut d = doc();
        let mut h = History::new(HistoryLimits { max_entries: 3, max_bytes: usize::MAX });
        for i in 0..10 {
            {
                let c = add(&d, rect(i + 1, 0.0));
                h.execute(&mut d, c, None);
            }
        }
        assert_eq!(h.undo_len(), 3);
        assert_eq!(d.objects().len(), 10);
        for _ in 0..5 {
            h.undo(&mut d);
        }
        assert_eq!(d.objects().len(), 7, "only 3 steps were undoable");
        let mut tiny = History::new(HistoryLimits { max_entries: 100, max_bytes: 1 });
        let mut d2 = doc();
        {
            let c = add(&d2, rect(1, 0.0));
            tiny.execute(&mut d2, c, None);
        }
        {
            let c = add(&d2, rect(2, 0.0));
            tiny.execute(&mut d2, c, None);
        }
        assert_eq!(tiny.undo_len(), 1, "byte budget evicts, newest step always survives");
    }

    #[test]
    fn set_document_shares_base_and_is_cheap() {
        let mut d = doc();
        let mut h = History::default();
        let before = Box::new(d.clone());
        let mut after = d.clone();
        after.set_step_start(5);
        let cmd = Command::SetDocument { before, after: Box::new(after) };
        assert!(cmd.approx_bytes() < 2000, "same base image: no pixel bytes charged");
        h.execute(&mut d, cmd, None);
        assert_eq!(d.step_start(), 5);
        h.undo(&mut d);
        assert_eq!(d.step_start(), 1);
        let mut grown = d.clone();
        grown.resize_canvas(1, 1, 1, 1).unwrap();
        let big = Command::SetDocument { before: Box::new(d.clone()), after: Box::new(grown) };
        assert!(big.approx_bytes() < 2000, "padding-only change keeps the same base");
        let mut cropped = d.clone();
        cropped.crop(ssx_types::Rect::new(0, 0, 5, 5)).unwrap();
        let c2 = Command::SetDocument { before: Box::new(d.clone()), after: Box::new(cropped) };
        assert!(c2.approx_bytes() >= 5 * 5 * 4);
    }

    #[test]
    fn discard_open_removes_unsealed_creation_only() {
        let mut d = doc();
        let mut h = History::default();
        let key = Some(CoalesceKey { kind: "drag", tag: 1 });
        {
            let c = add(&d, rect(1, 0.0));
            h.execute(&mut d, c, key);
        }
        assert!(h.discard_open(&mut d, |c| matches!(c, Command::Add { .. })));
        assert!(d.objects().is_empty() && !h.can_undo() && !h.can_redo());
        {
            let c = add(&d, rect(1, 0.0));
            h.execute(&mut d, c, key);
        }
        h.seal();
        assert!(!h.discard_open(&mut d, |_| true), "sealed entries are permanent");
    }

    #[test]
    fn batch_applies_and_reverts_in_order() {
        let mut d = doc();
        let mut h = History::default();
        let a = add(&d, rect(1, 0.0));
        let b = Command::Add { items: vec![(1, rect(2, 3.0))], prev_next_id: 2 };
        h.execute(&mut d, Command::Batch(vec![a, b]), None);
        assert_eq!(d.objects().len(), 2);
        h.undo(&mut d);
        assert!(d.objects().is_empty());
        assert!(Command::Batch(vec![]).affected().is_none());
    }
}
