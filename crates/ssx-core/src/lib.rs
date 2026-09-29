//! Portable core of ssx.
//!
//! This crate contains everything that is not tied to a GUI toolkit, an OS capture API
//! or a specific uploader, so it builds and is fully testable on Linux, Windows and macOS:
//!
//! * [`settings`] — the typed, versioned TOML configuration, including the user's
//!   [`Workflow`](settings::Workflow)s (ShareX "task settings" equivalents).
//! * [`pattern`] — the ShareX-compatible filename/folder pattern engine (`%y-%mo-%d`, …),
//!   filename sanitising and race-free unique-file creation.
//! * [`history`] — the SQLite capture/upload history with thumbnails.
//! * [`workflow`] — the synchronous, cancellable engine that runs a workflow
//!   (capture → after-capture tasks → upload → after-upload tasks) against a set of
//!   *service traits* ([`workflow::Services`]). Real implementations of those traits live
//!   in the platform/upload/editor crates and are plugged in by the application layer.
//! * [`ipc`] — the request/response message types (and a JSON-lines codec) shared by the
//!   tray app, the CLI and the file-manager shell shims.
//!
//! Design rule: nothing here talks to the OS beyond `std::fs`, environment variables and the
//! wall clock, and every such dependency is injectable so behaviour is deterministic in
//! tests.

#![forbid(unsafe_code)]

pub mod history;
// pub mod ipc;
pub mod pattern;
pub mod settings;
// pub mod workflow;
