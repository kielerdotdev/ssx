//! The History page without any drawing: filters, paging, and the actions an entry offers.
//!
//! [`HistoryFilter`] is the state of the search box, the kind chips, the date range and the
//! uploader menu; [`HistoryFilter::to_query`] turns it into the `ssx_core::history::Query`
//! the database understands. **Uploader filtering is done here**, not in SQL: the core query
//! has no uploader column filter (its text search would also match file names and notes that
//! happen to contain the word), so [`load_page`] scans the newest [`SCAN_LIMIT`] matching rows
//! without thumbnails and keeps the entries whose uploader is exactly the chosen one. A core
//! `Query::uploader` would make this a single indexed query (noted in the crate README).

use std::{collections::BTreeSet, path::Path};

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone};
use ssx_core::{
    history::{Entry, EntryKind, History, HistoryError, PrunePolicy, Query},
    settings::{DestinationType, HistorySettings},
};

/// Entries per page.
pub const DEFAULT_PAGE_SIZE: usize = 48;
/// The most rows scanned when the uploader filter has to be applied here.
pub const SCAN_LIMIT: usize = 5000;
/// Rows fetched per database call while scanning (the core clamps to 1000).
const CHUNK: usize = 500;

/// The date ranges offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateRange {
    /// No limit.
    #[default]
    Any,
    /// Since local midnight.
    Today,
    /// The last 7 days.
    Week,
    /// The last 30 days.
    Month,
    /// Between the dates typed in the two fields.
    Custom,
}

impl DateRange {
    /// All ranges in menu order.
    pub const ALL: [DateRange; 5] =
        [DateRange::Any, DateRange::Today, DateRange::Week, DateRange::Month, DateRange::Custom];

    /// Menu label.
    pub const fn label(self) -> &'static str {
        match self {
            DateRange::Any => "Any time",
            DateRange::Today => "Today",
            DateRange::Week => "Last 7 days",
            DateRange::Month => "Last 30 days",
            DateRange::Custom => "Between dates",
        }
    }
}

/// Parses `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .map_err(|_| format!("{text:?} is not a date; write it as 2025-01-31"))
}

fn day_start_ms(date: NaiveDate, tz: FixedOffset) -> Option<i64> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    tz.from_local_datetime(&naive).single().map(|d| d.timestamp_millis())
}

/// Everything the filter bar edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFilter {
    /// Search text (every word must prefix-match a word of the path, URL, title, ...).
    pub text: String,
    /// Kinds to show; empty means all.
    pub kinds: BTreeSet<KindKey>,
    /// The date range.
    pub range: DateRange,
    /// `YYYY-MM-DD` for [`DateRange::Custom`] (from).
    pub from_text: String,
    /// `YYYY-MM-DD` for [`DateRange::Custom`] (to, inclusive).
    pub to_text: String,
    /// Only entries uploaded by this destination.
    pub uploader: Option<String>,
    /// Only entries that have a URL.
    pub uploaded_only: bool,
    /// Zero-based page.
    pub page: usize,
    /// Entries per page.
    pub page_size: usize,
}

/// [`EntryKind`] made orderable so a set of them can live in the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KindKey(pub u8);

impl KindKey {
    /// The key of a kind.
    pub const fn of(k: EntryKind) -> Self {
        Self(match k {
            EntryKind::Image => 0,
            EntryKind::Video => 1,
            EntryKind::File => 2,
            EntryKind::Text => 3,
            EntryKind::Url => 4,
        })
    }

    /// The kind.
    pub const fn kind(self) -> EntryKind {
        match self.0 {
            0 => EntryKind::Image,
            1 => EntryKind::Video,
            2 => EntryKind::File,
            3 => EntryKind::Text,
            _ => EntryKind::Url,
        }
    }
}

/// Every kind, in chip order.
pub const KINDS: [EntryKind; 5] =
    [EntryKind::Image, EntryKind::Video, EntryKind::File, EntryKind::Text, EntryKind::Url];

/// The chip label of a kind.
pub const fn kind_label(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Image => "Images",
        EntryKind::Video => "Videos",
        EntryKind::File => "Files",
        EntryKind::Text => "Text",
        EntryKind::Url => "Links",
    }
}

