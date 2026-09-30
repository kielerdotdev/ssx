//! History: search and browse what was captured and uploaded, with thumbnails, a detail pane
//! and the actions that make sense for an entry.
//!
//! Database work (`list`, `count`, orphan checks, deletes, prune) runs on worker threads and
//! delivers into slots that `ui()` polls, thumbnails are decoded by [`Loader`] and kept in a
//! bounded [`Cache`], and the file system is only asked about the entries on screen. The
//! window never blocks on any of it.

use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use egui::{Color32, ColorImage, RichText, TextureHandle, TextureOptions, Ui, vec2};
use ssx_core::{
    history::{Entry, EntryKind, History, Orphan, UploadInfo, now_ms},
    settings::DestinationType,
    workflow::{CancelToken, UploadOutcome},
};
use ssx_editor_ui::{
    icons::{self, Icon, IconColors},
    ui::{theme, widgets::input_style},
};

use super::{Cx, shared::destination_picker};
use crate::{
    debounce::Debouncer,
    history_view::{
        DateRange, EntryActions, HistoryFilter, HistoryPage, KINDS, destination_for, entry_title,
        format_time, format_time_relative, human_bytes, kind_label, kind_word, load_page, prune_policy,
    },
    task::Slot,
    thumbs::{Cache, Loader, Request, State as ThumbState},
    ui_kit::{self, Answer},
    uploader_registry::{TestJob, mime_for},
};

/// Grid or list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Thumbnail cards.
    #[default]
    Grid,
    /// One row per entry.
    List,
}

/// A destructive action waiting for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Delete one entry (and maybe its file).
    Delete(i64),
    /// Prune to the retention limits.
    Prune,
    /// Remove the entries whose file is gone.
    RemoveMissing,
}

/// What a background operation reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpResult {
    /// Entries were deleted.
    Deleted {
        /// How many entries.
        entries: usize,
        /// A file that could not be removed, with why.
        file_error: Option<String>,
    },
    /// Retention was applied.
    Pruned(usize),
    /// Missing-file entries were removed.
    MissingRemoved(usize),
    /// An upload record was updated after a re-upload.
    Reuploaded(String),
    /// It failed.
    Failed(String),
}

/// The re-upload dialog.
#[derive(Debug, Default)]
pub struct Reupload {
    /// The entry.
    pub id: i64,
    /// The destination chosen (`None`: the default for its type).
    pub destination: Option<String>,
    slot: Slot<Result<(UploadOutcome, String), String>>,
    progress: Arc<Mutex<Option<(u64, Option<u64>)>>>,
    cancel: Option<CancelToken>,
    /// Why the last attempt failed.
    pub error: Option<String>,
}

/// State of the History page.
pub struct State {
    /// The filters and the page.
    pub filter: HistoryFilter,
    /// Grid or list.
    pub view: View,
    /// The selected entry.
    pub selected: Option<i64>,
    /// A confirmation dialog.
    pub confirm: Option<Confirm>,
    /// Delete the file on disk too.
    pub delete_file: bool,
    /// The re-upload dialog.
    pub reupload: Option<Reupload>,
    /// The page on screen.
    pub page: Option<HistoryPage>,
    /// Why the database could not be opened or read.
    pub error: Option<String>,
    /// Uploaders seen, for the filter menu.
    pub uploaders: BTreeSet<String>,
    search_edit: String,
    search: Debouncer<String>,
    db: Option<Arc<History>>,
    loader: Option<Loader>,
    thumbs: Cache<TextureHandle>,
    load: Slot<Result<(HistoryFilter, HistoryPage), String>>,
    requested: Option<HistoryFilter>,
    loaded_for: Option<HistoryFilter>,
    orphans: HashSet<i64>,
    orphan_slot: Slot<Result<Vec<Orphan>, String>>,
    op: Slot<OpResult>,
    next_refresh: Option<Instant>,
    entered: bool,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("filter", &self.filter)
            .field("selected", &self.selected)
            .field("entries", &self.page.as_ref().map(|p| p.entries.len()))
            .finish_non_exhaustive()
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            filter: HistoryFilter::default(),
            view: View::Grid,
            selected: None,
            confirm: None,
            delete_file: false,
            reupload: None,
            page: None,
            error: None,
            uploaders: BTreeSet::new(),
            search_edit: String::new(),
            search: Debouncer::new(Duration::from_millis(250)),
            db: None,
            loader: None,
            thumbs: Cache::new(400),
            load: Slot::default(),
            requested: None,
            loaded_for: None,
            orphans: HashSet::new(),
            orphan_slot: Slot::default(),
            op: Slot::default(),
            next_refresh: None,
            entered: false,
        }
    }
}

impl State {
    /// Whether background work is running.
    pub fn busy(&self) -> bool {
        self.load.running()
            || self.orphan_slot.running()
            || self.op.running()
            || self.reupload.as_ref().is_some_and(|r| r.slot.running())
            || self.loader.as_ref().is_some_and(|l| l.pending() > 0)
            || self.search.is_pending()
    }

