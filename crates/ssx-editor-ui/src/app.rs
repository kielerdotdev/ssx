//! [`EditorApp`]: wires the document, the window state and the panels together and implements
//! what every [`Action`] does.
//!
//! The split that matters for testing: [`EditorApp::apply`] and [`EditorApp::process_actions`]
//! need no display (they only use the injected [`Services`]), while [`EditorApp::logic`] and
//! [`EditorApp::show`] are the per-frame egui entry points that the eframe shell and the
//! `egui_kittest` harness both call.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use egui::{Context, Event, ImeEvent, Key, ViewportCommand};
use ssx_editor::{
    DocError, EditorSession, Fill, Modifiers, PointF, Tool, object::TextOutline, style::Shadow,
};
use ssx_types::Frame;

use crate::{
    action::{Action, DialogKind, Finish, UnsavedAnswer},
    document::{EditorDoc, Pumped, merge},
    export::{self, ExportError, SaveFormat},
    keymap::{self, Routed},
    prefs::Prefs,
    preview::Preview,
    props::{self, ColorField, PropEdit, Props},
    request::{DevOptions, EditorInput, EditorOutcome, EditorRequest, OutcomeAction, RunError},
    services::{DialogPurpose, DialogReply, Services},
    state::{AppState, Continuation, Dialog},
    tools::ToolId,
    ui::{
        canvas::{Canvas, CanvasInput},
        dialogs, layers, menubar, props_bar, statusbar, theme, toolbar,
    },
};

/// The editor application.
pub struct EditorApp {
    /// Window-level state.
    pub state: AppState,
    /// The open document.
    pub doc: EditorDoc,
    /// The canvas widget state.
    pub canvas: Canvas,
    /// Clipboard and file dialogs.
    pub services: Services,
    /// Effect preview.
    pub preview: Preview,
    /// Set when the session is over; the shell closes the window.
    pub finished: Option<EditorOutcome>,
    /// The last successful save, reported if the window is closed afterwards.
    last_saved: Option<PathBuf>,
    /// The object clipboard holds the newest copy (as opposed to the system clipboard image).
    internal_clip: bool,
    swallow_press: bool,
    last_title: String,
    pending_clipboard_text: Option<String>,
    frame_index: u64,
    dev: DevOptions,
    bench: Option<crate::bench::Bench>,
    themed: bool,
    temp_serial: u32,
    /// The window was asked to close and we answered with the unsaved-changes flow.
    exiting: bool,
}

impl std::fmt::Debug for EditorApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorApp").field("doc", &self.doc).finish_non_exhaustive()
    }
}

/// Where a plain "Save" writes.
enum SaveTarget {
    Image(PathBuf),
    Project(PathBuf),
}

impl EditorApp {
    /// Builds the app around an already loaded document.
    pub fn new(
        doc: EditorDoc,
        output: Option<PathBuf>,
        prefs: Prefs,
        services: Services,
        dev: DevOptions,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let mut state = AppState::new(prefs);
        state.output = output;
        let (cw, ch) = doc.doc().canvas_size();
        let bench = dev.bench.map(crate::bench::Bench::new);
        let mut app = Self {
            state,
            canvas: Canvas::new(egui::vec2(cw as f32, ch as f32)),
            doc,
            services,
            preview: Preview::new(wake),
            finished: None,
            last_saved: None,
            internal_clip: false,
            swallow_press: false,
            last_title: String::new(),
            pending_clipboard_text: None,
            frame_index: 0,
            dev,
            bench,
            themed: false,
            temp_serial: 0,
            exiting: false,
        };
        app.install_document_state();
        app
    }

    /// Convenience for tests: fake services, ephemeral prefs.
    pub fn for_test(doc: EditorDoc) -> Self {
        Self::new(doc, None, Prefs::default(), Services::fake(), DevOptions::default(), || {})
    }