/// The singular word of a kind.
pub const fn kind_word(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Image => "Image",
        EntryKind::Video => "Video",
        EntryKind::File => "File",
        EntryKind::Text => "Text",
        EntryKind::Url => "Link",
    }
}

impl Default for HistoryFilter {
    fn default() -> Self {
        Self {
            text: String::new(),
            kinds: BTreeSet::new(),
            range: DateRange::Any,
            from_text: String::new(),
            to_text: String::new(),
            uploader: None,
            uploaded_only: false,
            page: 0,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl HistoryFilter {
    /// Changes the search text (back to the first page when it changed).
    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            text.clone_into(&mut self.text);
            self.page = 0;
        }
    }

    /// Flips one kind chip.
    pub fn toggle_kind(&mut self, k: EntryKind) {
        let key = KindKey::of(k);
        if !self.kinds.remove(&key) {
            self.kinds.insert(key);
        }
        self.page = 0;
    }

    /// Whether the kind chip is on.
    pub fn has_kind(&self, k: EntryKind) -> bool {
        self.kinds.contains(&KindKey::of(k))
    }

    /// Chooses the date range.
    pub fn set_range(&mut self, r: DateRange) {
        if self.range != r {
            self.range = r;
            self.page = 0;
        }
    }

    /// Chooses the uploader (`None` = any).
    pub fn set_uploader(&mut self, u: Option<String>) {
        if self.uploader != u {
            self.uploader = u;
            self.page = 0;
        }
    }

    /// Flips "uploaded only".
    pub fn set_uploaded_only(&mut self, on: bool) {
        if self.uploaded_only != on {
            self.uploaded_only = on;
            self.page = 0;
        }
    }

    /// Clears everything except the page size.
    pub fn clear(&mut self) {
        *self = Self { page_size: self.page_size, ..Self::default() };
    }

    /// `true` if any filter is set (the UI shows a "Clear filters" button).
    pub fn is_filtering(&self) -> bool {
        !self.text.trim().is_empty()
            || !self.kinds.is_empty()
            || self.range != DateRange::Any
            || self.uploader.is_some()
            || self.uploaded_only
    }

    /// Problems with the custom date fields (empty when fine or not in use).
    pub fn date_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.range != DateRange::Custom {
            return out;
        }
        let from = (!self.from_text.trim().is_empty()).then(|| parse_date(&self.from_text));
        let to = (!self.to_text.trim().is_empty()).then(|| parse_date(&self.to_text));
        if let Some(Err(e)) = &from {
            out.push(e.clone());
        }
        if let Some(Err(e)) = &to {
            out.push(e.clone());
        }
        if let (Some(Ok(f)), Some(Ok(t))) = (&from, &to)
            && f > t
        {
            out.push("the first date is after the second".to_owned());
        }
        out
    }

    /// The `(since, until)` bounds in Unix milliseconds for `now` in time zone `tz`.
    /// `until` is exclusive.
    pub fn bounds(&self, now: DateTime<FixedOffset>) -> (Option<i64>, Option<i64>) {
        let tz = *now.offset();
        match self.range {
            DateRange::Any => (None, None),
            DateRange::Today => (day_start_ms(now.date_naive(), tz), None),
            DateRange::Week => (Some((now - Duration::days(7)).timestamp_millis()), None),
            DateRange::Month => (Some((now - Duration::days(30)).timestamp_millis()), None),
            DateRange::Custom => {
                let since = parse_date(&self.from_text).ok().and_then(|d| day_start_ms(d, tz));
                let until = parse_date(&self.to_text)
                    .ok()
                    .and_then(|d| d.succ_opt())
                    .and_then(|d| day_start_ms(d, tz));
                (since, until)
            }
        }
    }

    /// The database query for the current page (without the uploader filter, see the module
    /// docs). `thumbnails` is always off: they are loaded lazily.
    pub fn to_query(&self, now: DateTime<FixedOffset>) -> Query {
        let (since, until) = self.bounds(now);
        Query {
            text: (!self.text.trim().is_empty()).then(|| self.text.trim().to_owned()),
            kinds: self.kinds.iter().map(|k| k.kind()).collect(),
            since,
            until,
            uploaded_only: self.uploaded_only,
            // Deliberately not `self.uploader`: the uploader menu learns every name seen while
            // scanning, which a server-side filter would hide. (`Query::uploader` exists now
            // and would make the filtered case a single query; see the crate README.)
            uploader: None,
            limit: self.page_size.clamp(1, 1000),
            offset: self.page * self.page_size,
            thumbnails: false,
        }
    }

