//! Capture/upload history in SQLite.
//!
//! * **Bundled SQLite, WAL mode.** Readers (the history window) never block the writer (a
//!   workflow finishing), and several processes (tray app, CLI invocations from the shell
//!   menu) can share the file. `busy_timeout` absorbs short contention; if it expires the
//!   caller gets [`HistoryError::Busy`] (retry later) instead of a hang.
//! * **Thread safe handle.** [`History`] wraps the connection in a `Mutex`; it is `Send +
//!   Sync` and cheap to share behind an `Arc`. Queries are short, so one connection is
//!   plenty; there is no pool to size.
//! * **Search.** With FTS5 (always the case with the bundled build) every whitespace-
//!   separated term must *prefix-match a word* of the file path, URL, window title, process,
//!   uploader, note or workflow id. Without FTS5, or when a term has no letters/digits, the
//!   fallback is a case-insensitive *substring* match on the same columns.
//! * **Corruption.** [`History::open`] reports a damaged file as
//!   [`HistoryError::Corrupt`] and never deletes it; [`History::open_or_recover`] moves it
//!   aside and starts a fresh database so the app keeps working.
//! * **Paths** are stored as UTF-8 text; a non-UTF-8 Linux path is stored lossily.

mod model;
mod schema;
mod thumbnail;

#[cfg(test)]
mod tests;

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, params, params_from_iter, types::Value};

pub use model::{
    Entry, EntryKind, NewEntry, Orphan, PrunePolicy, Query, UploadInfo, now_ms, sha256_file,
    sha256_hex, sha256_reader,
};
pub use schema::SCHEMA_VERSION;
pub use thumbnail::{
    ThumbnailError, ThumbnailOptions, thumbnail_from_bytes, thumbnail_from_frame,
    thumbnail_from_image,
};

use crate::pattern::truncate_bytes;

/// Everything that can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The database stayed locked for longer than the busy timeout. Safe to retry.
    #[error("the history database is locked by another ssx process; try again in a moment")]
    Busy,
    /// The file is not a usable database.
    #[error("the history database is damaged: {message}")]
    Corrupt {
        /// The file, when known.
        path: Option<PathBuf>,
        /// SQLite's description.
        message: String,
    },
    /// The database was written by a newer ssx.
    #[error(
        "the history database has schema version {found} but this ssx supports up to {supported}; upgrade ssx (the file was not modified)"
    )]
    TooNew {
        /// Version in the file.
        found: u32,
        /// Highest supported version.
        supported: u32,
    },
    /// Any other SQLite failure.
    #[error("history database error: {0}")]
    Sqlite(#[source] rusqlite::Error),
    /// File-system failure (creating directories, moving a corrupt file aside).
    #[error("history file error: {0}")]
    Io(#[from] std::io::Error),
    /// A value was rejected before reaching the database.
    #[error("invalid history entry: {0}")]
    Invalid(String),
    /// Thumbnail generation failed.
    #[error(transparent)]
    Thumbnail(#[from] ThumbnailError),
}

impl From<rusqlite::Error> for HistoryError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        if let rusqlite::Error::SqliteFailure(f, msg) = &e {
            match f.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => return Self::Busy,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                    return Self::Corrupt {
                        path: None,
                        message: msg.clone().unwrap_or_else(|| f.to_string()),
                    };
                }
                _ => {}
            }
        }
        Self::Sqlite(e)
    }
}

/// Whether to use the FTS5 index for text search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtsMode {
    /// Use FTS5 when the index exists or can be created (default).
    #[default]
    Auto,
    /// Never use it for searching (substring `LIKE`); mainly for tests and tiny setups.
    Off,
}

/// Open-time settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryConfig {
    /// How long a statement waits for a lock held by another connection.
    pub busy_timeout: Duration,
    /// Full-text search mode.
    pub fts: FtsMode,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { busy_timeout: Duration::from_secs(5), fts: FtsMode::Auto }
    }
}

const MAX_THUMBNAIL_BYTES: usize = 1024 * 1024;
const MAX_NOTE_BYTES: usize = 4096;
const MAX_FIELD_BYTES: usize = 4096;

const COLUMNS: &str = "id, created_at, kind, local_path, upload_url, thumbnail_url, deletion_url, \
                       uploader, window_title, process_name, width, height, size_bytes, sha256, \
                       workflow_id, note";

/// The history database handle.
#[derive(Debug)]
pub struct History {
    conn: Mutex<Connection>,
    /// Search with FTS5 (index exists and the config allows it).
    fts: bool,
    path: Option<PathBuf>,
}

