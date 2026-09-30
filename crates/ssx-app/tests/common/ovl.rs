//! The pieces of `ssx-overlay`'s test support that its `sway.rs` expects from its parent
//! module, plus the include of that file (a private headless sway, virtual pointer/keyboard
//! injection, `grim`). Used by `tests/sway.rs` only.
#![allow(dead_code, clippy::pedantic)]

use std::path::{Path, PathBuf};

pub fn have(prog: &str) -> bool {
    super::common::have(prog)
}

pub fn helper_bin() -> PathBuf {
    super::common::overlay_bin()
}

pub fn wrapper(_: &Path, _: &[(&str, &str)], _: &[&str], target: &Path) -> PathBuf {
    target.to_path_buf()
}

#[path = "../../../ssx-overlay/tests/common/sway.rs"]
pub mod sway;
