//! Everything the user can ask the editor to do, as plain data.
//!
//! Menus, toolbar buttons, keyboard shortcuts, dialogs and the canvas all *queue* an
//! [`Action`] instead of mutating state directly. The app drains the queue while it draws a
//! frame. That keeps the drawing code trivial, makes every command reachable from tests without
//! a display, and gives the shortcut table one thing to point at.

use std::path::PathBuf;

use ssx_editor::{
    Color, ObjectId,
    object::{Axis, Orient},
};
use ssx_imgfx::{Effect, ResizeFilter};

use crate::{export::ExportSettings, props::PropEdit, tools::ToolId};

pub use crate::effects::EffectKind;

/// How a "Done" button ends the editing session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Write the image and close.
    Save,
    /// Put the image on the clipboard and close.
    Copy,
    /// Write the image, then let the caller upload it.
    Upload,
    /// Discard everything.
    Cancel,
}

/// The dialogs the editor can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Save as (path, format, quality).
    SaveAs,
    /// Open by path.
    Open,
    /// Scale the whole image.
    Resize,
    /// Grow/shrink the canvas, padding and background.
    Canvas,
    /// Numeric crop and auto-crop.
    Crop,
    /// Numeric cut-out.
    CutOut,
    /// Keyboard cheat sheet.
    Shortcuts,
    /// Preferences.
    Settings,
}

/// The answer to the "unsaved changes" question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsavedAnswer {
    /// Save, then continue.
    Save,
    /// Continue without saving.
    Discard,
    /// Stay.
    Cancel,
}

/// A user command.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // ---- file -------------------------------------------------------------------------
    /// Replace the document with the clipboard image.
    NewFromClipboard,
    /// Choose a file to open.
    Open,
    /// Open this file (image or `.ssxe`).
    OpenPath(PathBuf),
    /// Save to the current file (asks for a path when there is none).
    Save,
    /// Save under a new name.
    SaveAs,
    /// Save the editable `.ssxe` project.
    SaveProject,
    /// Write the file chosen in the Save dialog.
    SaveTo {
        /// Target file; the extension picks the format.
        path: PathBuf,
        /// Encoder options.
        settings: ExportSettings,
    },
    /// Browse for the Save dialog's target.
    BrowseSaveTarget,
    /// Copy the finished image to the system clipboard.
    CopyImage,
    /// Hand the image to the caller for upload and close.
    Upload,
    /// Close the editor with this outcome.
    Done(Finish),
    /// The window was asked to close (checks for unsaved changes first).
    RequestClose,
    /// Answer to the unsaved-changes prompt.
    Unsaved(UnsavedAnswer),

    // ---- edit -------------------------------------------------------------------------
    /// Undo one step.
    Undo,
    /// Redo one step.
    Redo,
    /// Undo this many steps at once (history menu).
    UndoSteps(usize),
    /// Redo this many steps at once (history menu).
    RedoSteps(usize),
    /// Delete the selected objects.
    DeleteSelection,
    /// Select every object.
    SelectAll,
    /// Clear the selection.
    Deselect,
    /// Select these objects (object list); `additive` toggles them in the current selection.
    SelectObjects {
        /// The objects.
        ids: Vec<ObjectId>,
        /// Toggle instead of replace.
        additive: bool,
    },
    /// Show or hide an object.
    SetVisible(ObjectId, bool),
    /// Lock or unlock an object.
    SetLocked(ObjectId, bool),
    /// Delete one object (object list).
    DeleteObject(ObjectId),
    /// Duplicate the selection.
    Duplicate,
    /// Copy: the selected objects, or the whole image when nothing is selected.
    Copy,
    /// Cut the selected objects.
    Cut,
    /// Paste objects, or the clipboard image.
    Paste,
    /// Paste the system clipboard image as an image object.
    PasteImage,
    /// Group the selection.
    Group,
    /// Ungroup the selection.
    Ungroup,
    /// Selection to the top of the z-order.
    BringToFront,
    /// Selection to the bottom of the z-order.
    SendToBack,
    /// Selection one step up.
    Raise,
    /// Selection one step down.
    Lower,
    /// Change a property of the selection or the active tool.
    Prop(PropEdit),
    /// Set the number the first step marker shows.
    SetStepStart(u32),

    // ---- tools ------------------------------------------------------------------------
    /// Choose a tool.
    SetTool(ToolId),
    /// Choose an image file for the image tool.
    PickImageFile,
    /// Use the clipboard image for the image tool.
    PickImageClipboard,

    // ---- view -------------------------------------------------------------------------
    /// One zoom step in.
    ZoomIn,
    /// One zoom step out.
    ZoomOut,
    /// Fit to window.
    ZoomFit,
    /// 100 %.
    ZoomActual,
    /// A specific zoom (1.0 = 100 %).
    ZoomTo(f32),
    /// Show/hide the pixel grid at high zoom.
    TogglePixelGrid,
    /// Show/hide the object list.
    ToggleLayers,

    // ---- image ------------------------------------------------------------------------
    /// Rotate or flip the whole image.
    Orient(Orient),
    /// Apply the pending crop.
    ApplyPendingCrop,
    /// Discard the pending crop.
    CancelPendingCrop,
    /// Crop to this rectangle (image pixels).
    CropTo(ssx_types::Rect),
    /// Crop away uniform borders (tolerance 0-255).
    AutoCrop(u8),
    /// Remove a strip and join the rest.
    CutOut {
        /// Which axis the strip spans.
        axis: Axis,
        /// First removed pixel.
        start: i32,
        /// One past the last removed pixel.
        end: i32,
    },
    /// Scale the whole image.
    Resize {
        /// New width.
        width: u32,
        /// New height.
        height: u32,
        /// Resampling filter.
        filter: ResizeFilter,
    },
    /// Grow or shrink the canvas.
    ResizeCanvas {
        /// Left delta.
        left: i32,
        /// Top delta.
        top: i32,
        /// Right delta.
        right: i32,
        /// Bottom delta.
        bottom: i32,
        /// Canvas fill (`None` = transparent).
        background: Option<Color>,
    },
    /// Bake all annotations into the image.
    Flatten,
    /// Apply an image effect to the whole image.
    ApplyEffect(Effect),
    /// Open a dialog.
    OpenDialog(DialogKind),
    /// Open the live-preview dialog of an effect.
    OpenEffect(EffectKind),
    /// Dismiss whatever dialog is open.
    CloseDialog,

    // ---- misc -------------------------------------------------------------------------
    /// Forget the remembered tool styles.
    ResetToolStyles,
    /// Forget the recent colours.
    ClearRecentColors,
}

impl Action {
    /// `true` for actions that replace the document (and so must ask about unsaved changes).
    pub fn replaces_document(&self) -> bool {
        matches!(self, Action::NewFromClipboard | Action::OpenPath(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_document_replacing_actions_are_flagged() {
        assert!(Action::NewFromClipboard.replaces_document());
        assert!(Action::OpenPath(PathBuf::from("/x.png")).replaces_document());
        assert!(!Action::Open.replaces_document(), "choosing a file changes nothing yet");
        assert!(!Action::Undo.replaces_document());
    }
}
