//! History database tests: in-memory and on-disk, migrations, concurrency, damage.

use std::{sync::Arc, thread, time::Duration};

use rusqlite::Connection;

use super::*;

const T0: i64 = 1_700_000_000_000; // 2023-11-14, in ms

fn entry(kind: EntryKind, at: i64) -> NewEntry {
    NewEntry { created_at: at, ..NewEntry::new(kind) }
}

fn image(at: i64, path: &str) -> NewEntry {
    NewEntry { local_path: Some(PathBuf::from(path)), ..entry(EntryKind::Image, at) }
}

fn mem() -> History {
    History::open_in_memory().unwrap()
}

fn ids(v: &[Entry]) -> Vec<i64> {
    v.iter().map(|e| e.id).collect()
}

#[test]
fn history_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<History>();
}

#[test]
fn insert_get_round_trip_all_fields() {
    let h = mem();
    let full = NewEntry {
        created_at: T0,
        kind: EntryKind::Image,
        local_path: Some(PathBuf::from("/home/u/Pictures/ssx/shot.png")),
        thumbnail: Some(vec![1, 2, 3, 4]),
        upload_url: Some("https://i.example/abc.png".into()),
        thumbnail_url: Some("https://i.example/abc_t.png".into()),
        deletion_url: Some("https://i.example/del/abc".into()),
        uploader: Some("imgur".into()),
        window_title: Some("Terminal — ~/src".into()),
        process_name: Some("alacritty".into()),
        width: Some(1920),
        height: Some(1080),
        size_bytes: Some(123_456),
        sha256: Some(sha256_hex(b"x")),
        workflow_id: Some("capture-region".into()),
        note: Some("hello".into()),
    };
    let id = h.insert(&full).unwrap();
    let got = h.get(id).unwrap().unwrap();
    assert_eq!(got.id, id);
    assert_eq!(got.created_at, T0);
    assert_eq!(got.kind, EntryKind::Image);
    assert_eq!(got.local_path, full.local_path);
    assert_eq!(got.thumbnail, full.thumbnail);
    assert_eq!(got.upload_url, full.upload_url);
    assert_eq!(got.thumbnail_url, full.thumbnail_url);
    assert_eq!(got.deletion_url, full.deletion_url);
    assert_eq!(got.uploader, full.uploader);
    assert_eq!(got.window_title, full.window_title);
    assert_eq!(got.process_name, full.process_name);
    assert_eq!((got.width, got.height, got.size_bytes), (Some(1920), Some(1080), Some(123_456)));
    assert_eq!(got.sha256, full.sha256);
    assert_eq!(got.workflow_id, full.workflow_id);
    assert_eq!(got.note, full.note);
}

#[test]
fn minimal_entry_and_missing_id() {
    let h = mem();
    let id = h.insert(&entry(EntryKind::Url, T0)).unwrap();
    let e = h.get(id).unwrap().unwrap();
    assert!(e.local_path.is_none() && e.thumbnail.is_none() && e.upload_url.is_none());
    assert!(h.get(id + 1000).unwrap().is_none());
}

#[test]
fn ids_are_never_reused() {
    let h = mem();
    let a = h.insert(&entry(EntryKind::File, T0)).unwrap();
    assert!(h.delete(a).unwrap());
    let b = h.insert(&entry(EntryKind::File, T0)).unwrap();
    assert!(b > a);
}

#[test]
fn invalid_inserts_are_rejected() {
    let h = mem();
    let big = NewEntry { thumbnail: Some(vec![0; 2 * 1024 * 1024]), ..entry(EntryKind::Image, T0) };
    assert!(matches!(h.insert(&big), Err(HistoryError::Invalid(m)) if m.contains("thumbnail")));
    let bad = NewEntry { sha256: Some("xyz".into()), ..entry(EntryKind::Image, T0) };
    assert!(matches!(h.insert(&bad), Err(HistoryError::Invalid(m)) if m.contains("sha256")));
    let upper =
        NewEntry { sha256: Some(sha256_hex(b"a").to_uppercase()), ..entry(EntryKind::Image, T0) };
    let id = h.insert(&upper).unwrap();
    assert_eq!(
        h.get(id).unwrap().unwrap().sha256,
        Some(sha256_hex(b"a")),
        "normalised to lower case"
    );
}

