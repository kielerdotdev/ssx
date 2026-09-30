//! Nautilus (GNOME Files).
//!
//! Two variants, because Nautilus has no way to add a plain menu entry without code:
//!
//! * **Scripts** (`~/.local/share/nautilus/scripts/<label>`): zero dependencies, but the entries
//!   live in the "Scripts" submenu and cannot be hidden by file type. Nautilus passes the
//!   selected *local* files as `argv`, relative to the folder being viewed (and, for local files
//!   only, newline-joined in `NAUTILUS_SCRIPT_SELECTED_FILE_PATHS`). We use `argv`: the
//!   environment variable cannot represent a file name containing a newline.
//! * **Extension** (`~/.local/share/nautilus-python/extensions/ssx-shell.py`): a proper
//!   top-level menu item filtered by type, but needs the `nautilus-python` package
//!   (`python3-nautilus` / `nautilus-python`) and a `nautilus -q` restart.
//!
//! `Auto` picks the extension when `nautilus-python` is installed, otherwise scripts.

use std::path::PathBuf;

use crate::action::Action;
use crate::context::{Context, Platform};
use crate::error::{Result, ShellError};
use crate::fsutil::{MARKER, ManagedFile, files_current, install_files, uninstall_files};
use crate::integration::{Description, Detection, InstallOutcome, Integration, UninstallOutcome};
use crate::quote::{push_fmt, py_str, sh_case_ext_pattern, sh_single_quote, sh_word};

/// Which Nautilus mechanism to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NautilusVariant {
    /// Extension if `nautilus-python` is present, else scripts.
    #[default]
    Auto,
    /// Only `~/.local/share/nautilus/scripts`.
    Scripts,
    /// Only the `nautilus-python` extension.
    Extension,
    /// Both (duplicated entries; mainly for testing).
    Both,
}

/// Nautilus / GNOME Files integration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nautilus {
    variant: NautilusVariant,
}

impl Nautilus {
    /// `Auto` variant.
    pub fn new() -> Self {
        Self::default()
    }

    /// Chooses the variant explicitly.
    pub fn with_variant(variant: NautilusVariant) -> Self {
        Self { variant }
    }

    /// Whether the `nautilus-python` loader library is installed.
    pub fn nautilus_python_available(ctx: &Context) -> bool {
        ctx.lib_dirs.iter().any(|d| {
            ["extensions-4", "extensions-3.0"]
                .iter()
                .any(|e| d.join("nautilus").join(e).join("libnautilus-python.so").exists())
        })
    }

    fn resolved(self, ctx: &Context) -> (bool, bool) {
        match self.variant {
            NautilusVariant::Scripts => (true, false),
            NautilusVariant::Extension => (false, true),
            NautilusVariant::Both => (true, true),
            NautilusVariant::Auto => {
                if Self::nautilus_python_available(ctx) {
                    (false, true)
                } else {
                    (true, false)
                }
            }
        }
    }

    fn scripts_dir(ctx: &Context) -> PathBuf {
        ctx.data_home.join("nautilus").join("scripts")
    }

    fn extension_path(ctx: &Context) -> PathBuf {
        ctx.data_home.join("nautilus-python").join("extensions").join("ssx-shell.py")
    }

    fn script_files(ctx: &Context) -> Result<Vec<ManagedFile>> {
        ctx.actions
            .iter()
            .map(|a| {
                if a.label.contains('/') || a.label.starts_with('.') {
                    return Err(ShellError::InvalidAction {
                        id: a.id.clone(),
                        reason: "Nautilus scripts are named after the label, which must not contain '/' or start with '.'".into(),
                    });
                }
                Ok(ManagedFile::new(
                    Self::scripts_dir(ctx).join(&a.label),
                    script_source(ctx, a)?,
                    0o755,
                ))
            })
            .collect()
    }

    fn extension_file(ctx: &Context) -> Result<ManagedFile> {
        Ok(ManagedFile::new(Self::extension_path(ctx), extension_source(ctx)?, 0o644))
    }

    fn wanted(self, ctx: &Context) -> Result<Vec<ManagedFile>> {
        let (scripts, ext) = self.resolved(ctx);
        let mut files = Vec::new();
        if scripts {
            files.extend(Self::script_files(ctx)?);
        }
        if ext {
            files.push(Self::extension_file(ctx)?);
        }
        Ok(files)
    }

