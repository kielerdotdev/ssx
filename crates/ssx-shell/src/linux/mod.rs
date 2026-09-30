//! Linux file managers. Each module documents the format quirks it works around.

pub mod desktop_entry;
pub mod dolphin;
pub mod nautilus;
pub mod nemo;
pub mod snippets;
pub mod thunar;

use std::sync::Arc;

use crate::integration::Integration;

/// All Linux integrations with default options, in the order they are reported.
pub fn integrations() -> Vec<Arc<dyn Integration>> {
    vec![
        Arc::new(nautilus::Nautilus::new()),
        Arc::new(dolphin::Dolphin::new()),
        Arc::new(thunar::Thunar),
        Arc::new(nemo::Nemo),
        Arc::new(desktop_entry::DesktopEntries::new()),
    ]
}
