//! Session operations that are not pointer gestures: undo/redo, deletion, clipboard of
//! objects, z-order, grouping, and the global (whole-image) operations.

use ssx_imgfx::{Effect, ResizeFilter};
use ssx_types::Rect;

use super::EditorSession;
use crate::{
    DocError,
    doc::{Document, Padding},
    geom::PointF,
    history::{CoalesceKey, Command},
    object::{Axis, Object, ObjectId, Orient},
    render::RenderOptions,
    style::Fill,
    tool::Tool,
};

/// Offset applied to each successive paste/duplicate so copies do not hide the original.
const PASTE_OFFSET: f32 = 16.0;

impl EditorSession {
    // ---------------------------------------------------------------------------------
    // Undo / redo
    // ---------------------------------------------------------------------------------

    /// Undoes the last step (also cancels an in-progress drag first).
    pub fn undo(&mut self) {
        self.cancel_drag();
        self.history.seal();
        let info = self.history.undo(&mut self.doc).map(|c| (c.affected(), matches!(c, Command::SetDocument { .. })));
        if let Some((affected, canvas)) = info {
            self.after_change(affected, canvas);
            self.clamp_text_edit();
        }
    }

    /// Re-applies the last undone step.
    pub fn redo(&mut self) {
        self.cancel_drag();
        self.history.seal();
        let info = self.history.redo(&mut self.doc).map(|c| (c.affected(), matches!(c, Command::SetDocument { .. })));
        if let Some((affected, canvas)) = info {
            self.after_change(affected, canvas);
            self.clamp_text_edit();
        }
    }

    // ---------------------------------------------------------------------------------
    // Selection edits
    // ---------------------------------------------------------------------------------

    /// Selects the given objects (groups are expanded).
    pub fn select(&mut self, ids: &[ObjectId]) {
        let valid: Vec<ObjectId> = ids.iter().copied().filter(|i| self.doc.object(*i).is_some()).collect();
        self.commit_text_edit();
        self.set_selection(valid);
    }

    /// Selects everything selectable.
    pub fn select_all(&mut self) {
        let ids = self.doc.objects().iter().filter(|o| o.visible && !o.locked).map(|o| o.id).collect();
        self.set_selection(ids);
    }

    /// Clears the selection.
    pub fn clear_selection(&mut self) {
        self.commit_text_edit();
        self.set_selection(Vec::new());
    }

    fn selected_sorted(&self) -> Vec<(usize, Object)> {
        let mut v: Vec<(usize, Object)> = self
            .selection
            .iter()
            .filter_map(|id| self.doc.index_of(*id).map(|i| (i, self.doc.objects()[i].clone())))
            .collect();
        v.sort_by_key(|(i, _)| *i);
        v
    }

    /// Deletes the selected (unlocked) objects.
    pub fn delete_selection(&mut self) {
        self.commit_text_edit();
        let items: Vec<(usize, Object)> = self.selected_sorted().into_iter().filter(|(_, o)| !o.locked).collect();
        if items.is_empty() {
            return;
        }
        self.exec(Command::Remove { items }, None);
    }

    /// Moves the selection by `(dx, dy)` image pixels. Repeated nudges merge into one undo
    /// step until something else happens.
    pub fn nudge(&mut self, dx: f32, dy: f32) {
        let before: Vec<Object> =
            self.selection.iter().filter_map(|i| self.doc.object(*i).cloned()).filter(|o| !o.locked).collect();
        if before.is_empty() {
            return;
        }
        let after: Vec<Object> = before
            .iter()
            .map(|o| {
                let mut n = o.clone();
                n.translate_with_source(dx, dy);
                n
            })
            .collect();
        let tag = before.iter().fold(7u64, |a, o| a.wrapping_mul(31).wrapping_add(o.id.0));
        self.modify(before, after, Some(CoalesceKey { kind: "nudge", tag }));
    }