    /// Asks for a fresh load of the current page.
    pub fn refresh(&mut self) {
        self.loaded_for = None;
        self.requested = None;
    }

    /// Drops everything from the database (the page was left or the source changed).
    pub fn reset_source(&mut self) {
        self.db = None;
        self.loader = None;
        self.thumbs.clear();
        self.page = None;
        self.refresh();
        self.entered = false;
    }

    /// The entry with `id` on the current page.
    pub fn entry(&self, id: i64) -> Option<&Entry> {
        self.page.as_ref()?.entries.iter().find(|e| e.id == id)
    }

    /// Whether the entry's file is known to be gone.
    pub fn is_orphan(&self, id: i64) -> bool {
        self.orphans.contains(&id)
    }

    /// How many entries have a missing file.
    pub fn orphan_count(&self) -> usize {
        self.orphans.len()
    }

    /// The cached thumbnail state (tests).
    pub fn thumb_state(&mut self, id: i64) -> Option<&ThumbState<TextureHandle>> {
        self.thumbs.get(id)
    }
}

fn open_db(st: &mut State, cx: &Cx<'_>) {
    if st.db.is_some() || st.error.is_some() {
        return;
    }
    match cx.host.history.open() {
        Ok(db) => {
            let w = cx.wake.clone();
            st.loader = Some(Loader::new(db.clone(), 2, move || w()));
            st.db = Some(db);
        }
        Err(e) => st.error = Some(e),
    }
}

fn poll(st: &mut State, cx: &mut Cx<'_>, ctx: &egui::Context) {
    let t = ctx.input(|i| i.time);
    if let Some(r) = st.load.poll() {
        match r {
            Ok((filter, page)) => {
                st.error = None;
                st.uploaders.extend(page.uploaders.iter().cloned());
                // The list shrank below the current page: go to the last one (which reloads).
                st.filter.clamp_page(page.total);
                st.page = Some(page);
                st.loaded_for = Some(filter);
                if let Some(id) = st.selected
                    && st.entry(id).is_none()
                {
                    st.selected = None;
                }
            }
            Err(e) => st.error = Some(e),
        }
    }
    if let Some(Ok(list)) = st.orphan_slot.poll() {
        st.orphans = list.into_iter().map(|o| o.id).collect();
    }
    if let Some(r) = st.op.poll() {
        match r {
            OpResult::Deleted { entries, file_error } => {
                cx.toasts.success(t, format!("Deleted {entries} entr{}", if entries == 1 { "y" } else { "ies" }));
                if let Some(e) = file_error {
                    cx.toasts.error(t, format!("The file could not be deleted: {e}"));
                }
            }
            OpResult::Pruned(n) => cx.toasts.success(t, format!("Pruned {n} entr{}", if n == 1 { "y" } else { "ies" })),
            OpResult::MissingRemoved(n) => cx.toasts.success(t, format!("Removed {n} entr{} whose file is gone", if n == 1 { "y" } else { "ies" })),
            OpResult::Reuploaded(url) => cx.toasts.success(t, format!("Uploaded again: {url}")),
            OpResult::Failed(e) => cx.toasts.error(t, e),
        }
        st.refresh();
    }
    if let Some(l) = &mut st.loader {
        for r in l.drain() {
            let state = match r.result {
                Ok(d) => {
                    let img = ColorImage::from_rgba_unmultiplied([d.width as usize, d.height as usize], &d.rgba);
                    ThumbState::Ready(ctx.load_texture(format!("hist-{}", r.id), img, TextureOptions::LINEAR))
                }
                Err(e) => ThumbState::Missing(e),
            };
            st.thumbs.set(r.id, state);
        }
    }
    // Debounced search box.
    st.search.submit(st.search_edit.clone(), Instant::now());
    if let Some(text) = st.search.take_due(Instant::now()) {
        st.filter.set_text(&text);
    }
    if let Some(wait) = st.search.time_until_due(Instant::now()) {
        ctx.request_repaint_after(wait);
    }
}

