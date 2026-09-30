//! The native "save as" dialog (feature `save-dialog`, built on `rfd`).
//!
//! Off by default: it pulls in the XDG portal client on Linux and is only useful in a GUI
//! process (the tray app). The CLI has no use for it.

use std::path::{Path, PathBuf};

use ssx_core::workflow::{SaveDialog, ServiceError};

/// The [`SaveDialog`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeSaveDialog;

impl SaveDialog for NativeSaveDialog {
    fn choose_path(&self, suggested: &Path) -> Result<Option<PathBuf>, ServiceError> {
        let mut dialog = rfd::FileDialog::new().set_title("Save as");
        if let Some(dir) = suggested.parent().filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(name) = suggested.file_name() {
            dialog = dialog.set_file_name(name.to_string_lossy());
        }
        Ok(dialog.save_file())
    }
}