    /// Shows or hides an object.
    pub fn set_visible(&mut self, id: ObjectId, visible: bool) {
        self.edit_object(id, |o| o.visible = visible);
    }

    /// Locks or unlocks an object.
    pub fn set_locked(&mut self, id: ObjectId, locked: bool) {
        self.edit_object(id, |o| o.locked = locked);
        if locked {
            self.selection.retain(|s| *s != id);
        }
    }

    fn edit_object(&mut self, id: ObjectId, f: impl Fn(&mut Object)) {
        if let Some(before) = self.doc.object(id).cloned() {
            let mut after = before.clone();
            f(&mut after);
            self.modify(vec![before], vec![after], None);
        }
    }

    /// Groups the selection so it moves and selects as one.
    pub fn group_selection(&mut self) {
        if self.selection.len() < 2 {
            return;
        }
        let g = self.doc.fresh_group_id();
        let before: Vec<Object> = self.selection.iter().filter_map(|i| self.doc.object(*i).cloned()).collect();
        let after = before
            .iter()
            .map(|o| {
                let mut n = o.clone();
                n.group = Some(g);
                n
            })
            .collect();
        self.modify(before, after, None);
    }

    /// Dissolves the groups of the selected objects.
    pub fn ungroup_selection(&mut self) {
        let before: Vec<Object> =
            self.selection.iter().filter_map(|i| self.doc.object(*i).cloned()).filter(|o| o.group.is_some()).collect();
        let after = before
            .iter()
            .map(|o| {
                let mut n = o.clone();
                n.group = None;
                n
            })
            .collect();
        self.modify(before, after, None);
    }

    // ---------------------------------------------------------------------------------
    // Z-order
    // ---------------------------------------------------------------------------------

    fn reorder(&mut self, f: impl Fn(&[ObjectId], &[ObjectId]) -> Vec<ObjectId>) {
        let before = self.doc.order();
        let sel: Vec<ObjectId> = before.iter().copied().filter(|i| self.selection.contains(i)).collect();
        if sel.is_empty() {
            return;
        }
        let after = f(&before, &sel);
        if after != before {
            self.exec(Command::Reorder { before, after }, None);
        }
    }

    /// Brings the selection to the front.
    pub fn bring_to_front(&mut self) {
        self.reorder(|all, sel| all.iter().copied().filter(|i| !sel.contains(i)).chain(sel.iter().copied()).collect());
    }

    /// Sends the selection to the back.
    pub fn send_to_back(&mut self) {
        self.reorder(|all, sel| sel.iter().copied().chain(all.iter().copied().filter(|i| !sel.contains(i))).collect());
    }

    /// Raises the selection one step.
    pub fn raise(&mut self) {
        self.reorder(|all, sel| {
            let mut v = all.to_vec();
            for i in (0..v.len().saturating_sub(1)).rev() {
                if sel.contains(&v[i]) && !sel.contains(&v[i + 1]) {
                    v.swap(i, i + 1);
                }
            }
            v
        });
    }

    /// Lowers the selection one step.
    pub fn lower(&mut self) {
        self.reorder(|all, sel| {
            let mut v = all.to_vec();
            for i in 1..v.len() {
                if sel.contains(&v[i]) && !sel.contains(&v[i - 1]) {
                    v.swap(i, i - 1);
                }
            }
            v
        });
    }

    // ---------------------------------------------------------------------------------
    // Object clipboard
    // ---------------------------------------------------------------------------------

    /// Copies the selection into the session's object clipboard.
    pub fn copy(&mut self) {
        self.clipboard = self.selected_sorted().into_iter().map(|(_, o)| o).collect();
        self.paste_serial = 0;
    }

    /// Copies then deletes the selection.
    pub fn cut(&mut self) {
        self.copy();
        self.delete_selection();
    }

