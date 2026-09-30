//! Real implementations of the `ssx-core` service traits.
//!
//! `ssx-core` runs workflows against 15 small traits (see
//! [`ssx_core::workflow::Services`]) and knows nothing about screens, HTTP or desktops. This
//! crate is the application layer that implements them, once, for both the CLI and the tray
//! app:
//!
//! | Trait | Implementation | Built on |
//! |---|---|---|
//! | `Capturer` | [`ScreenCapturer`] | `ssx-platform` `ScreenSource`, `ssx-hdr` tone mapping |
//! | `Uploaders`, `UrlShortener` | [`UploadService`] | `ssx-upload` (Imgur, S3, HTTP, `.sxcu`, shorteners) |
//! | `Clipboard` | [`SystemClipboard`] | `arboard`, `wl-copy` / `xclip` |
//! | `Notifier`, `UrlOpener` | [`DesktopNotifier`], [`SystemOpener`] | `notify-rust`, `opener` |
//! | `CommandRunner` | [`SystemCommandRunner`] | `std::process`, no shell |
//! | `Zipper` | [`FolderZipper`] | `zip` |
//! | `Editor` | [`ExternalEditor`] | the `ssx-editor-ui` helper program |
//! | `SaveDialog` | `NativeSaveDialog` (feature `save-dialog`) | `rfd` |
//! | `Recorder`, `Pinner`, `Ocr` | [`stubs`] | not built yet, fail with a message naming the future crate |
//! | `FileSystem`, `QrRenderer` | `ssx-core`'s own `StdFileSystem`, `QrCodeRenderer` | |
//!
//! [`ProductionServices`] owns all of them and lends them out as the `Services` bundle the
//! engine takes. Every service is lazy and fails with a [`ServiceError`] whose message says
//! what to do, so a missing display, keyring daemon or clipboard tool degrades one step
//! instead of breaking the program.
//!
//! [`ServiceError`]: ssx_core::workflow::ServiceError
//!
//! The crate compiles on Linux, Windows and macOS. Capture backends are chosen by
//! `ssx-platform` at run time.

#![forbid(unsafe_code)]

pub mod capture;
pub mod clipboard;
pub mod command;
pub mod desktop;
pub mod editor;
pub mod hdr;
pub mod helpers;
pub mod overlay;
pub mod production;
pub mod runtime;
pub mod secrets;
pub mod stubs;
pub mod upload;
pub mod zipper;

#[cfg(feature = "save-dialog")]
pub mod dialog;

pub use capture::{
    BackendReport, ExplicitTarget, LastRegionStore, PickRequest, Picked, RegionSelector,
    ScreenCapturer, map_capture_error,
};
pub use clipboard::{ClipboardDiagnosis, ClipboardOptions, FileListFlavor, SystemClipboard};
pub use command::SystemCommandRunner;
pub use desktop::{DesktopNotifier, SystemOpener, probe_notifications};
pub use editor::ExternalEditor;
pub use hdr::tonemap_settings;
pub use helpers::{Discovery, discover as discover_helper};
pub use overlay::{OverlaySelector, PickMode};
pub use production::{ProductionOptions, ProductionServices};
pub use runtime::SharedRuntime;
pub use secrets::{LayeredSecretStore, LazySecrets, SecretStatus};
pub use upload::{UploadService, UploaderInfo};
pub use zipper::{FolderZipper, ZipLimits};
