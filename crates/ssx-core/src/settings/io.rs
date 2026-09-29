//! Crash-safe file replacement.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.tmp-{}-{seq}", std::process::id()))
}

/// Replaces `path` with `bytes` so that a crash or power loss leaves either the complete
/// old file or the complete new file, never a truncated one:
///
/// 1. write a uniquely named temp file in the **same directory** (rename is only atomic
///    within a file system),
/// 2. `fsync` it,
/// 3. rename over the target (atomic replace on POSIX; `MoveFileEx(REPLACE_EXISTING)` on
///    Windows via `std::fs::rename`),
/// 4. `fsync` the directory on Unix so the rename itself is durable.
///
/// The temp file is removed if any step fails. Parent directories are created.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = temp_sibling(path);
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(dir);
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    // Best effort: some file systems refuse to fsync a directory; the data is already safe.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {
    // Windows cannot fsync a directory handle; NTFS journals the rename.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sub/settings.toml");
        atomic_write(&p, b"one").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        atomic_write(&p, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        atomic_write(&p, b"x").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x", "shorter content leaves no tail");
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.toml");
        for i in 0..5 {
            atomic_write(&p, format!("{i}").as_bytes()).unwrap();
        }
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.toml".to_owned()]);
    }

    #[test]
    fn failed_write_keeps_old_file_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("target");
        // Target is a non-empty directory: rename over it must fail.
        std::fs::create_dir(&p).unwrap();
        std::fs::write(p.join("child"), b"").unwrap();
        assert!(atomic_write(&p, b"data").is_err());
        assert!(p.is_dir(), "old target untouched");
        let leftovers = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file removed");
    }

    #[test]
    fn concurrent_writers_never_produce_torn_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = std::sync::Arc::new(tmp.path().join("shared"));
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let p = std::sync::Arc::clone(&p);
                std::thread::spawn(move || {
                    let body = vec![b'a' + i as u8; 10_000];
                    for _ in 0..20 {
                        atomic_write(&p, &body).unwrap();
                        let read = std::fs::read(&*p).unwrap();
                        assert_eq!(read.len(), 10_000);
                        assert!(read.iter().all(|b| *b == read[0]), "torn file");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