#[test]
fn long_text_fields_are_truncated_on_char_boundaries() {
    let h = mem();
    let e = NewEntry { note: Some("é".repeat(5000)), ..entry(EntryKind::Text, T0) };
    let id = h.insert(&e).unwrap();
    let note = h.get(id).unwrap().unwrap().note.unwrap();
    assert!(note.len() <= 4096 && note.chars().all(|c| c == 'é'));
}

#[test]
fn list_is_newest_first_with_stable_tiebreak() {
    let h = mem();
    let a = h.insert(&entry(EntryKind::File, T0)).unwrap();
    let b = h.insert(&entry(EntryKind::File, T0 + 10)).unwrap();
    let c = h.insert(&entry(EntryKind::File, T0 + 10)).unwrap();
    let all = h.list(&Query::default()).unwrap();
    assert_eq!(ids(&all), vec![c, b, a]);
}

#[test]
fn paging() {
    let h = mem();
    let all: Vec<i64> =
        (0..25).map(|i| h.insert(&entry(EntryKind::File, T0 + i)).unwrap()).rev().collect();
    let page = |offset, limit| ids(&h.list(&Query { limit, offset, ..Query::default() }).unwrap());
    assert_eq!(page(0, 10), all[0..10]);
    assert_eq!(page(10, 10), all[10..20]);
    assert_eq!(page(20, 10), all[20..25]);
    assert!(page(25, 10).is_empty());
    assert_eq!(page(0, 0).len(), 1, "limit is clamped to at least 1");
    assert_eq!(h.count(&Query::default()).unwrap(), 25);
    assert_eq!(
        h.count(&Query { limit: 1, offset: 5, ..Query::default() }).unwrap(),
        25,
        "count ignores paging"
    );
}

#[test]
fn kind_date_and_upload_filters() {
    let h = mem();
    let img = h.insert(&entry(EntryKind::Image, T0)).unwrap();
    let vid = h.insert(&entry(EntryKind::Video, T0 + 1000)).unwrap();
    let url = h
        .insert(&NewEntry {
            upload_url: Some("https://x".into()),
            ..entry(EntryKind::Url, T0 + 2000)
        })
        .unwrap();
    let q = |f: &dyn Fn(&mut Query)| {
        let mut q = Query::default();
        f(&mut q);
        ids(&h.list(&q).unwrap())
    };
    assert_eq!(q(&|q| q.kinds = vec![EntryKind::Image]), vec![img]);
    assert_eq!(q(&|q| q.kinds = vec![EntryKind::Image, EntryKind::Video]), vec![vid, img]);
    assert_eq!(q(&|q| q.since = Some(T0 + 1000)), vec![url, vid], "since is inclusive");
    assert_eq!(q(&|q| q.until = Some(T0 + 1000)), vec![img], "until is exclusive");
    assert_eq!(
        q(&|q| {
            q.since = Some(T0 + 1);
            q.until = Some(T0 + 2000);
        }),
        vec![vid]
    );
    assert_eq!(q(&|q| q.uploaded_only = true), vec![url]);
    assert_eq!(
        q(&|q| {
            q.uploaded_only = true;
            q.kinds = vec![EntryKind::Image];
        }),
        Vec::<i64>::new()
    );
    assert_eq!(h.count(&Query { kinds: vec![EntryKind::Video], ..Query::default() }).unwrap(), 1);
}

#[test]
fn uploader_filter_matches_the_exact_name_only() {
    let h = History::open_in_memory().unwrap();
    let mk = |name: &str, at| NewEntry {
        upload_url: Some(format!("https://x/{name}")),
        uploader: Some(name.to_owned()),
        ..entry(EntryKind::Image, at)
    };
    let imgur = h.insert(&mk("imgur", T0)).unwrap();
    let s3 = h.insert(&mk("s3", T0 + 1)).unwrap();
    let local = h.insert(&entry(EntryKind::Image, T0 + 2)).unwrap();
    let by = |name: Option<&str>| {
        ids(&h.list(&Query { uploader: name.map(str::to_owned), ..Query::default() }).unwrap())
    };
    assert_eq!(by(Some("imgur")), vec![imgur]);
    assert_eq!(by(Some("s3")), vec![s3]);
    assert_eq!(by(Some("im")), Vec::<i64>::new(), "exact match, not a prefix");
    assert_eq!(by(Some("")), vec![local, s3, imgur], "an empty name means no filter");
    assert_eq!(by(None), vec![local, s3, imgur]);
}