    /// Number of pages for `total` matches (at least 1).
    pub fn page_count(&self, total: usize) -> usize {
        total.div_ceil(self.page_size.max(1)).max(1)
    }

    /// Keeps `page` inside `0..page_count(total)`.
    pub fn clamp_page(&mut self, total: usize) {
        self.page = self.page.min(self.page_count(total) - 1);
    }
}

/// One loaded page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryPage {
    /// The entries, newest first.
    pub entries: Vec<Entry>,
    /// How many entries match the filter in total.
    pub total: usize,
    /// `true` if the uploader filter stopped at [`SCAN_LIMIT`] rows.
    pub truncated: bool,
    /// The uploaders seen on this page (to fill the menu).
    pub uploaders: BTreeSet<String>,
}

/// Loads the page `filter` describes. Never returns thumbnails.
pub fn load_page(
    history: &History,
    filter: &HistoryFilter,
    now: DateTime<FixedOffset>,
) -> Result<HistoryPage, HistoryError> {
    let mut q = filter.to_query(now);
    let Some(uploader) = filter.uploader.as_deref() else {
        let total = usize::try_from(history.count(&q)?).unwrap_or(usize::MAX);
        let entries = history.list(&q)?;
        let uploaders = entries.iter().filter_map(|e| e.uploader.clone()).collect();
        return Ok(HistoryPage { entries, total, truncated: false, uploaders });
    };
    // Uploader filter: scan matching rows and keep the exact matches.
    q.limit = CHUNK;
    let mut matches = Vec::new();
    let mut seen = BTreeSet::new();
    let mut offset = 0;
    let mut truncated = false;
    loop {
        q.offset = offset;
        let chunk = history.list(&q)?;
        let n = chunk.len();
        for e in chunk {
            if let Some(u) = &e.uploader {
                seen.insert(u.clone());
            }
            if e.uploader.as_deref() == Some(uploader) {
                matches.push(e);
            }
        }
        offset += n;
        if n < CHUNK {
            break;
        }
        if offset >= SCAN_LIMIT {
            truncated = true;
            break;
        }
    }
    let total = matches.len();
    let start = (filter.page * filter.page_size).min(total);
    let entries = matches.into_iter().skip(start).take(filter.page_size).collect();
    Ok(HistoryPage { entries, total, truncated, uploaders: seen })
}

// ---- entry presentation -------------------------------------------------------------------

/// A one-line title: the file name, else the URL, else the window title, else the kind.
pub fn entry_title(e: &Entry) -> String {
    if let Some(name) = e.local_path.as_deref().and_then(Path::file_name) {
        return name.to_string_lossy().into_owned();
    }
    if let Some(url) = e.upload_url.as_deref() {
        return url.trim_start_matches("https://").trim_start_matches("http://").to_owned();
    }
    e.window_title.clone().unwrap_or_else(|| kind_word(e.kind).to_owned())
}

/// `1.5 MB`, `312 KB`, `12 B`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[i]) } else { format!("{v:.1} {}", UNITS[i]) }
}

/// `2025-01-31 14:05` in `tz`.
pub fn format_time(ms: i64, tz: FixedOffset) -> String {
    match tz.timestamp_millis_opt(ms).single() {
        Some(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        None => "unknown time".to_owned(),
    }
}

/// A short relative day: `Today 14:05`, `Yesterday 09:10`, else the date.
pub fn format_time_relative(ms: i64, now: DateTime<FixedOffset>) -> String {
    let tz = *now.offset();
    let Some(d) = tz.timestamp_millis_opt(ms).single() else { return "unknown time".to_owned() };
    let days = now.date_naive().num_days_from_ce() - d.date_naive().num_days_from_ce();
    match days {
        0 => format!("Today {}", d.format("%H:%M")),
        1 => format!("Yesterday {}", d.format("%H:%M")),
        _ => d.format("%Y-%m-%d %H:%M").to_string(),
    }
}

/// What can be done with an entry right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryActions {
    /// There is a URL to copy.
    pub copy_url: bool,
    /// There is a web URL to open.
    pub open_url: bool,
    /// The local file exists.
    pub open_file: bool,
    /// The folder of the local file exists.
    pub open_folder: bool,
    /// The local file can be uploaded again.
    pub reupload: bool,
    /// The local file is gone although the entry says there is one.
    pub orphan: bool,
    /// The entry has a deletion URL.
    pub has_deletion_url: bool,
}

