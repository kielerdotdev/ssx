//! Classic Explorer shell verbs under `HKCU\Software\Classes` (no admin rights, no signing).
//!
//! Layout per action `<id>` (verb key `ssx.<id>`):
//!
//! ```text
//! HKCU\Software\Classes\*\shell\ssx.upload                   any file       (Filter::Any)
//! HKCU\Software\Classes\Directory\shell\ssx.upload           folders
//! HKCU\Software\Classes\SystemFileAssociations\image\shell\ssx.edit        images
//! HKCU\Software\Classes\SystemFileAssociations\.webp\shell\ssx.edit        image types Windows
//!                                                                          has no perceived type for
//!     MUIVerb=<label>  Icon="<exe>",0  MultiSelectModel=Player|Single  NeverDefault=  SsxManaged=1
//!     command\(Default) = "<exe>" post-file --coalesce -- "%1"
//! ```
//!
//! ## How multiple selected files reach ssx
//!
//! Explorer's contract for *static* verbs is thin and has version-dependent quirks, so the
//! design does not rely on a particular one:
//! * `MultiSelectModel=Player` makes the verb appear for a multi-selection (the default
//!   `Single`/`Document` models hide it or spawn one window per item; classic verbs are shown
//!   for at most 15 items with `Document`, at most 100 with `Player`, a limit governed by
//!   Explorer's `MultipleInvokePromptMinimum` that we do not touch).
//! * The command line only has `%1`. Depending on the Explorer version a multi-selection
//!   results in one process per file (each with its own `%1`) or one process that also gets the
//!   other paths appended. Both are handled by `--coalesce`: every invocation forwards its
//!   paths over `ssx-ipc` to the running app with a `coalesce` flag, and the app merges
//!   requests that arrive within a short window (~400 ms) into a single upload batch.
//! * Command lines are limited to 32 767 characters (and the legacy shell path buffers to
//!   `MAX_PATH` per item), so very large selections should use **Send To** (see
//!   [`super::SendTo`]) or the Windows 11 `IExplorerCommand` extension, both of which hand
//!   all paths over in one call.
//!
//! The CLI must therefore accept `--coalesce` for `post-file` (documented in the crate README).

use std::sync::Arc;

use crate::action::{Action, FilterKind};
use crate::context::{Context, Platform};
use crate::error::{Result, ShellError};
use crate::integration::{Description, Detection, InstallOutcome, Integration, UninstallOutcome};
use crate::quote::win_command_quote;

use super::registry::RegistryBackend;

/// Marker value written into every verb key we create.
const MANAGED_VALUE: &str = "SsxManaged";
const CLASSES: &str = r"Software\Classes";

/// Image types Windows does not reliably tag with the `image` perceived type.
const IMAGE_EXTRA: &[&str] = &["webp", "avif", "heic", "heif"];
/// Video types Windows does not reliably tag with the `video` perceived type.
const VIDEO_EXTRA: &[&str] = &["mkv", "webm", "flv", "ogv", "m4v"];

/// Registry-based classic context-menu verbs.
#[derive(Debug, Clone)]
pub struct ClassicVerbs {
    registry: Arc<dyn RegistryBackend>,
}

/// One verb key to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerbKey {
    /// Key path relative to HKCU.
    pub path: String,
    /// `(name, value)` pairs on the verb key.
    pub values: Vec<(String, String)>,
    /// `command` subkey default value.
    pub command: String,
}

impl ClassicVerbs {
    /// Uses `registry` (the real one on Windows, [`super::registry::MemoryRegistry`] in tests).
    pub fn new(registry: Arc<dyn RegistryBackend>) -> Self {
        Self { registry }
    }

    fn plan(ctx: &Context) -> Result<Vec<VerbKey>> {
        let mut keys = Vec::new();
        for a in &ctx.actions {
            let command = command_line(ctx, a)?;
            let icon = format!("{},0", win_command_quote(ctx.exe_str()?)?);
            for class in class_keys(a) {
                keys.push(VerbKey {
                    path: format!(r"{CLASSES}\{class}\shell\ssx.{}", a.id),
                    values: vec![
                        ("MUIVerb".into(), a.label.clone()),
                        ("Icon".into(), icon.clone()),
                        (
                            "MultiSelectModel".into(),
                            if a.multi_select { "Player" } else { "Single" }.into(),
                        ),
                        ("NeverDefault".into(), String::new()),
                        (MANAGED_VALUE.into(), "1".into()),
                    ],
                    command: command.clone(),
                });
            }
        }
        Ok(keys)
    }

    fn key_current(&self, k: &VerbKey) -> Result<bool> {
        let reg = |e| ShellError::registry("reading", &k.path)(e);
        for (name, value) in &k.values {
            if self.registry.get_string(&k.path, name).map_err(reg)?.as_deref() != Some(value) {
                return Ok(false);
            }
        }
        let cmd_path = format!(r"{}\command", k.path);
        let cmd = self
            .registry
            .get_string(&cmd_path, "")
            .map_err(ShellError::registry("reading", &cmd_path))?;
        Ok(cmd.as_deref() == Some(k.command.as_str()))
    }

    fn is_ours(&self, path: &str) -> Result<bool> {
        Ok(self
            .registry
            .get_string(path, MANAGED_VALUE)
            .map_err(ShellError::registry("reading", path))?
            .as_deref()
            == Some("1"))
    }
}