#[test]
fn thumbnails_are_optional_in_listings() {
    let h = mem();
    let id =
        h.insert(&NewEntry { thumbnail: Some(vec![9, 9]), ..entry(EntryKind::Image, T0) }).unwrap();
    let with = h.list(&Query::default()).unwrap();
    assert_eq!(with[0].thumbnail.as_deref(), Some(&[9u8, 9][..]));
    let without = h.list(&Query { thumbnails: false, ..Query::default() }).unwrap();
    assert!(without[0].thumbnail.is_none());
    assert_eq!(h.get(id).unwrap().unwrap().thumbnail.as_deref(), Some(&[9u8, 9][..]));
}

fn searchable(h: &History) -> (i64, i64, i64) {
    let a = h
        .insert(&NewEntry {
            local_path: Some("/home/u/Pictures/ssx/Screenshot_2024-03-09.png".into()),
            window_title: Some("Terminal — Café".into()),
            process_name: Some("alacritty".into()),
            ..entry(EntryKind::Image, T0)
        })
        .unwrap();
    let b = h
        .insert(&NewEntry {
            local_path: Some("/home/u/Videos/demo.mp4".into()),
            upload_url: Some("https://files.example.com/x/demo.mp4".into()),
            uploader: Some("my-s3".into()),
            window_title: Some("Firefox".into()),
            ..entry(EntryKind::Video, T0 + 1)
        })
        .unwrap();
    let c = h
        .insert(&NewEntry {
            note: Some("hello world, 100% done_ok".into()),
            ..entry(EntryKind::Text, T0 + 2)
        })
        .unwrap();
    (a, b, c)
}

fn search(h: &History, text: &str) -> Vec<i64> {
    ids(&h.list(&Query { text: Some(text.into()), ..Query::default() }).unwrap())
}

#[test]
fn fts_is_available_with_the_bundled_sqlite() {
    assert!(mem().uses_fts(), "bundled SQLite must have FTS5");
}

#[test]
fn fts_search_semantics() {
    let h = mem();
    let (a, b, c) = searchable(&h);
    assert_eq!(search(&h, "terminal"), vec![a]);
    assert_eq!(search(&h, "TERM"), vec![a], "prefix, case-insensitive");
    assert_eq!(search(&h, "cafe"), vec![a], "diacritics folded");
    assert_eq!(search(&h, "café"), vec![a]);
    assert_eq!(search(&h, "demo"), vec![b]);
    assert_eq!(search(&h, "my-s3"), vec![b], "punctuation inside a term becomes a phrase");
    assert_eq!(search(&h, "files.example.com"), vec![b]);
    assert_eq!(search(&h, "hello world"), vec![c], "all terms must match");
    assert_eq!(search(&h, "hello firefox"), Vec::<i64>::new());
    assert_eq!(search(&h, "2024-03-09"), vec![a]);
    assert_eq!(search(&h, ""), vec![c, b, a], "blank means no filter");
    assert_eq!(search(&h, "   "), vec![c, b, a]);
    assert_eq!(search(&h, "nomatchatall"), Vec::<i64>::new());
}

#[test]
fn search_input_cannot_break_the_query() {
    let h = mem();
    let _ = searchable(&h);
    for nasty in [
        "\"",
        "\"\"",
        "\" OR 1=1 --",
        "*",
        "a*b",
        "(",
        ")",
        "NEAR(",
        "a AND",
        "'; DROP TABLE entries;--",
        "col:x",
        "-",
        "^",
        "%",
        "_",
        "\\",
        "🎉",
        "a\u{0}b",
    ] {
        let r = h.list(&Query { text: Some(nasty.into()), ..Query::default() });
        assert!(r.is_ok(), "{nasty:?}: {r:?}");
        assert!(h.count(&Query { text: Some(nasty.into()), ..Query::default() }).is_ok());
    }
    assert_eq!(h.count(&Query::default()).unwrap(), 3, "table intact");
}

