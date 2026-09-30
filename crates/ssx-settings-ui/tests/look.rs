//! `SSX_UI_DUMP=dir cargo test -p ssx-settings-ui --test look` writes a PNG of every page for
//! eyeballing (tall windows, so the whole page is visible). Without the variable these tests
//! only check that every page renders.

mod common;

use common::*;
use egui::vec2;
use ssx_settings_ui::nav::Page;

/// The height that shows the whole page.
fn tall(page: Page) -> f32 {
    match page {
        Page::General | Page::Capture | Page::Hotkeys | Page::Integration => 1500.0,
        Page::Workflows => 1300.0,
        Page::Uploaders => 1100.0,
        _ => 900.0,
    }
}

#[test]
fn every_page_renders_at_the_default_size() {
    for page in Page::ALL {
        let (app, _fx) = demo_app(page);
        let mut h = window(app, vec2(1120.0, 760.0));
        settle(&mut h);
        if page == Page::Capture {
            wait_preview(&mut h);
        }
        wait_busy(&mut h, 30);
        h.remove_cursor();
        settle(&mut h);
        dump(&mut h, &format!("page-{}", page.slug()));
    }
}

#[test]
fn every_page_renders_completely_in_a_tall_window() {
    for page in Page::ALL {
        let (app, _fx) = demo_app(page);
        let mut h = window(app, vec2(1120.0, tall(page)));
        settle(&mut h);
        if page == Page::Capture {
            wait_preview(&mut h);
        }
        wait_busy(&mut h, 30);
        h.remove_cursor();
        settle(&mut h);
        dump(&mut h, &format!("tall-{}", page.slug()));
    }
}
