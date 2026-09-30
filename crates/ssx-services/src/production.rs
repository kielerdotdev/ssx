//! [`ProductionServices`]: every real service, ready to plug into the workflow engine.
//!
//! `ssx-core`'s [`Services`] bundle holds *borrowed* trait objects, so something must own the
//! real implementations. [`ProductionServices`] is that owner; [`ProductionServices::services`]
//! lends them out as the bundle the engine takes. (A process that wants a `Services<'static>`,
//! such as a tray app that builds its services once, can [`leak`](ProductionServices::leak)
//! the owner; the CLI simply keeps it on the stack.)
//!
//! Everything expensive is lazy: the capture backend is detected on the first capture, the
//! HTTP client and the async runtime on the first upload, the OS credential store on the
//! first secret and the clipboard connection on the first copy, so building the services is
//! cheap and cannot fail. `ssx upload` in an SSH session works without a display, and
//! `ssx capture` without a keyring daemon works without a credential store.

use std::sync::Arc;

use ssx_core::{
    history::History,
    settings::{Paths, Settings},
    workflow::{QrCodeRenderer, Services, StdFileSystem},
};
use ssx_platform::BackendKind;
use ssx_upload::{RetryPolicy, SecretStore};

use crate::{
    capture::{LastRegionStore, RegionSelector, ScreenCapturer},
    clipboard::{ClipboardOptions, SystemClipboard},
    command::SystemCommandRunner,
    desktop::{DesktopNotifier, SystemOpener},
    editor::ExternalEditor,
    secrets::LazySecrets,
    stubs::{StubOcr, StubPinner, StubRecorder},
    upload::UploadService,
    zipper::FolderZipper,
};

/// Knobs for [`ProductionServices::new`].
#[derive(Clone, Default)]
pub struct ProductionOptions {
    /// Force a capture backend (as `SSX_BACKEND` does; the flag wins over the variable).
    pub backend: Option<BackendKind>,
    /// Prefer `wl-copy` / `xclip` over `arboard` (right for short-lived processes such as
    /// the CLI, see [`crate::clipboard`]).
    pub prefer_external_clipboard: bool,
    /// Upload retry policy.
    pub retry: RetryPolicy,
    /// Secret store; the OS credential store (opened lazily) when `None`.
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// Interactive region selector (the overlay), if the caller has one.
    pub selector: Option<Arc<dyn RegionSelector>>,
}

impl std::fmt::Debug for ProductionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductionOptions")
            .field("backend", &self.backend)
            .field("prefer_external_clipboard", &self.prefer_external_clipboard)
            .field("selector", &self.selector.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "save-dialog")]
type SaveDialogImpl = crate::dialog::NativeSaveDialog;
#[cfg(not(feature = "save-dialog"))]
type SaveDialogImpl = crate::stubs::StubSaveDialog;

/// Owner of every real service. See the [module docs](self).
pub struct ProductionServices {
    /// Screenshots (also monitors, windows, backend diagnostics).
    pub capturer: ScreenCapturer,
    /// Uploads and URL shortening.
    pub uploads: UploadService,
    /// Clipboard.
    pub clipboard: SystemClipboard,
    /// Notifications and QR display.
    pub notifier: DesktopNotifier,
    /// Browser launching.
    pub opener: SystemOpener,
    /// `run_command`.
    pub commands: SystemCommandRunner,
    /// Folder zipping.
    pub zipper: FolderZipper,
    /// Image editor (external helper).
    pub editor: ExternalEditor,
    /// Recording (stub).
    pub recorder: StubRecorder,
    /// Pinning (stub).
    pub pinner: StubPinner,
    /// OCR (stub).
    pub ocr: StubOcr,
    /// Save-as dialog.
    #[cfg(feature = "save-dialog")]
    pub save_dialog: SaveDialogImpl,
    /// Save-as dialog (unavailable in this build).
    #[cfg(not(feature = "save-dialog"))]
    pub save_dialog: SaveDialogImpl,
    /// The secret store handed to the uploaders.
    pub secrets: Arc<dyn SecretStore>,
    fs: StdFileSystem,
    qr: QrCodeRenderer,
}