#[test]
fn like_fallback_is_substring_and_escapes_wildcards() {
    let h =
        History::open_in_memory_with(&HistoryConfig { fts: FtsMode::Off, ..Default::default() })
            .unwrap();
    assert!(!h.uses_fts());
    let (a, b, c) = searchable(&h);
    assert_eq!(search(&h, "hot"), vec![a], "substring inside 'Screenshot'");
    assert_eq!(search(&h, "TERMINAL"), vec![a], "case-insensitive");
    assert_eq!(search(&h, "demo mp4"), vec![b]);
    assert_eq!(search(&h, "100%"), vec![c], "percent is literal");
    assert_eq!(search(&h, "done_ok"), vec![c]);
    assert_eq!(search(&h, "done_o_"), Vec::<i64>::new(), "underscore is not a wildcard");
    assert_eq!(search(&h, "%"), vec![c], "a bare percent matches only literal percents");
    assert_eq!(search(&h, "100\\"), Vec::<i64>::new(), "backslash is literal");
}

#[test]
fn punctuation_only_terms_use_like_even_with_fts() {
    let h = mem();
    let (_, _, c) = searchable(&h);
    assert_eq!(search(&h, "100%"), vec![c]);
    assert_eq!(search(&h, "%"), vec![c]);
}

#[test]
fn fts_index_tracks_updates_and_deletes() {
    let h = mem();
    let (a, b, _) = searchable(&h);
    assert!(search(&h, "imgur").is_empty());
    assert!(
        h.update_upload(
            a,
            &UploadInfo {
                url: Some("https://imgur.example/z".into()),
                uploader: Some("imgur".into()),
                ..Default::default()
            }
        )
        .unwrap()
    );
    assert_eq!(search(&h, "imgur"), vec![a]);
    // replacing the upload removes the old terms from the index
    assert!(
        h.update_upload(a, &UploadInfo { uploader: Some("other".into()), ..Default::default() })
            .unwrap()
    );
    assert!(search(&h, "imgur").is_empty());
    assert_eq!(search(&h, "other"), vec![a]);
    assert!(h.delete(b).unwrap());
    assert!(search(&h, "demo").is_empty());
    assert_eq!(h.count(&Query::default()).unwrap(), 2);
}

#[test]
fn update_upload_semantics() {
    let h = mem();
    let id = h.insert(&entry(EntryKind::Image, T0)).unwrap();
    let info = UploadInfo {
        url: Some("https://u".into()),
        thumbnail_url: Some("https://t".into()),
        deletion_url: Some("https://d".into()),
        uploader: Some("imgur".into()),
    };
    assert!(h.update_upload(id, &info).unwrap());
    let e = h.get(id).unwrap().unwrap();
    assert_eq!(e.upload_url.as_deref(), Some("https://u"));
    assert_eq!(e.deletion_url.as_deref(), Some("https://d"));
    assert!(h.update_upload(id, &UploadInfo::default()).unwrap());
    let e = h.get(id).unwrap().unwrap();
    assert!(e.upload_url.is_none() && e.uploader.is_none(), "None clears");
    assert!(!h.update_upload(id + 99, &info).unwrap(), "missing entry reports false");
}

#[test]
fn delete_and_delete_many() {
    let h = mem();
    let ids_: Vec<i64> =
        (0..5).map(|i| h.insert(&entry(EntryKind::File, T0 + i)).unwrap()).collect();
    assert!(h.delete(ids_[0]).unwrap());
    assert!(!h.delete(ids_[0]).unwrap());
    assert_eq!(h.delete_many(&[ids_[1], ids_[2], 9999]).unwrap(), 2);
    assert_eq!(h.count(&Query::default()).unwrap(), 2);
    assert_eq!(h.delete_many(&[]).unwrap(), 0);
}