    /// Loads the request's input into a document (before any window exists, so errors are
    /// reported plainly).
    pub fn load_input(
        request: &EditorRequest,
        services: &mut Services,
    ) -> Result<EditorDoc, RunError> {
        match &request.input {
            EditorInput::Path(p) => EditorDoc::open(p),
            EditorInput::Frame(f) => EditorDoc::from_frame(f.clone()),
            EditorInput::Clipboard => {
                let frame = services.clipboard.image().ok_or(RunError::NoClipboardImage)?;
                EditorDoc::from_frame(frame)
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Document lifecycle
    // ---------------------------------------------------------------------------------------

    /// Prepares a freshly installed document: remembered tool styles, tool and canvas.
    fn install_document_state(&mut self) {
        *self.doc.session.styles_mut() = self.state.prefs.styles.clone();
        let tool = self.state.tool;
        self.select_tool(tool);
        let (cw, ch) = self.doc.doc().canvas_size();
        self.canvas.reset_for_new_document(egui::vec2(cw as f32, ch as f32));
        self.state.image_size = self.doc.doc().image_size();
        self.state.crop_pending = false;
        self.state.has_pending_image = false;
        self.preview.clear();
        let _ = self.doc.pump();
        let _ = self.doc.take_render_events();
    }

    /// Replaces the open document.
    pub fn replace_document(&mut self, doc: EditorDoc) {
        self.remember_styles();
        self.doc = doc;
        self.last_saved = None;
        self.install_document_state();
    }

    fn remember_styles(&mut self) {
        self.state.prefs.styles = self.doc.session.styles().clone();
    }

    // ---------------------------------------------------------------------------------------
    // Per-frame entry points
    // ---------------------------------------------------------------------------------------

    /// Everything that happens before drawing: services, keyboard, drops, close requests and
    /// the queued actions.
    pub fn logic(&mut self, ctx: &Context) {
        if !self.themed {
            theme::apply(ctx);
            self.themed = true;
        }
        while let Some(reply) = self.services.dialogs.poll() {
            self.handle_dialog_reply(reply);
        }
        if self.preview.poll() {
            ctx.request_repaint();
        }
        self.drive_effect_preview();
        if let Some(text) = self.pending_clipboard_text.take() {
            ctx.copy_text(text);
        }
        self.swallow_press = egui::Popup::is_any_open(ctx);
        self.track_window(ctx);
        if ctx.input(|i| i.viewport().close_requested()) && !self.exiting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.state.push(Action::RequestClose);
        }
        self.handle_dropped_files(ctx);
        self.handle_keyboard(ctx);
        self.process_actions();
        self.apply_picked_color();
        self.update_title(ctx);
        self.state.expire_toast(Instant::now());
        if self.state.toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.frame_index += 1;
        self.run_dev_hooks(ctx);
        if self.finished.is_some() {
            self.exiting = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    /// Draws all panels, the canvas and the dialogs.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.show_menubar(ui);
        self.show_toolbar(ui);
        self.show_props(ui);
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::symmetric(10, 3)),
            )
            .show_separator_line(false)
            .show(ui, |ui| statusbar::show(ui, &mut self.state));
        if self.state.prefs.show_layers {
            egui::Panel::right("layers")
                .default_size(230.0)
                .size_range(180.0..=360.0)
                .frame(egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::same(8)))
                .show(ui, |ui| layers::show(ui, &mut self.state, &self.doc));
        }
        // Actions from the bars take effect in the same frame.
        self.process_actions();

        let swallow = self.swallow_press;
        let mut pumped = Pumped::default();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::CANVAS_BG)).show(
            ui,
            |ui| {
                pumped = self.canvas.show(
                    ui,
                    CanvasInput {
                        doc: &mut self.doc,
                        state: &mut self.state,
                        preview: &mut self.preview,
                        swallow_press: swallow,
                    },
                );
                self.canvas.crop_buttons(ui, &self.doc, &mut self.state);
            },
        );
        self.absorb(&ctx, &pumped);
        self.drop_overlay(&ctx);
        dialogs::show(&ctx, &mut self.state, &self.doc, &self.preview);
        self.process_actions();
        if self.preview.active() && !matches!(self.state.dialog, Some(Dialog::Effect(_))) {
            self.preview.clear();
        }
        self.state.crop_pending = self.doc.session.pending_crop().is_some();
        self.state.text_editing = self.doc.session.text_edit_state().is_some();
    }

    /// The menu / undo / finish row (exposed so tests can snapshot the bars on their own).
    pub fn show_menubar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menubar")
            .frame(
                egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show_separator_line(false)
            .show(ui, |ui| menubar::show(ui, &mut self.state, &self.doc));
    }

    /// The tool bar row.
    pub fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(theme::TOOLBAR_BG)
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show_separator_line(false)
            .show(ui, |ui| toolbar::show(ui, &mut self.state, &self.doc));
    }

    /// The properties bar row.
    pub fn show_props(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("props")
            .frame(
                egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::symmetric(10, 5)),
            )
            .show_separator_line(false)
            .show(ui, |ui| props_bar::show(ui, &mut self.state, &self.doc));
    }

    fn drop_overlay(&self, ctx: &Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        let rect = ctx.content_rect();
        let painter = ctx
            .layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drop-overlay")));
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(150));
        painter.rect_stroke(
            rect.shrink(12.0),
            12.0,
            egui::Stroke::new(3.0, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop an image to open it\n(hold Shift to insert it as an object)",
            egui::FontId::proportional(20.0),
            egui::Color32::WHITE,
        );
    }

    /// Reacts to what the session reported while the canvas handled input.
    fn absorb(&mut self, ctx: &Context, p: &Pumped) {
        if let Some(t) = p.tool_changed {
            let ui_tool = ToolId::from_engine(t);
            if self.state.tool.engine() != t {
                self.state.select_tool(ui_tool);
            }
        }
        if let Some(text) = &p.clipboard_text {
            ctx.copy_text(text.clone());
        }
    }

    fn update_title(&mut self, ctx: &Context) {
        let t = self.doc.title();
        if t != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(t.clone()));
            self.last_title = t;
        }
    }

    fn track_window(&mut self, ctx: &Context) {
        ctx.input(|i| {
            let v = i.viewport();
            let w = &mut self.state.prefs.window;
            w.maximized = v.maximized.unwrap_or(false);
            if !w.maximized && v.fullscreen != Some(true) {
                if let Some(r) = v.inner_rect {
                    if r.width() > 100.0 && r.height() > 100.0 {
                        w.width = r.width();
                        w.height = r.height();
                    }
                }
                if let Some(r) = v.outer_rect {
                    w.x = Some(r.min.x);
                    w.y = Some(r.min.y);
                }
            }
        });
    }

    fn drive_effect_preview(&mut self) {
        if let Some(Dialog::Effect(f)) = &self.state.dialog {
            let e = f.effect();
            self.preview.request(self.doc.doc(), e);
        }
    }

    fn run_dev_hooks(&mut self, ctx: &Context) {
        if let Some(n) = self.dev.exit_after_frames {
            // egui repaints on demand; a frame-count exit needs frames to keep coming.
            ctx.request_repaint();
            if self.frame_index >= u64::from(n) && self.finished.is_none() {
                self.finished = Some(EditorOutcome::cancelled());
            }
        }
        if let Some(mut b) = self.bench.take() {
            let done = b.step(self, ctx);
            if done {
                b.report(self);
                if self.finished.is_none() {
                    self.finished = Some(EditorOutcome::cancelled());
                }
            } else {
                ctx.request_repaint();
            }
            self.bench = Some(b);
        }
    }

    // ---------------------------------------------------------------------------------------
    // Input
    // ---------------------------------------------------------------------------------------

    fn handle_dropped_files(&mut self, ctx: &Context) {
        let (files, shift) = ctx.input(|i| (i.raw.dropped_files.clone(), i.modifiers.shift));
        if self.state.dialog.is_some() {
            return;
        }
        for f in files {
            let Some(path) = f.path else { continue };
            if shift {
                match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| {
                    self.doc.session.insert_image_bytes(&b, None).map_err(|e| e.to_string())
                }) {
                    Ok(()) => {}
                    Err(e) => self.error("Cannot insert image", format!("{}: {e}", path.display())),
                }
                let p = self.doc.pump();
                self.absorb(ctx, &p);
            } else {
                self.state.push(Action::OpenPath(path));
            }
            break;
        }
    }

    fn handle_keyboard(&mut self, ctx: &Context) {
        if self.state.dialog.is_some() && !matches!(self.state.dialog, Some(Dialog::Effect(_))) {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        let wants = ctx.egui_wants_keyboard_input();
        let editing = self.doc.session.text_edit_state().is_some();
        if wants && !editing {
            return;
        }
        let mut pumped = Pumped::default();
        for ev in events {
            match ev {
                Event::Key { key, pressed: true, modifiers, repeat, .. } => {
                    if key == Key::Escape && self.state.eyedropper.is_some() {
                        self.state.eyedropper = None;
                        continue;
                    }
                    match keymap::route(key, &modifiers, editing) {
                        Routed::App(a) => {
                            let repeatable = matches!(
                                a,
                                Action::Undo
                                    | Action::Redo
                                    | Action::ZoomIn
                                    | Action::ZoomOut
                                    | Action::DeleteSelection
                            );
                            if !repeat || repeatable {
                                self.state.push(a);
                            }
                        }
                        Routed::Session(k, m) => {
                            let used = self.doc.session.key_down(k, m);
                            if !used && key == Key::Escape && self.state.tool != ToolId::Select {
                                self.state.push(Action::SetTool(ToolId::Select));
                            }
                        }
                        Routed::Ignore => {}
                    }
                }
                Event::Text(s) if editing => {
                    let clean: String =
                        s.chars().filter(|c| !c.is_control() || *c == '\n').collect();
                    self.doc.session.text_insert(&clean);
                }
                Event::Ime(ImeEvent::Preedit { text, .. }) if editing => {
                    self.doc.session.ime_preedit(if text.is_empty() {
                        None
                    } else {
                        Some((text, None))
                    });
                }
                Event::Ime(ImeEvent::Commit(s)) if editing => {
                    self.doc.session.ime_preedit(None);
                    self.doc.session.text_insert(&s);
                }
                Event::Paste(s) if editing => self.doc.session.text_insert(&s),
                Event::Copy if editing => {
                    self.doc.session.key_down(ssx_editor::Key::Char('c'), Modifiers::CTRL);
                }
                Event::Cut if editing => {
                    self.doc.session.key_down(ssx_editor::Key::Char('x'), Modifiers::CTRL);
                }
                _ => {}
            }
        }
        merge(&mut pumped, self.doc.pump());
        self.absorb(ctx, &pumped);
    }

    // ---------------------------------------------------------------------------------------
    // Actions
    // ---------------------------------------------------------------------------------------

    /// Runs queued actions (including ones queued by earlier actions) in order.
    pub fn process_actions(&mut self) {
        let mut guard = 0;
        while self.state.has_actions() && guard < 64 {
            guard += 1;
            for a in self.state.take_actions() {
                self.apply(a);
            }
        }
    }

    fn error(&mut self, title: &str, message: String) {
        tracing::warn!("{title}: {message}");
        self.state.open_dialog(Dialog::Error { title: title.to_owned(), message });
    }

    fn refresh(&mut self) -> Pumped {
        let p = self.doc.pump();
        if let Some(t) = &p.clipboard_text {
            self.pending_clipboard_text = Some(t.clone());
        }
        if let Some(t) = p.tool_changed {
            if self.state.tool.engine() != t {
                self.state.select_tool(ToolId::from_engine(t));
            }
        }
        p
    }

    fn global(&mut self, label: &str, f: impl FnOnce(&mut EditorSession) -> Result<(), DocError>) {
        self.doc.pending_label = Some(label.to_owned());
        if let Err(e) = f(&mut self.doc.session) {
            self.doc.pending_label = None;
            self.error(&format!("Cannot {}", label.to_lowercase()), e.to_string());
        }
        self.refresh();
    }

    fn view_center(&self) -> egui::Pos2 {
        (self.canvas.viewport.view() / 2.0).to_pos2()
    }

    /// Carries out one action.
    #[allow(clippy::too_many_lines)] // a flat dispatch table
    pub fn apply(&mut self, action: Action) {
        // Actions that replace the document ask first when there is something to lose.
        if action.replaces_document() && self.doc.is_dirty() && self.state.after_save.is_none() {
            self.state.open_dialog(Dialog::Unsaved(Continuation::Run(action)));
            return;
        }
        match action {
            // ---- file ----
            Action::NewFromClipboard => match self.services.clipboard.image() {
                Some(f) => match EditorDoc::from_frame(f) {
                    Ok(d) => self.replace_document(d),
                    Err(e) => self.error("Cannot use the clipboard image", e.to_string()),
                },
                None => self
                    .error("Nothing to paste", "The clipboard does not contain an image.".into()),
            },
            Action::Open => {
                let dir = self.state.prefs.last_dir.clone();
                self.services.dialogs.request(DialogPurpose::OpenDocument, dir.as_deref(), None);
            }
            Action::OpenPath(p) => match EditorDoc::open(&p) {
                Ok(d) => {
                    self.state.prefs.last_dir = p.parent().map(Path::to_path_buf);
                    self.replace_document(d);
                    if matches!(self.state.dialog, Some(Dialog::Open(_))) {
                        self.state.close_dialog();
                    }
                }
                Err(e) => self.error("Cannot open the file", e.to_string()),
            },
            Action::Save => self.save(),
            Action::SaveAs => self.open_save_dialog(false),
            Action::SaveProject => match self.doc.project_path.clone() {
                Some(p) => self.write_to(&p, self.state.export_settings()),
                None => self.open_save_dialog(true),
            },
            Action::SaveTo { path, settings } => {
                self.write_to(&path, settings);
                if self.state.dialog.is_none()
                    || matches!(self.state.dialog, Some(Dialog::SaveAs(_)))
                {
                    if !matches!(self.state.dialog, Some(Dialog::Error { .. })) {
                        self.state.close_dialog();
                    }
                }
            }
            Action::BrowseSaveTarget => {
                let (dir, name) = match &self.state.dialog {
                    Some(Dialog::SaveAs(f)) => {
                        let p = PathBuf::from(f.path.trim());
                        (
                            p.parent().map(Path::to_path_buf),
                            p.file_name().and_then(|n| n.to_str()).map(str::to_owned),
                        )
                    }
                    _ => (None, None),
                };
                self.services.dialogs.request(
                    DialogPurpose::SaveTarget,
                    dir.as_deref(),
                    name.as_deref(),
                );
            }
            Action::CopyImage => self.copy_image(),
            Action::Upload => self.done(Finish::Upload),
            Action::Done(f) => self.done(f),
            Action::RequestClose => self.request_close(),
            Action::Unsaved(a) => self.unsaved_answer(a),

            // ---- edit ----
            Action::Undo => {
                self.doc.undo();
                self.refresh();
            }
            Action::Redo => {
                self.doc.redo();
                self.refresh();
            }
            Action::UndoSteps(n) => {
                self.doc.undo_steps(n);
                self.refresh();
            }
            Action::RedoSteps(n) => {
                self.doc.redo_steps(n);
                self.refresh();
            }
            Action::DeleteSelection => {
                self.doc.session.delete_selection();
                self.refresh();
            }
            Action::SelectAll => {
                self.doc.session.select_all();
                self.refresh();
            }
            Action::Deselect => {
                self.doc.session.clear_selection();
                self.refresh();
            }
            Action::SelectObjects { ids, additive } => {
                let mut sel: Vec<_> =
                    if additive { self.doc.session.selection().to_vec() } else { Vec::new() };
                for id in ids {
                    if let Some(i) = sel.iter().position(|s| *s == id) {
                        if additive {
                            sel.remove(i);
                        }
                    } else {
                        sel.push(id);
                    }
                }
                self.doc.session.select(&sel);
                self.refresh();
            }
            Action::SetVisible(id, v) => {
                self.doc.session.set_visible(id, v);
                self.refresh();
            }
            Action::SetLocked(id, v) => {
                self.doc.session.set_locked(id, v);
                self.refresh();
            }
            Action::DeleteObject(id) => {
                self.doc.session.select(&[id]);
                self.doc.session.delete_selection();
                self.refresh();
            }
            Action::Duplicate => {
                self.doc.session.duplicate();
                self.refresh();
            }
            Action::Copy => {
                if self.doc.session.selection().is_empty() {
                    self.copy_image();
                } else {
                    self.doc.session.copy();
                    self.internal_clip = true;
                    self.refresh();
                }
            }
            Action::Cut => {
                if !self.doc.session.selection().is_empty() {
                    self.doc.session.cut();
                    self.internal_clip = true;
                    self.refresh();
                }
            }
            Action::Paste => {
                if self.internal_clip && !self.doc.session.object_clipboard().is_empty() {
                    self.doc.session.paste();
                    self.refresh();
                } else {
                    self.paste_image();
                }
            }
            Action::PasteImage => self.paste_image(),
            Action::Group => {
                self.doc.session.group_selection();
                self.refresh();
            }
            Action::Ungroup => {
                self.doc.session.ungroup_selection();
                self.refresh();
            }
            Action::BringToFront => {
                self.doc.session.bring_to_front();
                self.refresh();
            }
            Action::SendToBack => {
                self.doc.session.send_to_back();
                self.refresh();
            }
            Action::Raise => {
                self.doc.session.raise();
                self.refresh();
            }
            Action::Lower => {
                self.doc.session.lower();
                self.refresh();
            }
            Action::Prop(e) => {
                props::apply(&mut self.doc.session, &e);
                self.refresh();
            }
            Action::SetStepStart(n) => self.global("Set step start", |s| s.set_step_start(n)),

            // ---- tools ----
            Action::SetTool(t) => self.select_tool(t),
            Action::PickImageFile => {
                let dir = self.state.prefs.last_dir.clone();
                self.services.dialogs.request(DialogPurpose::InsertImage, dir.as_deref(), None);
            }
            Action::PickImageClipboard => match self.services.clipboard.image() {
                Some(f) => self.set_pending_image(f),
                None => self.state.toast_error("The clipboard does not contain an image."),
            },

            // ---- view ----
            Action::ZoomIn => {
                let c = self.view_center();
                self.canvas.viewport.step_at(c, true);
            }
            Action::ZoomOut => {
                let c = self.view_center();
                self.canvas.viewport.step_at(c, false);
            }
            Action::ZoomFit => self.canvas.viewport.fit(),
            // The viewport works in physical pixels, so 100 % is one image pixel per screen pixel.
            Action::ZoomActual => self.canvas.viewport.zoom_to_centered(1.0),
            Action::ZoomTo(z) => self.canvas.viewport.zoom_to_centered(z),
            Action::TogglePixelGrid => self.state.prefs.pixel_grid = !self.state.prefs.pixel_grid,
            Action::ToggleLayers => self.state.prefs.show_layers = !self.state.prefs.show_layers,

            // ---- image ----
            Action::Orient(o) => {
                let label = match o {
                    ssx_editor::object::Orient::Rotate90 => "Rotate 90 degrees clockwise",
                    ssx_editor::object::Orient::Rotate180 => "Rotate 180 degrees",
                    ssx_editor::object::Orient::Rotate270 => "Rotate 90 degrees counter-clockwise",
                    ssx_editor::object::Orient::FlipH => "Flip horizontally",
                    ssx_editor::object::Orient::FlipV => "Flip vertically",
                };
                self.global(label, |s| s.orient(o));
            }
            Action::ApplyPendingCrop => self.global("Crop", EditorSession::apply_crop),
            Action::CancelPendingCrop => {
                self.doc.session.cancel_crop();
                self.refresh();
            }
            Action::CropTo(r) => {
                self.global("Crop", |s| s.crop_rect(r));
                self.state.close_dialog();
            }
            Action::AutoCrop(t) => {
                self.global("Auto-crop", |s| s.auto_crop(t));
                self.state.close_dialog();
            }
            Action::CutOut { axis, start, end } => {
                self.global("Cut out", |s| s.cut_out(axis, start, end));
                self.state.close_dialog();
            }
            Action::Resize { width, height, filter } => {
                self.global("Resize image", |s| s.resize(width, height, filter));
                self.state.close_dialog();
            }
            Action::ResizeCanvas { left, top, right, bottom, background } => {
                self.global("Canvas size", |s| {
                    s.resize_canvas(left, top, right, bottom)?;
                    s.set_background(background.map_or(Fill::None, Fill::solid))
                });
                self.state.close_dialog();
            }
            Action::Flatten => self.global("Flatten annotations", EditorSession::flatten),
            Action::ApplyEffect(e) => {
                let label = e.label();
                let precomputed = self.preview.result_for(&e);
                self.global(label, |s| match precomputed {
                    Some(doc) => s.global_op(|d, _| {
                        *d = doc;
                        Ok(())
                    }),
                    None => s.apply_effect(&e, None),
                });
            }
            Action::OpenDialog(kind) => {
                let size = self.doc.doc().image_size();
                let suggested = export::suggest_save_path(
                    self.doc.source.as_deref(),
                    self.state.prefs.last_dir.as_deref(),
                );
                let d = self.state.dialog_for(kind, size, suggested);
                self.state.open_dialog(d);
            }
            Action::OpenEffect(k) => self.state.open_dialog(AppState::effect_dialog(k)),
            Action::CloseDialog => self.state.close_dialog(),

            // ---- misc ----
            Action::ResetToolStyles => {
                self.doc.session.styles_mut().reset_all();
                self.state.prefs.styles.reset_all();
                self.state.toast("Tool styles reset");
            }
            Action::ClearRecentColors => self.state.prefs.recent_colors.clear(),
        }
    }

    /// Switches tool (toolbar identity plus the engine tool and its side effects).
    pub fn select_tool(&mut self, t: ToolId) {
        let prev = self.state.tool;
        // The engine has a single Text preset; keep one per toolbar identity so the plain and
        // the boxed text tools do not overwrite each other's colours.
        let current_text = self.doc.session.styles().get(Tool::Text);
        match prev {
            ToolId::Text => self.state.prefs.text_plain = current_text,
            ToolId::TextBoxed => self.state.prefs.text_boxed = current_text,
            _ => {}
        }
        self.state.select_tool(t);
        let engine = t.engine();
        if t != ToolId::Select {
            self.doc.session.clear_selection();
        }
        self.doc.session.set_tool(engine);
        match t {
            ToolId::Text => match self.state.prefs.text_plain.clone() {
                Some(p) => self.doc.session.styles_mut().remember(Tool::Text, p),
                None => self.doc.session.styles_mut().reset(Tool::Text),
            },
            ToolId::TextBoxed => match self.state.prefs.text_boxed.clone() {
                Some(p) => self.doc.session.styles_mut().remember(Tool::Text, p),
                None => {
                    self.doc.session.styles_mut().reset(Tool::Text);
                    props::make_text_boxed(&mut self.doc.session);
                }
            },
            _ => {}
        }
        if t == ToolId::Image && !self.state.has_pending_image {
            self.state.push(Action::PickImageFile);
        }
        self.refresh();
    }

    fn set_pending_image(&mut self, f: Frame) {
        self.doc.session.set_pending_image(Some(f));
        self.state.has_pending_image = true;
        self.state.toast("Click on the picture to place the image");
    }

    fn paste_image(&mut self) {
        match self.services.clipboard.image() {
            Some(f) => {
                self.doc.session.insert_image(f, None);
                self.internal_clip = false;
                self.refresh();
            }
            None => self.state.toast_error("Nothing to paste"),
        }
    }

    fn copy_image(&mut self) {
        let frame = export::flatten(self.doc.doc());
        match self.services.clipboard.set_image(&frame) {
            Ok(()) => {
                self.internal_clip = false;
                self.state.toast(format!(
                    "Copied {} x {} image to the clipboard",
                    frame.width(),
                    frame.height()
                ));
            }
            Err(e) => self.error("Cannot copy to the clipboard", e),
        }
    }

    // ---- saving --------------------------------------------------------------------------

    fn save_target(&self) -> Option<SaveTarget> {
        if let Some(o) = &self.state.output {
            return Some(SaveTarget::Image(o.clone()));
        }
        if let Some(p) = &self.doc.project_path {
            return Some(SaveTarget::Project(p.clone()));
        }
        self.doc.image_path.clone().map(SaveTarget::Image)
    }

    fn open_save_dialog(&mut self, project: bool) {
        self.doc.session.commit_text_edit();
        let mut suggested = export::suggest_save_path(
            self.doc.source.as_deref(),
            self.state.prefs.last_dir.as_deref(),
        );
        if project {
            suggested.set_extension("ssxe");
        }
        let d = self.state.dialog_for(DialogKind::SaveAs, self.doc.doc().image_size(), suggested);
        self.state.open_dialog(d);
    }

    fn save(&mut self) {
        match self.save_target() {
            Some(SaveTarget::Image(p)) => self.write_to(&p, self.state.export_settings()),
            Some(SaveTarget::Project(p)) => self.write_to(&p, self.state.export_settings()),
            None => self.open_save_dialog(false),
        }
    }

    /// Writes the document to `path` (format from the extension) and updates the bookkeeping.
    fn write_to(&mut self, path: &Path, settings: export::ExportSettings) {
        self.doc.session.commit_text_edit();
        self.refresh();
        let result = match SaveFormat::from_path(path) {
            Some(SaveFormat::Project) => export::write_project(self.doc.doc(), path).map(|()| true),
            Some(SaveFormat::Image(_)) => {
                let frame = export::flatten(self.doc.doc());
                export::write_image(&frame, path, settings).map(|()| false)
            }
            None => Err(ExportError::UnknownFormat(
                path.extension().and_then(|e| e.to_str()).unwrap_or("").into(),
            )),
        };
        match result {
            Ok(is_project) => {
                if is_project {
                    self.doc.project_path = Some(path.to_path_buf());
                } else {
                    self.doc.image_path = Some(path.to_path_buf());
                }
                self.doc.mark_saved();
                self.last_saved = Some(path.to_path_buf());
                self.state.prefs.last_dir = path.parent().map(Path::to_path_buf);
                self.state.toast(format!("Saved {}", path.display()));
                if let Some(c) = self.state.after_save.take() {
                    match c {
                        Continuation::Close => self.finish_after_save(),
                        Continuation::Run(a) => {
                            self.state.after_save = Some(Continuation::Close);
                            self.apply(a);
                            self.state.after_save = None;
                        }
                    }
                }
            }
            Err(e) => {
                self.state.after_save = None;
                self.error("Cannot save the file", e.to_string());
            }
        }
    }

    fn finish_after_save(&mut self) {
        let path = self.last_saved.clone();
        self.finished = Some(EditorOutcome { action: OutcomeAction::Save, path });
    }

    fn temp_export_path(&mut self) -> PathBuf {
        self.temp_serial += 1;
        std::env::temp_dir().join(format!(
            "ssx-editor-{}-{}.png",
            std::process::id(),
            self.temp_serial
        ))
    }

    /// Ends the session the way a "Done" button asks.
    fn done(&mut self, f: Finish) {
        self.doc.session.commit_text_edit();
        self.refresh();
        match f {
            Finish::Cancel => self.request_close_discard(),
            Finish::Save => match self.save_target() {
                Some(SaveTarget::Image(p) | SaveTarget::Project(p)) => {
                    self.state.after_save = Some(Continuation::Close);
                    self.write_to(&p, self.state.export_settings());
                    self.state.after_save = None;
                }
                None => {
                    // Ask where to save, then come back here.
                    self.state.after_save = Some(Continuation::Run(Action::Done(Finish::Save)));
                    self.open_save_dialog(false);
                }
            },
            Finish::Copy => {
                let frame = export::flatten(self.doc.doc());
                if let Err(e) = self.services.clipboard.set_image(&frame) {
                    self.error("Cannot copy to the clipboard", e);
                    return;
                }
                let path = self.state.output.clone();
                if let Some(p) = &path {
                    if let Err(e) = export::write_image(&frame, p, self.state.export_settings()) {
                        self.error("Cannot save the file", e.to_string());
                        return;
                    }
                    self.doc.mark_saved();
                }
                self.finished = Some(EditorOutcome { action: OutcomeAction::Copy, path });
            }
            Finish::Upload => {
                let frame = export::flatten(self.doc.doc());
                let path = match self.state.output.clone() {
                    Some(p) => p,
                    None => self.temp_export_path(),
                };
                match export::write_image(&frame, &path, self.state.export_settings()) {
                    Ok(()) => {
                        self.doc.mark_saved();
                        self.finished =
                            Some(EditorOutcome { action: OutcomeAction::Upload, path: Some(path) });
                    }
                    Err(e) => self.error("Cannot prepare the upload", e.to_string()),
                }
            }
        }
    }

    fn request_close(&mut self) {
        self.doc.session.commit_text_edit();
        self.refresh();
        if self.doc.is_dirty() {
            self.state.open_dialog(Dialog::Unsaved(Continuation::Close));
        } else {
            self.request_close_discard();
        }
    }

    /// Ends the session without further questions: cancelled, unless a save already happened.
    fn request_close_discard(&mut self) {
        self.remember_styles();
        self.finished = Some(match &self.last_saved {
            Some(p) => EditorOutcome { action: OutcomeAction::Save, path: Some(p.clone()) },
            None => EditorOutcome::cancelled(),
        });
    }

    fn unsaved_answer(&mut self, a: UnsavedAnswer) {
        let cont = self.state.after_save.take();
        match a {
            UnsavedAnswer::Cancel => {}
            UnsavedAnswer::Discard => match cont {
                Some(Continuation::Close) => self.request_close_discard(),
                Some(Continuation::Run(act)) => {
                    // Replace the document without asking again.
                    self.doc.mark_saved();
                    self.apply(act);
                }
                None => {}
            },
            UnsavedAnswer::Save => {
                self.state.after_save = cont;
                match self.save_target() {
                    Some(SaveTarget::Image(p) | SaveTarget::Project(p)) => {
                        self.write_to(&p, self.state.export_settings());
                    }
                    None => self.open_save_dialog(false),
                }
            }
        }
    }

    // ---- replies and colours --------------------------------------------------------------

    fn handle_dialog_reply(&mut self, r: DialogReply) {
        let Some(path) = r.path else { return };
        match r.purpose {
            DialogPurpose::OpenDocument => self.state.push(Action::OpenPath(path)),
            DialogPurpose::SaveTarget => {
                if let Some(Dialog::SaveAs(f)) = &mut self.state.dialog {
                    f.path = path.display().to_string();
                    f.sync_format_from_path();
                }
            }
            DialogPurpose::InsertImage => {
                match std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| Frame::decode(&b).map_err(|e| e.to_string()))
                {
                    Ok(f) => {
                        self.state.prefs.last_dir = path.parent().map(Path::to_path_buf);
                        self.set_pending_image(f);
                    }
                    Err(e) => {
                        self.error("Cannot load the image", format!("{}: {e}", path.display()))
                    }
                }
            }
        }
    }

    fn apply_picked_color(&mut self) {
        let Some((field, c)) = self.state.picked.take() else { return };
        self.state.note_color(c);
        let props = Props::current(&self.doc.session, self.state.tool.engine());
        let edit: Option<PropEdit> = match field {
            ColorField::Stroke => Some(PropEdit::Stroke(c)),
            ColorField::Fill => Some(PropEdit::Fill(Fill::solid(c))),
            ColorField::Text => Some(PropEdit::TextColor(c)),
            ColorField::TextOutline => {
                let cur = props.as_ref().and_then(|p| p.text().and_then(|t| t.outline));
                Some(PropEdit::TextOutline(Some(TextOutline {
                    color: c,
                    width: cur.map_or(2.0, |o| o.width),
                })))
            }
            ColorField::TextBackground => Some(PropEdit::TextBackground(Some(c))),
            ColorField::Shadow => {
                let cur = props.as_ref().and_then(|p| p.style.shadow).unwrap_or_default();
                Some(PropEdit::Shadow(Some(Shadow { color: c, ..cur })))
            }
            ColorField::SpotlightDim => Some(PropEdit::SpotlightDim(c)),
            ColorField::StepText => Some(PropEdit::StepText(c)),
            ColorField::Highlight => Some(PropEdit::HighlightColor(c)),
            ColorField::Canvas => {
                if let Some(Dialog::Canvas(f)) = &mut self.state.dialog {
                    f.background = Some(c);
                }
                None
            }
            ColorField::Effect => {
                if let Some(Dialog::Effect(f)) = &mut self.state.dialog {
                    f.color = c.to_array();
                }
                None
            }
        };
        if let Some(e) = edit {
            self.apply(Action::Prop(e));
        }
    }

    /// Finishes: `Some(outcome)` once the session is over.
    pub fn outcome(&self) -> Option<&EditorOutcome> {
        self.finished.as_ref()
    }

    /// Saves preferences (styles, window) at exit.
    pub fn save_prefs(&mut self) {
        self.remember_styles();
        self.state.prefs.last_tool =
            if self.state.tool == ToolId::CutOut { ToolId::Select } else { self.state.tool };
        if !self.dev.ephemeral_state {
            self.state.prefs.save();
        }
    }

    /// Draws a rectangle through the session (bench helper).
    pub(crate) fn bench_draw(&mut self, tool: Tool, a: PointF, b: PointF) {
        self.doc.session.set_tool(tool);
        self.doc.session.pointer_down(a, Modifiers::NONE, None);
        self.doc.session.pointer_move(b, Modifiers::NONE, None);
        self.doc.session.pointer_up(b, Modifiers::NONE);
        self.refresh();
    }

    /// Zoom the canvas about the centre (bench helper).
    pub(crate) fn bench_zoom(&mut self, factor: f32) {
        let c = self.view_center();
        self.canvas.viewport.zoom_at(c, factor);
    }

    /// Pan the canvas (bench helper).
    pub(crate) fn bench_pan(&mut self, d: egui::Vec2) {
        self.canvas.viewport.pan_by(d);
    }
}
