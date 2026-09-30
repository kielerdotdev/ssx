//! The real window driven through egui events (`egui_kittest`): clicks, typing, toggles,
//! drags and key presses on every page, with assertions on the resulting `Settings`, the
//! files on disk and the fakes behind the host. One test binary (linking egui is slow), one
//! module per area.

mod common;

#[path = "ui/general.rs"]
mod general;
#[path = "ui/hotkeys.rs"]
mod hotkeys;
#[path = "ui/shell.rs"]
mod shell;
#[path = "ui/uploaders.rs"]
mod uploaders;
#[path = "ui/workflows.rs"]
mod workflows;
