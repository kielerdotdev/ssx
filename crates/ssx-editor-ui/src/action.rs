//! Everything the user can ask the editor to do, as plain data.
//!
//! Menus, toolbar buttons, keyboard shortcuts and the canvas all *queue* an [`Action`] instead
//! of mutating state directly. The app drains the queue once per frame. That keeps the drawing
//! code trivial, makes every command reachable from the tests without a display, and gives the
//! shortcut table one thing to point at.

use std::path::PathBuf;

use ssx_editor::object::Orient;

use crate::tools::ToolId;

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

/// Identifies which image effect a dialog edits (see [`crate::effects`]).
pub use crate::effects::EffectKind;

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
    /// Copy the finished image to the system clipboard.
    CopyImage,
    /// Hand the image to the caller for upload and close.
    Upload,
    /// Close the editor with this outcome.
    Done(Finish),
    /// The window was asked to close (checks for unsaved changes first).
    RequestClose,

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
    ApplyCrop,
    /// Discard the pending crop.
    CancelCrop,
    /// Crop away uniform borders.
    AutoCrop,
    /// Bake all annotations into the image.
    Flatten,
    /// Open a dialog.
    OpenDialog(DialogKind),
    /// Open the live-preview dialog of an effect.
    OpenEffect(EffectKind),

    // ---- misc -------------------------------------------------------------------------
    /// Dismiss whatever dialog is open.
    CloseDialog,
}

impl Action {
    /// `true` for actions that replace the document (and so must ask about unsaved changes).
    pub fn replaces_document(&self) -> bool {
        matches!(self, Action::NewFromClipboard | Action::OpenPath(_))
    }
}
