//! Wayland backend (stub while X11 is being verified).

use super::{Failure, plan::WaylandCaps};
use crate::app::OverlayApp;

/// Which surface role to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavour {
    LayerShell,
    Fullscreen,
}

pub(super) fn probe() -> Result<WaylandCaps, String> {
    Err("not implemented yet".into())
}

pub(super) fn run(_app: &mut OverlayApp, _f: Flavour) -> Result<(), Failure> {
    Err(Failure::Setup("not implemented yet".into()))
}
