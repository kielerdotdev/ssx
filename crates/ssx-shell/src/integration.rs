//! The [`Integration`] trait, its outcome types and the [`Integrations`] registry with the
//! structured [`Report`].

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::context::{Context, Platform};
use crate::error::Result;

/// Whether an integration applies to this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The target file manager / OS feature seems to be present.
    pub available: bool,
    /// Why (evidence found, or what is missing). Shown to the user.
    pub reason: String,
}

impl Detection {
    /// Present, because of `reason`.
    pub fn found(reason: impl Into<String>) -> Self {
        Self { available: true, reason: reason.into() }
    }

    /// Absent, because of `reason`.
    pub fn missing(reason: impl Into<String>) -> Self {
        Self { available: false, reason: reason.into() }
    }
}

/// What [`Integration::install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Newly installed.
    Installed,
    /// Was installed but stale (other exe path, older layout); rewritten.
    Updated,
    /// Already installed and up to date; nothing changed on disk.
    AlreadyPresent,
}

/// What [`Integration::uninstall`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Removed at least one artefact.
    Removed,
    /// Nothing of ours was there.
    NotPresent,
}

/// A human-readable plan: what [`Integration::install`] would write and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// Integration id.
    pub id: &'static str,
    /// One-line summary.
    pub summary: String,
    /// Files (or registry keys, prefixed `HKCU\`) that are created.
    pub artefacts: Vec<String>,
    /// Caveats and follow-up steps (restart the file manager, install a dependency, ...).
    pub notes: Vec<String>,
}

impl Description {
    pub(crate) fn paths(id: &'static str, summary: impl Into<String>, paths: &[PathBuf]) -> Self {
        Self {
            id,
            summary: summary.into(),
            artefacts: paths.iter().map(|p| p.display().to_string()).collect(),
            notes: Vec::new(),
        }
    }

    pub(crate) fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

impl fmt::Display for Description {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.summary)?;
        for a in &self.artefacts {
            writeln!(f, "  {a}")?;
        }
        for n in &self.notes {
            writeln!(f, "  note: {n}")?;
        }
        Ok(())
    }
}

/// One way of adding "Upload with ssx" style entries to a file manager.
pub trait Integration: Send + Sync + fmt::Debug {
    /// Stable machine id (`nautilus`, `dolphin`, ...).
    fn id(&self) -> &'static str;
    /// Display name.
    fn name(&self) -> &'static str;
    /// OS family this integration belongs to.
    fn platform(&self) -> Platform;
    /// Whether the target looks installed. Never modifies anything.
    fn detect(&self, ctx: &Context) -> Detection;
    /// Installs (or updates) the entries. Idempotent.
    fn install(&self, ctx: &Context) -> Result<InstallOutcome>;
    /// Removes exactly what [`install`](Self::install) created, leaving other files alone.
    fn uninstall(&self, ctx: &Context) -> Result<UninstallOutcome>;
    /// Whether everything is installed *and current* (would `install` be a no-op).
    fn is_installed(&self, ctx: &Context) -> Result<bool>;
    /// What `install` would create.
    fn describe(&self, ctx: &Context) -> Description;
}

/// Per-integration result in a [`Report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Newly installed.
    Installed,
    /// Refreshed an outdated install.
    Updated,
    /// Already up to date.
    AlreadyPresent,
    /// Uninstalled.
    Removed,
    /// Uninstall found nothing.
    NotPresent,
    /// Not attempted, with the reason.
    Skipped(String),
    /// Attempted and failed, with the error message.
    Failed(String),
}

impl Status {
    /// Whether this is a failure.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One line of a [`Report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// Integration id.
    pub id: &'static str,
    /// Integration display name.
    pub name: &'static str,
    /// Result.
    pub status: Status,
}

/// Structured outcome of `install_all` / `uninstall_all`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One entry per integration, in registry order.
    pub entries: Vec<ReportEntry>,
}

impl Report {
    /// Whether any integration failed.
    pub fn has_failures(&self) -> bool {
        self.entries.iter().any(|e| e.status.is_failure())
    }

