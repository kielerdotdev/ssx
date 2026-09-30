//! Everything that draws with egui. Logic lives elsewhere; these modules turn state into
//! pixels and clicks into [`crate::action::Action`]s.

pub mod canvas;
pub mod color;
pub mod dialogs;
pub mod layers;
pub mod menubar;
pub mod props_bar;
pub mod statusbar;
pub mod theme;
pub mod toolbar;
pub mod widgets;

/// Version string shown in the settings dialog.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
