//! Dolphin (KDE) service menus.
//!
//! One `.desktop` file per action in `~/.local/share/kio/servicemenus/` (read by KDE
//! Frameworks 5 and 6; Plasma 6 no longer scans the older `kservices5/ServiceMenus`, which is
//! optionally written too for KDE 4 and early-KF5 systems). One file per action because a
//! service-menu file has a single `MimeType=` for all of its `[Desktop Action]`s, and our
//! actions have different type filters.
//!
//! The files are `chmod 755`: KDE only honours service menus outside system directories when
//! they are executable (a guard against downloaded `.desktop` files running commands).
//! `X-KDE-ServiceTypes=KonqPopupMenu/Plugin` is required by KF5 and ignored by KF6; keeping it
//! makes one file work on both.

use std::path::PathBuf;

use crate::action::{Action, FilterKind};
use crate::context::{Context, Platform};
use crate::error::Result;
use crate::fsutil::{MARKER, ManagedFile, files_current, install_files, uninstall_files};
use crate::integration::{Description, Detection, InstallOutcome, Integration, UninstallOutcome};
use crate::quote::{desktop_exec_arg, keyfile_value};

/// Dolphin / KDE integration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Dolphin {
    legacy_kservices5: bool,
}

impl Dolphin {
    /// Modern location only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also write to `~/.local/share/kservices5/ServiceMenus` for KDE 4 / early KF5.
    pub fn with_legacy_kservices5(mut self, enabled: bool) -> Self {
        self.legacy_kservices5 = enabled;
        self
    }

    fn dir(ctx: &Context) -> PathBuf {
        ctx.data_home.join("kio").join("servicemenus")
    }

    fn legacy_dir(ctx: &Context) -> PathBuf {
        ctx.data_home.join("kservices5").join("ServiceMenus")
    }

    fn files_in(dir: &std::path::Path, ctx: &Context) -> Result<Vec<ManagedFile>> {
        ctx.actions
            .iter()
            .map(|a| {
                Ok(ManagedFile::new(
                    dir.join(format!("ssx-{}.desktop", a.id)),
                    service_menu_source(ctx, a)?,
                    0o755,
                ))
            })
            .collect()
    }

    fn wanted(&self, ctx: &Context) -> Result<Vec<ManagedFile>> {
        let mut files = Self::files_in(&Self::dir(ctx), ctx)?;
        if self.legacy_kservices5 {
            files.extend(Self::files_in(&Self::legacy_dir(ctx), ctx)?);
        }
        Ok(files)
    }
}

impl Integration for Dolphin {
    fn id(&self) -> &'static str {
        "dolphin"
    }

    fn name(&self) -> &'static str {
        "Dolphin (KDE)"
    }

    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn detect(&self, ctx: &Context) -> Detection {
        if ctx.has_binary("dolphin") {
            Detection::found("`dolphin` found in PATH")
        } else if ctx.config_home.join("dolphinrc").exists() {
            Detection::found("~/.config/dolphinrc exists")
        } else if ctx.desktop_is("kde") {
            Detection::found("XDG_CURRENT_DESKTOP is KDE")
        } else {
            Detection::missing("Dolphin is not installed")
        }
    }

    fn install(&self, ctx: &Context) -> Result<InstallOutcome> {
        let outcome = install_files(&self.wanted(ctx)?)?;
        if !self.legacy_kservices5 {
            // Switching the legacy copy off must not leave a duplicate behind.
            let legacy = Self::files_in(&Self::legacy_dir(ctx), ctx)?;
            if uninstall_files(&legacy, ctx)? == UninstallOutcome::Removed
                && outcome == InstallOutcome::AlreadyPresent
            {
                return Ok(InstallOutcome::Updated);
            }
        }
        Ok(outcome)
    }

    fn uninstall(&self, ctx: &Context) -> Result<UninstallOutcome> {
        let mut files = Self::files_in(&Self::dir(ctx), ctx)?;
        files.extend(Self::files_in(&Self::legacy_dir(ctx), ctx)?);
        uninstall_files(&files, ctx)
    }

    fn is_installed(&self, ctx: &Context) -> Result<bool> {
        files_current(&self.wanted(ctx)?)
    }

    fn describe(&self, ctx: &Context) -> Description {
        let paths: Vec<PathBuf> =
            self.wanted(ctx).map(|f| f.into_iter().map(|f| f.path).collect()).unwrap_or_default();
        Description::paths("dolphin", "Dolphin service menu entries (right-click menu)", &paths)
            .note("Dolphin picks the entries up on the next right-click; restart it if they do not show")
            .note("the files are executable on purpose: KDE ignores non-executable service menus in user directories")
    }
}

fn mime_for(action: &Action) -> String {
    match action.filter.kind {
        FilterKind::Any => "all/all;".to_owned(),
        FilterKind::Images => "image/*;".to_owned(),
        FilterKind::Videos => "video/*;".to_owned(),
        FilterKind::Custom => {
            let mut s: String = action.filter.mime_types.iter().map(|m| format!("{m};")).collect();
            if action.filter.directories {
                s.push_str("inode/directory;");
            }
            if s.is_empty() {
                s.push_str("all/allfiles;");
            }
            s
        }
    }
}

/// The `.desktop` service menu for one action.
pub(crate) fn service_menu_source(ctx: &Context, action: &Action) -> Result<String> {
    let exe = desktop_exec_arg(ctx.exe_str()?);
    let args: Vec<String> = action.exec_args.iter().map(|a| desktop_exec_arg(a)).collect();
    let code = if action.multi_select { "%F" } else { "%f" };
    let group = action.camel_id();
    Ok(format!(
        "[Desktop Entry]\n\
         # {MARKER}: generated by ssx. Edits are overwritten; remove via ssx or delete this file.\n\
         Type=Service\n\
         MimeType={mime}\n\
         Actions={group};\n\
         X-KDE-ServiceTypes=KonqPopupMenu/Plugin\n\
         X-KDE-Priority=TopLevel\n\
         X-KDE-Protocols=file\n\
         X-KDE-StartupNotify=false\n\
         \n\
         [Desktop Action {group}]\n\
         Name={name}\n\
         Icon={icon}\n\
         Exec={exe} {args} -- {code}\n",
        mime = mime_for(action),
        name = keyfile_value(&action.label),
        icon = action.icon,
        args = args.join(" "),
    ))
}
