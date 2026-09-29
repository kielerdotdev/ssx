//! Settings-file schema migrations.
//!
//! The file carries a top-level `version`. On load the raw TOML table is upgraded step by
//! step (`vN -> vN+1`) *before* it is deserialised into the typed structs, so migrations can
//! rename, move and reshape keys that the current structs no longer know. A file written by
//! a **newer** ssx is refused rather than half-understood (and so never overwritten).

use toml::{Table, Value};

/// Schema version this build writes.
pub const CURRENT_VERSION: u32 = 1;

/// One upgrade step: transforms a table of version `from` into version `from + 1`.
/// It must not touch the `version` key; the driver stamps it.
pub type MigrationFn = fn(&mut Table) -> Result<(), String>;

/// A registered migration.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Version this step upgrades *from*.
    pub from: u32,
    /// The transformation.
    pub apply: MigrationFn,
}

/// Migrations shipped with this build, ordered by `from`.
pub const MIGRATIONS: &[Migration] = &[Migration { from: 0, apply: v0_to_v1 }];

/// Why migration failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrateError {
    /// The file is from a newer ssx.
    #[error(
        "the settings file has version {found}, but this ssx only understands up to version {supported}; upgrade ssx (the file was not modified)"
    )]
    TooNew {
        /// Version in the file.
        found: u32,
        /// Highest version this build supports.
        supported: u32,
    },
    /// The `version` key is present but not a non-negative integer.
    #[error(
        "the settings file has an invalid `version` value ({0}); it must be a whole number such as {CURRENT_VERSION}"
    )]
    BadVersion(String),
    /// No migration is registered for a needed step (a bug in ssx).
    #[error("no migration from settings version {0}; this is a bug in ssx")]
    MissingStep(u32),
    /// A migration step failed.
    #[error("migrating settings from version {from} failed: {reason}")]
    Step {
        /// Version being upgraded.
        from: u32,
        /// What went wrong.
        reason: String,
    },
}

/// Reads the `version` key. Files without one are treated as version 0 (written before
/// versioning existed, or by hand).
pub fn read_version(table: &Table) -> Result<u32, MigrateError> {
    match table.get("version") {
        None => Ok(0),
        Some(Value::Integer(i)) => {
            u32::try_from(*i).map_err(|_| MigrateError::BadVersion(i.to_string()))
        }
        Some(other) => Err(MigrateError::BadVersion(other.to_string())),
    }
}

/// Upgrades `table` to `target` using `steps`, returning the version it started at.
pub fn migrate_with(
    table: &mut Table,
    target: u32,
    steps: &[Migration],
) -> Result<u32, MigrateError> {
    let start = read_version(table)?;
    if start > target {
        return Err(MigrateError::TooNew { found: start, supported: target });
    }
    let mut v = start;
    while v < target {
        let step = steps.iter().find(|m| m.from == v).ok_or(MigrateError::MissingStep(v))?;
        (step.apply)(table).map_err(|reason| MigrateError::Step { from: v, reason })?;
        v += 1;
        table.insert("version".to_owned(), Value::Integer(i64::from(v)));
    }
    Ok(start)
}

/// Upgrades `table` to [`CURRENT_VERSION`]; returns the version it was at.
pub fn migrate(table: &mut Table) -> Result<u32, MigrateError> {
    migrate_with(table, CURRENT_VERSION, MIGRATIONS)
}

/// v0 -> v1: development builds stored a few keys under different names. Renames
/// `general.jpg_quality` -> `general.image_quality` and `capture.cursor` ->
/// `capture.show_cursor`, and `destinations.videos` -> `destinations.video`.
fn v0_to_v1(t: &mut Table) -> Result<(), String> {
    rename_key(t, "general", "jpg_quality", "image_quality")?;
    rename_key(t, "capture", "cursor", "show_cursor")?;
    rename_key(t, "destinations", "videos", "video")?;
    Ok(())
}