    /// Everything of ours that may exist for either variant (for removal).
    fn all_known(ctx: &Context) -> Result<Vec<ManagedFile>> {
        let mut files = Self::script_files(ctx)?;
        files.push(Self::extension_file(ctx)?);
        Ok(files)
    }
}

impl Integration for Nautilus {
    fn id(&self) -> &'static str {
        "nautilus"
    }

    fn name(&self) -> &'static str {
        "Nautilus (GNOME Files)"
    }

    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn detect(&self, ctx: &Context) -> Detection {
        if ctx.has_binary("nautilus") {
            Detection::found("`nautilus` found in PATH")
        } else if ctx.data_home.join("nautilus").is_dir() {
            Detection::found("~/.local/share/nautilus exists")
        } else {
            Detection::missing("Nautilus is not installed")
        }
    }

    fn install(&self, ctx: &Context) -> Result<InstallOutcome> {
        let wanted = self.wanted(ctx)?;
        let (want_scripts, want_ext) = self.resolved(ctx);
        let mut outcome = install_files(&wanted)?;
        // Drop the variant we are not installing, otherwise both would show up.
        let mut stale = Vec::new();
        if !want_scripts {
            stale.extend(Self::script_files(ctx)?);
        }
        if !want_ext {
            stale.push(Self::extension_file(ctx)?);
        }
        if uninstall_files(&stale, ctx)? == UninstallOutcome::Removed
            && outcome == InstallOutcome::AlreadyPresent
        {
            outcome = InstallOutcome::Updated;
        }
        Ok(outcome)
    }

    fn uninstall(&self, ctx: &Context) -> Result<UninstallOutcome> {
        uninstall_files(&Self::all_known(ctx)?, ctx)
    }

    fn is_installed(&self, ctx: &Context) -> Result<bool> {
        files_current(&self.wanted(ctx)?)
    }

    fn describe(&self, ctx: &Context) -> Description {
        let (scripts, ext) = self.resolved(ctx);
        let mut paths = Vec::new();
        if scripts {
            paths.extend(ctx.actions.iter().map(|a| Self::scripts_dir(ctx).join(&a.label)));
        }
        if ext {
            paths.push(Self::extension_path(ctx));
        }
        let d = Description::paths(
            "nautilus",
            if ext {
                "Nautilus menu items via a nautilus-python extension"
            } else {
                "Nautilus scripts (right-click > Scripts)"
            },
            &paths,
        );
        if ext {
            d.note("requires the nautilus-python package (python3-nautilus / nautilus-python)")
                .note("restart Nautilus once: `nautilus -q`")
        } else {
            d.note(
                "scripts appear under the \"Scripts\" submenu and cannot be filtered by file type",
            )
            .note("for top-level, type-filtered entries install nautilus-python and reinstall")
        }
    }
}

/// The POSIX shell script for one action.
pub(crate) fn script_source(ctx: &Context, action: &Action) -> Result<String> {
    let exe = sh_single_quote(ctx.exe_str()?);
    let args: Vec<String> = action.exec_args.iter().map(|a| sh_word(a)).collect();
    let args = args.join(" ");
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    push_fmt!(
        s,
        "# {MARKER}: generated by ssx for Nautilus. Edits are overwritten; delete via ssx or remove this file.\n"
    );
    push_fmt!(s, "# Entry: {} ({})\n", action.label, action.id);
    push_fmt!(s, "SSX={exe}\n\n");
    s.push_str("# Nautilus passes the selected local files as arguments, relative to the folder\n");
    s.push_str("# being viewed. Rebuild \"$@\" with absolute paths (the app has another working\n");
    s.push_str("# directory) without word-splitting, globbing or evaluating any file name.\n");
    s.push_str("n=$#\nwhile [ \"$n\" -gt 0 ]; do\n    f=$1\n    shift\n    case $f in\n        /*) ;;\n        *) f=${PWD%/}/$f ;;\n    esac\n    set -- \"$@\" \"$f\"\n    n=$((n - 1))\ndone\n");
    if !action.filter.is_any() && !action.filter.extensions.is_empty() {
        let patterns: Vec<String> =
            action.filter.extensions.iter().map(|e| sh_case_ext_pattern(e)).collect();
        s.push_str("\n# Scripts cannot be filtered by type in Nautilus, so filter here.\n");
        s.push_str("n=$#\nwhile [ \"$n\" -gt 0 ]; do\n    f=$1\n    shift\n");
        push_fmt!(
            s,
            "    case $f in\n        {}) set -- \"$@\" \"$f\" ;;\n    esac\n",
            patterns.join("|")
        );
        s.push_str("    n=$((n - 1))\ndone\n");
    }
    s.push_str("\n[ \"$#\" -gt 0 ] || exit 0\n");
    if action.multi_select {
        push_fmt!(s, "exec \"$SSX\" {args} -- \"$@\"\n");
    } else {
        push_fmt!(s, "for f in \"$@\"; do\n    \"$SSX\" {args} -- \"$f\"\ndone\n");
    }
    Ok(s)
}

