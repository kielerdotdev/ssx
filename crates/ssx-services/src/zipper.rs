//! The [`Zipper`] service: a folder becomes a temporary `.zip` for `post_file`.
//!
//! A folder handed to `ssx post-file` (a right-click on a directory) is user-controlled input
//! walked with the user's privileges, so the walk is defensive:
//!
//! * **Symlinks are never followed.** A link that points *outside* the folder is refused
//!   outright (uploading `~/docs/shared -> ~/.ssh` must not leak keys); a link that stays
//!   inside is skipped, because following it would duplicate content or loop.
//! * **Only regular files and directories** are archived. FIFOs, sockets and devices are
//!   skipped (reading a FIFO would hang forever).
//! * **Bounded:** total bytes, file count and depth are limited, both when planning and while
//!   copying (a file that grows during the walk cannot bypass the limit).
//! * Entry names are relative to the folder's *parent*, with `/` separators and the folder
//!   name as the single top-level entry (what a file manager's "Compress" does); there is no
//!   way to produce `..` or absolute names because names come only from the walk.
//! * The archive is written into a fresh private temp directory and removed again on error or
//!   cancellation. On success the engine deletes the archive when it has finished with it.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use ssx_core::workflow::{CancelToken, ServiceError, Zipper};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

/// Limits for [`FolderZipper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipLimits {
    /// Most uncompressed bytes in the archive.
    pub max_total_bytes: u64,
    /// Most files (directories do not count).
    pub max_files: usize,
    /// Deepest directory nesting.
    pub max_depth: usize,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self { max_total_bytes: 2 * 1024 * 1024 * 1024, max_files: 50_000, max_depth: 64 }
    }
}

/// The [`Zipper`] implementation.
#[derive(Debug, Clone)]
pub struct FolderZipper {
    limits: ZipLimits,
    temp_root: Option<PathBuf>,
}

impl Default for FolderZipper {
    fn default() -> Self {
        Self::new(ZipLimits::default())
    }
}

#[derive(Debug)]
enum Node {
    Dir { rel: String },
    File { path: PathBuf, rel: String, size: u64, mode: u32 },
}

impl FolderZipper {
    /// A zipper with the given limits, writing to the system temp directory.
    pub fn new(limits: ZipLimits) -> Self {
        Self { limits, temp_root: None }
    }