fn opt_str(v: Option<&str>, max: usize) -> Option<String> {
    v.map(|s| truncate_bytes(s, max).to_owned())
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl History {
    /// Opens (creating and migrating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, HistoryError> {
        Self::open_with(path, &HistoryConfig::default())
    }

    /// [`open`](Self::open) with explicit settings.
    pub fn open_with(path: &Path, cfg: &HistoryConfig) -> Result<Self, HistoryError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| Self::with_path(e.into(), path))?;
        Self::init(conn, cfg, Some(path.to_path_buf()), true).map_err(|e| Self::with_path(e, path))
    }

    /// A private in-memory database (tests, `--no-history`).
    pub fn open_in_memory() -> Result<Self, HistoryError> {
        Self::open_in_memory_with(&HistoryConfig::default())
    }

    /// [`open_in_memory`](Self::open_in_memory) with explicit settings.
    pub fn open_in_memory_with(cfg: &HistoryConfig) -> Result<Self, HistoryError> {
        Self::init(Connection::open_in_memory()?, cfg, None, false)
    }

    /// Like [`open`](Self::open), but a *damaged* database is renamed to
    /// `<file>.corrupt-<unix time>` (with its `-wal`/`-shm` files) and a fresh one is
    /// created. Returns the new location of the damaged file, if any. Other errors —
    /// including [`HistoryError::TooNew`] and [`HistoryError::Busy`] — are returned as is.
    pub fn open_or_recover(
        path: &Path,
        cfg: &HistoryConfig,
    ) -> Result<(Self, Option<PathBuf>), HistoryError> {
        let damaged = |e: &HistoryError| matches!(e, HistoryError::Corrupt { .. });
        match Self::open_with(path, cfg) {
            Ok(h) => match h.integrity_check() {
                Ok(problems) if problems.is_empty() => Ok((h, None)),
                Ok(_) => {
                    drop(h);
                    Self::recover(path, cfg)
                }
                Err(e) if damaged(&e) => {
                    drop(h);
                    Self::recover(path, cfg)
                }
                Err(e) => Err(e),
            },
            Err(e) if damaged(&e) => Self::recover(path, cfg),
            Err(e) => Err(e),
        }
    }

    fn recover(path: &Path, cfg: &HistoryConfig) -> Result<(Self, Option<PathBuf>), HistoryError> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let aside = path.with_file_name(format!("{base}.corrupt-{stamp}"));
        std::fs::rename(path, &aside)?;
        for suffix in ["-wal", "-shm"] {
            let side = path.with_file_name(format!("{base}{suffix}"));
            if side.exists() {
                let _ = std::fs::rename(
                    &side,
                    aside.with_file_name(format!(
                        "{}{suffix}",
                        aside
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    )),
                );
            }
        }
        tracing::warn!(path = %path.display(), moved_to = %aside.display(), "history database was damaged; starting a new one");
        Ok((Self::open_with(path, cfg)?, Some(aside)))
    }

    fn with_path(e: HistoryError, path: &Path) -> HistoryError {
        match e {
            HistoryError::Corrupt { message, .. } => {
                HistoryError::Corrupt { path: Some(path.to_path_buf()), message }
            }
            other => other,
        }
    }

    fn init(
        mut conn: Connection,
        cfg: &HistoryConfig,
        path: Option<PathBuf>,
        wal: bool,
    ) -> Result<Self, HistoryError> {
        conn.busy_timeout(cfg.busy_timeout)?;
        if wal {
            // Returns the resulting mode as a row; "wal" on success. Network file systems
            // may refuse WAL and stay in "delete" mode, which still works, just slower.
            let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                tracing::warn!(mode, "history database could not enter WAL mode");
            }
            // NORMAL is durable enough under WAL (a power cut can lose the last
            // transactions, never corrupt the file).
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        let has_fts = schema::ensure_fts(&mut conn);
        Ok(Self { conn: Mutex::new(conn), fts: has_fts && cfg.fts == FtsMode::Auto, path })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave the connection unusable (rusqlite
        // transactions roll back on drop), so poisoning is safe to ignore.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The database file (`None` for in-memory).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// `true` if text search uses FTS5.
    pub fn uses_fts(&self) -> bool {
        self.fts
    }

    /// The schema version stored in the file.
    pub fn schema_version(&self) -> Result<u32, HistoryError> {
        schema::user_version(&self.lock())
    }

    /// Runs SQLite's integrity check; returns the problems found (empty = healthy).
    pub fn integrity_check(&self) -> Result<Vec<String>, HistoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let rows: Vec<String> =
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
        Ok(rows.into_iter().filter(|r| r != "ok").collect())
    }

    /// Inserts an entry and returns its id.
    pub fn insert(&self, e: &NewEntry) -> Result<i64, HistoryError> {
        if let Some(t) = &e.thumbnail
            && t.len() > MAX_THUMBNAIL_BYTES
        {
            return Err(HistoryError::Invalid(format!(
                "thumbnail is {} bytes; the limit is {MAX_THUMBNAIL_BYTES} (use history::thumbnail_from_frame)",
                t.len()
            )));
        }
        if let Some(h) = &e.sha256
            && (h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(HistoryError::Invalid(format!(
                "sha256 must be 64 hex characters, got {h:?}"
            )));
        }
        let conn = self.lock();
        conn.execute(
            "INSERT INTO entries (created_at, kind, local_path, thumbnail, upload_url, \
             thumbnail_url, deletion_url, uploader, window_title, process_name, width, height, \
             size_bytes, sha256, workflow_id, note) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                e.created_at,
                e.kind.as_str(),
                e.local_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                e.thumbnail,
                opt_str(e.upload_url.as_deref(), MAX_FIELD_BYTES),
                opt_str(e.thumbnail_url.as_deref(), MAX_FIELD_BYTES),
                opt_str(e.deletion_url.as_deref(), MAX_FIELD_BYTES),
                opt_str(e.uploader.as_deref(), 256),
                opt_str(e.window_title.as_deref(), 1024),
                opt_str(e.process_name.as_deref(), 256),
                e.width,
                e.height,
                e.size_bytes.map(to_i64),
                e.sha256.as_deref().map(str::to_ascii_lowercase),
                opt_str(e.workflow_id.as_deref(), 128),
                opt_str(e.note.as_deref(), MAX_NOTE_BYTES),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
        let kind: String = r.get("kind")?;
        let kind = EntryKind::parse(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("unknown entry kind {kind:?}").into(),
            )
        })?;
        let size: Option<i64> = r.get("size_bytes")?;
        Ok(Entry {
            id: r.get("id")?,
            created_at: r.get("created_at")?,
            kind,
            local_path: r.get::<_, Option<String>>("local_path")?.map(PathBuf::from),
            thumbnail: None,
            upload_url: r.get("upload_url")?,
            thumbnail_url: r.get("thumbnail_url")?,
            deletion_url: r.get("deletion_url")?,
            uploader: r.get("uploader")?,
            window_title: r.get("window_title")?,
            process_name: r.get("process_name")?,
            width: r.get("width")?,
            height: r.get("height")?,
            size_bytes: size.and_then(|s| u64::try_from(s).ok()),
            sha256: r.get("sha256")?,
            workflow_id: r.get("workflow_id")?,
            note: r.get("note")?,
        })
    }

    /// Fetches one entry (with its thumbnail).
    pub fn get(&self, id: i64) -> Result<Option<Entry>, HistoryError> {
        let conn = self.lock();
        let mut entry = match conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM entries WHERE id = ?1"),
                [id],
                Self::row_to_entry,
            )
            .map(Some)
        {
            Ok(e) => e,
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(e) = &mut entry {
            e.thumbnail =
                conn.query_row("SELECT thumbnail FROM entries WHERE id = ?1", [id], |r| r.get(0))?;
        }
        Ok(entry)
    }

    fn build_where(&self, q: &Query) -> (String, Vec<Value>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();

        // NUL would terminate the FTS query string early ("unterminated string").
        let text: String = q.text.as_deref().unwrap_or("").chars().filter(|c| *c != '\0').collect();
        let terms: Vec<&str> = text.split_whitespace().collect();
        if !terms.is_empty() {
            let fts_ok = self.fts && terms.iter().all(|t| t.chars().any(char::is_alphanumeric));
            if fts_ok {
                let m = terms
                    .iter()
                    .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" ");
                clauses
                    .push("id IN (SELECT rowid FROM entries_fts WHERE entries_fts MATCH ?)".into());
                args.push(Value::Text(m));
            } else {
                for t in terms {
                    let like = format!(
                        "%{}%",
                        t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
                    );
                    let cols = [
                        "local_path",
                        "upload_url",
                        "window_title",
                        "process_name",
                        "uploader",
                        "note",
                        "workflow_id",
                    ];
                    let any = cols
                        .iter()
                        .map(|c| format!("{c} LIKE ? ESCAPE '\\'"))
                        .collect::<Vec<_>>()
                        .join(" OR ");
                    clauses.push(format!("({any})"));
                    args.extend(std::iter::repeat_n(Value::Text(like), cols.len()));
                }
            }
        }
        if !q.kinds.is_empty() {
            let marks = vec!["?"; q.kinds.len()].join(",");
            clauses.push(format!("kind IN ({marks})"));
            args.extend(q.kinds.iter().map(|k| Value::Text(k.as_str().to_owned())));
        }
        if let Some(s) = q.since {
            clauses.push("created_at >= ?".into());
            args.push(Value::Integer(s));
        }
        if let Some(u) = q.until {
            clauses.push("created_at < ?".into());
            args.push(Value::Integer(u));
        }
        if q.uploaded_only {
            clauses.push("upload_url IS NOT NULL AND upload_url != ''".into());
        }
        if let Some(name) = q.uploader.as_deref().filter(|n| !n.is_empty()) {
            clauses.push("uploader = ?".into());
            args.push(Value::Text(name.to_owned()));
        }
        let sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        (sql, args)
    }

    /// Lists entries, newest first.
    pub fn list(&self, q: &Query) -> Result<Vec<Entry>, HistoryError> {
        let (where_sql, mut args) = self.build_where(q);
        let limit = q.limit.clamp(1, 1000);
        args.push(Value::Integer(i64::try_from(limit).unwrap_or(1000)));
        args.push(Value::Integer(i64::try_from(q.offset).unwrap_or(i64::MAX)));
        let thumb = if q.thumbnails { ", thumbnail" } else { "" };
        let sql = format!(
            "SELECT {COLUMNS}{thumb} FROM entries{where_sql} ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            let mut e = Self::row_to_entry(r)?;
            if q.thumbnails {
                e.thumbnail = r.get("thumbnail")?;
            }
            Ok(e)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Number of entries matching `q` (ignoring paging).
    pub fn count(&self, q: &Query) -> Result<u64, HistoryError> {
        let (where_sql, args) = self.build_where(q);
        let conn = self.lock();
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM entries{where_sql}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Replaces the upload information of an entry (a new upload supersedes the old one;
    /// `None` fields are cleared). Returns `false` if the entry does not exist.
    pub fn update_upload(&self, id: i64, info: &UploadInfo) -> Result<bool, HistoryError> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE entries SET upload_url = ?2, thumbnail_url = ?3, deletion_url = ?4, uploader = ?5 WHERE id = ?1",
            params![
                id,
                opt_str(info.url.as_deref(), MAX_FIELD_BYTES),
                opt_str(info.thumbnail_url.as_deref(), MAX_FIELD_BYTES),
                opt_str(info.deletion_url.as_deref(), MAX_FIELD_BYTES),
                opt_str(info.uploader.as_deref(), 256),
            ],
        )?;
        Ok(n > 0)
    }

    /// Deletes one entry (never touches the file on disk). Returns whether it existed.
    pub fn delete(&self, id: i64) -> Result<bool, HistoryError> {
        Ok(self.lock().execute("DELETE FROM entries WHERE id = ?1", [id])? > 0)
    }

    /// Deletes many entries in one transaction; returns how many existed.
    pub fn delete_many(&self, ids: &[i64]) -> Result<usize, HistoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM entries WHERE id = ?1")?;
            for id in ids {
                n += stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Applies `policy` relative to `now_ms` (Unix ms) and returns how many entries were
    /// removed. The age rule runs first, then the count rule keeps the newest entries.
    pub fn prune(&self, policy: &PrunePolicy, now_ms: i64) -> Result<usize, HistoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut removed = 0;
        if let Some(age) = policy.max_age {
            let cutoff = now_ms.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
            removed += tx.execute("DELETE FROM entries WHERE created_at < ?1", [cutoff])?;
        }
        if let Some(max) = policy.max_entries {
            removed += tx.execute(
                "DELETE FROM entries WHERE id NOT IN \
                 (SELECT id FROM entries ORDER BY created_at DESC, id DESC LIMIT ?1)",
                [i64::from(max)],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Reclaims free pages and truncates the WAL. Can be slow on big databases; run it from
    /// a background thread.
    pub fn vacuum(&self) -> Result<(), HistoryError> {
        let conn = self.lock();
        conn.execute_batch("VACUUM")?;
        if self.path.is_some() {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        }
        Ok(())
    }

    /// Entries whose file is gone, judged by `exists`. The disk is queried **without** the
    /// database lock held, so a slow network share does not stall other threads.
    pub fn find_orphans_with(
        &self,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Result<Vec<Orphan>, HistoryError> {
        let candidates: Vec<(i64, PathBuf)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT id, local_path FROM entries WHERE local_path IS NOT NULL ORDER BY id",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?))))?
                .collect::<Result<_, _>>()?
        };
        Ok(candidates
            .into_iter()
            .filter(|(_, p)| !exists(p))
            .map(|(id, path)| Orphan { id, path })
            .collect())
    }

    /// Entries whose `local_path` no longer exists on disk. Paths that cannot be checked
    /// (permission errors) are *not* reported.
    pub fn find_orphans(&self) -> Result<Vec<Orphan>, HistoryError> {
        self.find_orphans_with(&|p| p.try_exists().unwrap_or(true))
    }

    /// Deletes every orphan and returns how many were removed.
    pub fn remove_orphans(&self) -> Result<usize, HistoryError> {
        let ids: Vec<i64> = self.find_orphans()?.into_iter().map(|o| o.id).collect();
        self.delete_many(&ids)
    }
}