fn start_load(st: &mut State, cx: &Cx<'_>) {
    let Some(db) = st.db.clone() else { return };
    let want = st.filter.clone();
    let due = st.next_refresh.is_some_and(|t| Instant::now() >= t);
    if (st.loaded_for.as_ref() == Some(&want) && !due) || st.requested.as_ref() == Some(&want) {
        return;
    }
    if st.load.running() {
        return;
    }
    st.next_refresh = Some(Instant::now() + Duration::from_secs(10));
    st.requested = Some(want.clone());
    let now = cx.now;
    let d2 = db.clone();
    st.load.start(cx.wake, move || load_page(&db, &want, now).map(|p| (want.clone(), p)).map_err(|e| e.to_string()));
    if !st.orphan_slot.running() {
        st.orphan_slot.start(cx.wake, move || d2.find_orphans().map_err(|e| e.to_string()));
    }
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let ctx = ui.ctx().clone();
    if !st.entered {
        st.entered = true;
        st.refresh();
    }
    open_db(st, cx);
    poll(st, cx, &ctx);
    start_load(st, cx);
    if let Some(next) = st.next_refresh {
        ctx.request_repaint_after(next.saturating_duration_since(Instant::now()).max(Duration::from_millis(200)));
    }

    if let Some(e) = st.error.clone() {
        ui_kit::card(ui, Some("The history cannot be opened"), |ui| {
            ui.add(egui::Label::new(RichText::new(&e).color(ui_kit::ERROR_TEXT)).wrap());
            ui_kit::hint(ui, "The file is never deleted or replaced by this window. Check its permissions, or move it aside yourself to start a new history.");
            if ui_kit::button(ui, "Try again").clicked() {
                st.error = None;
                st.reset_source();
            }
        });
        return;
    }

    toolbar(ui, st, cx);
    ui.add_space(8.0);
    let footer_h = 34.0;
    let avail = ui.available_height() - footer_h;
    let total_w = ui.available_width();
    let detail_w = 330.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.allocate_ui(vec2((total_w - detail_w - 30.0).max(300.0), avail.max(200.0)), |ui| {
            egui::ScrollArea::vertical().id_salt("history-scroll").auto_shrink([false, false]).show(ui, |ui| match st.view {
                View::Grid => grid(ui, st, cx),
                View::List => list(ui, st, cx),
            });
        });
        ui.allocate_ui(vec2(detail_w, avail.max(200.0)), |ui| {
            egui::ScrollArea::vertical().id_salt("history-detail").auto_shrink([false, false]).show(ui, |ui| detail(ui, st, cx));
        });
    });
    paging(ui, st);
    dialogs(&ctx, st, cx);
}

fn toolbar(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, None, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            input_style(ui);
            let r = ui_kit::text_input(ui, "Search the history", &mut st.search_edit, "Search names, links, titles, notes...", 260.0);
            let _ = r;
            for k in KINDS {
                if ui_kit::chip(ui, kind_label(k), st.filter.has_kind(k)).clicked() {
                    st.filter.toggle_kind(k);
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            input_style(ui);
            let range = egui::ComboBox::from_id_salt("history-range").selected_text(st.filter.range.label()).show_ui(ui, |ui| {
                for r in DateRange::ALL {
                    if ui.selectable_label(st.filter.range == r, r.label()).clicked() {
                        st.filter.set_range(r);
                    }
                }
            });
            range.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Date range"));
            if st.filter.range == DateRange::Custom {
                ui_kit::text_input(ui, "From date", &mut st.filter.from_text, "2025-01-01", 96.0);
                ui.label("to");
                ui_kit::text_input(ui, "To date", &mut st.filter.to_text, "2025-01-31", 96.0);
                for p in st.filter.date_problems() {
                    ui.label(RichText::new(p).size(12.0).color(ui_kit::ERROR_TEXT));
                }
            }
            let selected = st.filter.uploader.clone();
            let mut names: BTreeSet<String> = st.uploaders.clone();
            names.extend(cx.registry.names());
            if let Some(u) = &selected {
                names.insert(u.clone());
            }
            let up = egui::ComboBox::from_id_salt("history-uploader")
                .selected_text(selected.clone().unwrap_or_else(|| "Any uploader".to_owned()))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(selected.is_none(), "Any uploader").clicked() {
                        st.filter.set_uploader(None);
                    }
                    for n in &names {
                        if ui.selectable_label(selected.as_deref() == Some(n.as_str()), n).clicked() {
                            st.filter.set_uploader(Some(n.clone()));
                        }
                    }
                });
            up.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Uploader"));
            let mut only = st.filter.uploaded_only;
            if ui_kit::switch(ui, "Uploaded only", &mut only).changed() {
                st.filter.set_uploaded_only(only);
            }
            if st.filter.is_filtering() && ui_kit::button(ui, "Clear filters").clicked() {
                st.filter.clear();
                st.search_edit.clear();
                let _ = st.search.take_now();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            let options = [(View::Grid, "Grid"), (View::List, "List")];
            if let Some(v) = ui_kit::segmented_row(ui, st.view, &options) {
                st.view = v;
            }
            if ui_kit::button(ui, "Refresh").on_hover_text("Load the newest entries").clicked() {
                st.refresh();
            }
            let policy = prune_policy(&cx.settings.history);
            let limited = policy.max_entries.is_some() || policy.max_age.is_some();
            if ui_kit::button_if(ui, "Prune now...", limited, "No limits are set: see History on the General page").on_hover_text("Remove entries beyond the retention limits").clicked() {
                st.confirm = Some(Confirm::Prune);
            }
            let n = st.orphan_count();
            if ui_kit::button_if(ui, &format!("Remove {n} with missing files..."), n > 0, "Every entry's file still exists").clicked() {
                st.confirm = Some(Confirm::RemoveMissing);
            }
        });
    });
}