    /// Writes archives below `dir` instead of the system temp directory.
    #[must_use]
    pub fn in_temp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.temp_root = Some(dir.into());
        self
    }

    fn plan(
        &self,
        root: &Path,
        top: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<Node>, ServiceError> {
        let mut nodes = vec![Node::Dir { rel: format!("{top}/") }];
        let (mut files, mut total) = (0usize, 0u64);
        // (directory, archive prefix, depth)
        let mut stack = vec![(root.to_path_buf(), format!("{top}/"), 0usize)];
        while let Some((dir, prefix, depth)) = stack.pop() {
            cancel.check().map_err(|_| ServiceError::Cancelled)?;
            if depth >= self.limits.max_depth {
                return Err(ServiceError::failed(format!(
                    "the folder is nested more than {} levels deep ({}); zip it yourself if that is intended",
                    self.limits.max_depth,
                    dir.display()
                )));
            }
            let mut entries: Vec<_> = fs::read_dir(&dir)
                .map_err(|e| ServiceError::failed(format!("cannot read {}: {e}", dir.display())))?
                .collect::<Result<_, _>>()
                .map_err(|e| ServiceError::failed(format!("cannot read {}: {e}", dir.display())))?;
            entries.sort_by_key(fs::DirEntry::file_name);
            // Pushed in reverse so the walk is depth-first in name order.
            let mut subdirs = Vec::new();
            for entry in entries {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = format!("{prefix}{name}");
                let meta = fs::symlink_metadata(&path).map_err(|e| {
                    ServiceError::failed(format!("cannot read {}: {e}", path.display()))
                })?;
                let ft = meta.file_type();
                if ft.is_symlink() {
                    match fs::canonicalize(&path) {
                        Ok(target) if target.starts_with(root) => {
                            tracing::warn!(link = %path.display(), "skipping a symlink inside the folder");
                        }
                        Ok(target) => {
                            return Err(ServiceError::failed(format!(
                                "refusing to zip {}: it is a symbolic link to {}, outside the folder. \
                                 Remove the link or upload its target directly",
                                path.display(),
                                target.display()
                            )));
                        }
                        Err(_) => {
                            tracing::warn!(link = %path.display(), "skipping a broken symlink");
                        }
                    }
                } else if ft.is_dir() {
                    nodes.push(Node::Dir { rel: format!("{rel}/") });
                    subdirs.push((path, format!("{rel}/"), depth + 1));
                } else if ft.is_file() {
                    files += 1;
                    total = total.saturating_add(meta.len());
                    if files > self.limits.max_files {
                        return Err(ServiceError::failed(format!(
                            "the folder has more than {} files; zip it yourself if that is intended",
                            self.limits.max_files
                        )));
                    }
                    if total > self.limits.max_total_bytes {
                        return Err(ServiceError::failed(format!(
                            "the folder is larger than {} MiB uncompressed; zip it yourself if that is intended",
                            self.limits.max_total_bytes / (1024 * 1024)
                        )));
                    }
                    nodes.push(Node::File { path, rel, size: meta.len(), mode: mode_of(&meta) });
                } else {
                    tracing::warn!(path = %path.display(), "skipping a special file (fifo, socket or device)");
                }
            }
            stack.extend(subdirs.into_iter().rev());
        }
        Ok(nodes)
    }

    fn write(
        &self,
        nodes: &[Node],
        archive: &Path,
        cancel: &CancelToken,
    ) -> Result<(), ServiceError> {
        let io =
            |e: std::io::Error| ServiceError::failed(format!("cannot write the zip archive: {e}"));
        let zip_err = |e: zip::result::ZipError| {
            ServiceError::failed(format!("cannot write the zip archive: {e}"))
        };
        let mut zw = ZipWriter::new(File::create(archive).map_err(io)?);
        let base = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut written: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        for node in nodes {
            cancel.check().map_err(|_| ServiceError::Cancelled)?;
            match node {
                Node::Dir { rel } => zw.add_directory(rel, base).map_err(zip_err)?,
                Node::File { path, rel, size, mode } => {
                    // Re-check right before opening: a file swapped for a symlink since the
                    // plan is refused instead of followed.
                    let meta = fs::symlink_metadata(path).map_err(io)?;
                    if !meta.file_type().is_file() {
                        return Err(ServiceError::failed(format!(
                            "{} changed while it was being zipped",
                            path.display()
                        )));
                    }
                    let mut src = File::open(path).map_err(|e| {
                        ServiceError::failed(format!("cannot read {}: {e}", path.display()))
                    })?;
                    let opts =
                        base.unix_permissions(*mode).large_file(*size >= u64::from(u32::MAX));
                    zw.start_file(rel, opts).map_err(zip_err)?;
                    loop {
                        cancel.check().map_err(|_| ServiceError::Cancelled)?;
                        let n = src.read(&mut buf).map_err(|e| {
                            ServiceError::failed(format!("cannot read {}: {e}", path.display()))
                        })?;
                        if n == 0 {
                            break;
                        }
                        written += n as u64;
                        if written > self.limits.max_total_bytes {
                            return Err(ServiceError::failed(
                                "the folder grew past the size limit while it was being zipped",
                            ));
                        }
                        zw.write_all(&buf[..n]).map_err(io)?;
                    }
                }
            }
        }
        zw.finish().map_err(zip_err)?.sync_all().map_err(io)
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_: &fs::Metadata) -> u32 {
    0o644
}

impl Zipper for FolderZipper {
    fn zip_folder(&self, folder: &Path, cancel: &CancelToken) -> Result<PathBuf, ServiceError> {
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let root = fs::canonicalize(folder)
            .map_err(|e| ServiceError::failed(format!("cannot open {}: {e}", folder.display())))?;
        if !root.is_dir() {
            return Err(ServiceError::failed(format!("{} is not a folder", folder.display())));
        }
        let top = root
            .file_name()
            .map_or_else(|| "folder".to_owned(), |n| n.to_string_lossy().into_owned());
        let nodes = self.plan(&root, &top, cancel)?;

        // A private directory per archive: the archive keeps a clean, predictable name (that is
        // what the upload service shows) without clashing with another run.
        let mut builder = tempfile::Builder::new();
        builder.prefix("ssx-zip-");
        let dir = match &self.temp_root {
            Some(r) => builder.tempdir_in(r),
            None => builder.tempdir(),
        }?;
        let archive = dir.path().join(format!("{top}.zip"));
        // On any failure the directory (and the partial archive) is removed by `dir`'s drop.
        self.write(&nodes, &archive, cancel)?;
        let kept = dir.keep();
        Ok(kept.join(format!("{top}.zip")))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("my folder");
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a.txt"), "alpha").unwrap();
        fs::write(root.join("sub/b b.txt"), "bravo").unwrap();
        fs::write(root.join("sub/deeper/c.bin"), vec![9u8; 100_000]).unwrap();
        fs::write(root.join("-rf; $(x).txt"), "hostile name").unwrap();
        t
    }

    fn entries(zip: &Path) -> Vec<(String, Vec<u8>)> {
        let mut ar = zip::ZipArchive::new(File::open(zip).unwrap()).unwrap();
        (0..ar.len())
            .map(|i| {
                let mut f = ar.by_index(i).unwrap();
                let mut data = Vec::new();
                f.read_to_end(&mut data).unwrap();
                (f.name().to_owned(), data)
            })
            .collect()
    }

    fn cleanup(zip: &Path) {
        let _ = fs::remove_dir_all(zip.parent().unwrap());
    }

    #[test]
    fn zips_a_folder_with_a_single_top_level_entry() {
        let t = tree();
        let zip = FolderZipper::default()
            .zip_folder(&t.path().join("my folder"), &CancelToken::new())
            .unwrap();
        assert_eq!(zip.file_name().unwrap(), "my folder.zip");
        let e = entries(&zip);
        let names: Vec<&str> = e.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.iter().all(|n| n.starts_with("my folder/")), "{names:?}");
        for expected in [
            "my folder/",
            "my folder/a.txt",
            "my folder/sub/",
            "my folder/sub/b b.txt",
            "my folder/sub/deeper/c.bin",
            "my folder/empty/",
            "my folder/-rf; $(x).txt",
        ] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
        let get = |n: &str| e.iter().find(|(x, _)| x == n).unwrap().1.clone();
        assert_eq!(get("my folder/a.txt"), b"alpha");
        assert_eq!(get("my folder/sub/deeper/c.bin").len(), 100_000);
        assert!(
            names.iter().all(|n| !n.contains("..") && !n.starts_with('/') && !n.contains('\\'))
        );
        cleanup(&zip);
    }

    #[test]
    fn output_is_deterministic_in_order() {
        let t = tree();
        let z = FolderZipper::default();
        let a = z.zip_folder(&t.path().join("my folder"), &CancelToken::new()).unwrap();
        let b = z.zip_folder(&t.path().join("my folder"), &CancelToken::new()).unwrap();
        let names = |p: &Path| entries(p).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(&a), names(&b));
        cleanup(&a);
        cleanup(&b);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_folder_are_refused_and_inner_ones_skipped() {
        use std::os::unix::fs::symlink;
        let t = tree();
        let root = t.path().join("my folder");
        let secret = t.path().join("secret");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("id_rsa"), "PRIVATE").unwrap();

        // Inner link, broken link: both skipped, the zip still works.
        symlink(root.join("a.txt"), root.join("inner-link")).unwrap();
        symlink("/definitely/missing", root.join("broken-link")).unwrap();
        let zip = FolderZipper::default().zip_folder(&root, &CancelToken::new()).unwrap();
        let names: Vec<String> = entries(&zip).into_iter().map(|(n, _)| n).collect();
        assert!(!names.iter().any(|n| n.contains("link")), "{names:?}");
        cleanup(&zip);

        // Escaping links (file and directory): refused, nothing written.
        for (link, target) in [("escape-dir", &secret), ("escape-file", &secret.join("id_rsa"))] {
            symlink(target, root.join(link)).unwrap();
            let e = FolderZipper::default().zip_folder(&root, &CancelToken::new()).unwrap_err();
            let m = e.to_string();
            assert!(m.contains("refusing") && m.contains(link) && m.contains("outside"), "{m}");
            fs::remove_file(root.join(link)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_argument_is_resolved_and_special_files_are_skipped() {
        use std::os::unix::fs::symlink;
        let t = tree();
        let root = t.path().join("my folder");
        symlink(&root, t.path().join("alias")).unwrap();
        // A fifo would block a naive reader forever.
        let fifo = root.join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let zip = FolderZipper::default()
            .zip_folder(&t.path().join("alias"), &CancelToken::new())
            .unwrap();
        let names: Vec<String> = entries(&zip).into_iter().map(|(n, _)| n).collect();
        assert!(
            names.iter().all(|n| n.starts_with("my folder/")),
            "named after the real folder: {names:?}"
        );
        assert!(!names.iter().any(|n| n.ends_with("pipe")));
        cleanup(&zip);
    }

    #[test]
    fn limits_are_enforced_and_leave_no_archive_behind() {
        let t = tree();
        let temp = tempfile::tempdir().unwrap();
        let root = t.path().join("my folder");
        let small = |l: ZipLimits| FolderZipper::new(l).in_temp_dir(temp.path());
        let big = ZipLimits::default();

        let e = small(ZipLimits { max_total_bytes: 1000, ..big })
            .zip_folder(&root, &CancelToken::new())
            .unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
        let e = small(ZipLimits { max_files: 2, ..big })
            .zip_folder(&root, &CancelToken::new())
            .unwrap_err();
        assert!(e.to_string().contains("more than 2 files"), "{e}");
        let e = small(ZipLimits { max_depth: 2, ..big })
            .zip_folder(&root, &CancelToken::new())
            .unwrap_err();
        assert!(e.to_string().contains("nested"), "{e}");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0, "failures leave nothing behind");
    }

    #[test]
    fn cancellation_stops_and_cleans_up() {
        let t = tree();
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let e = FolderZipper::default()
            .in_temp_dir(temp.path())
            .zip_folder(&t.path().join("my folder"), &cancel)
            .unwrap_err();
        assert!(e.is_cancelled());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn not_a_folder_and_missing_paths_are_clear_errors() {
        let t = tree();
        let z = FolderZipper::default();
        let e = z.zip_folder(&t.path().join("my folder/a.txt"), &CancelToken::new()).unwrap_err();
        assert!(e.to_string().contains("not a folder"), "{e}");
        let e = z.zip_folder(&t.path().join("nope"), &CancelToken::new()).unwrap_err();
        assert!(e.to_string().contains("cannot open"), "{e}");
    }
}
