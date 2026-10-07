//! Golden-image tests of the real window, rendered headlessly through wgpu (lavapipe on CI).
//!
//! Regenerate after an intentional visual change with
//! `UPDATE_SNAPSHOTS=1 cargo test -p ssx-settings-ui --test snapshots`. Comparison is
//! tolerance-based (a per-pixel colour threshold plus a small budget of differing pixels) so
//! different Vulkan drivers' anti-aliasing does not cause false alarms.
//!
//! Everything a picture shows is fixed: the demo history and settings, the clock, the user
//! and machine names, and the folder the paths point into.
//!
//! Linux only: the golden PNGs were rendered on Linux (lavapipe, this platform's fonts and
//! rasteriser), and some pages list platform-specific content (file managers). Windows
//! renders different pixels and different integrations, so comparing against these images
//! there can only fail; the behaviour is covered by the `ui` tests instead.
#![cfg(target_os = "linux")]

mod common;

use common::*;
use egui::vec2;
use egui_kittest::{Harness, SnapshotOptions};
use ssx_settings_ui::{SettingsApp, nav::Page};

/// The window size a user sees first.
const SIZE: egui::Vec2 = vec2(1120.0, 760.0);

/// A tall window, for pages whose interesting part is below the first screen.
const TALL: egui::Vec2 = vec2(1120.0, 1500.0);

fn opts() -> SnapshotOptions {
    SnapshotOptions::new().threshold(0.8).failed_pixel_count_threshold(2500)
}

fn shot(page: Page, tag: &str) -> (Harness<'static, SettingsApp>, Fixture) {
    shot_sized(page, tag, SIZE)
}

fn shot_sized(page: Page, tag: &str, size: egui::Vec2) -> (Harness<'static, SettingsApp>, Fixture) {
    let fx = Fixture::fixed(tag);
    let (app, fx) = demo_app_in(fx, page, |_| {}, |_| {});
    let mut h = window(app, size);
    finish(&mut h, page);
    (h, fx)
}

/// Waits for everything that happens off the UI thread and hides the mouse pointer.
fn finish(h: &mut Harness<'_, SettingsApp>, page: Page) {
    if page == Page::Capture {
        wait_preview(h);
    }
    wait_busy(h, 60);
    h.remove_cursor();
    settle(h);
}

fn check(h: &mut Harness<'_, SettingsApp>, name: &str) {
    h.snapshot_options(name, &opts());
}

#[test]
fn general() {
    let (mut h, _fx) = shot(Page::General, "general");
    check(&mut h, "page-general");
}

#[test]
fn capture() {
    let (mut h, _fx) = shot_sized(Page::Capture, "capture", TALL);
    check(&mut h, "page-capture");
}

#[test]
fn capture_with_highlights_preserved() {
    let (mut h, _fx) = shot_sized(Page::Capture, "capture-preserve", TALL);
    click_contains(&mut h, "Preserve highlights");
    finish(&mut h, Page::Capture);
    check(&mut h, "page-capture-preserve-highlights");
}

#[test]
fn workflows() {
    let (mut h, _fx) = shot(Page::Workflows, "workflows");
    check(&mut h, "page-workflows");
}

#[test]
fn hotkeys() {
    let (mut h, _fx) = shot(Page::Hotkeys, "hotkeys");
    check(&mut h, "page-hotkeys");
}

#[test]
fn uploaders() {
    let (mut h, _fx) = shot(Page::Uploaders, "uploaders");
    check(&mut h, "page-uploaders");
}

#[test]
fn uploaders_defaults() {
    let (mut h, _fx) = shot(Page::Uploaders, "uploaders-defaults");
    click(&mut h, "Defaults");
    finish(&mut h, Page::Uploaders);
    check(&mut h, "page-uploaders-defaults");
}

#[test]
fn history() {
    let (mut h, _fx) = shot(Page::History, "history");
    check(&mut h, "page-history");
}

#[test]
fn history_with_an_entry_selected() {
    let (mut h, _fx) = shot(Page::History, "history-selected");
    click(&mut h, "Image Screenshot_2025-03-09_14-00-00.png");
    finish(&mut h, Page::History);
    check(&mut h, "page-history-selected");
}

#[test]
fn integration() {
    let (mut h, _fx) = shot(Page::Integration, "integration");
    check(&mut h, "page-integration");
}

#[test]
fn about() {
    let (mut h, _fx) = shot(Page::About, "about");
    check(&mut h, "page-about");
}

#[test]
fn general_with_problems_blocks_saving_and_says_where() {
    let fx = Fixture::fixed("general-problems");
    let (app, _fx) = demo_app_in(
        fx,
        Page::General,
        |s| {
            s.general.file_name_pattern = "shot_%date: %y".to_owned();
            s.general.image_quality = 250;
            s.history.thumbnail_max_edge = 4;
        },
        |_| {},
    );
    let mut h = window(app, TALL);
    finish(&mut h, Page::General);
    check(&mut h, "page-general-problems");
}