#[test]
fn prune_by_count_keeps_newest() {
    let h = mem();
    let all: Vec<i64> =
        (0..10).map(|i| h.insert(&entry(EntryKind::File, T0 + i)).unwrap()).collect();
    let removed = h.prune(&PrunePolicy { max_entries: Some(3), max_age: None }, T0 + 100).unwrap();
    assert_eq!(removed, 7);
    let left = ids(&h.list(&Query::default()).unwrap());
    assert_eq!(left, vec![all[9], all[8], all[7]]);
    assert_eq!(h.prune(&PrunePolicy { max_entries: Some(3), max_age: None }, T0).unwrap(), 0);
    assert_eq!(h.prune(&PrunePolicy { max_entries: Some(0), max_age: None }, T0).unwrap(), 3);
}

#[test]
fn prune_by_age() {
    let h = mem();
    let day = 86_400_000;
    for d in 0..10 {
        h.insert(&entry(EntryKind::File, T0 - d * day)).unwrap();
    }
    let policy = PrunePolicy { max_entries: None, max_age: Some(Duration::from_secs(3 * 86_400)) };
    // entries strictly older than now-3d go; the one exactly 3 days old stays
    assert_eq!(h.prune(&policy, T0).unwrap(), 6);
    assert_eq!(h.count(&Query::default()).unwrap(), 4);
}

#[test]
fn prune_combined_and_noop() {
    let h = mem();
    for i in 0..5 {
        h.insert(&entry(EntryKind::File, T0 + i * 1000)).unwrap();
    }
    assert_eq!(h.prune(&PrunePolicy::default(), T0 + 10_000).unwrap(), 0);
    let p = PrunePolicy { max_entries: Some(4), max_age: Some(Duration::from_millis(2500)) };
    // now = T0+4000 -> cutoff T0+1500: removes T0 and T0+1000 (2), leaving 3 <= 4
    assert_eq!(h.prune(&p, T0 + 4000).unwrap(), 2);
    assert_eq!(h.count(&Query::default()).unwrap(), 3);
    // prune with a huge age must not overflow
    let huge = PrunePolicy { max_entries: None, max_age: Some(Duration::MAX) };
    assert_eq!(h.prune(&huge, T0).unwrap(), 0);
}

#[test]
fn prune_removes_fts_rows_too() {
    let h = mem();
    let (a, ..) = searchable(&h);
    h.prune(&PrunePolicy { max_entries: Some(1), max_age: None }, T0).unwrap();
    assert!(search(&h, "terminal").is_empty(), "entry {a} pruned from index");
}

#[test]
fn orphan_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let present = tmp.path().join("present.png");
    std::fs::write(&present, b"x").unwrap();
    let missing = tmp.path().join("missing.png");
    let h = mem();
    let a = h.insert(&image(T0, present.to_str().unwrap())).unwrap();
    let b = h.insert(&image(T0 + 1, missing.to_str().unwrap())).unwrap();
    let _url_only = h.insert(&entry(EntryKind::Url, T0 + 2)).unwrap();
    let orphans = h.find_orphans().unwrap();
    assert_eq!(orphans, vec![Orphan { id: b, path: missing.clone() }]);
    // injected existence check
    let all_gone = h.find_orphans_with(&|_| false).unwrap();
    assert_eq!(all_gone.len(), 2, "entries without a path are never orphans");
    assert_eq!(h.remove_orphans().unwrap(), 1);
    assert!(h.get(a).unwrap().is_some());
    assert!(h.get(b).unwrap().is_none());
    std::fs::remove_file(&present).unwrap();
    assert_eq!(h.find_orphans().unwrap().len(), 1);
}

#[test]
fn orphan_check_does_not_hold_the_database_lock() {
    let h = Arc::new(mem());
    h.insert(&image(T0, "/nonexistent/a.png")).unwrap();
    let h2 = Arc::clone(&h);
    // The callback itself uses the database: would deadlock if the lock were held.
    let orphans = h
        .find_orphans_with(&move |_| {
            let _ = h2.count(&Query::default()).unwrap();
            false
        })
        .unwrap();
    assert_eq!(orphans.len(), 1);
}

// ---- on-disk behaviour -------------------------------------------------------------

#[test]
fn file_database_persists_and_uses_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested/dir/history.sqlite3");
    let id = {
        let h = History::open(&path).unwrap();
        assert_eq!(h.path(), Some(path.as_path()));
        assert_eq!(h.schema_version().unwrap(), SCHEMA_VERSION);
        h.insert(&image(T0, "/a.png")).unwrap()
    };
    let h = History::open(&path).unwrap();
    assert!(h.get(id).unwrap().is_some());
    let raw = Connection::open(&path).unwrap();
    let mode: String = raw.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    assert!(h.integrity_check().unwrap().is_empty());
}