    /// Pastes the object clipboard on top, offset a little more each time. Groups are kept as
    /// groups (with fresh group ids).
    pub fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        self.paste_serial += 1;
        let off = PASTE_OFFSET * self.paste_serial as f32;
        let objs = self.clipboard.clone();
        self.paste_objects(objs, off);
    }

    /// Duplicates the selection (copy + paste in one step, without touching the clipboard).
    pub fn duplicate(&mut self) {
        let objs: Vec<Object> = self.selected_sorted().into_iter().map(|(_, o)| o).collect();
        self.paste_objects(objs, PASTE_OFFSET);
    }

    fn paste_objects(&mut self, objs: Vec<Object>, offset: f32) {
        if objs.is_empty() {
            return;
        }
        self.commit_text_edit();
        let mut next = self.doc.peek_next_id().0;
        let prev_next_id = next;
        let mut group_map: Vec<(u32, u32)> = Vec::new();
        let mut fresh = self.doc.fresh_group_id();
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for mut o in objs {
            o.id = ObjectId(next);
            next += 1;
            o.locked = false;
            o.translate_with_source(offset, offset);
            if let Some(g) = o.group {
                let ng = match group_map.iter().find(|(old, _)| *old == g) {
                    Some((_, n)) => *n,
                    None => {
                        group_map.push((g, fresh));
                        fresh += 1;
                        fresh - 1
                    }
                };
                o.group = Some(ng);
            }
            ids.push(o.id);
            items.push((self.doc.objects().len() + items.len(), o));
        }
        self.exec(Command::Add { items, prev_next_id }, None);
        self.set_selection(ids);
    }

    /// The object clipboard as JSON, for the system clipboard.
    pub fn clipboard_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.clipboard)
    }

    /// Replaces the object clipboard from JSON produced by [`Self::clipboard_json`] and pastes.
    pub fn paste_json(&mut self, json: &str) -> Result<(), serde_json::Error> {
        self.clipboard = serde_json::from_str(json)?;
        self.paste_serial = 0;
        self.paste();
        Ok(())
    }

    /// The object clipboard.
    pub fn object_clipboard(&self) -> &[Object] {
        &self.clipboard
    }

    // ---------------------------------------------------------------------------------
    // Global operations (each is one undo step)
    // ---------------------------------------------------------------------------------

    /// Runs a whole-document edit as a single undoable command. On error nothing changes.
    pub fn global_op(&mut self, f: impl FnOnce(&mut Document, &mut crate::render::Renderer) -> Result<(), DocError>) -> Result<(), DocError> {
        self.commit_text_edit();
        self.cancel_drag();
        let before = self.doc.clone();
        let mut after = self.doc.clone();
        f(&mut after, &mut self.renderer)?;
        if after == before {
            return Ok(());
        }
        self.crop = None;
        self.exec(Command::SetDocument { before: Box::new(before), after: Box::new(after) }, None);
        Ok(())
    }

    /// Crops to `rect` (image space). Annotations stay editable and keep their place.
    pub fn crop_rect(&mut self, rect: Rect) -> Result<(), DocError> {
        self.global_op(|d, _| d.crop(rect))
    }

    /// Alias of [`Self::crop_rect`].
    pub fn crop(&mut self, rect: Rect) -> Result<(), DocError> {
        self.crop_rect(rect)
    }

    /// Removes a horizontal (`Axis::Y`) or vertical (`Axis::X`) strip and joins the rest.
    pub fn cut_out(&mut self, axis: Axis, start: i32, end: i32) -> Result<(), DocError> {
        self.global_op(|d, _| d.cut_out(axis, start, end))
    }

    /// Grows (positive) or shrinks (negative) the canvas on each side.
    pub fn resize_canvas(&mut self, left: i32, top: i32, right: i32, bottom: i32) -> Result<(), DocError> {
        self.global_op(|d, _| d.resize_canvas(left, top, right, bottom))
    }

    /// Sets the canvas padding.
    pub fn set_padding(&mut self, padding: Padding) -> Result<(), DocError> {
        self.global_op(|d, _| {
            d.set_padding(padding);
            Ok(())
        })
    }

    /// Sets the canvas background.
    pub fn set_background(&mut self, background: Fill) -> Result<(), DocError> {
        self.global_op(|d, _| {
            d.set_background(background);
            Ok(())
        })
    }

    /// Rotates/flips the whole document.
    pub fn orient(&mut self, o: Orient) -> Result<(), DocError> {
        self.global_op(|d, _| d.orient(o))
    }

    /// Scales the whole document.
    pub fn resize(&mut self, w: u32, h: u32, filter: ResizeFilter) -> Result<(), DocError> {
        self.global_op(|d, _| d.resize(w, h, filter))
    }

    /// Applies an image effect to the base image (region in image space, `None` = all).
    pub fn apply_effect(&mut self, effect: &Effect, region: Option<Rect>) -> Result<(), DocError> {
        self.global_op(|d, _| d.apply_effect(effect, region))
    }

    /// Crops away uniform borders.
    pub fn auto_crop(&mut self, tolerance: u8) -> Result<(), DocError> {
        self.global_op(|d, _| d.auto_crop(tolerance).map(|_| ()))
    }

    /// Sets the number the first step marker shows.
    pub fn set_step_start(&mut self, start: u32) -> Result<(), DocError> {
        self.global_op(|d, _| {
            d.set_step_start(start);
            Ok(())
        })
    }

    /// Bakes every annotation into the base image (irreversible except through undo).
    pub fn flatten(&mut self) -> Result<(), DocError> {
        self.global_op(|d, r| {
            let frame = r.render(d, &RenderOptions::default());
            flatten_into(d, frame)
        })
    }

    pub(super) fn crop_shaped(&mut self, r: Rect, c: &super::CropPending) -> Result<(), DocError> {
        let polygon = c.polygon.clone();
        let ellipse = c.tool == Tool::CropEllipse;
        self.global_op(move |d, renderer| {
            let (ox, oy) = d.canvas_offset();
            let full = renderer.render(d, &RenderOptions::default());
            // Rectangle in canvas pixels.
            let rc = Rect::new(r.x + ox as i32, r.y + oy as i32, r.width, r.height);
            let piece = ssx_imgfx::crop(&full, rc)?;
            let (w, h) = (piece.width(), piece.height());
            let mut mask = tiny_skia::Pixmap::new(w.max(1), h.max(1)).ok_or(DocError::EmptySize(w, h))?;
            let paint = {
                let mut p = tiny_skia::Paint::default();
                p.set_color_rgba8(255, 255, 255, 255);
                p.anti_alias = true;
                p
            };
            let local = |p: PointF| (p.x - r.x as f32, p.y - r.y as f32);
            let path = if ellipse {
                tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).and_then(tiny_skia::PathBuilder::from_oval)
            } else {
                let mut pb = tiny_skia::PathBuilder::new();
                for (i, p) in polygon.iter().enumerate() {
                    let (x, y) = local(*p);
                    if i == 0 {
                        pb.move_to(x, y);
                    } else {
                        pb.line_to(x, y);
                    }
                }
                pb.close();
                pb.finish()
            };
            let Some(path) = path else { return Err(DocError::EmptySize(w, h)) };
            mask.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            let mut out = piece;
            for (px, m) in out.data_mut().chunks_exact_mut(4).zip(mask.data().chunks_exact(4)) {
                px[3] = ((u32::from(px[3]) * u32::from(m[3]) + 127) / 255) as u8;
            }
            flatten_into(d, out)?;
            d.set_background(Fill::None);
            Ok(())
        })?;
        Ok(())
    }
}

/// Replaces the document content with `frame` (all annotations and padding gone).
fn flatten_into(d: &mut Document, frame: ssx_types::Frame) -> Result<(), DocError> {
    d.replace_base(frame)?;
    d.clear_annotations();
    Ok(())
}
