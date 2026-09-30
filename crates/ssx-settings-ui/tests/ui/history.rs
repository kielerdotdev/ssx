//! The History page: loading, search and filters, paging, the detail pane and its actions.
//!
//! The demo history has 40 entries. Entry `i` (0 = newest, id `i + 1`) is named
//! `Screenshot_2025-03-<9 - i/12>_<14 - i%12>-<i*7 % 60>-00.<ext>`; every second one was
//! uploaded (`imgur` when `i % 4 == 0`, else `my-s3`); every fifth (`i % 5 == 3`) has lost its
//! file (8 of them); kinds: `i % 9 == 4` video (4), 6 file (4), 7 text (4), the rest images.

use std::{sync::Arc, time::Duration};

use egui::vec2;
use egui_kittest::Harness;
use ssx_core::history::History;
use ssx_settings_ui::{
    SettingsApp,
    host::{BrokenHistory, RecordingOpener},
    nav::Page,
    thumbs::State as ThumbState,
    uploader_registry::FakeTester,
};

use crate::common::*;

const FIRST: &str = "Screenshot_2025-03-09_14-00-00.png"; // i = 0, imgur upload, file exists
const NO_LINK: &str = "Screenshot_2025-03-09_13-07-00.png"; // i = 1, never uploaded, file exists
const LOST: &str = "Screenshot_2025-03-09_11-21-00.png"; // i = 3, file gone

fn open() -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) = demo_app(Page::History);
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    (h, fx)
}

fn open_recording() -> (Harness<'static, SettingsApp>, Arc<RecordingOpener>, Fixture) {
    let opener = Arc::new(RecordingOpener::default());
    let o2 = opener.clone();
    let (app, fx) = demo_app_custom(Page::History, |_| {}, move |host| host.opener = o2);
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    (h, opener, fx)
}

fn card(name: &str) -> String {
    format!("Image {name}")
}

fn range(from: usize, to: usize, total: usize) -> String {
    format!("{from}\u{2013}{to} of {total}")
}

fn total(h: &Harness<'_, SettingsApp>) -> usize {
    h.state().history.page.as_ref().map_or(0, |p| p.total)
}

#[test]
fn the_history_loads_off_the_ui_thread_and_shows_the_entries() {
    let (app, _fx) = demo_app(Page::History);
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 40);
    assert!(has_exact(&h, &range(1, 40, 40)));
    assert!(has_exact(&h, &card(FIRST)));
    assert!(has(&h, "Select an entry to see its details"));
}

#[test]
fn an_empty_history_says_so() {
    let (app, _fx) = app(Page::History);
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    assert!(has(&h, "The history is empty."));
    assert!(has_exact(&h, "Nothing here"));
}

#[test]
fn a_history_that_cannot_be_opened_is_reported_and_not_touched() {
    let (app, _fx) = demo_app_custom(
        Page::History,
        |_| {},
        |host| host.history = Arc::new(BrokenHistory("database is locked".into())),
    );
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    assert!(has(&h, "database is locked"));
    assert!(has(&h, "never deleted or replaced"));
    click(&mut h, "Try again");
    wait_busy(&mut h, 20);
    assert!(has(&h, "database is locked"), "still broken, still reported, no panic");
}

#[test]
fn a_kind_chip_narrows_the_list_and_clear_filters_brings_everything_back() {
    let (mut h, _fx) = open();
    click(&mut h, "Videos");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 4);
    assert!(has_exact(&h, &range(1, 4, 4)));
    click(&mut h, "Text");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 8, "chips add up");
    click(&mut h, "Clear filters");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 40);
    assert!(!has_exact(&h, "Clear filters"));
}

#[test]
fn typing_in_the_search_box_filters_after_a_short_pause() {
    let (mut h, _fx) = open();
    set_text(&mut h, "Search the history", "2025-03-09_14-00-00");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 1);
    assert!(has_exact(&h, &range(1, 1, 1)));
    assert!(has_exact(&h, &card(FIRST)));
    set_text(&mut h, "Search the history", "no-such-thing-anywhere");
    wait_busy(&mut h, 20);
    assert!(has(&h, "No entry matches these filters."));
    click(&mut h, "Clear filters");
    wait_busy(&mut h, 20);
    assert_eq!(text_of(&h, "Search the history"), "");
    assert_eq!(total(&h), 40);
}

#[test]
fn the_search_matches_links_too() {
    let (mut h, _fx) = open();
    set_text(&mut h, "Search the history", "cdn.example.com");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 10, "the 10 my-s3 uploads");
}

#[test]
fn the_uploader_filter_and_uploaded_only_work_together() {
    let (mut h, _fx) = open();
    click(&mut h, "Uploaded only");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 20);
    click(&mut h, "Uploader");
    click(&mut h, "imgur");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 10);
    assert!(has_exact(&h, &card(FIRST)));
}