fn paging(ui: &mut Ui, st: &mut State) {
    let total = st.page.as_ref().map_or(0, |p| p.total);
    let pages = st.filter.page_count(total);
    ui.horizontal(|ui| {
        input_style(ui);
        let text = if total == 0 {
            if st.load.running() || st.page.is_none() { "Loading...".to_owned() } else { "Nothing here".to_owned() }
        } else {
            let from = st.filter.page * st.filter.page_size + 1;
            let to = (from + st.page.as_ref().map_or(0, |p| p.entries.len())).saturating_sub(1);
            format!("{from}\u{2013}{to} of {total}")
        };
        ui.label(RichText::new(text).color(theme::TEXT));
        if st.page.as_ref().is_some_and(|p| p.truncated) {
            ui_kit::hint(ui, &format!("(the uploader filter looked at the newest {} entries)", crate::history_view::SCAN_LIMIT));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui_kit::button_if(ui, "Next", st.filter.page + 1 < pages, "Last page").clicked() {
                st.filter.page += 1;
            }
            ui.label(RichText::new(format!("Page {} of {pages}", st.filter.page + 1)).color(theme::TEXT_DIM));
            if ui_kit::button_if(ui, "Previous", st.filter.page > 0, "First page").clicked() {
                st.filter.page -= 1;
            }
        });
    });
}

fn kind_icon(k: EntryKind) -> Icon {
    match k {
        EntryKind::Image => Icon::Image,
        EntryKind::Video => Icon::Spotlight,
        EntryKind::File => Icon::Save,
        EntryKind::Text => Icon::Text,
        EntryKind::Url => Icon::Upload,
    }
}

fn request_thumb(st: &mut State, e: &Entry) {
    if st.thumbs.contains(e.id) {
        return;
    }
    if let Some(l) = &mut st.loader
        && l.request(Request { id: e.id, kind: e.kind, path: e.local_path.clone() })
    {
        st.thumbs.set(e.id, ThumbState::Pending);
    }
}

/// Paints entry `e`'s thumbnail (or a placeholder) into `rect`.
fn paint_thumb(ui: &mut Ui, st: &mut State, e: &Entry, rect: egui::Rect) {
    request_thumb(st, e);
    ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(30, 31, 35));
    match st.thumbs.get(e.id) {
        Some(ThumbState::Ready(tex)) => {
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let r = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        Some(ThumbState::Pending) | None => {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "...", egui::FontId::proportional(14.0), theme::TEXT_DIM);
        }
        Some(ThumbState::Missing(_)) => {
            let s = rect.height().min(34.0);
            icons::paint(ui.painter(), kind_icon(e.kind), egui::Rect::from_center_size(rect.center(), vec2(s, s)), IconColors::with_ink(theme::TEXT_DIM));
        }
    }
}

fn subtitle(e: &Entry, now: chrono::DateTime<chrono::FixedOffset>) -> String {
    let mut parts = vec![format_time_relative(e.created_at, now)];
    if let Some(n) = e.size_bytes {
        parts.push(human_bytes(n));
    }
    parts.join("  \u{b7}  ")
}

fn grid(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let Some(page) = st.page.clone() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading the history...");
        });
        return;
    };
    if page.entries.is_empty() {
        empty(ui, st);
        return;
    }
    let card = vec2(164.0, 150.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
        for e in &page.entries {
            let (rect, resp) = ui.allocate_exact_size(card, egui::Sense::click());
            let selected = st.selected == Some(e.id);
            let title = entry_title(e);
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, format!("{} {title}", kind_word(e.kind))));
            if resp.clicked() {
                st.selected = Some(e.id);
            }
            let hovered = resp.hovered();
            let fill = if selected { theme::ACTIVE_BG } else if hovered { theme::HOVER_BG } else { ui_kit::CARD_BG };
            ui.painter().rect_filled(rect, 6.0, fill);
            ui.painter().rect_stroke(rect, 6.0, egui::Stroke::new(1.0, if selected { theme::ACCENT } else { ui_kit::CARD_STROKE }), egui::StrokeKind::Inside);
            if resp.has_focus() {
                ui.painter().rect_stroke(rect.expand(1.0), 7.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
            }
            let thumb = egui::Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(card.x - 12.0, 96.0));
            paint_thumb(ui, st, e, thumb);
            badges(ui, st, e, thumb);
            let text_pos = egui::pos2(rect.left() + 8.0, thumb.bottom() + 6.0);
            let title_galley = ui.painter().layout_job(ui_kit::truncated(&title, egui::FontId::proportional(12.5), if selected { Color32::WHITE } else { theme::TEXT }, card.x - 16.0));
            ui.painter().galley(text_pos, title_galley, theme::TEXT);
            ui.painter().text(text_pos + vec2(0.0, 20.0), egui::Align2::LEFT_TOP, subtitle(e, cx.now), egui::FontId::proportional(11.0), if selected { Color32::from_rgb(200, 210, 230) } else { theme::TEXT_DIM });
            resp.on_hover_text(title);
        }
    });
}

