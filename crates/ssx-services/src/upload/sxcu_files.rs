//! `.sxcu` files in the config directory: discovery, import and removal.
//!
//! `ShareX` users bring their custom uploaders as `.sxcu` files. ssx keeps imported copies in
//! `<config dir>/uploaders/<name>.sxcu`, so the set of destinations is visible, backed up
//! and hand-editable together with `settings.toml`; the file stem is the uploader name.

use std::path::{Path, PathBuf};

use super::config::{self, Built};

/// The folder that holds imported `.sxcu` files.
pub fn sxcu_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("uploaders")
}

/// The characters an uploader name may use (same rule as `ssx-core`'s validator).
pub fn valid_uploader_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Turns arbitrary text (a file stem, an uploader's display name) into a valid uploader name.
pub fn sanitize_name(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    while s.starts_with(['.', '-']) {
        s.remove(0);
    }
    s.truncate(64);
    let s = s.trim_end_matches(['.', '-']).to_owned();
    if s.is_empty() { "custom".to_owned() } else { s }
}

/// Loads every `*.sxcu` file, sorted by file name. Returns `(name, path, result)`; a file
/// that cannot be loaded is reported, never skipped silently and never fatal.
pub fn load_dir(config_dir: &Path) -> Vec<(String, PathBuf, Result<Built, String>)> {
    let dir = sxcu_dir(config_dir);
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sxcu")) && p.is_file())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let stem =
                path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let name = sanitize_name(&stem);
            let built = config::load_sxcu_file(&path);
            (name, path, built)
        })
        .collect()
}

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The source file could not be read or is not a usable `.sxcu`.
    #[error("{0}")]
    Invalid(String),
    /// `--name` is not a valid uploader name.
    #[error(
        "{0:?} is not a valid uploader name; use letters, digits, '.', '_' or '-' (at most 64 characters)"
    )]
    BadName(String),
    /// A file with this name is already imported.
    #[error("{} already exists; choose another --name, remove it first or pass --force", .0.display())]
    Exists(PathBuf),
    /// Writing the copy failed.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// Destination file.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// A successful import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// Name to use in settings and `--to`.
    pub name: String,
    /// Where the copy was written.
    pub path: PathBuf,
    /// The uploader's display name from the file.
    pub display_name: String,
    /// Non-fatal findings from the `.sxcu` checker.
    pub warnings: Vec<String>,
}

/// Validates `source` and copies it to `<config dir>/uploaders/<name>.sxcu`.
///
/// The name is `name` if given, else derived from the file's `Name` (or its file stem). An
/// existing file is only replaced with `overwrite`.
pub fn import_sxcu(
    config_dir: &Path,
    source: &Path,
    name: Option<&str>,
    overwrite: bool,
) -> Result<Imported, ImportError> {
    let (def, warnings) = config::read_sxcu(source).map_err(ImportError::Invalid)?;
    let display_name = def.display_name();
    let name = match name {
        Some(n) if valid_uploader_name(n) => n.to_owned(),
        Some(n) => return Err(ImportError::BadName(n.to_owned())),
        None => {
            let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned());
            sanitize_name(&if def.name.trim().is_empty() {
                stem.unwrap_or_default()
            } else {
                def.name.clone()
            })
        }
    };
    let dest = sxcu_dir(config_dir).join(format!("{name}.sxcu"));
    if dest.exists() && !overwrite {
        return Err(ImportError::Exists(dest));
    }
    // Re-serialise instead of copying bytes: the stored file is then always in the canonical
    // ShareX form, whatever quirks (BOM, odd key case) the original had.
    let text = def.to_json_string();
    let write = |source| ImportError::Write { path: dest.clone(), source };
    std::fs::create_dir_all(sxcu_dir(config_dir)).map_err(write)?;
    ssx_core::settings::atomic_write(&dest, text.as_bytes()).map_err(write)?;
    Ok(Imported { name, path: dest, display_name, warnings })
}