/// Class keys (relative to `Software\Classes`) an action is registered under.
fn class_keys(a: &Action) -> Vec<String> {
    let mut keys = Vec::new();
    match a.filter.kind {
        FilterKind::Any => keys.push("*".to_owned()),
        FilterKind::Images => {
            keys.push(r"SystemFileAssociations\image".to_owned());
            keys.extend(extras(a, IMAGE_EXTRA));
        }
        FilterKind::Videos => {
            keys.push(r"SystemFileAssociations\video".to_owned());
            keys.extend(extras(a, VIDEO_EXTRA));
        }
        FilterKind::Custom => {
            if a.filter.extensions.is_empty() {
                keys.push("*".to_owned());
            } else {
                keys.extend(
                    a.filter.extensions.iter().map(|e| format!(r"SystemFileAssociations\.{e}")),
                );
            }
        }
    }
    if a.filter.directories {
        keys.push("Directory".to_owned());
    }
    keys
}

fn extras(a: &Action, list: &[&str]) -> Vec<String> {
    list.iter()
        .filter(|e| a.filter.extensions.iter().any(|x| x == *e))
        .map(|e| format!(r"SystemFileAssociations\.{e}"))
        .collect()
}

/// `"exe" <args> [--coalesce] -- "%1"`.
pub(crate) fn command_line(ctx: &Context, a: &Action) -> Result<String> {
    let mut parts = vec![win_command_quote(ctx.exe_str()?)?];
    for arg in &a.exec_args {
        parts.push(win_arg(a, arg)?);
    }
    if a.multi_select {
        parts.push("--coalesce".to_owned());
    }
    parts.push("--".to_owned());
    parts.push("\"%1\"".to_owned());
    Ok(parts.join(" "))
}

/// Quotes one of *our* fixed arguments for `CommandLineToArgvW`. (User paths are `"%1"`.)
fn win_arg(a: &Action, arg: &str) -> Result<String> {
    if arg.contains('"') || arg.contains('%') || arg.chars().any(char::is_control) {
        return Err(ShellError::InvalidAction {
            id: a.id.clone(),
            reason: format!("argument {arg:?} cannot be used in a Windows command line"),
        });
    }
    Ok(if arg.is_empty() || arg.contains([' ', '\t']) {
        format!("\"{arg}\"")
    } else {
        arg.to_owned()
    })
}

impl Integration for ClassicVerbs {
    fn id(&self) -> &'static str {
        "windows-verbs"
    }

    fn name(&self) -> &'static str {
        "Windows Explorer (classic menu)"
    }

    fn platform(&self) -> Platform {
        Platform::Windows
    }

    fn detect(&self, _ctx: &Context) -> Detection {
        Detection::found("Windows Explorer")
    }

    fn install(&self, ctx: &Context) -> Result<InstallOutcome> {
        let plan = Self::plan(ctx)?;
        // Check everything first so a foreign key aborts before anything is written.
        let mut existing = 0;
        let mut current = 0;
        for k in &plan {
            if self
                .registry
                .key_exists(&k.path)
                .map_err(ShellError::registry("reading", &k.path))?
            {
                if !self.is_ours(&k.path)? {
                    return Err(ShellError::Unavailable(format!(
                        r"HKCU\{} already exists and was not created by ssx",
                        k.path
                    )));
                }
                existing += 1;
                if self.key_current(k)? {
                    current += 1;
                }
            }
        }
        if current == plan.len() {
            return Ok(InstallOutcome::AlreadyPresent);
        }
        for k in &plan {
            for (name, value) in &k.values {
                self.registry
                    .set_string(&k.path, name, value)
                    .map_err(ShellError::registry("writing", &k.path))?;
            }
            let cmd_path = format!(r"{}\command", k.path);
            self.registry
                .set_string(&cmd_path, "", &k.command)
                .map_err(ShellError::registry("writing", &cmd_path))?;
        }
        Ok(if existing > 0 { InstallOutcome::Updated } else { InstallOutcome::Installed })
    }

    fn uninstall(&self, ctx: &Context) -> Result<UninstallOutcome> {
        let mut removed = false;
        for k in Self::plan(ctx)? {
            if !self
                .registry
                .key_exists(&k.path)
                .map_err(ShellError::registry("reading", &k.path))?
            {
                continue;
            }
            if !self.is_ours(&k.path)? {
                tracing::warn!(key = %k.path, "leaving a registry key that ssx did not create");
                continue;
            }
            self.registry
                .delete_tree(&k.path)
                .map_err(ShellError::registry("deleting", &k.path))?;
            removed = true;
            // Remove the parents we may have created, stopping at the first non-empty one.
            let mut cur = k.path.as_str();
            while let Some((parent, _)) = cur.rsplit_once('\\') {
                if parent.eq_ignore_ascii_case(CLASSES) {
                    break;
                }
                if !self
                    .registry
                    .delete_key_if_empty(parent)
                    .map_err(ShellError::registry("deleting", parent))?
                {
                    break;
                }
                cur = parent;
            }
        }
        Ok(if removed { UninstallOutcome::Removed } else { UninstallOutcome::NotPresent })
    }

    fn is_installed(&self, ctx: &Context) -> Result<bool> {
        for k in Self::plan(ctx)? {
            if !self.key_current(&k)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn describe(&self, ctx: &Context) -> Description {
        let keys: Vec<String> = Self::plan(ctx)
            .map(|p| p.into_iter().map(|k| format!(r"HKCU\{}", k.path)).collect())
            .unwrap_or_default();
        let mut d = Description {
            id: "windows-verbs",
            summary:
                "Classic Explorer context-menu verbs (Windows 11: under \"Show more options\")"
                    .into(),
            artefacts: keys,
            notes: Vec::new(),
        };
        d.notes.push("Windows 11 shows classic verbs only under \"Show more options\" (Shift+F10); a top-level entry needs the signed IExplorerCommand extension (see docs/windows11-context-menu.md)".into());
        d.notes.push("selections are forwarded with --coalesce so per-file launches are merged into one batch; for very large selections use Send To".into());
        d
    }
}