fn badges(ui: &mut Ui, st: &State, e: &Entry, thumb: egui::Rect) {
    let mut x = thumb.right() - 4.0;
    let mut put = |ui: &mut Ui, text: &str, color: Color32, left: bool| {
        let g = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(10.5), color);
        let w = g.size().x + 10.0;
        let r = if left {
            egui::Rect::from_min_size(thumb.min + vec2(4.0, 4.0), vec2(w, 16.0))
        } else {
            let r = egui::Rect::from_min_size(egui::pos2(x - w, thumb.top() + 4.0), vec2(w, 16.0));
            x -= w + 3.0;
            r
        };
        ui.painter().rect_filled(r, 8.0, Color32::from_black_alpha(190));
        ui.painter().galley(r.min + vec2(5.0, 2.0), g, color);
    };
    if st.is_orphan(e.id) {
        put(ui, "file missing", ui_kit::WARN_TEXT, true);
    }
    if e.upload_url.as_deref().is_some_and(|u| !u.is_empty()) {
        put(ui, "uploaded", theme::ACCENT, false);
    }
    if e.kind != EntryKind::Image {
        put(ui, kind_word(e.kind), theme::TEXT, false);
    }
}

fn list(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let Some(page) = st.page.clone() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading the history...");
        });
        return;
    };
    if page.entries.is_empty() {
        empty(ui, st);
        return;
    }
    for e in &page.entries {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), egui::Sense::click());
        let selected = st.selected == Some(e.id);
        let title = entry_title(e);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, format!("{} {title}", kind_word(e.kind))));
        if resp.clicked() {
            st.selected = Some(e.id);
        }
        let fill = if selected { theme::ACTIVE_BG } else if resp.hovered() { theme::HOVER_BG } else { ui_kit::CARD_BG };
        ui.painter().rect_filled(rect, 5.0, fill);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, 5.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
        }
        let thumb = egui::Rect::from_min_size(rect.min + vec2(5.0, 4.0), vec2(56.0, 36.0));
        paint_thumb(ui, st, e, thumb);
        let x = thumb.right() + 10.0;
        let g = ui.painter().layout_job(ui_kit::truncated(&title, egui::FontId::proportional(13.0), if selected { Color32::WHITE } else { theme::TEXT }, (rect.right() - x - 190.0).max(80.0)));
        ui.painter().galley(egui::pos2(x, rect.top() + 7.0), g, theme::TEXT);
        ui.painter().text(egui::pos2(x, rect.top() + 25.0), egui::Align2::LEFT_TOP, subtitle(e, cx.now), egui::FontId::proportional(11.0), theme::TEXT_DIM);
        let mut rx = rect.right() - 10.0;
        for (text, color) in [
            (st.is_orphan(e.id).then_some("file missing"), ui_kit::WARN_TEXT),
            (e.uploader.as_deref(), theme::ACCENT),
            (Some(kind_word(e.kind)), theme::TEXT_DIM),
        ] {
            if let Some(t) = text {
                let g = ui.painter().layout_no_wrap(t.to_owned(), egui::FontId::proportional(11.5), color);
                rx -= g.size().x;
                ui.painter().galley(egui::pos2(rx, rect.center().y - g.size().y / 2.0), g, color);
                rx -= 14.0;
            }
        }
        resp.on_hover_text(title);
        ui.add_space(2.0);
    }
    let _ = cx;
}

fn empty(ui: &mut Ui, st: &State) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        if st.filter.is_filtering() {
            ui.label(RichText::new("No entry matches these filters.").size(15.0).color(theme::TEXT));
            ui_kit::hint(ui, "Try fewer words, or clear the filters.");
        } else {
            ui.label(RichText::new("The history is empty.").size(15.0).color(theme::TEXT));
            ui_kit::hint(ui, "Screenshots and uploads appear here once ssx has made some.");
        }
    });
}

// ---- detail pane --------------------------------------------------------------------------

