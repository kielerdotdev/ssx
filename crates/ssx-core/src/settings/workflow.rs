//! The user-configurable description of a workflow (ShareX "task settings").
//!
//! These are *data only*; [`crate::workflow::Engine`] gives them meaning. Step lists are
//! executed **in the order written**, which lets users express things ShareX cannot (for
//! example uploading before copying the image), but [`crate::settings::Settings::validate`]
//! warns about orderings that cannot work (such as `delete_local_file` before `upload`).

use serde::{Deserialize, Serialize};

use super::destinations::DestinationOverride;

/// What starts a workflow and how it can be addressed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trigger {
    /// Global hotkey, e.g. `"Ctrl+PrintScreen"` (see [`super::Hotkey`] for the syntax).
    pub hotkey: Option<String>,
    /// Name for `ssx run <name>` and the shell-extension shims.
    pub cli_name: Option<String>,
}

/// Where a workflow gets its content from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// Interactive region selection (overlay).
    CaptureRegion,
    /// The whole virtual desktop.
    CaptureFullscreen,
    /// The monitor under the cursor.
    CaptureMonitor,
    /// The active window.
    CaptureWindow,
    /// Repeat the previously selected region without showing the overlay.
    CaptureLastRegion,
    /// Record the screen to a video file; the hotkey toggles start/stop.
    RecordScreen,
    /// Record the screen to an animated GIF; the hotkey toggles start/stop.
    RecordGif,
    /// Existing files handed in by the caller (CLI, shell menu, drag and drop).
    Files,
    /// Whatever is on the clipboard (image, text or file list).
    Clipboard,
}

impl InputKind {
    /// `true` for inputs that produce a still image.
    pub const fn is_still_capture(self) -> bool {
        matches!(
            self,
            Self::CaptureRegion
                | Self::CaptureFullscreen
                | Self::CaptureMonitor
                | Self::CaptureWindow
                | Self::CaptureLastRegion
        )
    }

    /// `true` for inputs that produce a video file.
    pub const fn is_recording(self) -> bool {
        matches!(self, Self::RecordScreen | Self::RecordGif)
    }
}

/// A task run on the captured content before/while it is uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterCapture {
    /// Open the image editor; the edited result replaces the capture. Cancelling the editor
    /// cancels the whole workflow (nothing is saved or uploaded), like ShareX.
    OpenEditor,
    /// Put the image on the clipboard.
    CopyImageToClipboard,
    /// Save into the configured save folder using the filename pattern.
    SaveToFile,
    /// Ask where to save. Needs a UI; the engine delegates to the
    /// [`SaveDialog`](crate::workflow::SaveDialog) service. Cancelling the dialog skips only
    /// this step.
    SaveAsDialog,
    /// Show the image in an always-on-top window.
    PinToScreen,
    /// Recognise text and copy it to the clipboard.
    Ocr,
    /// Upload using the resolved image / video / file destination.
    Upload,
    /// Delete the local file this workflow created. Only runs after a *confirmed* upload.
    DeleteLocalFile,
}

/// A task run after a successful upload, working on the resulting URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterUpload {
    /// Copy the current URL (the shortened one after `shorten_url`).
    CopyUrl,
    /// Copy the shortened URL, shortening first if `shorten_url` has not run. Does not change
    /// which URL later steps see.
    CopyShortUrl,
    /// Open the current URL in the default browser.
    OpenUrl,
    /// Shorten the URL; later steps see the short URL as *the* URL.
    ShortenUrl,
    /// Show a QR code for the current URL.
    ShowQrCode,
    /// Show a desktop notification with the result.
    ShowNotification,
    /// Run a program. **No shell is involved**: `program` is executed directly and each entry
    /// of `args` is one argument, with `{path}`, `{url}`, `{file_name}`, `{short_url}`,
    /// `{thumbnail_url}` and `{deletion_url}` substituted inside that argument only.
    RunCommand {
        /// Executable to run (looked up on `PATH` if it has no directory part).
        program: String,
        /// Argument templates.
        #[serde(default)]
        args: Vec<String>,
    },
}