#[test]
fn vacuum_shrinks_and_keeps_data() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    let h = History::open(&path).unwrap();
    for i in 0..200 {
        h.insert(&NewEntry { thumbnail: Some(vec![7; 20_000]), ..entry(EntryKind::Image, T0 + i) })
            .unwrap();
    }
    h.prune(&PrunePolicy { max_entries: Some(5), max_age: None }, T0).unwrap();
    h.vacuum().unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size < 500_000, "database still {size} bytes after vacuum");
    assert_eq!(h.count(&Query::default()).unwrap(), 5);
    mem().vacuum().unwrap();
}

// ---- migrations --------------------------------------------------------------------

/// The v1 schema exactly as shipped in the first release (an independent fixture: do not
/// derive it from the migration code).
const V1_FIXTURE: &str = "
CREATE TABLE entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT, created_at INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('image','video','file','text','url')),
    local_path TEXT, thumbnail BLOB, upload_url TEXT, thumbnail_url TEXT, deletion_url TEXT,
    uploader TEXT, window_title TEXT, process_name TEXT, width INTEGER, height INTEGER,
    size_bytes INTEGER, sha256 TEXT);
CREATE INDEX entries_created_at ON entries (created_at);
CREATE INDEX entries_kind_created ON entries (kind, created_at);
INSERT INTO entries (created_at, kind, local_path, upload_url, uploader, window_title, width, height, size_bytes)
  VALUES (1700000000000, 'image', '/old/Screenshot_1.png', 'https://old.example/1', 'imgur', 'Legacy Window', 800, 600, 4242);
INSERT INTO entries (created_at, kind, local_path) VALUES (1700000001000, 'video', '/old/rec.mp4');
PRAGMA user_version = 1;
";

#[test]
fn migrates_a_v1_database_preserving_data() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    Connection::open(&path).unwrap().execute_batch(V1_FIXTURE).unwrap();

    let h = History::open(&path).unwrap();
    assert_eq!(h.schema_version().unwrap(), 2);
    let all = h.list(&Query::default()).unwrap();
    assert_eq!(all.len(), 2);
    let img = all.iter().find(|e| e.kind == EntryKind::Image).unwrap();
    assert_eq!(img.local_path.as_deref(), Some(Path::new("/old/Screenshot_1.png")));
    assert_eq!(img.uploader.as_deref(), Some("imgur"));
    assert_eq!((img.width, img.height, img.size_bytes), (Some(800), Some(600), Some(4242)));
    assert!(img.workflow_id.is_none() && img.note.is_none(), "new columns are NULL");
    // the FTS index was built from the pre-existing rows
    assert!(h.uses_fts());
    assert_eq!(search(&h, "legacy"), vec![img.id]);
    // and the new columns work
    let id = h
        .insert(&NewEntry {
            workflow_id: Some("wf".into()),
            note: Some("n".into()),
            ..entry(EntryKind::Text, T0)
        })
        .unwrap();
    assert_eq!(h.get(id).unwrap().unwrap().workflow_id.as_deref(), Some("wf"));
    // reopening is a no-op
    drop(h);
    let h = History::open(&path).unwrap();
    assert_eq!(h.count(&Query::default()).unwrap(), 3);
}

#[test]
fn concurrent_open_of_an_old_database_migrates_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = Arc::new(tmp.path().join("h.sqlite3"));
    Connection::open(&*path).unwrap().execute_batch(V1_FIXTURE).unwrap();
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let p = Arc::clone(&path);
            thread::spawn(move || {
                let h = History::open(&p).expect("concurrent open");
                h.count(&Query::default()).unwrap()
            })
        })
        .collect();
    for hnd in handles {
        assert_eq!(hnd.join().unwrap(), 2);
    }
}