impl std::fmt::Debug for ProductionServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductionServices").field("uploads", &self.uploads).finish_non_exhaustive()
    }
}

impl ProductionServices {
    /// Builds the services for `settings` and `paths`.
    pub fn new(settings: &Settings, paths: &Paths, options: ProductionOptions) -> Self {
        let secrets: Arc<dyn SecretStore> =
            options.secrets.unwrap_or_else(|| Arc::new(LazySecrets::new()));
        let mut capturer =
            ScreenCapturer::detect(options.backend, LastRegionStore::in_dir(&paths.data_dir));
        if let Some(selector) = options.selector {
            capturer = capturer.with_selector(selector);
        }
        Self {
            capturer,
            uploads: UploadService::new(
                settings,
                &paths.config_dir,
                secrets.clone(),
                &options.retry,
            ),
            clipboard: SystemClipboard::new(ClipboardOptions {
                prefer_external: options.prefer_external_clipboard,
            }),
            notifier: DesktopNotifier::default(),
            opener: SystemOpener::default(),
            commands: SystemCommandRunner,
            zipper: FolderZipper::default(),
            editor: ExternalEditor::discover(),
            recorder: StubRecorder,
            pinner: StubPinner,
            ocr: StubOcr,
            save_dialog: SaveDialogImpl::default(),
            secrets,
            fs: StdFileSystem,
            qr: QrCodeRenderer,
        }
    }

    /// The production services with default options and an optional interactive region
    /// selector (the overlay). Shorthand for [`ProductionServices::new`].
    pub fn production(
        settings: &Settings,
        paths: &Paths,
        selector: Option<Arc<dyn RegionSelector>>,
    ) -> Self {
        Self::new(settings, paths, ProductionOptions { selector, ..ProductionOptions::default() })
    }

    /// The bundle the workflow engine takes. `history` enables recording of finished runs.
    pub fn services<'a>(&'a self, history: Option<&'a History>) -> Services<'a> {
        Services {
            capturer: &self.capturer,
            recorder: &self.recorder,
            editor: &self.editor,
            uploaders: &self.uploads,
            shortener: &self.uploads,
            clipboard: &self.clipboard,
            notifier: &self.notifier,
            opener: &self.opener,
            qr: &self.qr,
            commands: &self.commands,
            fs: &self.fs,
            zipper: &self.zipper,
            save_dialog: &self.save_dialog,
            pinner: &self.pinner,
            ocr: &self.ocr,
            history,
        }
    }

    /// Leaks the owner so [`services`](Self::services) yields a `Services<'static>`. Intended
    /// for a process that builds its services once and keeps them until it exits.
    pub fn leak(self) -> &'static Self {
        Box::leak(Box::new(self))
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::workflow::{CancelToken, CaptureRequest, CaptureTarget};

    use super::*;

    #[test]
    fn building_the_bundle_is_cheap_and_never_needs_a_display_or_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(dir.path());
        let services =
            ProductionServices::new(&Settings::default(), &paths, ProductionOptions::default());
        let bundle = services.services(None);
        assert!(bundle.history.is_none());
        // The stubs are wired into the engine's traits and say what they are.
        let e = bundle.ocr.recognize(
            &ssx_types::Frame::from_rgba8(1, 1, vec![0; 4]).unwrap(),
            &CancelToken::new(),
        );
        assert!(e.unwrap_err().to_string().contains("ssx-ocr"));
        // Without a selector an interactive region says so (the capture backend is not even
        // touched, so this works with no display).
        let e = bundle
            .capturer
            .capture(
                &CaptureRequest {
                    target: CaptureTarget::Region,
                    include_cursor: false,
                    hdr: Settings::default().capture.hdr,
                },
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(e.to_string().contains("--rect"), "{e}");
        assert!(services.uploads.names().contains(&"local"));
    }

    #[test]
    fn the_bundle_is_thread_safe() {
        fn check<T: Send + Sync>() {}
        check::<ProductionServices>();
    }
}
