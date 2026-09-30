//! The two things the editor needs from the desktop that a unit test cannot have: the system
//! clipboard and native file dialogs. Both sit behind small traits with a real implementation
//! (`arboard`, `rfd`) and an in-memory/scripted fake, so the whole action pipeline runs in
//! tests without a display, D-Bus or portal.
//!
//! File dialogs are *asynchronous*: [`FileDialogs::request`] returns immediately, the real
//! implementation runs the blocking `rfd` call on a worker thread, and the app polls for the
//! answer each frame. A modal native dialog on the UI thread would freeze repainting (and on
//! Wayland make the window look hung).

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, channel},
};

use ssx_types::Frame;

/// What a file dialog is for; echoed back in the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogPurpose {
    /// Open an image or project as the document.
    OpenDocument,
    /// Choose the "save as" target from the Save dialog's Browse button.
    SaveTarget,
    /// Choose an image to insert as an object.
    InsertImage,
}

/// The answer to a [`FileDialogs::request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogReply {
    /// The purpose of the request.
    pub purpose: DialogPurpose,
    /// The chosen file, `None` when cancelled.
    pub path: Option<PathBuf>,
}

/// Native file pickers.
pub trait FileDialogs {
    /// Starts a dialog. `dir` is the folder to start in, `file_name` the suggested name for
    /// save dialogs.
    fn request(&mut self, purpose: DialogPurpose, dir: Option<&Path>, file_name: Option<&str>);
    /// The answer to a finished dialog, if any.
    fn poll(&mut self) -> Option<DialogReply>;
}

/// System clipboard access.
pub trait ClipboardService {
    /// The image on the clipboard, if any.
    fn image(&mut self) -> Option<Frame>;
    /// Puts an image on the clipboard.
    fn set_image(&mut self, frame: &Frame) -> Result<(), String>;
}

/// Everything the app needs from the outside world.
pub struct Services {
    /// File pickers.
    pub dialogs: Box<dyn FileDialogs>,
    /// Clipboard.
    pub clipboard: Box<dyn ClipboardService>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

impl Services {
    /// The real desktop services. `repaint` is called when a dialog answers so the UI wakes up.
    pub fn system(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            dialogs: Box::new(SystemDialogs::new(repaint)),
            clipboard: Box::new(SystemClipboard::default()),
        }
    }

    /// Scripted in-memory services for tests.
    pub fn fake() -> Self {
        Self {
            dialogs: Box::new(FakeDialogs::default()),
            clipboard: Box::new(MemoryClipboard::default()),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Real implementations
// -------------------------------------------------------------------------------------------

/// `rfd` dialogs on a worker thread.
pub struct SystemDialogs {
    tx: Sender<DialogReply>,
    rx: Receiver<DialogReply>,
    repaint: std::sync::Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for SystemDialogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemDialogs").finish_non_exhaustive()
    }
}

impl SystemDialogs {
    /// Creates the dialog runner.
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, repaint: std::sync::Arc::new(repaint) }
    }
}

impl FileDialogs for SystemDialogs {
    fn request(&mut self, purpose: DialogPurpose, dir: Option<&Path>, file_name: Option<&str>) {
        let tx = self.tx.clone();
        let repaint = self.repaint.clone();
        let dir = dir.map(Path::to_path_buf);
        let name = file_name.map(str::to_owned);
        let spawned = std::thread::Builder::new().name("ssx-file-dialog".into()).spawn(move || {
            let mut d = rfd::FileDialog::new();
            if let Some(dir) = dir.filter(|p| p.is_dir()) {
                d = d.set_directory(dir);
            }
            let path = match purpose {
                DialogPurpose::OpenDocument => d
                    .add_filter(
                        "Images and projects",
                        &["png", "jpg", "jpeg", "webp", "bmp", "gif", "ssxe"],
                    )
                    .pick_file(),
                DialogPurpose::InsertImage => d
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
                    .pick_file(),
                DialogPurpose::SaveTarget => {
                    if let Some(n) = name {
                        d = d.set_file_name(n);
                    }
                    d.add_filter("PNG", &["png"])
                        .add_filter("JPEG", &["jpg", "jpeg"])
                        .add_filter("WebP", &["webp"])
                        .add_filter("ssx project", &["ssxe"])
                        .save_file()
                }
            };
            let _ = tx.send(DialogReply { purpose, path });
            repaint();
        });
        if let Err(e) = spawned {
            tracing::warn!("cannot start the file dialog thread: {e}");
            let _ = self.tx.send(DialogReply { purpose, path: None });
        }
    }

