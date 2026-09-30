//! `ssx history`: list, search, show, delete, prune and open entries.

use std::time::Duration;

use serde::Serialize;
use ssx_core::{
    history::{Entry, EntryKind, PrunePolicy, Query, now_ms},
    workflow::UrlOpener as _,
};

use crate::{
    app::App,
    cli::{HistoryCmd, HistoryFilter, HistoryKindArg},
    commands::list::truncate,
    error::{CliError, CliResult},
    output::{Style, Table, err_line, human_bytes, local_time, out_line, out_text, rfc3339},
};

/// An entry plus a readable timestamp for JSON output.
#[derive(Debug, Serialize)]
pub struct EntryJson<'a> {
    /// The entry itself (thumbnails are never included).
    #[serde(flatten)]
    pub entry: &'a Entry,
    /// `created_at` as RFC 3339 UTC.
    pub created: String,
}

/// The query for a listing.
pub fn query_for(filter: &HistoryFilter, text: Option<String>) -> Query {
    Query {
        text,
        kinds: filter.kind.map(kind_of).into_iter().collect(),
        uploaded_only: filter.uploaded,
        limit: filter.limit.clamp(1, 1000),
        thumbnails: false,
        ..Query::default()
    }
}

fn kind_of(k: HistoryKindArg) -> EntryKind {
    match k {
        HistoryKindArg::Image => EntryKind::Image,
        HistoryKindArg::Video => EntryKind::Video,
        HistoryKindArg::File => EntryKind::File,
        HistoryKindArg::Text => EntryKind::Text,
        HistoryKindArg::Url => EntryKind::Url,
    }
}

/// The listing table.
pub fn render_entries(entries: &[Entry], style: Style) -> String {
    let mut t = Table::new(["ID", "TIME", "KIND", "FILE", "URL"]);
    for e in entries {
        let file = e
            .local_path
            .as_ref()
            .map_or_else(String::new, |p| truncate(&p.display().to_string(), 48));
        t.row([
            e.id.to_string(),
            local_time(e.created_at),
            e.kind.as_str().to_owned(),
            file,
            e.upload_url.clone().unwrap_or_default(),
        ]);
    }
    t.render(style)
}

/// The detail view of one entry.
pub fn render_entry(e: &Entry) -> String {
    let mut lines = vec![
        format!("id:          {}", e.id),
        format!("created:     {} ({})", local_time(e.created_at), rfc3339(e.created_at)),
        format!("kind:        {}", e.kind.as_str()),
    ];
    let mut opt = |label: &str, v: Option<String>| {
        if let Some(v) = v {
            lines.push(format!("{label:<12} {v}"));
        }
    };
    opt("file:", e.local_path.as_ref().map(|p| p.display().to_string()));
    opt("url:", e.upload_url.clone());
    opt("thumbnail:", e.thumbnail_url.clone());
    opt("delete url:", e.deletion_url.clone());
    opt("uploader:", e.uploader.clone());
    opt("window:", e.window_title.clone());
    opt("process:", e.process_name.clone());
    opt("size:", match (e.width, e.height) {
        (Some(w), Some(h)) => Some(format!("{w}x{h} px")),
        _ => None,
    });
    opt("bytes:", e.size_bytes.map(human_bytes));
    opt("sha256:", e.sha256.clone());
    opt("workflow:", e.workflow_id.clone());
    opt("note:", e.note.clone());
    lines.join("\n") + "\n"
}

fn print_entries(app: &App, entries: &[Entry], json: bool) -> CliResult<()> {
    if json {
        let items: Vec<EntryJson<'_>> =
            entries.iter().map(|e| EntryJson { entry: e, created: rfc3339(e.created_at) }).collect();
        out_line(&serde_json::to_string_pretty(&items)?);
    } else if entries.is_empty() {
        err_line("nothing in the history");
    } else {
        out_text(&render_entries(entries, app.out));
    }
    Ok(())
}