/// Removes the imported `.sxcu` called `name`. Returns `false` if there was none.
pub fn remove_sxcu(config_dir: &Path, name: &str) -> std::io::Result<bool> {
    if !valid_uploader_name(name) {
        return Ok(false);
    }
    match std::fs::remove_file(sxcu_dir(config_dir).join(format!("{name}.sxcu"))) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"Version":"14.0.0","Name":"My Host!","DestinationType":"ImageUploader",
        "RequestMethod":"POST","RequestURL":"http://127.0.0.1:9/up","Body":"MultipartFormData",
        "FileFormName":"file","URL":"{json:url}"}"#;

    #[test]
    fn names_are_sanitised_and_validated() {
        assert_eq!(sanitize_name("My Host!"), "My-Host");
        assert_eq!(sanitize_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_name("  "), "custom");
        assert_eq!(sanitize_name("é"), "custom");
        assert!(sanitize_name(&"a".repeat(200)).len() <= 64);
        assert!(valid_uploader_name("a.b_c-d"));
        for bad in ["", "a b", "a/b", "a\\b", &"x".repeat(65)] {
            assert!(!valid_uploader_name(bad), "{bad}");
        }
    }

    #[test]
    fn import_list_and_remove_round_trip() {
        let cfg = tempfile::tempdir().unwrap();
        let src = cfg.path().join("download.sxcu");
        std::fs::write(&src, GOOD).unwrap();

        let imp = import_sxcu(cfg.path(), &src, None, false).unwrap();
        assert_eq!(imp.name, "My-Host", "derived from the Name field");
        assert_eq!(imp.display_name, "My Host!");
        assert!(imp.path.ends_with("uploaders/My-Host.sxcu"));

        let dir = load_dir(cfg.path());
        assert_eq!(dir.len(), 1);
        assert_eq!(dir[0].0, "My-Host");
        assert!(dir[0].2.is_ok());

        let again = import_sxcu(cfg.path(), &src, None, false).unwrap_err();
        assert!(matches!(again, ImportError::Exists(_)), "{again}");
        assert!(import_sxcu(cfg.path(), &src, None, true).is_ok(), "--force replaces");
        let named = import_sxcu(cfg.path(), &src, Some("other"), false).unwrap();
        assert_eq!(named.name, "other");
        assert!(matches!(
            import_sxcu(cfg.path(), &src, Some("a/b"), false),
            Err(ImportError::BadName(_))
        ));

        assert!(remove_sxcu(cfg.path(), "My-Host").unwrap());
        assert!(!remove_sxcu(cfg.path(), "My-Host").unwrap());
        assert!(!remove_sxcu(cfg.path(), "../evil").unwrap(), "no path traversal");
        assert_eq!(load_dir(cfg.path()).len(), 1);
    }

    #[test]
    fn invalid_files_are_rejected_and_broken_ones_are_reported() {
        let cfg = tempfile::tempdir().unwrap();
        let bad = cfg.path().join("bad.sxcu");
        std::fs::write(&bad, "{nope").unwrap();
        assert!(matches!(import_sxcu(cfg.path(), &bad, None, false), Err(ImportError::Invalid(_))));
        assert!(!sxcu_dir(cfg.path()).exists(), "nothing was created for a rejected import");

        std::fs::create_dir_all(sxcu_dir(cfg.path())).unwrap();
        std::fs::write(sxcu_dir(cfg.path()).join("broken.sxcu"), "{nope").unwrap();
        std::fs::write(sxcu_dir(cfg.path()).join("fine.SXCU"), GOOD).unwrap();
        std::fs::write(sxcu_dir(cfg.path()).join("notes.txt"), "ignored").unwrap();
        let dir = load_dir(cfg.path());
        assert_eq!(dir.len(), 2, "only .sxcu files, any case");
        assert!(dir.iter().find(|d| d.0 == "broken").unwrap().2.is_err());
        assert!(dir.iter().find(|d| d.0 == "fine").unwrap().2.is_ok());
        assert!(load_dir(&cfg.path().join("missing")).is_empty());
    }
}