fn detail(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let Some(e) = st.selected.and_then(|id| st.entry(id).cloned()) else {
        ui_kit::card(ui, None, |ui| {
            ui_kit::hint(ui, "Select an entry to see its details and what you can do with it.");
        });
        return;
    };
    let file_exists = e.local_path.as_deref().is_some_and(|p| p.exists());
    let folder_exists = e.local_path.as_deref().and_then(Path::parent).is_some_and(Path::exists);
    let actions = EntryActions::of(&e, file_exists && !st.is_orphan(e.id), folder_exists);
    ui_kit::card(ui, None, |ui| {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 170.0), egui::Sense::hover());
        paint_thumb(ui, st, &e, rect);
        ui.add_space(8.0);
        ui.label(RichText::new(entry_title(&e)).strong().size(14.0).color(Color32::WHITE));
        ui.horizontal_wrapped(|ui| {
            ui_kit::badge(ui, kind_word(e.kind), theme::ACCENT);
            if actions.orphan {
                ui_kit::badge(ui, "file missing", ui_kit::WARN_TEXT);
            }
            if let Some(u) = &e.uploader {
                ui_kit::badge(ui, u, ui_kit::OK_TEXT);
            }
        });
        ui.add_space(6.0);
        let tz = *cx.now.offset();
        let mut rows: Vec<(&str, String)> = vec![("When", format_time(e.created_at, tz))];
        if let Some(p) = &e.local_path {
            rows.push(("File", p.display().to_string()));
        }
        if let Some(n) = e.size_bytes {
            rows.push(("Size", human_bytes(n)));
        }
        if let (Some(w), Some(h)) = (e.width, e.height) {
            rows.push(("Dimensions", format!("{w} x {h}")));
        }
        if let Some(u) = &e.upload_url {
            rows.push(("Link", u.clone()));
        }
        if let Some(u) = &e.deletion_url {
            rows.push(("Delete link", u.clone()));
        }
        if let Some(t) = &e.window_title {
            rows.push(("Window", t.clone()));
        }
        if let Some(t) = &e.process_name {
            rows.push(("Program", t.clone()));
        }
        if let Some(t) = &e.workflow_id {
            rows.push(("Workflow", t.clone()));
        }
        if let Some(t) = &e.note {
            rows.push(("Note", t.clone()));
        }
        for (k, v) in rows {
            ui.label(RichText::new(k).size(11.5).color(theme::TEXT_DIM));
            ui.add(egui::Label::new(RichText::new(v).size(12.5)).wrap().selectable(true));
            ui.add_space(3.0);
        }
        if actions.orphan {
            ui.horizontal_top(|ui| {
                ui_kit::severity_icon(ui, ssx_core::settings::Severity::Warning);
                ui.add(egui::Label::new(RichText::new("The file was moved or deleted. The entry and its link are still here.").size(12.0).color(ui_kit::WARN_TEXT)).wrap());
            });
        }
    });
    ui_kit::card(ui, Some("Actions"), |ui| {
        let t = cx.time(ui.ctx());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            if ui_kit::button_if(ui, "Copy link", actions.copy_url, "This entry has no link").clicked()
                && let Some(u) = &e.upload_url
            {
                ui.ctx().copy_text(u.clone());
                cx.toasts.success(t, "Link copied");
            }
            if ui_kit::button_if(ui, "Open link", actions.open_url, "This entry has no web link").clicked()
                && let Some(u) = &e.upload_url
                && let Err(err) = cx.host.opener.open_url(u)
            {
                cx.toasts.error(t, err);
            }
            if ui_kit::button_if(ui, "Open file", actions.open_file, "The file does not exist").clicked()
                && let Some(p) = &e.local_path
                && let Err(err) = cx.host.opener.open_path(p)
            {
                cx.toasts.error(t, err);
            }
            if ui_kit::button_if(ui, "Open folder", actions.open_folder, "The folder does not exist").clicked()
                && let Some(d) = e.local_path.as_deref().and_then(Path::parent)
                && let Err(err) = cx.host.opener.open_path(d)
            {
                cx.toasts.error(t, err);
            }
            if ui_kit::button_if(ui, "Upload again...", actions.reupload, "There is no file to upload").clicked() {
                st.reupload = Some(Reupload { id: e.id, ..Reupload::default() });
            }
            if actions.has_deletion_url
                && ui_kit::button(ui, "Open delete link").on_hover_text("Open the link that removes the uploaded copy").clicked()
                && let Some(u) = &e.deletion_url
                && let Err(err) = cx.host.opener.open_url(u)
            {
                cx.toasts.error(t, err);
            }
        });
        ui.add_space(6.0);
        if ui_kit::danger(ui, "Delete this entry...").clicked() {
            st.confirm = Some(Confirm::Delete(e.id));
            st.delete_file = false;
        }
    });
}

// ---- dialogs and operations ---------------------------------------------------------------

fn dialogs(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    confirm_dialog(ctx, st, cx);
    reupload_dialog(ctx, st, cx);
}

