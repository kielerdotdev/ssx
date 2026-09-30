//! Safe, reversible file installation shared by the file-based integrations.
//!
//! Every file we write contains [`MARKER`]. That is how we distinguish "ours, safe to update or
//! delete" from "the user's, never touch": install refuses to overwrite an unmarked file and
//! uninstall never deletes one. Writes are atomic (temp file + rename) so a crash cannot leave a
//! half-written script that a file manager would then try to run.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::error::{Result, ShellError};
use crate::integration::{InstallOutcome, UninstallOutcome};

/// Text embedded in every generated file so ssx can recognise its own output.
pub const MARKER: &str = "ssx-shell-managed";

/// A file the installer owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedFile {
    pub path: PathBuf,
    pub content: String,
    /// Unix permission bits (ignored elsewhere).
    pub mode: u32,
}

impl ManagedFile {
    pub(crate) fn new(path: PathBuf, content: String, mode: u32) -> Self {
        debug_assert!(
            content.contains(MARKER),
            "generated file {} lacks the marker",
            path.display()
        );
        Self { path, content, mode }
    }
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ShellError::io("reading", path)(e)),
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777)
}

/// Whether `file` exists with identical content (and mode on Unix).
fn is_current(file: &ManagedFile) -> Result<bool> {
    let same_content = read_existing(&file.path)?.is_some_and(|c| c == file.content);
    #[cfg(unix)]
    let same_mode = mode_of(&file.path) == Some(file.mode);
    #[cfg(not(unix))]
    let same_mode = true;
    Ok(same_content && same_mode)
}

/// Installs `files` all-or-nothing with respect to conflicts: if any target exists and is not
/// ours, nothing is written.
pub(crate) fn install_files(files: &[ManagedFile]) -> Result<InstallOutcome> {
    for f in files {
        if let Some(existing) = read_existing(&f.path)?
            && !existing.contains(MARKER)
        {
            return Err(ShellError::NotManaged { path: f.path.clone() });
        }
    }
    let mut created = false;
    let mut updated = false;
    for f in files {
        if is_current(f)? {
            continue;
        }
        if f.path.exists() {
            updated = true;
        } else {
            created = true;
        }
        write_atomic(&f.path, f.content.as_bytes(), f.mode)?;
    }
    Ok(match (created, updated) {
        (false, false) => InstallOutcome::AlreadyPresent,
        (_, true) => InstallOutcome::Updated,
        (true, false) => InstallOutcome::Installed,
    })
}

/// Whether every file is present and current.
pub(crate) fn files_current(files: &[ManagedFile]) -> Result<bool> {
    for f in files {
        if !is_current(f)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Removes those `files` that carry our marker; empties parent directories that became empty.
pub(crate) fn uninstall_files(files: &[ManagedFile], ctx: &Context) -> Result<UninstallOutcome> {
    let mut removed = false;
    for f in files {
        let Some(existing) = read_existing(&f.path)? else { continue };
        if !existing.contains(MARKER) {
            tracing::warn!(path = %f.path.display(), "leaving a file that ssx did not create");
            continue;
        }
        fs::remove_file(&f.path).map_err(ShellError::io("removing", &f.path))?;
        removed = true;
        if let Some(parent) = f.path.parent() {
            prune_empty_dirs(parent, ctx);
        }
    }
    Ok(if removed { UninstallOutcome::Removed } else { UninstallOutcome::NotPresent })
}

/// Removes `dir` and its ancestors while they are empty, stopping at the user's home (or, for
/// paths outside it, at the XDG base directory), so an uninstall leaves the tree as it was
/// before install even when install had to create `~/.local/share/...`.
pub(crate) fn prune_empty_dirs(dir: &Path, ctx: &Context) {
    let stop: &Path = if dir.starts_with(&ctx.home) {
        &ctx.home
    } else if dir.starts_with(&ctx.data_home) {
        &ctx.data_home
    } else if dir.starts_with(&ctx.config_home) {
        &ctx.config_home
    } else {
        return;
    };
    let mut cur = dir;
    while cur != stop && cur.starts_with(stop) {
        if fs::remove_dir(cur).is_err() {
            break; // not empty (or not ours to remove)
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
}

/// Writes `bytes` to `path` atomically with the given Unix mode, creating parent directories.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| ShellError::Unavailable(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent).map_err(ShellError::io("creating", parent))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!(".{name}.ssx-tmp"));
    let result = (|| -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The open() mode is masked by umask; make it exact.
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        drop(fs::remove_file(&tmp));
        return Err(ShellError::io("writing", path)(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(root: &Path) -> Context {
        Context::sandboxed(root, "/usr/bin/ssx")
    }

    fn file(root: &Path, rel: &str, body: &str) -> ManagedFile {
        ManagedFile::new(root.join("home").join(rel), format!("# {MARKER}\n{body}\n"), 0o644)
    }

    #[test]
    fn install_update_uninstall_cycle_restores_tree() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = ctx(tmp.path());
        fs::create_dir_all(&c.home).expect("home");
        let f = file(tmp.path(), ".local/share/x/y/a.txt", "one");
        assert_eq!(install_files(std::slice::from_ref(&f)).expect("i"), InstallOutcome::Installed);
        assert_eq!(
            install_files(std::slice::from_ref(&f)).expect("i2"),
            InstallOutcome::AlreadyPresent
        );
        let f2 = file(tmp.path(), ".local/share/x/y/a.txt", "two");
        assert_eq!(install_files(std::slice::from_ref(&f2)).expect("i3"), InstallOutcome::Updated);
        assert!(files_current(std::slice::from_ref(&f2)).expect("cur"));
        assert!(!files_current(std::slice::from_ref(&f)).expect("cur"));
        assert_eq!(
            uninstall_files(std::slice::from_ref(&f2), &c).expect("u"),
            UninstallOutcome::Removed
        );
        assert_eq!(uninstall_files(&[f2], &c).expect("u2"), UninstallOutcome::NotPresent);
        assert_eq!(fs::read_dir(&c.home).expect("ls").count(), 0, "empty dirs must be pruned");
    }

    #[test]
    fn refuses_to_clobber_foreign_files_and_keeps_them_on_uninstall() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = ctx(tmp.path());
        let f = file(tmp.path(), ".local/share/x/a.txt", "ours");
        fs::create_dir_all(f.path.parent().expect("parent")).expect("mkdir");
        fs::write(&f.path, "user data").expect("write");
        let other = file(tmp.path(), ".local/share/x/b.txt", "ours");
        let err = install_files(&[other.clone(), f.clone()]).expect_err("conflict");
        assert!(matches!(err, ShellError::NotManaged { .. }), "{err}");
        assert!(!other.path.exists(), "all-or-nothing: nothing may be written");
        assert_eq!(
            uninstall_files(std::slice::from_ref(&f), &c).expect("u"),
            UninstallOutcome::NotPresent
        );
        assert_eq!(fs::read_to_string(&f.path).expect("read"), "user data");
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_exact_and_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tmp");
        let mut f = file(tmp.path(), "s.sh", "x");
        f.mode = 0o755;
        install_files(std::slice::from_ref(&f)).expect("i");
        assert_eq!(mode_of(&f.path), Some(0o755));
        fs::set_permissions(&f.path, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(!files_current(std::slice::from_ref(&f)).expect("cur"));
        assert_eq!(install_files(std::slice::from_ref(&f)).expect("i"), InstallOutcome::Updated);
        assert_eq!(mode_of(&f.path), Some(0o755));
    }
}