#[test]
fn a_date_range_narrows_the_list_and_a_bad_date_is_explained() {
    let (mut h, _fx) = open();
    click(&mut h, "Date range");
    click(&mut h, "Today");
    wait_busy(&mut h, 20);
    let today = total(&h);
    assert!(today > 0 && today < 40, "some but not all entries are from today: {today}");
    click(&mut h, "Date range");
    click(&mut h, "Between dates");
    set_text(&mut h, "From date", "yesterday");
    wait_busy(&mut h, 20);
    assert!(has(&h, "is not a date"));
}

#[test]
fn paging_moves_through_the_entries_without_going_past_the_ends() {
    let (mut h, _fx) = open();
    h.state_mut().history.filter.page_size = 15;
    h.state_mut().history.refresh();
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, &range(1, 15, 40)));
    click(&mut h, "Next");
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, &range(16, 30, 40)));
    click(&mut h, "Next");
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, &range(31, 40, 40)));
    click(&mut h, "Next"); // disabled on the last page
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, &range(31, 40, 40)));
    click(&mut h, "Previous");
    wait_busy(&mut h, 20);
    assert!(has_exact(&h, &range(16, 30, 40)));
}

#[test]
fn selecting_an_entry_shows_its_details() {
    let (mut h, _fx) = open();
    click(&mut h, &card(FIRST));
    assert!(has_exact(&h, "Delete this entry..."));
    assert!(has(&h, "https://i.imgur.com/k000Ab.png"), "the link is shown");
    assert!(has(&h, "1920 x 1080"));
    assert!(has(&h, "capture-region"));
    assert!(has_exact(&h, "imgur"), "the uploader badge");
}

#[test]
fn the_list_view_offers_the_same_entries() {
    let (mut h, _fx) = open();
    click(&mut h, "List");
    assert!(has_exact(&h, &card(FIRST)));
    click(&mut h, &card(FIRST));
    assert!(has_exact(&h, "Copy link"));
    click(&mut h, "Grid");
    assert!(has_exact(&h, &card(FIRST)));
}

#[test]
fn copy_link_and_open_link_use_the_entrys_link() {
    let (mut h, opener, _fx) = open_recording();
    click(&mut h, &card(FIRST));
    let copied = click_and_copied(&mut h, "Copy link");
    assert_eq!(copied, ["https://i.imgur.com/k000Ab.png"]);
    click(&mut h, "Open link");
    click(&mut h, "Open delete link");
    assert_eq!(
        *opener.urls.lock().unwrap(),
        ["https://i.imgur.com/k000Ab.png", "https://imgur.com/delete/000xyz"]
    );
}

#[test]
fn open_file_and_open_folder_go_through_the_opener() {
    let (mut h, opener, fx) = open_recording();
    click(&mut h, &card(FIRST));
    click(&mut h, "Open file");
    click(&mut h, "Open folder");
    let paths = opener.paths.lock().unwrap().clone();
    assert_eq!(paths, [fx.root().join("shots").join(FIRST), fx.root().join("shots")]);
}

#[test]
fn an_entry_that_was_never_uploaded_has_no_link_actions() {
    let (mut h, opener, _fx) = open_recording();
    click(&mut h, &card(NO_LINK));
    let copied = click_and_copied(&mut h, "Copy link");
    assert!(copied.is_empty());
    click(&mut h, "Open link");
    assert!(opener.urls.lock().unwrap().is_empty());
    assert!(!has_exact(&h, "Open delete link"));
}

#[test]
fn an_entry_whose_file_is_gone_is_marked_and_cannot_open_it() {
    let (mut h, opener, _fx) = open_recording();
    assert!(has_exact(&h, "Remove 8 with missing files..."), "the page counts them");
    click(&mut h, &card(LOST));
    assert!(has_exact(&h, "file missing"));
    click(&mut h, "Open file");
    assert!(
        opener.paths.lock().unwrap().is_empty(),
        "nothing is opened for a file that does not exist"
    );
}

#[test]
fn entries_with_missing_files_can_be_removed_in_one_go() {
    let (mut h, _fx) = open();
    click(&mut h, "Remove 8 with missing files...");
    assert!(has(&h, "Remove entries whose file is gone?"));
    click(&mut h, "Cancel");
    assert_eq!(total(&h), 40);
    click(&mut h, "Remove 8 with missing files...");
    click(&mut h, "Remove");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 32);
    assert!(
        has_exact(&h, "Remove 0 with missing files..."),
        "no more orphans, the button is disabled"
    );
}

#[test]
fn deleting_an_entry_keeps_the_file_unless_asked() {
    let (mut h, fx) = open();
    let file = fx.root().join("shots").join(NO_LINK);
    click(&mut h, &card(NO_LINK));
    click(&mut h, "Delete this entry...");
    assert!(has(&h, "Delete this history entry?"));
    click(&mut h, "Cancel");
    assert_eq!(total(&h), 40);
    click(&mut h, "Delete this entry...");
    click(&mut h, "Delete");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 39);
    assert!(!has_exact(&h, &card(NO_LINK)));
    assert!(file.is_file(), "the file stays");
}

#[test]
fn deleting_an_entry_can_delete_its_file_too() {
    let (mut h, fx) = open();
    let file = fx.root().join("shots").join(NO_LINK);
    click(&mut h, &card(NO_LINK));
    click(&mut h, "Delete this entry...");
    click(&mut h, "Also delete the file on disk");
    assert!(has(&h, "will be deleted for good"));
    click(&mut h, "Delete");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 39);
    assert!(!file.exists());
}

