//! [`AppState`]: everything about the window that is not the document and not drawing:
//! the selected tool, the open dialog, the queue of pending [`Action`]s, preferences, the
//! status-bar readouts and transient messages.
//!
//! It has no egui dependency on purpose. All of its rules (tool variants remember their last
//! choice, only one dialog at a time, actions run in order, toasts expire) are unit-tested
//! without a display.

use std::{collections::VecDeque, path::PathBuf, time::{Duration, Instant}};

use ssx_editor::Color;

use crate::{
    action::{Action, DialogKind},
    effects::{EffectForm, EffectKind},
    forms::{CanvasForm, CropForm, CutForm, OpenForm, ResizeForm, SaveForm},
    export::ExportSettings,
    prefs::Prefs,
    props::ColorField,
    tools::{Slot, TOOLBAR, ToolId},
};

/// What to do once an "unsaved changes" question is answered with Save or Discard.
#[derive(Debug, Clone, PartialEq)]
pub enum Continuation {
    /// Close the window (finish as cancelled/saved as appropriate).
    Close,
    /// Run this action (open another file, new from clipboard...).
    Run(Action),
}

/// The open modal dialog, with its form state.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Save as.
    SaveAs(SaveForm),
    /// Open by path.
    Open(OpenForm),
    /// Resize image.
    Resize(ResizeForm),
    /// Canvas size.
    Canvas(CanvasForm),
    /// Numeric crop.
    Crop(CropForm),
    /// Numeric cut-out.
    CutOut(CutForm),
    /// Effect with live preview.
    Effect(EffectForm),
    /// Keyboard cheat sheet.
    Shortcuts,
    /// Preferences.
    Settings,
    /// Unsaved changes question.
    Unsaved(Continuation),
    /// A message the user must acknowledge.
    Error {
        /// Title.
        title: String,
        /// Body.
        message: String,
    },
}

/// A transient message in the status bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    /// Text.
    pub text: String,
    /// Error styling.
    pub error: bool,
    /// When it disappears.
    pub until: Instant,
}

/// What is under the pointer, for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hover {
    /// Image-space pixel under the pointer.
    pub pixel: Option<(i32, i32)>,
    /// Colour of that pixel in the rendered result.
    pub color: Option<[u8; 4]>,
}

/// Window-level state.
#[derive(Debug)]
pub struct AppState {
    /// Persistent preferences.
    pub prefs: Prefs,
    /// The selected toolbar tool.
    pub tool: ToolId,
    /// Open dialog.
    pub dialog: Option<Dialog>,
    queue: VecDeque<Action>,
    /// Current toast.
    pub toast: Option<Toast>,
    /// While set, the next canvas click picks a colour for this field.
    pub eyedropper: Option<ColorField>,
    /// The colour picked by the eyedropper, delivered to the widget that asked.
    pub picked: Option<(ColorField, Color)>,
    /// Where finishing writes the PNG (workflow mode when set).
    pub output: Option<PathBuf>,
    /// What to do after a save triggered from the unsaved-changes prompt completes.
    pub after_save: Option<Continuation>,
    /// Pointer readout.
    pub hover: Hover,
    /// Zoom shown in the status bar (percent).
    pub zoom_percent: f32,
    /// A crop is waiting for Enter.
    pub crop_pending: bool,
    /// The session is editing text.
    pub text_editing: bool,
    /// The current image size in pixels.
    pub image_size: (u32, u32),
    /// The image for the Image tool is loaded.
    pub has_pending_image: bool,
}

impl AppState {
    /// A fresh state around `prefs`.
    pub fn new(prefs: Prefs) -> Self {
        Self {
            tool: prefs.last_tool,
            prefs,
            dialog: None,
            queue: VecDeque::new(),
            toast: None,
            eyedropper: None,
            picked: None,
            output: None,
            after_save: None,
            hover: Hover::default(),
            zoom_percent: 100.0,
            crop_pending: false,
            text_editing: false,
            image_size: (0, 0),
            has_pending_image: false,
        }
    }