    fn poll(&mut self) -> Option<DialogReply> {
        self.rx.try_recv().ok()
    }
}

/// `arboard` clipboard, created on first use (creation can fail without a display server, and
/// on X11 the instance must stay alive for the contents to remain available).
#[derive(Default)]
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
}

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemClipboard").field("connected", &self.inner.is_some()).finish()
    }
}

impl SystemClipboard {
    fn get(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.inner.is_none() {
            self.inner = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        self.inner.as_mut().ok_or_else(|| "clipboard unavailable".to_owned())
    }
}

impl ClipboardService for SystemClipboard {
    fn image(&mut self) -> Option<Frame> {
        let img = self.get().ok()?.get_image().ok()?;
        Frame::from_rgba8(
            u32::try_from(img.width).ok()?,
            u32::try_from(img.height).ok()?,
            img.bytes.into_owned(),
        )
        .ok()
    }

    fn set_image(&mut self, frame: &Frame) -> Result<(), String> {
        let rgba = frame.clone().into_rgba8().map_err(|e| e.to_string())?;
        let data = arboard::ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_data()),
        };
        self.get()?.set_image(data).map_err(|e| e.to_string())
    }
}

// -------------------------------------------------------------------------------------------
// Fakes
// -------------------------------------------------------------------------------------------

/// A clipboard that lives in memory.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    /// The current image.
    pub frame: Option<Frame>,
}

impl ClipboardService for MemoryClipboard {
    fn image(&mut self) -> Option<Frame> {
        self.frame.clone()
    }

    fn set_image(&mut self, frame: &Frame) -> Result<(), String> {
        self.frame = Some(frame.clone());
        Ok(())
    }
}

/// Dialogs that answer from a script.
#[derive(Debug, Default)]
pub struct FakeDialogs {
    /// Answers handed out in order (`None` = the user cancelled).
    pub answers: VecDeque<Option<PathBuf>>,
    /// Requests seen, for assertions.
    pub requests: Vec<(DialogPurpose, Option<PathBuf>, Option<String>)>,
    ready: VecDeque<DialogReply>,
}

impl FileDialogs for FakeDialogs {
    fn request(&mut self, purpose: DialogPurpose, dir: Option<&Path>, file_name: Option<&str>) {
        self.requests.push((purpose, dir.map(Path::to_path_buf), file_name.map(str::to_owned)));
        let path = self.answers.pop_front().flatten();
        self.ready.push_back(DialogReply { purpose, path });
    }

    fn poll(&mut self) -> Option<DialogReply> {
        self.ready.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use ssx_imgfx::solid_frame;

    use super::*;

    #[test]
    fn memory_clipboard_round_trips() {
        let mut c = MemoryClipboard::default();
        assert!(c.image().is_none());
        let f = solid_frame(3, 2, [1, 2, 3, 4]);
        c.set_image(&f).unwrap();
        assert_eq!(c.image(), Some(f));
    }

    #[test]
    fn fake_dialogs_answer_in_order_and_record() {
        let mut d = FakeDialogs::default();
        d.answers.push_back(Some(PathBuf::from("/a.png")));
        d.answers.push_back(None);
        d.request(DialogPurpose::OpenDocument, Some(Path::new("/x")), None);
        d.request(DialogPurpose::SaveTarget, None, Some("n.png"));
        assert_eq!(
            d.poll(),
            Some(DialogReply {
                purpose: DialogPurpose::OpenDocument,
                path: Some(PathBuf::from("/a.png"))
            })
        );
        assert_eq!(d.poll(), Some(DialogReply { purpose: DialogPurpose::SaveTarget, path: None }));
        assert_eq!(d.poll(), None);
        assert_eq!(d.requests.len(), 2);
        assert_eq!(d.requests[1].2.as_deref(), Some("n.png"));
    }

    #[test]
    fn services_debug_does_not_panic() {
        let s = Services::fake();
        assert!(format!("{s:?}").contains("Services"));
    }
}