#[test]
fn pruning_asks_first_and_applies_the_retention_limits() {
    let (app, _fx) = demo_app_custom(Page::History, |s| s.history.max_entries = 10, |_| {});
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    click(&mut h, "Prune now...");
    assert!(has(&h, "keep at most 10 entries"));
    click(&mut h, "Cancel");
    assert_eq!(total(&h), 40);
    click(&mut h, "Prune now...");
    click(&mut h, "Prune");
    wait_busy(&mut h, 20);
    assert_eq!(total(&h), 10);
    assert!(has_exact(&h, &card(FIRST)), "the newest survive");
}

#[test]
fn prune_is_unavailable_without_limits() {
    let (app, _fx) = demo_app_custom(
        Page::History,
        |s| {
            s.history.max_entries = 0;
            s.history.max_age_days = 0;
        },
        |_| {},
    );
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    click(&mut h, "Prune now...");
    assert!(!has(&h, "Prune the history?"));
}

#[test]
fn thumbnails_are_decoded_lazily_and_a_missing_one_is_a_placeholder() {
    let (mut h, _fx) = open();
    let st = &mut h.state_mut().history;
    assert!(
        matches!(st.thumb_state(1), Some(ThumbState::Ready(_))),
        "entry 0 has a stored thumbnail"
    );
    // entry 5 (id 6) has neither a stored thumbnail nor a decodable file
    assert!(matches!(st.thumb_state(6), Some(ThumbState::Missing(_))));
}

fn reupload_app(tester: FakeTester) -> (Harness<'static, SettingsApp>, Fixture) {
    let (app, fx) =
        demo_app_custom(Page::History, |_| {}, move |host| host.uploads = Arc::new(tester));
    let mut h = window(app, vec2(1500.0, 1200.0));
    wait_busy(&mut h, 20);
    (h, fx)
}

fn link_of(h: &Harness<'_, SettingsApp>, id: i64) -> Option<String> {
    h.state().history.entry(id).and_then(|e| e.upload_url.clone())
}

#[test]
fn uploading_again_records_the_new_link_in_the_history() {
    let (mut h, _fx) = reupload_app(FakeTester::default());
    click(&mut h, &card(NO_LINK));
    assert_eq!(link_of(&h, 2), None);
    click(&mut h, "Upload again...");
    assert!(has_exact(&h, "Upload again"));
    click(&mut h, "Upload");
    wait_busy(&mut h, 20);
    assert!(!has_exact(&h, "Upload again"), "the dialog closed");
    assert_eq!(link_of(&h, 2).as_deref(), Some("https://i.example.com/ssx-test.png"));
    assert!(has(&h, "https://i.example.com/delete/abc"));
}

#[test]
fn a_failed_re_upload_stays_in_the_dialog_with_the_reason_and_changes_nothing() {
    let t = FakeTester { result: Err("connection refused".into()), ..FakeTester::default() };
    let (mut h, _fx) = reupload_app(t);
    click(&mut h, &card(NO_LINK));
    click(&mut h, "Upload again...");
    click(&mut h, "Upload");
    wait_busy(&mut h, 20);
    assert!(has(&h, "connection refused"));
    assert!(has_exact(&h, "Upload again"), "the dialog is still open, to try again or close");
    assert_eq!(link_of(&h, 2), None);
    click(&mut h, "Close");
    assert!(!has_exact(&h, "Upload again"));
}

#[test]
fn a_re_upload_can_be_cancelled_and_then_changes_nothing() {
    let t = FakeTester {
        steps: vec![(0, 10), (5, 10), (10, 10)],
        delay: Duration::from_millis(300),
        ..FakeTester::default()
    };
    let (mut h, _fx) = reupload_app(t);
    click(&mut h, &card(NO_LINK));
    click(&mut h, "Upload again...");
    click(&mut h, "Upload");
    assert!(has_exact(&h, "Cancel upload"), "progress is shown while it runs");
    click(&mut h, "Cancel upload");
    std::thread::sleep(Duration::from_millis(1200));
    wait_busy(&mut h, 20);
    assert_eq!(link_of(&h, 2), None);
}

#[test]
fn an_entry_without_a_file_cannot_be_uploaded_again() {
    let (mut h, _fx) = open();
    click(&mut h, &card(LOST));
    click(&mut h, "Upload again...");
    assert!(!has_exact(&h, "Upload again"), "the button is disabled");
}

#[test]
fn the_history_never_dirties_the_settings() {
    let (mut h, _fx) = open();
    click(&mut h, "Videos");
    wait_busy(&mut h, 20);
    click(&mut h, "Video Screenshot_2025-03-09_10-28-00.mp4");
    assert!(!h.state().model.is_dirty());
    let db: Arc<History> = h.state().host.history.open().unwrap();
    assert_eq!(
        db.count(&ssx_core::history::Query::default()).unwrap(),
        40,
        "browsing deletes nothing"
    );
}