#[test]
fn fresh_and_migrated_databases_have_the_same_columns() {
    let cols = |c: &Connection| -> Vec<String> {
        let mut s =
            c.prepare("SELECT name FROM pragma_table_info('entries') ORDER BY cid").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("old.sqlite3");
    Connection::open(&old).unwrap().execute_batch(V1_FIXTURE).unwrap();
    drop(History::open(&old).unwrap());
    let fresh = tmp.path().join("fresh.sqlite3");
    drop(History::open(&fresh).unwrap());
    assert_eq!(cols(&Connection::open(&old).unwrap()), cols(&Connection::open(&fresh).unwrap()));
}

#[test]
fn newer_schema_is_refused_and_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE future (x); PRAGMA user_version = 99;").unwrap();
    }
    let e = History::open(&path).unwrap_err();
    assert!(matches!(e, HistoryError::TooNew { found: 99, supported: SCHEMA_VERSION }), "{e}");
    assert!(e.to_string().contains("upgrade ssx"));
    let e = History::open_or_recover(&path, &HistoryConfig::default()).unwrap_err();
    assert!(matches!(e, HistoryError::TooNew { .. }), "never recover over a newer database");
    let c = Connection::open(&path).unwrap();
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 99);
    assert!(c.prepare("SELECT x FROM future").is_ok());
}

// ---- concurrency -------------------------------------------------------------------

#[test]
fn many_threads_share_one_handle() {
    let h = Arc::new(mem());
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let h = Arc::clone(&h);
            thread::spawn(move || {
                (0..50)
                    .map(|i| h.insert(&entry(EntryKind::File, T0 + t * 1000 + i)).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut all: Vec<i64> = handles.into_iter().flat_map(|t| t.join().unwrap()).collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 400, "unique ids");
    assert_eq!(h.count(&Query::default()).unwrap(), 400);
}

#[test]
fn concurrent_writers_with_separate_handles_on_one_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = Arc::new(tmp.path().join("h.sqlite3"));
    drop(History::open(&path).unwrap());
    let handles: Vec<_> = (0..6)
        .map(|t| {
            let p = Arc::clone(&path);
            thread::spawn(move || {
                let h = History::open(&p).unwrap(); // its own connection, like another process
                for i in 0..40 {
                    h.insert(&NewEntry {
                        note: Some(format!("writer{t}-{i}")),
                        ..entry(EntryKind::Text, T0 + i)
                    })
                    .expect("insert must wait out contention, not fail");
                    if i % 10 == 0 {
                        h.list(&Query { limit: 5, ..Query::default() }).unwrap();
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let h = History::open(&path).unwrap();
    assert_eq!(h.count(&Query::default()).unwrap(), 240);
    assert_eq!(search(&h, "writer3").len(), 40);
    assert!(h.integrity_check().unwrap().is_empty());
}

#[test]
fn readers_do_not_block_on_a_writer_in_wal_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    let h = History::open(&path).unwrap();
    h.insert(&entry(EntryKind::File, T0)).unwrap();
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    raw.execute("INSERT INTO entries (created_at, kind) VALUES (1, 'file')", []).unwrap();
    let cfg = HistoryConfig { busy_timeout: Duration::from_millis(50), ..Default::default() };
    let reader = History::open_with(&path, &cfg).unwrap();
    assert_eq!(reader.count(&Query::default()).unwrap(), 1, "readers see the last committed state");
    raw.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn busy_database_reports_busy_instead_of_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    let cfg = HistoryConfig { busy_timeout: Duration::from_millis(60), ..Default::default() };
    let h = History::open_with(&path, &cfg).unwrap();
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap(); // holds the write lock
    let started = std::time::Instant::now();
    let e = h.insert(&entry(EntryKind::File, T0)).unwrap_err();
    assert!(matches!(e, HistoryError::Busy), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "must not block long");
    assert!(e.to_string().contains("try again"));
    raw.execute_batch("ROLLBACK").unwrap();
    h.insert(&entry(EntryKind::File, T0)).expect("works again once the lock is released");
}

#[test]
fn busy_wait_succeeds_when_lock_is_released_in_time() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    let cfg = HistoryConfig { busy_timeout: Duration::from_secs(5), ..Default::default() };
    let h = History::open_with(&path, &cfg).unwrap();
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        raw.execute_batch("COMMIT").unwrap();
    });
    h.insert(&entry(EntryKind::File, T0)).expect("waits for the lock");
    releaser.join().unwrap();
}

// ---- damage ------------------------------------------------------------------------

#[test]
fn garbage_file_is_reported_as_corrupt_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    let junk = b"this is definitely not a sqlite database, just some text padding it out to more than a hundred bytes long.....";
    std::fs::write(&path, junk).unwrap();
    let e = History::open(&path).unwrap_err();
    match &e {
        HistoryError::Corrupt { path: p, .. } => assert_eq!(p.as_deref(), Some(path.as_path())),
        other => panic!("expected Corrupt, got {other:?}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), junk, "plain open never modifies a damaged file");
}

#[test]
fn open_or_recover_moves_damaged_file_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    std::fs::write(&path, vec![0xAB; 4096]).unwrap();
    let (h, aside) = History::open_or_recover(&path, &HistoryConfig::default()).unwrap();
    let aside = aside.expect("reports the moved file");
    assert!(aside.file_name().unwrap().to_string_lossy().starts_with("h.sqlite3.corrupt-"));
    assert_eq!(std::fs::read(&aside).unwrap(), vec![0xAB; 4096]);
    h.insert(&entry(EntryKind::File, T0)).unwrap();
    assert_eq!(h.count(&Query::default()).unwrap(), 1);
}

#[test]
fn open_or_recover_leaves_healthy_databases_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    History::open(&path).unwrap().insert(&entry(EntryKind::File, T0)).unwrap();
    let (h, aside) = History::open_or_recover(&path, &HistoryConfig::default()).unwrap();
    assert!(aside.is_none());
    assert_eq!(h.count(&Query::default()).unwrap(), 1);
}

#[test]
fn damaged_database_body_is_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("h.sqlite3");
    {
        let h = History::open(&path).unwrap();
        for i in 0..300 {
            h.insert(&NewEntry {
                thumbnail: Some(vec![i as u8; 2000]),
                ..entry(EntryKind::Image, T0 + i)
            })
            .unwrap();
        }
        h.vacuum().unwrap();
    }
    // Zero out a chunk in the middle of the file, keeping the header valid.
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 100_000);
    let mid = bytes.len() / 2;
    for b in &mut bytes[mid..mid + 30_000] {
        *b = 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();
    let (h, aside) = History::open_or_recover(&path, &HistoryConfig::default()).unwrap();
    assert!(aside.is_some(), "integrity check must catch the damage");
    assert_eq!(h.count(&Query::default()).unwrap(), 0, "fresh database");
}

#[test]
fn opening_a_directory_is_an_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(History::open(tmp.path()).is_err());
}