/// Dispatches `ssx history ...`.
pub fn run(app: &App, cmd: HistoryCmd) -> CliResult<()> {
    let settings = app.load_settings()?;
    let history = app.open_history()?;
    match cmd {
        HistoryCmd::List(filter) => {
            let entries = history.list(&query_for(&filter, None))?;
            print_entries(app, &entries, filter.json)
        }
        HistoryCmd::Search { text, filter } => {
            let entries = history.list(&query_for(&filter, Some(text.join(" "))))?;
            print_entries(app, &entries, filter.json)
        }
        HistoryCmd::Show { id, json } => {
            let entry = history.get(id)?.ok_or_else(|| no_such_entry(id))?;
            if json {
                out_line(&serde_json::to_string_pretty(&EntryJson {
                    created: rfc3339(entry.created_at),
                    entry: &entry,
                })?);
            } else {
                out_text(&render_entry(&entry));
            }
            Ok(())
        }
        HistoryCmd::Delete { ids } => {
            let mut missing = Vec::new();
            let mut deleted = 0usize;
            for id in ids {
                if history.delete(id)? {
                    deleted += 1;
                } else {
                    missing.push(id.to_string());
                }
            }
            out_line(&format!("deleted {deleted} entr{}", if deleted == 1 { "y" } else { "ies" }));
            if missing.is_empty() {
                Ok(())
            } else {
                Err(CliError::new(format!("no such history entries: {}", missing.join(", ")))
                    .hint("`ssx history list` shows the ids"))
            }
        }
        HistoryCmd::Prune { max_entries, max_age_days, orphans } => {
            let policy = prune_policy(
                max_entries.or(Some(settings.history.max_entries)),
                max_age_days.or(Some(settings.history.max_age_days)),
            );
            let removed = history.prune(&policy, now_ms())?;
            out_line(&format!("removed {removed} old entr{}", if removed == 1 { "y" } else { "ies" }));
            if orphans {
                let n = history.remove_orphans()?;
                out_line(&format!("removed {n} entr{} whose file is gone", if n == 1 { "y" } else { "ies" }));
            }
            Ok(())
        }
        HistoryCmd::Open { id, file } => {
            let entry = history.get(id)?.ok_or_else(|| no_such_entry(id))?;
            let services = app.services(&settings)?;
            if file {
                let path = entry.local_path.ok_or_else(|| {
                    CliError::new(format!("entry {id} has no local file"))
                        .hint("open its URL instead: ssx history open ID")
                })?;
                if !path.exists() {
                    return Err(CliError::new(format!("{} no longer exists", path.display())));
                }
                services.opener.open_path(&path)?;
            } else {
                let url = entry.upload_url.ok_or_else(|| {
                    CliError::new(format!("entry {id} was never uploaded"))
                        .hint("open its file instead: ssx history open ID --file")
                })?;
                services.opener.open(&url)?;
            }
            Ok(())
        }
    }
}

fn no_such_entry(id: i64) -> CliError {
    CliError::new(format!("there is no history entry {id}")).hint("`ssx history list` shows the ids")
}

/// Retention policy from the two limits; 0 means "no limit".
pub fn prune_policy(max_entries: Option<u32>, max_age_days: Option<u32>) -> PrunePolicy {
    PrunePolicy {
        max_entries: max_entries.filter(|n| *n > 0),
        max_age: max_age_days.filter(|d| *d > 0).map(|d| Duration::from_secs(u64::from(d) * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn entry(id: i64) -> Entry {
        Entry {
            id,
            created_at: 1_700_000_000_000,
            kind: EntryKind::Image,
            local_path: Some(PathBuf::from("/home/u/Pictures/ssx/Screenshots/2026-09/shot.png")),
            thumbnail: None,
            upload_url: Some("https://x.example/a.png".into()),
            thumbnail_url: None,
            deletion_url: Some("https://x.example/del".into()),
            uploader: Some("mine".into()),
            window_title: Some("Terminal".into()),
            process_name: None,
            width: Some(800),
            height: Some(600),
            size_bytes: Some(123_456),
            sha256: Some("ab".into()),
            workflow_id: Some("cli-capture".into()),
            note: None,
        }
    }

    #[test]
    fn queries_reflect_the_filters() {
        let f = HistoryFilter { limit: 5000, kind: Some(HistoryKindArg::Video), uploaded: true, json: false };
        let q = query_for(&f, Some("cat".into()));
        assert_eq!(q.kinds, [EntryKind::Video]);
        assert!(q.uploaded_only && !q.thumbnails);
        assert_eq!(q.limit, 1000, "clamped");
        assert_eq!(q.text.as_deref(), Some("cat"));
        let f = HistoryFilter { limit: 0, kind: None, uploaded: false, json: false };
        let q = query_for(&f, None);
        assert!(q.kinds.is_empty());
        assert_eq!(q.limit, 1);
    }

    #[test]
    fn listing_and_detail_views() {
        let out = render_entries(&[entry(7)], Style::plain());
        assert!(out.lines().next().unwrap().starts_with("ID"));
        assert!(out.contains("7 ") && out.contains("image") && out.contains("https://x.example/a.png"), "{out}");
        assert!(out.contains('\u{2026}'), "long paths are cut: {out}");

        let d = render_entry(&entry(7));
        for needle in [
            "id:          7",
            "2023-11-14T22:13:20.000Z",
            "url:         https://x.example/a.png",
            "size:        800x600 px",
            "bytes:       123.5 kB",
            "workflow:    cli-capture",
            "window:      Terminal",
        ] {
            assert!(d.contains(needle), "{needle} missing from\n{d}");
        }
        assert!(!d.contains("process:"), "absent fields are omitted");
    }

    #[test]
    fn entries_serialise_with_a_readable_timestamp() {
        let e = entry(1);
        let v = serde_json::to_value(EntryJson { created: rfc3339(e.created_at), entry: &e }).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["created"], "2023-11-14T22:13:20.000Z");
        assert_eq!(v["kind"], "image");
        assert!(v.get("thumbnail").is_none(), "thumbnails never leak into JSON");
    }

    #[test]
    fn prune_limits_of_zero_mean_unlimited() {
        assert_eq!(prune_policy(Some(0), Some(0)), PrunePolicy::default());
        let p = prune_policy(Some(100), Some(2));
        assert_eq!(p.max_entries, Some(100));
        assert_eq!(p.max_age, Some(Duration::from_secs(172_800)));
        assert_eq!(prune_policy(None, None), PrunePolicy::default());
    }
}
