//! Collision-free file creation.
//!
//! The naming scheme is ShareX's `GetUniqueFilePath`: `name.png`, `name (2).png`,
//! `name (3).png`, … and an existing `name (7).png` continues at `(8)`. Unlike ShareX
//! (`File.Exists` then write) creation goes through `O_EXCL`/`CREATE_NEW`
//! ([`OpenOptions::create_new`]), so two processes racing for the same name can never
//! both win and nothing is ever overwritten.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::sanitize::{MAX_FILE_NAME_BYTES, is_windows_reserved, truncate_bytes};

/// Give up after this many candidates (a directory with 100k same-named files is broken).
const MAX_ATTEMPTS: u32 = 100_000;

/// Splits `stem (n)` into (`stem`, `n`).
fn split_counter(stem: &str) -> Option<(&str, u32)> {
    let inner = stem.strip_suffix(')')?;
    let (base, num) = inner.rsplit_once(" (")?;
    if base.is_empty() || num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base, num.parse().ok()?))
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// The `attempt`th candidate for `file_name`: attempt 0 is the name itself, attempt `k`
/// appends ` (n)` where `n` continues from an existing ` (n)` suffix (minimum 2).
/// The stem is shortened (on grapheme boundaries) so the result stays within 255 bytes.
pub fn candidate_name(file_name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return file_name.to_owned();
    }
    let (stem, ext) = split_ext(file_name);
    let (base, start) = split_counter(stem).map_or((stem, 1), |(b, n)| (b, n));
    let n = start.saturating_add(attempt);
    let suffix = format!(" ({n}){ext}");
    let budget = MAX_FILE_NAME_BYTES.saturating_sub(suffix.len());
    let base = truncate_bytes(base, budget).trim_end_matches([' ', '.']);
    let name = format!("{base}{suffix}");
    if is_windows_reserved(&name) { format!("_{name}") } else { name }
}

/// Atomically creates a new, empty file named `file_name` in `dir` (creating `dir` if
/// needed), choosing a free name. Returns the final path and the open handle.
pub fn create_unique(dir: &Path, file_name: &str) -> io::Result<(PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    for attempt in 0..MAX_ATTEMPTS {
        let path = dir.join(candidate_name(file_name, attempt));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // On Windows a *directory* with that name, or a name held open with delete
            // pending, reports PermissionDenied rather than AlreadyExists. Skip if a
            // path with that name exists, otherwise it is a real permission error.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && path.exists() => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not find a free name for {file_name:?} in {}", dir.display()),
    ))
}

/// Creates a unique file in `dir` and writes `bytes` to it; on a write failure the empty
/// placeholder is removed again. Returns the final path.
pub fn write_unique(dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let (path, mut file) = create_unique(dir, file_name)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    match result {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc, thread};

    use super::*;

    #[test]
    fn candidate_sequence() {
        assert_eq!(candidate_name("a.png", 0), "a.png");
        assert_eq!(candidate_name("a.png", 1), "a (2).png");
        assert_eq!(candidate_name("a.png", 2), "a (3).png");
        assert_eq!(candidate_name("a (7).png", 1), "a (8).png");
        assert_eq!(candidate_name("noext", 1), "noext (2)");
        assert_eq!(candidate_name(".hidden", 1), ".hidden (2)");
        assert_eq!(candidate_name("a (x).png", 1), "a (x) (2).png");
        assert_eq!(candidate_name("(3).png", 1), "(3) (2).png");
    }

    #[test]
    fn candidate_stays_within_limit() {
        let long = format!("{}.png", "é".repeat(200));
        for attempt in [1, 9, 10, 999] {
            let c = candidate_name(&long, attempt);
            assert!(c.len() <= 255, "{attempt}: {}", c.len());
            assert!(c.ends_with(".png"));
        }
    }

    #[test]
    fn creates_dir_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/deeper");
        let p1 = write_unique(&dir, "shot.png", b"one").unwrap();
        let p2 = write_unique(&dir, "shot.png", b"two").unwrap();
        let p3 = write_unique(&dir, "shot.png", b"three").unwrap();
        assert_eq!(p1.file_name().unwrap(), "shot.png");
        assert_eq!(p2.file_name().unwrap(), "shot (2).png");
        assert_eq!(p3.file_name().unwrap(), "shot (3).png");
        assert_eq!(std::fs::read(&p1).unwrap(), b"one");
        assert_eq!(std::fs::read(&p2).unwrap(), b"two");
    }

    #[test]
    fn skips_directories_with_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("x.png")).unwrap();
        let p = write_unique(tmp.path(), "x.png", b"d").unwrap();
        assert_eq!(p.file_name().unwrap(), "x (2).png");
    }

    #[test]
    fn continues_after_existing_numbered_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.png"), b"").unwrap();
        std::fs::write(tmp.path().join("a (5).png"), b"").unwrap();
        // Requesting "a (5).png" (exists) must yield (6), not clobber or reuse (2).
        let p = write_unique(tmp.path(), "a (5).png", b"").unwrap();
        assert_eq!(p.file_name().unwrap(), "a (6).png");
    }

    #[test]
    fn concurrent_creation_yields_distinct_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Arc::new(tmp.path().to_path_buf());
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let dir = Arc::clone(&dir);
                thread::spawn(move || {
                    (0..8)
                        .map(|j| {
                            let body = format!("{i}-{j}");
                            let p = write_unique(&dir, "race.png", body.as_bytes()).unwrap();
                            (p, body)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for (p, body) in h.join().unwrap() {
                assert!(seen.insert(p.clone()), "duplicate path {p:?}");
                assert_eq!(std::fs::read_to_string(&p).unwrap(), body, "content clobbered");
            }
        }
        assert_eq!(seen.len(), 16 * 8);
    }

    #[test]
    fn missing_permission_is_a_real_error() {
        // A file used as a directory component cannot be created under.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file");
        std::fs::write(&f, b"").unwrap();
        assert!(create_unique(&f.join("sub"), "a.png").is_err());
    }
}