    /// Queues an action.
    pub fn push(&mut self, a: Action) {
        self.queue.push_back(a);
    }

    /// Takes all queued actions in order.
    pub fn take_actions(&mut self) -> Vec<Action> {
        self.queue.drain(..).collect()
    }

    /// `true` when actions are waiting.
    pub fn has_actions(&self) -> bool {
        !self.queue.is_empty()
    }

    /// `true` in workflow mode (the caller gave an output path).
    pub fn workflow(&self) -> bool {
        self.output.is_some()
    }

    /// Selects a tool, remembering slot variants (blur/pixelate, highlighter).
    pub fn select_tool(&mut self, tool: ToolId) {
        self.tool = tool;
        self.eyedropper = None;
        match tool {
            ToolId::Blur | ToolId::Pixelate => self.prefs.blur_variant = tool,
            ToolId::Highlight | ToolId::HighlightPen => self.prefs.highlight_variant = tool,
            _ => {}
        }
        if tool != ToolId::CutOut {
            self.prefs.last_tool = tool;
        }
    }

    /// The tool a toolbar slot currently shows (the remembered variant for multi-tool slots).
    pub fn slot_tool(&self, slot: &Slot) -> ToolId {
        if slot.contains(self.prefs.blur_variant) && slot.variants.len() > 1 {
            return self.prefs.blur_variant;
        }
        if slot.contains(self.prefs.highlight_variant) && slot.variants.len() > 1 {
            return self.prefs.highlight_variant;
        }
        slot.variants[0]
    }