#[test]
fn error_display_is_actionable() {
    assert!(HistoryError::Busy.to_string().contains("try again"));
    let c = HistoryError::Corrupt { path: None, message: "malformed".into() };
    assert!(c.to_string().contains("malformed"));
}

fn busy_error() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("database is locked".into()),
    )
}

#[test]
fn retry_busy_retries_until_the_operation_succeeds() {
    let mut calls = 0;
    let got = super::retry_busy(Duration::from_secs(5), || {
        calls += 1;
        if calls < 4 { Err(busy_error()) } else { Ok(calls) }
    });
    assert_eq!(got.unwrap(), 4);
}

#[test]
fn retry_busy_gives_up_after_the_timeout_and_returns_the_busy_error() {
    let start = std::time::Instant::now();
    let got: rusqlite::Result<()> =
        super::retry_busy(Duration::from_millis(60), || Err(busy_error()));
    assert!(matches!(
        got,
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy
    ));
    assert!(start.elapsed() >= Duration::from_millis(60), "must keep trying until the deadline");
    assert!(start.elapsed() < Duration::from_secs(2), "and must not overshoot wildly");
}

#[test]
fn retry_busy_does_not_retry_other_errors() {
    let mut calls = 0;
    let got: rusqlite::Result<()> = super::retry_busy(Duration::from_secs(5), || {
        calls += 1;
        Err(rusqlite::Error::QueryReturnedNoRows)
    });
    assert!(matches!(got, Err(rusqlite::Error::QueryReturnedNoRows)));
    assert_eq!(calls, 1);
}