fn confirm_dialog(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(c) = st.confirm.clone() else { return };
    let Some(db) = st.db.clone() else {
        st.confirm = None;
        return;
    };
    let mut answer = None;
    let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
    let entry = if let Confirm::Delete(id) = &c { st.entry(*id).cloned() } else { None };
    let m = egui::Modal::new(egui::Id::new("history-confirm")).frame(frame).show(ctx, |ui| {
        input_style(ui);
        ui.set_width(440.0);
        match &c {
            Confirm::Delete(_) => {
                ui.label(RichText::new("Delete this history entry?").heading().strong());
                ui.add_space(6.0);
                ui.add(egui::Label::new("The entry is removed from the history. An uploaded copy stays online (use its delete link to remove it).").wrap());
                if let Some(p) = entry.as_ref().and_then(|e| e.local_path.as_ref()).filter(|p| p.exists()) {
                    ui.add_space(8.0);
                    ui_kit::switch(ui, "Also delete the file on disk", &mut st.delete_file);
                    if st.delete_file {
                        ui.label(RichText::new(format!("{} will be deleted for good.", p.display())).size(12.0).color(ui_kit::WARN_TEXT));
                    }
                }
            }
            Confirm::Prune => {
                ui.label(RichText::new("Prune the history?").heading().strong());
                ui.add_space(6.0);
                let h = &cx.settings.history;
                let mut parts = Vec::new();
                if h.max_entries > 0 {
                    parts.push(format!("keep at most {} entries", h.max_entries));
                }
                if h.max_age_days > 0 {
                    parts.push(format!("drop entries older than {} days", h.max_age_days));
                }
                ui.add(egui::Label::new(format!("This will {}. Files on disk are not touched.", parts.join(" and "))).wrap());
            }
            Confirm::RemoveMissing => {
                ui.label(RichText::new("Remove entries whose file is gone?").heading().strong());
                ui.add_space(6.0);
                ui.add(egui::Label::new(format!("{} entries point to files that no longer exist. They are removed from the history; links stay valid on the servers.", st.orphan_count())).wrap());
            }
        }
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = match &c {
                Confirm::Delete(_) => "Delete",
                Confirm::Prune => "Prune",
                Confirm::RemoveMissing => "Remove",
            };
            if ui_kit::danger(ui, label).clicked() {
                answer = Some(Answer::Confirm);
            }
            if ui_kit::button(ui, "Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if answer.is_none() && m.should_close() {
        answer = Some(Answer::Cancel);
    }
    match answer {
        Some(Answer::Confirm) => {
            let also_file = st.delete_file;
            let policy = prune_policy(&cx.settings.history);
            match c {
                Confirm::Delete(id) => {
                    let path = entry.and_then(|e| e.local_path);
                    st.op.start(cx.wake, move || delete_entry(&db, id, path.as_deref().filter(|_| also_file)));
                    st.selected = None;
                }
                Confirm::Prune => st.op.start(cx.wake, move || match db.prune(&policy, now_ms()) {
                    Ok(n) => OpResult::Pruned(n),
                    Err(e) => OpResult::Failed(e.to_string()),
                }),
                Confirm::RemoveMissing => st.op.start(cx.wake, move || match db.remove_orphans() {
                    Ok(n) => OpResult::MissingRemoved(n),
                    Err(e) => OpResult::Failed(e.to_string()),
                }),
            }
            st.confirm = None;
        }
        Some(Answer::Cancel) => st.confirm = None,
        None => {}
    }
}

/// Deletes an entry and, if asked, its file. The entry is removed even when the file cannot
/// be (the message says so).
pub fn delete_entry(db: &History, id: i64, file: Option<&Path>) -> OpResult {
    let file_error = file.and_then(|p| match std::fs::remove_file(p) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("{}: {e}", p.display())),
    });
    match db.delete(id) {
        Ok(existed) => OpResult::Deleted { entries: usize::from(existed), file_error },
        Err(e) => OpResult::Failed(e.to_string()),
    }
}