    /// The slot containing `tool`.
    pub fn slot_of(tool: ToolId) -> Option<&'static Slot> {
        TOOLBAR.iter().flat_map(|g| g.iter()).find(|s| s.contains(tool))
    }

    /// Opens a dialog (replacing any other).
    pub fn open_dialog(&mut self, d: Dialog) {
        self.dialog = Some(d);
    }

    /// Closes the dialog.
    pub fn close_dialog(&mut self) {
        self.dialog = None;
    }

    /// Shows a message in the status bar for a few seconds.
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: false, until: Instant::now() + Duration::from_secs(4) });
    }

    /// Shows an error message, longer.
    pub fn toast_error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: true, until: Instant::now() + Duration::from_secs(8) });
    }

    /// Drops the toast when it has expired; returns whether one is still showing.
    pub fn expire_toast(&mut self, now: Instant) -> bool {
        if self.toast.as_ref().is_some_and(|t| t.until <= now) {
            self.toast = None;
        }
        self.toast.is_some()
    }

    /// Remembers a colour the user picked.
    pub fn note_color(&mut self, c: Color) {
        self.prefs.remember_color(c);
    }

    /// The encoder settings from the preferences.
    pub fn export_settings(&self) -> ExportSettings {
        ExportSettings { jpeg_quality: self.prefs.jpeg_quality, png_fast: self.prefs.png_fast }
    }

    /// Creates the dialog for `kind` for an image of `size`.
    pub fn dialog_for(&self, kind: DialogKind, size: (u32, u32), suggested_save: PathBuf) -> Dialog {
        match kind {
            DialogKind::SaveAs => Dialog::SaveAs(SaveForm::new(suggested_save, self.export_settings())),
            DialogKind::Open => Dialog::Open(OpenForm {
                path: self.prefs.last_dir.as_ref().map(|d| format!("{}/", d.display())).unwrap_or_default(),
            }),
            DialogKind::Resize => Dialog::Resize(ResizeForm::new(size.0, size.1)),
            DialogKind::Canvas => Dialog::Canvas(CanvasForm::new(size)),
            DialogKind::Crop => Dialog::Crop(CropForm::new(size)),
            DialogKind::CutOut => Dialog::CutOut(CutForm::new(size)),
            DialogKind::Shortcuts => Dialog::Shortcuts,
            DialogKind::Settings => Dialog::Settings,
        }
    }

    /// The dialog for an effect.
    pub fn effect_dialog(kind: EffectKind) -> Dialog {
        Dialog::Effect(EffectForm::new(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> AppState {
        AppState::new(Prefs::default())
    }

    #[test]
    fn actions_run_in_order_and_drain() {
        let mut s = st();
        assert!(!s.has_actions());
        s.push(Action::Undo);
        s.push(Action::Redo);
        s.push(Action::ZoomFit);
        assert!(s.has_actions());
        assert_eq!(s.take_actions(), vec![Action::Undo, Action::Redo, Action::ZoomFit]);
        assert!(s.take_actions().is_empty());
    }

    #[test]
    fn slot_variants_are_remembered() {
        let mut s = st();
        let slot = AppState::slot_of(ToolId::Blur).unwrap();
        assert_eq!(s.slot_tool(slot), ToolId::Blur);
        s.select_tool(ToolId::Pixelate);
        assert_eq!(s.slot_tool(slot), ToolId::Pixelate);
        s.select_tool(ToolId::Rectangle);
        assert_eq!(s.slot_tool(slot), ToolId::Pixelate, "variant sticks after leaving the slot");
        assert_eq!(s.prefs.last_tool, ToolId::Rectangle);
        let hl = AppState::slot_of(ToolId::HighlightPen).unwrap();
        assert_eq!(s.slot_tool(hl), ToolId::Highlight);
        s.select_tool(ToolId::HighlightPen);
        assert_eq!(s.slot_tool(hl), ToolId::HighlightPen);
        // Single tool slots are unaffected.
        let rect = AppState::slot_of(ToolId::Rectangle).unwrap();
        assert_eq!(s.slot_tool(rect), ToolId::Rectangle);
    }

    #[test]
    fn selecting_a_tool_cancels_the_eyedropper() {
        let mut s = st();
        s.eyedropper = Some(ColorField::Stroke);
        s.select_tool(ToolId::Line);
        assert!(s.eyedropper.is_none());
    }

    #[test]
    fn cut_out_is_not_remembered_as_last_tool() {
        let mut s = st();
        s.select_tool(ToolId::Ellipse);
        s.select_tool(ToolId::CutOut);
        assert_eq!(s.prefs.last_tool, ToolId::Ellipse);
    }

    #[test]
    fn toasts_expire() {
        let mut s = st();
        s.toast("hello");
        let now = Instant::now();
        assert!(s.expire_toast(now));
        assert!(!s.expire_toast(now + Duration::from_secs(60)));
        s.toast_error("bad");
        assert!(s.toast.as_ref().unwrap().error);
    }

    #[test]
    fn one_dialog_at_a_time() {
        let mut s = st();
        s.open_dialog(Dialog::Shortcuts);
        s.open_dialog(Dialog::Settings);
        assert_eq!(s.dialog, Some(Dialog::Settings));
        s.close_dialog();
        assert!(s.dialog.is_none());
    }

    #[test]
    fn dialogs_are_prefilled_from_the_image() {
        let s = st();
        let d = s.dialog_for(DialogKind::Resize, (640, 480), PathBuf::from("/x.png"));
        assert!(matches!(d, Dialog::Resize(ref f) if f.width == 640 && f.height == 480));
        let d = s.dialog_for(DialogKind::SaveAs, (1, 1), PathBuf::from("/x.png"));
        assert!(matches!(d, Dialog::SaveAs(ref f) if f.path == "/x.png"));
        for k in [DialogKind::Open, DialogKind::Canvas, DialogKind::Crop, DialogKind::CutOut, DialogKind::Shortcuts, DialogKind::Settings] {
            let _ = s.dialog_for(k, (10, 10), PathBuf::new());
        }
        assert!(matches!(AppState::effect_dialog(EffectKind::Sepia), Dialog::Effect(_)));
    }

    #[test]
    fn workflow_mode_follows_output() {
        let mut s = st();
        assert!(!s.workflow());
        s.output = Some(PathBuf::from("/tmp/o.png"));
        assert!(s.workflow());
    }

    #[test]
    fn colours_are_remembered() {
        let mut s = st();
        s.note_color(Color::rgb(1, 2, 3));
        assert_eq!(s.prefs.recent_colors, vec![Color::rgb(1, 2, 3)]);
    }
}
