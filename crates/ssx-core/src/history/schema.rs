//! Schema and migrations.
//!
//! `PRAGMA user_version` is the schema version. `MIGRATIONS[n]` upgrades version `n` to
//! `n + 1`; all pending steps run inside one `BEGIN IMMEDIATE` transaction, and the version
//! is re-read *inside* that transaction so two processes opening an old database at the same
//! moment (tray app and a CLI call) cannot both apply a step.
//!
//! The full-text index is deliberately **not** a numbered migration: it is an optional
//! accelerator created (or rebuilt) on open whenever the linked SQLite has FTS5, so a
//! database created by a build without FTS5 gains it later, and search falls back to `LIKE`
//! when it is unavailable.

use rusqlite::{Connection, TransactionBehavior};

use super::HistoryError;

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = 2;

/// v0 -> v1: the original table.
pub(super) const V1: &str = "
CREATE TABLE entries (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    INTEGER NOT NULL,
    kind          TEXT    NOT NULL CHECK (kind IN ('image','video','file','text','url')),
    local_path    TEXT,
    thumbnail     BLOB,
    upload_url    TEXT,
    thumbnail_url TEXT,
    deletion_url  TEXT,
    uploader      TEXT,
    window_title  TEXT,
    process_name  TEXT,
    width         INTEGER,
    height        INTEGER,
    size_bytes    INTEGER,
    sha256        TEXT
);
CREATE INDEX entries_created_at ON entries (created_at);
CREATE INDEX entries_kind_created ON entries (kind, created_at);
";

/// v1 -> v2: which workflow made the entry, and free text for snippets.
const V2: &str = "
ALTER TABLE entries ADD COLUMN workflow_id TEXT;
ALTER TABLE entries ADD COLUMN note        TEXT;
CREATE INDEX entries_sha256 ON entries (sha256) WHERE sha256 IS NOT NULL;
";

const MIGRATIONS: &[&str] = &[V1, V2];

const FTS: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    local_path, upload_url, window_title, process_name, uploader, note, workflow_id,
    content='entries', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS entries_fts_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts (rowid, local_path, upload_url, window_title, process_name, uploader, note, workflow_id)
    VALUES (new.id, new.local_path, new.upload_url, new.window_title, new.process_name, new.uploader, new.note, new.workflow_id);
END;
CREATE TRIGGER IF NOT EXISTS entries_fts_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, local_path, upload_url, window_title, process_name, uploader, note, workflow_id)
    VALUES ('delete', old.id, old.local_path, old.upload_url, old.window_title, old.process_name, old.uploader, old.note, old.workflow_id);
END;
CREATE TRIGGER IF NOT EXISTS entries_fts_au
AFTER UPDATE OF local_path, upload_url, window_title, process_name, uploader, note, workflow_id ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, local_path, upload_url, window_title, process_name, uploader, note, workflow_id)
    VALUES ('delete', old.id, old.local_path, old.upload_url, old.window_title, old.process_name, old.uploader, old.note, old.workflow_id);
    INSERT INTO entries_fts (rowid, local_path, upload_url, window_title, process_name, uploader, note, workflow_id)
    VALUES (new.id, new.local_path, new.upload_url, new.window_title, new.process_name, new.uploader, new.note, new.workflow_id);
END;
";

/// Reads `PRAGMA user_version`.
pub(super) fn user_version(conn: &Connection) -> Result<u32, HistoryError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(u32::try_from(v).unwrap_or(0))
}

/// Brings the database to [`SCHEMA_VERSION`].
pub(super) fn migrate(conn: &mut Connection) -> Result<(), HistoryError> {
    let found = user_version(conn)?;
    if found > SCHEMA_VERSION {
        return Err(HistoryError::TooNew { found, supported: SCHEMA_VERSION });
    }
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = user_version(&tx)?;
    if current > SCHEMA_VERSION {
        return Err(HistoryError::TooNew { found: current, supported: SCHEMA_VERSION });
    }
    for step in current..SCHEMA_VERSION {
        let sql = MIGRATIONS.get(step as usize).ok_or_else(|| {
            HistoryError::Invalid(format!("no history migration from schema version {step}"))
        })?;
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Creates the FTS5 index (and rebuilds it from the table) if it does not exist yet.
/// Returns whether full-text search is available.
pub(super) fn ensure_fts(conn: &mut Connection) -> bool {
    let exists = |c: &Connection| {
        c.query_row("SELECT 1 FROM sqlite_master WHERE name = 'entries_fts'", [], |_| Ok(()))
            .is_ok()
    };
    if exists(conn) {
        return true;
    }
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(FTS)?;
        tx.execute("INSERT INTO entries_fts (entries_fts) VALUES ('rebuild')", [])?;
        tx.commit()
    })();
    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "FTS5 unavailable; history search falls back to LIKE");
            false
        }
    }
}