fn reupload_dialog(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(mut r) = st.reupload.take() else { return };
    let Some(e) = st.entry(r.id).cloned() else { return };
    let ty = destination_for(e.kind);
    let ext = e.local_path.as_deref().and_then(|p| p.extension()).map(|x| x.to_string_lossy().into_owned());
    let default = cx.settings.destinations.resolve(ty, &Default::default(), ext.as_deref()).map(str::to_owned);
    let none_label = match &default {
        Some(d) => format!("Default for {} ({d})", crate::uploader_registry::type_word(ty)),
        None => format!("Default for {} (not set)", crate::uploader_registry::type_word(ty)),
    };
    let chosen = r.destination.clone().or(default);
    let running = r.slot.running();
    let mut answer = None;
    let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
    let registry = cx.registry.clone();
    let m = egui::Modal::new(egui::Id::new("history-reupload")).frame(frame).show(ctx, |ui| {
        input_style(ui);
        ui.set_width(460.0);
        ui.label(RichText::new("Upload again").heading().strong());
        ui.add_space(6.0);
        ui.label(entry_title(&e));
        ui.add_space(8.0);
        ui.add_enabled_ui(!running, |ui| {
            destination_picker(ui, "reupload-dest", "Destination", &mut r.destination, ty, &registry, &none_label);
        });
        if running {
            ui.add_space(8.0);
            let (sent, total) = *r.progress.lock().unwrap_or_else(PoisonError::into_inner).as_ref().unwrap_or(&(0, None));
            let bar = match super::uploaders::progress_fraction(sent, total) {
                Some(f) => egui::ProgressBar::new(f).text(super::uploaders::progress_text(sent, total)),
                None => egui::ProgressBar::new(0.0).animate(true).text("uploading..."),
            };
            ui.add(bar.desired_width(400.0));
        }
        if let Some(err) = &r.error {
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(err).color(ui_kit::ERROR_TEXT)).wrap());
        }
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !running && ui_kit::primary(ui, "Upload", chosen.is_some()).clicked() && chosen.is_some() {
                answer = Some(Answer::Confirm);
            }
            let label = if running { "Cancel upload" } else { "Close" };
            if ui_kit::button(ui, label).clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if answer.is_none() && m.should_close() {
        answer = Some(Answer::Cancel);
    }
    if let Some(res) = r.slot.poll() {
        match res {
            Ok((outcome, dest)) => {
                if let Some(db) = st.db.clone() {
                    let info = UploadInfo {
                        url: Some(outcome.url.clone()),
                        thumbnail_url: outcome.thumbnail_url.clone(),
                        deletion_url: outcome.deletion_url.clone(),
                        uploader: Some(dest),
                    };
                    let id = r.id;
                    let url = outcome.url;
                    st.op.start(cx.wake, move || match db.update_upload(id, &info) {
                        Ok(_) => OpResult::Reuploaded(url),
                        Err(e) => OpResult::Failed(e.to_string()),
                    });
                }
                return;
            }
            Err(e) => r.error = Some(e),
        }
    }
    match answer {
        Some(Answer::Confirm) => {
            if let (Some(dest), Some(path)) = (chosen, e.local_path.clone()) {
                start_reupload(&mut r, cx, &dest, &path, ty);
            }
            st.reupload = Some(r);
        }
        Some(Answer::Cancel) => {
            if let Some(c) = &r.cancel {
                c.cancel();
            }
        }
        None => st.reupload = Some(r),
    }
}

fn start_reupload(r: &mut Reupload, cx: &Cx<'_>, dest: &str, path: &Path, ty: DestinationType) {
    let cancel = CancelToken::new();
    let progress = Arc::new(Mutex::new(None));
    let job = TestJob {
        settings: cx.settings.clone(),
        config_dir: cx.host.paths.config_dir.clone(),
        name: dest.to_owned(),
        secrets: cx.host.vault.store(),
    };
    let (up, path, c, p, name) = (cx.host.uploads.clone(), path.to_path_buf(), cancel.clone(), progress.clone(), dest.to_owned());
    let _ = mime_for;
    r.error = None;
    r.cancel = Some(cancel);
    r.progress = progress;
    r.slot.start(cx.wake, move || {
        up.upload_file(&job, &path, ty, &c, &|pr| {
            *p.lock().unwrap_or_else(PoisonError::into_inner) = Some((pr.sent, pr.total));
        })
        .map(|o| (o, name))
    });
}

#[cfg(test)]
mod tests {
    use ssx_core::history::NewEntry;

    use super::*;

    #[test]
    fn deleting_an_entry_can_take_the_file_along() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let db = History::open_in_memory().unwrap();
        let mut e = NewEntry::new(EntryKind::Image);
        e.local_path = Some(file.clone());
        let id = db.insert(&e).unwrap();
        assert_eq!(delete_entry(&db, id, None), OpResult::Deleted { entries: 1, file_error: None });
        assert!(file.exists(), "the file stays unless asked");
        let id = db.insert(&e).unwrap();
        assert_eq!(delete_entry(&db, id, Some(&file)), OpResult::Deleted { entries: 1, file_error: None });
        assert!(!file.exists());
        // a file that is already gone is not an error; a missing entry deletes nothing
        let id = db.insert(&e).unwrap();
        assert_eq!(delete_entry(&db, id, Some(&file)), OpResult::Deleted { entries: 1, file_error: None });
        assert_eq!(delete_entry(&db, 9999, None), OpResult::Deleted { entries: 0, file_error: None });
    }

    #[test]
    fn an_undeletable_file_is_reported_but_the_entry_is_still_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).unwrap();
        let db = History::open_in_memory().unwrap();
        let mut e = NewEntry::new(EntryKind::File);
        e.local_path = Some(sub.clone());
        let id = db.insert(&e).unwrap();
        match delete_entry(&db, id, Some(&sub)) {
            OpResult::Deleted { entries: 1, file_error: Some(msg) } => assert!(msg.contains("d"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subtitles_show_time_and_size() {
        let now = chrono::DateTime::parse_from_rfc3339("2025-03-15T14:30:00+02:00").unwrap();
        let mut e = NewEntry::new(EntryKind::Image);
        e.created_at = chrono::DateTime::parse_from_rfc3339("2025-03-15T09:10:00+02:00").unwrap().timestamp_millis();
        e.size_bytes = Some(2048);
        let db = History::open_in_memory().unwrap();
        let id = db.insert(&e).unwrap();
        let entry = db.get(id).unwrap().unwrap();
        assert_eq!(subtitle(&entry, now), "Today 09:10  \u{b7}  2.0 KB");
    }
}