/// One user-visible pipeline: input → after-capture tasks → upload → after-upload tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    /// Stable identifier (`[a-z0-9._-]+`), used by IPC and history. Never shown.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Hotkey / CLI trigger.
    pub trigger: Trigger,
    /// Content source.
    pub input: InputKind,
    /// Tasks before/around upload, in order.
    pub after_capture: Vec<AfterCapture>,
    /// Per-workflow destination overrides.
    pub destination: DestinationOverride,
    /// Tasks after a successful upload, in order.
    pub after_upload: Vec<AfterUpload>,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            trigger: Trigger::default(),
            input: InputKind::CaptureRegion,
            after_capture: Vec::new(),
            destination: DestinationOverride::default(),
            after_upload: Vec::new(),
        }
    }
}

impl Workflow {
    /// `true` if the workflow contains an upload step.
    pub fn uploads(&self) -> bool {
        self.after_capture.contains(&AfterCapture::Upload)
    }
}

/// The workflows shipped with ssx. Hotkeys mirror ShareX's defaults where the key exists on
/// every platform.
pub fn builtin_workflows() -> Vec<Workflow> {
    use AfterCapture as C;
    use AfterUpload as U;
    let notify_copy = vec![U::CopyUrl, U::ShowNotification];
    let wf = |id: &str,
              name: &str,
              hotkey: Option<&str>,
              cli: &str,
              input: InputKind,
              after_capture: Vec<AfterCapture>,
              after_upload: Vec<AfterUpload>| Workflow {
        id: id.to_owned(),
        name: name.to_owned(),
        trigger: Trigger { hotkey: hotkey.map(str::to_owned), cli_name: Some(cli.to_owned()) },
        input,
        after_capture,
        destination: DestinationOverride::default(),
        after_upload,
    };
    vec![
        wf(
            "capture-region",
            "Capture region, save, copy and upload",
            Some("Ctrl+PrintScreen"),
            "region",
            InputKind::CaptureRegion,
            vec![C::SaveToFile, C::CopyImageToClipboard, C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "capture-region-edit",
            "Capture region, edit, save and upload",
            Some("Ctrl+Shift+PrintScreen"),
            "region-edit",
            InputKind::CaptureRegion,
            vec![C::OpenEditor, C::SaveToFile, C::CopyImageToClipboard, C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "capture-fullscreen",
            "Capture screen, save and copy",
            Some("PrintScreen"),
            "screen",
            InputKind::CaptureFullscreen,
            vec![C::SaveToFile, C::CopyImageToClipboard],
            vec![U::ShowNotification],
        ),
        wf(
            "capture-window",
            "Capture active window, save and upload",
            Some("Alt+PrintScreen"),
            "window",
            InputKind::CaptureWindow,
            vec![C::SaveToFile, C::CopyImageToClipboard, C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "capture-monitor",
            "Capture active monitor, save and copy",
            None,
            "monitor",
            InputKind::CaptureMonitor,
            vec![C::SaveToFile, C::CopyImageToClipboard],
            vec![U::ShowNotification],
        ),
        wf(
            "capture-last-region",
            "Repeat last region, save and upload",
            None,
            "last-region",
            InputKind::CaptureLastRegion,
            vec![C::SaveToFile, C::CopyImageToClipboard, C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "record-screen",
            "Record screen and upload",
            Some("Shift+PrintScreen"),
            "record",
            InputKind::RecordScreen,
            vec![C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "record-gif",
            "Record GIF and upload",
            Some("Ctrl+Shift+Alt+PrintScreen"),
            "record-gif",
            InputKind::RecordGif,
            vec![C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "upload-files",
            "Upload files",
            None,
            "upload",
            InputKind::Files,
            vec![C::Upload],
            notify_copy.clone(),
        ),
        wf(
            "upload-clipboard",
            "Upload clipboard contents",
            None,
            "upload-clipboard",
            InputKind::Clipboard,
            vec![C::Upload],
            notify_copy,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_and_cli_names_are_unique() {
        let wfs = builtin_workflows();
        let mut ids: Vec<_> = wfs.iter().map(|w| w.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), wfs.len());
        let mut cli: Vec<_> = wfs.iter().filter_map(|w| w.trigger.cli_name.as_deref()).collect();
        cli.sort_unstable();
        cli.dedup();
        assert_eq!(cli.len(), wfs.len());
    }

    #[test]
    fn input_classification() {
        assert!(InputKind::CaptureRegion.is_still_capture());
        assert!(!InputKind::Files.is_still_capture());
        assert!(InputKind::RecordGif.is_recording());
        assert!(!InputKind::Clipboard.is_recording());
    }
}