/// Renames `section.from` to `section.to` unless `to` already exists.
fn rename_key(root: &mut Table, section: &str, from: &str, to: &str) -> Result<(), String> {
    let Some(sec) = root.get_mut(section) else { return Ok(()) };
    let Some(sec) = sec.as_table_mut() else {
        return Err(format!("`{section}` must be a table"));
    };
    if let Some(v) = sec.remove(from) {
        sec.entry(to.to_owned()).or_insert(v);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Table {
        s.parse().unwrap()
    }

    #[test]
    fn missing_version_is_zero_and_gets_stamped() {
        let mut t = parse("[general]\nimage_quality = 80\n");
        assert_eq!(read_version(&t).unwrap(), 0);
        assert_eq!(migrate(&mut t).unwrap(), 0);
        assert_eq!(read_version(&t).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn v0_renames_keys() {
        let mut t = parse(
            "[general]\njpg_quality = 77\n[capture]\ncursor = true\n[destinations]\nvideos = \"yt\"\n",
        );
        migrate(&mut t).unwrap();
        assert_eq!(t["general"]["image_quality"].as_integer(), Some(77));
        assert!(t["general"].get("jpg_quality").is_none());
        assert_eq!(t["capture"]["show_cursor"].as_bool(), Some(true));
        assert_eq!(t["destinations"]["video"].as_str(), Some("yt"));
    }

    #[test]
    fn rename_does_not_clobber_existing_new_key() {
        let mut t = parse("[general]\njpg_quality = 1\nimage_quality = 2\n");
        migrate(&mut t).unwrap();
        assert_eq!(t["general"]["image_quality"].as_integer(), Some(2));
    }

    #[test]
    fn rename_with_wrong_section_type_fails_clearly() {
        let mut t = parse("general = 5\n");
        let e = migrate(&mut t).unwrap_err();
        assert!(matches!(e, MigrateError::Step { from: 0, .. }));
        assert!(e.to_string().contains("general"));
    }

    #[test]
    fn current_version_is_untouched() {
        let mut t = parse("version = 1\n[general]\njpg_quality = 5\n");
        assert_eq!(migrate(&mut t).unwrap(), 1);
        assert!(t["general"].get("jpg_quality").is_some(), "no migration ran");
    }

    #[test]
    fn newer_file_is_refused() {
        let mut t = parse("version = 99\n");
        let e = migrate(&mut t).unwrap_err();
        assert_eq!(e, MigrateError::TooNew { found: 99, supported: CURRENT_VERSION });
        assert!(e.to_string().contains("upgrade ssx"));
    }

    #[test]
    fn bad_version_values() {
        for src in ["version = \"1\"", "version = -1", "version = 1.5", "version = true"] {
            let mut t = parse(src);
            assert!(matches!(migrate(&mut t), Err(MigrateError::BadVersion(_))), "{src}");
        }
    }

    #[test]
    fn multi_step_chain_runs_in_order() {
        #[allow(clippy::unnecessary_wraps)] // must match `MigrationFn`
        fn a(t: &mut Table) -> Result<(), String> {
            t.insert("trace".into(), Value::String("a".into()));
            Ok(())
        }
        #[allow(clippy::unnecessary_wraps)] // must match `MigrationFn`
        fn b(t: &mut Table) -> Result<(), String> {
            let cur = t["trace"].as_str().unwrap().to_owned();
            t.insert("trace".into(), Value::String(cur + "b"));
            Ok(())
        }
        #[allow(clippy::unnecessary_wraps)] // must match `MigrationFn`
        fn c(t: &mut Table) -> Result<(), String> {
            let cur = t["trace"].as_str().unwrap().to_owned();
            t.insert("trace".into(), Value::String(cur + "c"));
            Ok(())
        }
        let steps = [
            Migration { from: 2, apply: c },
            Migration { from: 0, apply: a },
            Migration { from: 1, apply: b },
        ];
        let mut t = Table::new();
        assert_eq!(migrate_with(&mut t, 3, &steps).unwrap(), 0);
        assert_eq!(t["trace"].as_str(), Some("abc"));
        assert_eq!(read_version(&t).unwrap(), 3);
        // starting mid-way skips earlier steps
        let mut t = parse("version = 2\ntrace = \"x\"");
        migrate_with(&mut t, 3, &steps).unwrap();
        assert_eq!(t["trace"].as_str(), Some("xc"));
    }

    #[test]
    fn missing_step_is_reported() {
        let mut t = parse("version = 1");
        assert_eq!(migrate_with(&mut t, 2, &[]).unwrap_err(), MigrateError::MissingStep(1));
    }

    #[test]
    fn failed_step_reports_version() {
        fn boom(_: &mut Table) -> Result<(), String> {
            Err("nope".into())
        }
        let mut t = parse("version = 4");
        let e = migrate_with(&mut t, 5, &[Migration { from: 4, apply: boom }]).unwrap_err();
        assert_eq!(e, MigrateError::Step { from: 4, reason: "nope".into() });
    }
}
