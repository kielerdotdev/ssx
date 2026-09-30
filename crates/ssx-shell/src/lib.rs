//! File-manager "right click -> Upload with ssx" integration for Linux, Windows and macOS.
//!
//! Every entry launches the ssx CLI (`ssx post-file -- <paths>`, `ssx edit -- <image>`,
//! `ssx post-video -- <file>`); no logic lives in the shims. The crate is *installers only*:
//! it writes/removes the per-file-manager configuration and never runs the CLI.
//!
//! Design rules:
//! * **Idempotent and reversible.** Every generated file carries a marker; install refuses to
//!   overwrite files that are not ours, uninstall never deletes them, and uninstall restores
//!   the previous state including directories created by install.
//! * **Injectable environment.** All paths, `PATH`, the registry and helper processes go
//!   through [`Context`] / [`RegistryBackend`] / [`CommandRunner`], so tests run in temp dirs.
//! * **Selected files are argv, never a shell string.** See [`quote`].
//!
//! Use [`Integrations::native`] for the current OS, or build a set yourself.

#![forbid(unsafe_code)]

pub mod action;
pub mod context;
pub mod error;
pub mod integration;
pub mod linux;
pub mod macos;
pub mod quote;
pub mod windows;

mod fsutil;

use std::sync::Arc;

pub use action::{Action, Filter, FilterKind};
pub use context::{CommandOutput, CommandRunner, Context, Platform, RecordingRunner, SystemRunner};
pub use error::{Result, ShellError};
pub use fsutil::MARKER;
pub use integration::{
    Description, Detection, InstallOutcome, Integration, Integrations, Report, ReportEntry, Status,
    UninstallOutcome,
};
pub use windows::registry::{MemoryRegistry, RegistryBackend};

impl Integrations {
    /// The integrations for `platform`. On Windows the real registry is used when compiled for
    /// Windows; elsewhere (tests, cross-inspection) pass a [`RegistryBackend`] such as
    /// [`MemoryRegistry`] via [`Integrations::for_platform_with_registry`].
    pub fn for_platform(platform: Platform) -> Self {
        Self::for_platform_with_registry(platform, default_registry())
    }

    /// Like [`for_platform`](Self::for_platform) with an explicit registry backend.
    pub fn for_platform_with_registry(
        platform: Platform,
        registry: Option<Arc<dyn RegistryBackend>>,
    ) -> Self {
        match platform {
            Platform::Linux => Self::new(linux::integrations()),
            Platform::MacOs => Self::new(vec![Arc::new(macos::QuickActions::new())]),
            Platform::Windows => {
                let mut v: Vec<Arc<dyn Integration>> = Vec::new();
                if let Some(reg) = registry {
                    v.push(Arc::new(windows::ClassicVerbs::new(reg)));
                }
                v.push(Arc::new(windows::SendTo::new()));
                Self::new(v)
            }
        }
    }

    /// The integrations for the platform this binary runs on.
    pub fn native() -> Self {
        Self::for_platform(Platform::current())
    }

    /// Every integration of every platform (for documentation and `--list`).
    pub fn everything(registry: Arc<dyn RegistryBackend>) -> Self {
        let mut items = linux::integrations();
        items.push(Arc::new(macos::QuickActions::new()));
        items.push(Arc::new(windows::ClassicVerbs::new(registry)));
        items.push(Arc::new(windows::SendTo::new()));
        Self::new(items)
    }
}

#[cfg(windows)]
fn default_registry() -> Option<Arc<dyn RegistryBackend>> {
    Some(Arc::new(windows::registry::WindowsRegistry::new()))
}

#[cfg(not(windows))]
fn default_registry() -> Option<Arc<dyn RegistryBackend>> {
    None
}