    /// The entry for `id`.
    pub fn get(&self, id: &str) -> Option<&ReportEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self.entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
        for e in &self.entries {
            let (mark, text) = match &e.status {
                Status::Installed => ("ok  ", "installed".to_owned()),
                Status::Updated => ("ok  ", "updated".to_owned()),
                Status::AlreadyPresent => ("ok  ", "already installed".to_owned()),
                Status::Removed => ("ok  ", "removed".to_owned()),
                Status::NotPresent => ("--  ", "not installed".to_owned()),
                Status::Skipped(why) => ("skip", format!("skipped: {why}")),
                Status::Failed(err) => ("FAIL", format!("failed: {err}")),
            };
            writeln!(f, "[{mark}] {:<width$}  {text}", e.name)?;
        }
        let failed = self.entries.iter().filter(|e| e.status.is_failure()).count();
        if failed > 0 {
            writeln!(f, "{failed} integration(s) failed; the others were processed normally.")?;
        }
        Ok(())
    }
}

fn detect_one(i: &dyn Integration, ctx: &Context) -> Detection {
    if i.platform() == ctx.platform {
        i.detect(ctx)
    } else {
        Detection::missing(format!("{} integration only applies on {}", i.name(), i.platform()))
    }
}

/// A set of integrations processed together. Failures never abort the batch.
#[derive(Debug, Clone)]
pub struct Integrations {
    items: Vec<Arc<dyn Integration>>,
}

impl Integrations {
    /// An explicit set.
    pub fn new(items: Vec<Arc<dyn Integration>>) -> Self {
        Self { items }
    }

    /// The integrations members.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Integration>> {
        self.items.iter()
    }

    /// Looks one up by id.
    pub fn get(&self, id: &str) -> Option<&Arc<dyn Integration>> {
        self.items.iter().find(|i| i.id() == id)
    }

    /// Detection result for each integration (never modifies anything).
    pub fn detect_all(&self, ctx: &Context) -> Vec<(&'static str, Detection)> {
        self.items.iter().map(|i| (i.id(), detect_one(i.as_ref(), ctx))).collect()
    }

    /// Installs every integration whose target was detected.
    pub fn install_all(&self, ctx: &Context) -> Report {
        self.install_all_with(ctx, false)
    }

    /// Like [`install_all`](Self::install_all); with `force` also tries undetected targets
    /// on the current platform (useful before the file manager has ever been started).
    pub fn install_all_with(&self, ctx: &Context, force: bool) -> Report {
        let mut report = Report::default();
        for i in &self.items {
            let status = if let Err(e) = ctx.validate() {
                Status::Failed(e.to_string())
            } else {
                let det = detect_one(i.as_ref(), ctx);
                if det.available || (force && i.platform() == ctx.platform) {
                    match i.install(ctx) {
                        Ok(InstallOutcome::Installed) => Status::Installed,
                        Ok(InstallOutcome::Updated) => Status::Updated,
                        Ok(InstallOutcome::AlreadyPresent) => Status::AlreadyPresent,
                        Err(e) => {
                            tracing::warn!(integration = i.id(), error = %e, "install failed");
                            Status::Failed(e.to_string())
                        }
                    }
                } else {
                    Status::Skipped(det.reason)
                }
            };
            report.entries.push(ReportEntry { id: i.id(), name: i.name(), status });
        }
        report
    }

    /// Removes everything ssx installed, for every integration on the current platform
    /// (detection is deliberately ignored: the file manager may have been uninstalled since).
    pub fn uninstall_all(&self, ctx: &Context) -> Report {
        let mut report = Report::default();
        for i in &self.items {
            let status = if i.platform() == ctx.platform {
                match i.uninstall(ctx) {
                    Ok(UninstallOutcome::Removed) => Status::Removed,
                    Ok(UninstallOutcome::NotPresent) => Status::NotPresent,
                    Err(e) => {
                        tracing::warn!(integration = i.id(), error = %e, "uninstall failed");
                        Status::Failed(e.to_string())
                    }
                }
            } else {
                Status::Skipped(format!(
                    "{} integration only applies on {}",
                    i.name(),
                    i.platform()
                ))
            };
            report.entries.push(ReportEntry { id: i.id(), name: i.name(), status });
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_display() {
        let r = Report {
            entries: vec![
                ReportEntry { id: "a", name: "Alpha", status: Status::Installed },
                ReportEntry { id: "b", name: "Be", status: Status::Skipped("not found".into()) },
                ReportEntry { id: "c", name: "Gamma", status: Status::Failed("boom".into()) },
            ],
        };
        let text = r.to_string();
        assert!(text.contains("[ok  ] Alpha  installed"), "{text}");
        assert!(text.contains("[skip] Be     skipped: not found"), "{text}");
        assert!(text.contains("[FAIL] Gamma  failed: boom"), "{text}");
        assert!(text.contains("1 integration(s) failed"), "{text}");
        assert!(r.has_failures());
    }
}