const EXTENSION_TEMPLATE: &str = r#"# @@MARKER@@: generated by ssx for Nautilus. Edits are overwritten; remove via ssx or delete this file.
# Requires nautilus-python (python3-nautilus / nautilus-python). Restart Nautilus: nautilus -q
import subprocess
import sys

import gi

try:
    gi.require_version("Nautilus", "4.0")
except ValueError:
    gi.require_version("Nautilus", "3.0")
from gi.repository import GObject, Nautilus

SSX = @@SSX@@

ACTIONS = [
@@ACTIONS@@]


def _local_paths(files):
    """Absolute paths of the selection, or [] if anything is not a local file."""
    paths = []
    for f in files:
        if f.get_uri_scheme() != "file":
            return []
        location = f.get_location()
        path = location.get_path() if location is not None else None
        if not path:
            return []
        paths.append(path)
    return paths


def _matches(action, f):
    if f.is_directory():
        return action["directories"]
    exts, mimes = action["extensions"], action["mimes"]
    if not exts and not mimes:
        return True
    stem, dot, ext = f.get_name().lower().rpartition(".")
    if exts and dot and stem and ext in exts:
        return True
    return bool(mimes) and f.get_mime_type() in mimes


class SsxMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def _items(self, files):
        files = list(files)
        paths = _local_paths(files)
        if not files or not paths:
            return []
        items = []
        for action in ACTIONS:
            if not action["multi"] and len(files) != 1:
                continue
            if not all(_matches(action, f) for f in files):
                continue
            item = Nautilus.MenuItem(
                name="SsxShell::" + action["id"],
                label=action["label"],
                tip=action["tip"],
            )
            item.connect("activate", self._on_activate, action, paths)
            items.append(item)
        return items

    def _on_activate(self, _menu, action, paths):
        # An argv list, never a shell string; "--" stops option parsing for names like "-x".
        argv = [SSX] + action["args"] + ["--"] + paths
        try:
            subprocess.Popen(argv, close_fds=True, start_new_session=True)
        except OSError as err:
            print("ssx: cannot start %s: %s" % (SSX, err), file=sys.stderr)

    # Nautilus 43+ (GTK4) calls get_file_items(files); older versions (window, files).
    def get_file_items(self, *args):
        return self._items(args[-1])

    def get_background_items(self, *args):
        return []
"#;

fn py_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| py_str(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// The `nautilus-python` extension source.
pub(crate) fn extension_source(ctx: &Context) -> Result<String> {
    let mut actions = String::new();
    for a in &ctx.actions {
        actions.push_str("    {\n");
        push_fmt!(actions, "        \"id\": {},\n", py_str(&a.id));
        push_fmt!(actions, "        \"label\": {},\n", py_str(&a.label));
        push_fmt!(actions, "        \"tip\": {},\n", py_str(&a.description));
        push_fmt!(actions, "        \"args\": {},\n", py_list(&a.exec_args));
        push_fmt!(
            actions,
            "        \"multi\": {},\n",
            if a.multi_select { "True" } else { "False" }
        );
        push_fmt!(
            actions,
            "        \"directories\": {},\n",
            if a.filter.directories { "True" } else { "False" }
        );
        let (exts, mimes) = if a.filter.is_any() {
            (Vec::new(), Vec::new())
        } else {
            (a.filter.extensions.clone(), a.filter.mime_types.clone())
        };
        push_fmt!(actions, "        \"extensions\": {},\n", py_list(&exts));
        push_fmt!(actions, "        \"mimes\": {},\n", py_list(&mimes));
        actions.push_str("    },\n");
    }
    Ok(EXTENSION_TEMPLATE
        .replace("@@MARKER@@", MARKER)
        .replace("@@SSX@@", &py_str(ctx.exe_str()?))
        .replace("@@ACTIONS@@", &actions))
}
