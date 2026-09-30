//! The ssx background daemon (`ssx-app`): tray icon, global hotkeys, the IPC server that the
//! `ssx` command line and the file-manager entries talk to, the workflow supervisor and the
//! recording control.
//!
//! The crate is a library so that everything with logic can be unit-tested; `src/main.rs` is
//! a few lines. The map (see the README for the diagram and the lifecycle):
//!
//! | Module | Role |
//! |---|---|
//! | [`daemon`] | the supervisor: admission (one interactive capture, one recording, a queue for the rest), cancellation, shutdown |
//! | [`coalesce`] | merges `PostFiles` requests that arrive within ~400 ms into one batch |
//! | [`recording`] | the recording state machine and the recorder wrapper that feeds it |
//! | [`menu`] | the tray menu, tooltip and icon choice as pure data |
//! | [`icons`] | tray icons drawn in code |
//! | [`tray`], `tray_ksni`, `tray_native` | the thin per-platform tray backends |
//! | [`hotkeys_glue`] | settings to hotkey registrations, with the failures reported |
//! | [`reload`] | settings hot-reload: file watching and the accept/reject decision |
//! | [`requests`] | IPC requests and menu actions to jobs |
//! | [`ipc_server`] | the IPC request handler |
//! | [`runtime`] | the real services and the engine-driving job runner |
//! | [`notify`] | desktop notifications with a click target, and one-time notices |
//! | [`ui`] | the worker that turns events into tray state and notifications |
//! | [`app`] | assembly and the process lifecycle |
//!
//! Interactive region selection is `ssx_services::OverlaySelector` (the `ssx-overlay` helper);
//! the daemon only decides *when* it may run and which selection mode a request wants.

#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod clock;
pub mod coalesce;
pub mod daemon;
pub mod events;
pub mod hotkeys_glue;
pub mod icons;
pub mod ids;
pub mod ipc_server;
pub mod logging;
pub mod menu;
pub mod notify;
pub mod recording;
pub mod reload;
pub mod requests;
pub mod runtime;
pub mod tray;
pub mod ui;

#[cfg(target_os = "linux")]
pub mod tray_ksni;

#[cfg(any(windows, target_os = "macos"))]
pub mod tray_native;

pub use app::{APP_ID, App, run};