impl EntryActions {
    /// Works out the actions from the entry and what exists on disk.
    pub fn of(e: &Entry, file_exists: bool, folder_exists: bool) -> Self {
        let has_path = e.local_path.is_some();
        let web = |u: &Option<String>| {
            u.as_deref().is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
        };
        Self {
            copy_url: e.upload_url.as_deref().is_some_and(|u| !u.is_empty()),
            open_url: web(&e.upload_url),
            open_file: has_path && file_exists,
            open_folder: has_path && folder_exists,
            reupload: has_path && file_exists && e.kind != EntryKind::Url,
            orphan: has_path && !file_exists,
            has_deletion_url: web(&e.deletion_url),
        }
    }
}

/// The destination type an entry of `kind` is uploaded as.
pub const fn destination_for(kind: EntryKind) -> DestinationType {
    match kind {
        EntryKind::Image => DestinationType::Image,
        EntryKind::Video => DestinationType::Video,
        EntryKind::Text => DestinationType::Text,
        EntryKind::File | EntryKind::Url => DestinationType::File,
    }
}

/// The prune policy the retention settings describe (`0` means unlimited).
pub fn prune_policy(h: &HistorySettings) -> PrunePolicy {
    PrunePolicy {
        max_entries: (h.max_entries > 0).then_some(h.max_entries),
        max_age: (h.max_age_days > 0)
            .then(|| std::time::Duration::from_secs(u64::from(h.max_age_days) * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ssx_core::history::NewEntry;

    use super::*;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2025-03-15T14:30:00+02:00").unwrap()
    }

    fn ms(rfc: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc).unwrap().timestamp_millis()
    }

    #[test]
    fn default_filter_asks_for_everything_newest_first() {
        let q = HistoryFilter::default().to_query(now());
        assert_eq!(q.text, None);
        assert!(q.kinds.is_empty() && !q.uploaded_only && !q.thumbnails);
        assert_eq!((q.limit, q.offset), (DEFAULT_PAGE_SIZE, 0));
        assert_eq!((q.since, q.until), (None, None));
        assert!(!HistoryFilter::default().is_filtering());
    }

    #[test]
    fn text_kinds_and_flags_reach_the_query() {
        let mut f = HistoryFilter::default();
        f.set_text("  invoice ");
        f.toggle_kind(EntryKind::Image);
        f.toggle_kind(EntryKind::Video);
        f.set_uploaded_only(true);
        let q = f.to_query(now());
        assert_eq!(q.text.as_deref(), Some("invoice"));
        assert_eq!(q.kinds, [EntryKind::Image, EntryKind::Video]);
        assert!(q.uploaded_only);
        assert!(f.is_filtering());
        f.toggle_kind(EntryKind::Image);
        assert_eq!(f.to_query(now()).kinds, [EntryKind::Video]);
        assert!(f.has_kind(EntryKind::Video) && !f.has_kind(EntryKind::Image));
    }

    #[test]
    fn blank_text_is_no_text() {
        let mut f = HistoryFilter::default();
        f.set_text("   ");
        assert_eq!(f.to_query(now()).text, None);
        assert!(!f.is_filtering());
    }

    #[test]
    fn changing_a_filter_returns_to_the_first_page() {
        let mut f = HistoryFilter { page: 3, ..HistoryFilter::default() };
        f.set_text("");
        assert_eq!(f.page, 3, "an unchanged text keeps the page");
        f.set_text("a");
        assert_eq!(f.page, 0);
        for change in [
            |f: &mut HistoryFilter| f.toggle_kind(EntryKind::File),
            |f: &mut HistoryFilter| f.set_range(DateRange::Week),
            |f: &mut HistoryFilter| f.set_uploader(Some("imgur".into())),
            |f: &mut HistoryFilter| f.set_uploaded_only(true),
        ] {
            f.page = 5;
            change(&mut f);
            assert_eq!(f.page, 0);
        }
    }

    #[test]
    fn paging_maps_to_offsets_and_counts() {
        let f = HistoryFilter { page: 2, page_size: 10, ..HistoryFilter::default() };
        let q = f.to_query(now());
        assert_eq!((q.limit, q.offset), (10, 20));
        assert_eq!(f.page_count(0), 1);
        assert_eq!(f.page_count(10), 1);
        assert_eq!(f.page_count(11), 2);
        assert_eq!(f.page_count(95), 10);
        let mut f = HistoryFilter { page: 9, page_size: 10, ..HistoryFilter::default() };
        f.clamp_page(25);
        assert_eq!(f.page, 2);
        f.clamp_page(0);
        assert_eq!(f.page, 0);
    }

    #[test]
    fn today_starts_at_local_midnight() {
        let f = HistoryFilter { range: DateRange::Today, ..HistoryFilter::default() };
        assert_eq!(f.bounds(now()), (Some(ms("2025-03-15T00:00:00+02:00")), None));
        let late = DateTime::parse_from_rfc3339("2025-03-15T23:59:59+02:00").unwrap();
        assert_eq!(f.bounds(late).0, Some(ms("2025-03-15T00:00:00+02:00")));
    }

    #[test]
    fn week_and_month_are_rolling() {
        let mut f = HistoryFilter { range: DateRange::Week, ..HistoryFilter::default() };
        assert_eq!(f.bounds(now()).0, Some(ms("2025-03-08T14:30:00+02:00")));
        f.range = DateRange::Month;
        assert_eq!(f.bounds(now()).0, Some(ms("2025-02-13T14:30:00+02:00")));
    }

    #[test]
    fn custom_range_is_inclusive_of_the_last_day() {
        let f = HistoryFilter {
            range: DateRange::Custom,
            from_text: "2025-03-01".into(),
            to_text: "2025-03-10".into(),
            ..HistoryFilter::default()
        };
        assert_eq!(
            f.bounds(now()),
            (Some(ms("2025-03-01T00:00:00+02:00")), Some(ms("2025-03-11T00:00:00+02:00")))
        );
        assert!(f.date_problems().is_empty());
        let open = HistoryFilter { to_text: String::new(), ..f.clone() };
        assert_eq!(open.bounds(now()).1, None);
    }

    #[test]
    fn bad_custom_dates_are_reported_and_ignored() {
        let f = HistoryFilter {
            range: DateRange::Custom,
            from_text: "yesterday".into(),
            to_text: "2025-13-40".into(),
            ..HistoryFilter::default()
        };
        assert_eq!(f.date_problems().len(), 2);
        assert_eq!(f.bounds(now()), (None, None));
        let backwards = HistoryFilter {
            range: DateRange::Custom,
            from_text: "2025-03-10".into(),
            to_text: "2025-03-01".into(),
            ..HistoryFilter::default()
        };
        assert!(backwards.date_problems()[0].contains("after"));
        let unused = HistoryFilter {
            range: DateRange::Week,
            from_text: "junk".into(),
            ..HistoryFilter::default()
        };
        assert!(unused.date_problems().is_empty());
    }

    #[test]
    fn clear_keeps_only_the_page_size() {
        let mut f = HistoryFilter { page_size: 12, page: 4, ..HistoryFilter::default() };
        f.set_text("x");
        f.toggle_kind(EntryKind::Text);
        f.clear();
        assert_eq!(f, HistoryFilter { page_size: 12, ..HistoryFilter::default() });
    }

    #[test]
    fn kind_keys_round_trip() {
        for k in KINDS {
            assert_eq!(KindKey::of(k).kind(), k);
            assert!(!kind_label(k).is_empty() && !kind_word(k).is_empty());
        }
    }

    fn seeded() -> History {
        let h = History::open_in_memory().unwrap();
        let base = ms("2025-03-15T12:00:00+02:00");
        for i in 0..30 {
            let mut e = NewEntry::new(if i % 3 == 0 { EntryKind::Video } else { EntryKind::Image });
            e.created_at = base - i * 3_600_000;
            e.local_path = Some(format!("/shots/file-{i}.png").into());
            if i % 2 == 0 {
                e.upload_url = Some(format!("https://i.example.com/{i}"));
                e.uploader = Some(if i % 4 == 0 { "imgur".into() } else { "my-s3".into() });
            }
            e.window_title = Some(format!("Window {i}"));
            h.insert(&e).unwrap();
        }
        h
    }

    #[test]
    fn pages_come_from_the_database_with_totals() {
        let h = seeded();
        let mut f = HistoryFilter { page_size: 10, ..HistoryFilter::default() };
        let p = load_page(&h, &f, now()).unwrap();
        assert_eq!((p.entries.len(), p.total), (10, 30));
        assert!(p.entries.iter().all(|e| e.thumbnail.is_none()));
        assert!(p.entries[0].created_at > p.entries[9].created_at, "newest first");
        f.page = 2;
        let p3 = load_page(&h, &f, now()).unwrap();
        assert_eq!(p3.entries.len(), 10);
        assert!(p3.entries[0].created_at < p.entries[9].created_at);
        f.page = 3;
        assert!(load_page(&h, &f, now()).unwrap().entries.is_empty());
        assert!(p.uploaders.contains("imgur") && p.uploaders.contains("my-s3"));
    }

    #[test]
    fn kind_and_uploaded_filters_apply_in_the_database() {
        let h = seeded();
        let mut f = HistoryFilter::default();
        f.toggle_kind(EntryKind::Video);
        assert_eq!(load_page(&h, &f, now()).unwrap().total, 10);
        f.set_uploaded_only(true);
        let p = load_page(&h, &f, now()).unwrap();
        assert!(p.entries.iter().all(|e| e.kind == EntryKind::Video && e.upload_url.is_some()));
        assert_eq!(p.total, p.entries.len());
    }

    #[test]
    fn the_date_filter_applies() {
        let h = seeded();
        let f = HistoryFilter { range: DateRange::Today, ..HistoryFilter::default() };
        let p = load_page(&h, &f, now()).unwrap();
        // entries at 12:00, 11:00, ... 00:00 today (local): 13 of them
        assert_eq!(p.total, 13, "{:?}", p.entries.iter().map(|e| e.created_at).collect::<Vec<_>>());
    }

    #[test]
    fn uploader_filter_is_exact_and_pages_in_memory() {
        let h = seeded();
        let mut f = HistoryFilter { page_size: 5, ..HistoryFilter::default() };
        f.set_uploader(Some("imgur".into()));
        let p = load_page(&h, &f, now()).unwrap();
        assert_eq!(p.total, 8, "i % 4 == 0 among 0..30");
        assert_eq!(p.entries.len(), 5);
        assert!(p.entries.iter().all(|e| e.uploader.as_deref() == Some("imgur")));
        assert!(!p.truncated);
        assert!(p.uploaders.contains("my-s3"), "the menu learns every uploader seen");
        f.page = 1;
        assert_eq!(load_page(&h, &f, now()).unwrap().entries.len(), 3);
        f.page = 7;
        assert!(load_page(&h, &f, now()).unwrap().entries.is_empty());
        // "my" would text-match my-s3 but must not match a different uploader name
        f.page = 0;
        f.set_uploader(Some("my".into()));
        assert_eq!(load_page(&h, &f, now()).unwrap().total, 0);
    }

    #[test]
    fn uploader_and_kind_filters_combine() {
        let h = seeded();
        let mut f = HistoryFilter::default();
        f.set_uploader(Some("imgur".into()));
        f.toggle_kind(EntryKind::Video);
        let p = load_page(&h, &f, now()).unwrap();
        assert!(
            p.entries
                .iter()
                .all(|e| e.kind == EntryKind::Video && e.uploader.as_deref() == Some("imgur"))
        );
        assert_eq!(p.total, 3, "i % 12 == 0 among 0..30");
    }

    #[test]
    fn search_text_reaches_the_database() {
        let h = seeded();
        let mut f = HistoryFilter::default();
        f.set_text("file-7");
        let p = load_page(&h, &f, now()).unwrap();
        assert_eq!(p.total, 1);
        assert!(p.entries[0].local_path.as_ref().unwrap().ends_with("file-7.png"));
    }

    #[test]
    fn titles_prefer_the_file_name() {
        let mut e = Entry {
            id: 1,
            created_at: 0,
            kind: EntryKind::Image,
            local_path: Some("/a/b/shot.png".into()),
            thumbnail: None,
            upload_url: Some("https://x.example.com/abc".into()),
            thumbnail_url: None,
            deletion_url: None,
            uploader: None,
            window_title: Some("Win".into()),
            process_name: None,
            width: None,
            height: None,
            size_bytes: None,
            sha256: None,
            workflow_id: None,
            note: None,
        };
        assert_eq!(entry_title(&e), "shot.png");
        e.local_path = None;
        assert_eq!(entry_title(&e), "x.example.com/abc");
        e.upload_url = None;
        assert_eq!(entry_title(&e), "Win");
        e.window_title = None;
        assert_eq!(entry_title(&e), "Image");
    }

    #[test]
    fn byte_and_time_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(300 * 1024 * 1024), "300 MB");
        assert_eq!(format_time(ms("2025-03-15T14:05:00+02:00"), tz()), "2025-03-15 14:05");
        assert_eq!(format_time_relative(ms("2025-03-15T09:10:00+02:00"), now()), "Today 09:10");
        assert_eq!(format_time_relative(ms("2025-03-14T23:10:00+02:00"), now()), "Yesterday 23:10");
        assert_eq!(
            format_time_relative(ms("2025-03-01T08:00:00+02:00"), now()),
            "2025-03-01 08:00"
        );
    }

    #[test]
    fn actions_follow_what_exists() {
        let mut e = seeded().list(&Query::default()).unwrap().remove(0);
        e.upload_url = Some("https://i.example.com/1".into());
        e.deletion_url = Some("https://i.example.com/del".into());
        let a = EntryActions::of(&e, true, true);
        assert!(a.copy_url && a.open_url && a.open_file && a.open_folder && a.reupload);
        assert!(a.has_deletion_url && !a.orphan);
        let gone = EntryActions::of(&e, false, false);
        assert!(gone.orphan && !gone.open_file && !gone.reupload && gone.open_url);
        e.local_path = None;
        e.upload_url = Some("ftp://x".into());
        let a = EntryActions::of(&e, false, false);
        assert!(a.copy_url && !a.open_url && !a.orphan && !a.reupload);
        e.upload_url = None;
        assert!(!EntryActions::of(&e, false, false).copy_url);
    }

    #[test]
    fn prune_policy_treats_zero_as_unlimited() {
        let p = prune_policy(&HistorySettings {
            max_entries: 0,
            max_age_days: 0,
            ..HistorySettings::default()
        });
        assert_eq!(p, PrunePolicy::default());
        let p = prune_policy(&HistorySettings {
            max_entries: 50,
            max_age_days: 2,
            ..HistorySettings::default()
        });
        assert_eq!(p.max_entries, Some(50));
        assert_eq!(p.max_age, Some(std::time::Duration::from_secs(2 * 86_400)));
        assert_eq!(destination_for(EntryKind::Video), DestinationType::Video);
        assert_eq!(destination_for(EntryKind::Url), DestinationType::File);
    }

    proptest! {
        #[test]
        fn queries_never_exceed_the_core_limits(
            page in 0usize..1000,
            size in 0usize..5000,
            text in ".{0,30}",
        ) {
            let f = HistoryFilter { page, page_size: size, text, ..HistoryFilter::default() };
            let q = f.to_query(now());
            prop_assert!((1..=1000).contains(&q.limit));
            prop_assert_eq!(q.offset, page * size);
            prop_assert!(!q.thumbnails);
        }

        #[test]
        fn clamped_pages_are_always_valid(total in 0usize..10_000, page in 0usize..10_000, size in 1usize..200) {
            let mut f = HistoryFilter { page, page_size: size, ..HistoryFilter::default() };
            f.clamp_page(total);
            prop_assert!(f.page < f.page_count(total));
            prop_assert!(f.page * size < total.max(1));
        }
    }
}
