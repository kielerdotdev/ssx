//! The pages of the window and how settings paths map onto them.
//!
//! The mapping lives here (and not in the widgets) so that "which page is dirty" and "which
//! page holds the error the validator found" are answered by one tested function each.

use std::{fmt, str::FromStr};

use ssx_core::settings::Settings;
use ssx_editor_ui::icons::Icon;

/// One page of the settings window, in navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Page {
    /// Save folder, names, format, autostart, history retention.
    General,
    /// Cursor, delay, HDR tone mapping.
    Capture,
    /// The workflows and their steps.
    Workflows,
    /// Global hotkeys and how they are delivered on this desktop.
    Hotkeys,
    /// Upload destinations.
    Uploaders,
    /// The capture / upload history.
    History,
    /// File-manager integration and diagnostics.
    Integration,
    /// Version, licence and notices.
    About,
}

impl Page {
    /// Every page, in navigation order.
    pub const ALL: [Page; 8] = [
        Page::General,
        Page::Capture,
        Page::Workflows,
        Page::Hotkeys,
        Page::Uploaders,
        Page::History,
        Page::Integration,
        Page::About,
    ];

    /// The name shown in the navigation rail and the page title.
    pub const fn label(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Capture => "Capture & HDR",
            Page::Workflows => "Workflows",
            Page::Hotkeys => "Hotkeys",
            Page::Uploaders => "Uploaders",
            Page::History => "History",
            Page::Integration => "Integration",
            Page::About => "About",
        }
    }

    /// The value of `--page`.
    pub const fn slug(self) -> &'static str {
        match self {
            Page::General => "general",
            Page::Capture => "capture",
            Page::Workflows => "workflows",
            Page::Hotkeys => "hotkeys",
            Page::Uploaders => "uploaders",
            Page::History => "history",
            Page::Integration => "integration",
            Page::About => "about",
        }
    }

    /// One line under the page title.
    pub const fn blurb(self) -> &'static str {
        match self {
            Page::General => "Where files go, how they are named, and what is kept.",
            Page::Capture => "Cursor, delay and how HDR screens are converted to SDR.",
            Page::Workflows => {
                "What happens after a capture or an upload, and which key starts it."
            }
            Page::Hotkeys => "Global shortcuts and how this desktop delivers them.",
            Page::Uploaders => "Where screenshots, text, files and videos are sent.",
            Page::History => "Everything captured and uploaded so far.",
            Page::Integration => "Right-click menus in your file manager and a health check.",
            Page::About => "Version, licence and third-party notices.",
        }
    }

    /// The navigation icon (reused from the editor's icon set).
    pub const fn icon(self) -> Icon {
        match self {
            Page::General => Icon::Gear,
            Page::Capture => Icon::RegionRect,
            Page::Workflows => Icon::Layers,
            Page::Hotkeys => Icon::Keyboard,
            Page::Uploaders => Icon::Upload,
            Page::History => Icon::Clipboard,
            Page::Integration => Icon::Menu,
            Page::About => Icon::Help,
        }
    }

    /// The page after / before this one (wrapping), for keyboard navigation.
    pub fn step(self, delta: isize) -> Page {
        let n = Self::ALL.len().cast_signed();
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0).cast_signed();
        Self::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

impl fmt::Display for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// `--page` got a name that is not a page.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown page {:?}; use one of: {}", .0, Page::ALL.map(Page::slug).join(", "))]
pub struct UnknownPage(pub String);

impl FromStr for Page {
    type Err = UnknownPage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        Page::ALL.into_iter().find(|p| p.slug() == s).ok_or(UnknownPage(s))
    }
}

/// The page that shows the field a validation path (`general.image_quality`,
/// `workflows[2].trigger.hotkey`, ...) refers to.
pub fn page_for_path(path: &str) -> Page {
    let head = path.split(['.', '[']).next().unwrap_or("");
    match head {
        "capture" => Page::Capture,
        "workflows" => Page::Workflows,
        "hotkeys" => Page::Hotkeys,
        "destinations" | "uploaders" | "post_file" => Page::Uploaders,
        // `general` and `history` (retention) live on the first page; anything unknown too,
        // so an issue is never hidden.
        _ => Page::General,
    }
}

/// The pages whose part of the settings differs between `saved` and `working`.
///
/// `version` is not user data and is ignored. Workflow hotkeys are edited on the Hotkeys page
/// too, so changing one marks both pages.
pub fn dirty_pages(saved: &Settings, working: &Settings) -> Vec<Page> {
    let mut out = Vec::new();
    if saved.general != working.general || saved.history != working.history {
        out.push(Page::General);
    }
    if saved.capture != working.capture {
        out.push(Page::Capture);
    }
    if saved.workflows != working.workflows {
        out.push(Page::Workflows);
    }
    let hotkeys_of = |s: &Settings| -> Vec<(String, String)> {
        s.workflows
            .iter()
            .filter_map(|w| w.trigger.hotkey.clone().map(|h| (w.id.clone(), h)))
            .collect()
    };
    if saved.hotkeys != working.hotkeys || hotkeys_of(saved) != hotkeys_of(working) {
        out.push(Page::Hotkeys);
    }
    if saved.destinations != working.destinations
        || saved.uploaders != working.uploaders
        || saved.post_file != working.post_file
    {
        out.push(Page::Uploaders);
    }
    out
}

#[cfg(test)]
mod tests {
    use ssx_core::settings::{ImageFormatKind, Workflow};

    use super::*;

    #[test]
    fn slugs_round_trip_and_reject_unknown() {
        for p in Page::ALL {
            assert_eq!(p.slug().parse::<Page>().unwrap(), p);
            assert_eq!(p.slug().to_uppercase().parse::<Page>().unwrap(), p);
        }
        let e = "nope".parse::<Page>().unwrap_err();
        assert!(e.to_string().contains("general") && e.to_string().contains("about"), "{e}");
    }

    #[test]
    fn labels_and_slugs_are_unique() {
        let mut labels: Vec<_> = Page::ALL.iter().map(|p| p.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Page::ALL.len());
    }

    #[test]
    fn stepping_wraps_both_ways() {
        assert_eq!(Page::About.step(1), Page::General);
        assert_eq!(Page::General.step(-1), Page::About);
        assert_eq!(Page::Capture.step(2), Page::Hotkeys);
        for p in Page::ALL {
            assert_eq!(p.step(1).step(-1), p);
            assert_eq!(p.step(8), p);
        }
    }

    #[test]
    fn paths_map_to_pages() {
        assert_eq!(page_for_path("general.image_quality"), Page::General);
        assert_eq!(page_for_path("history.thumbnail_max_edge"), Page::General);
        assert_eq!(page_for_path("capture.hdr.knee"), Page::Capture);
        assert_eq!(page_for_path("workflows[2].trigger.hotkey"), Page::Workflows);
        assert_eq!(page_for_path("hotkeys.open_history"), Page::Hotkeys);
        assert_eq!(page_for_path("destinations.image"), Page::Uploaders);
        assert_eq!(page_for_path("destinations.extension_overrides.zip"), Page::Uploaders);
        assert_eq!(page_for_path("uploaders.my-s3.access_key_id"), Page::Uploaders);
        assert_eq!(page_for_path("post_file.max_parallel_uploads"), Page::Uploaders);
        assert_eq!(page_for_path("something.new"), Page::General);
    }

    #[test]
    fn dirty_pages_follow_the_edited_section() {
        let saved = Settings::default();
        assert!(dirty_pages(&saved, &saved.clone()).is_empty());

        let mut w = saved.clone();
        w.general.image_format = ImageFormatKind::Jpg;
        assert_eq!(dirty_pages(&saved, &w), [Page::General]);

        let mut w = saved.clone();
        w.capture.hdr.knee = 0.5;
        w.history.max_entries = 5;
        assert_eq!(dirty_pages(&saved, &w), [Page::General, Page::Capture]);

        let mut w = saved.clone();
        w.workflows.push(Workflow { id: "x".into(), ..Workflow::default() });
        assert_eq!(dirty_pages(&saved, &w), [Page::Workflows]);

        let mut w = saved.clone();
        w.workflows[0].trigger.hotkey = Some("F9".into());
        assert_eq!(dirty_pages(&saved, &w), [Page::Workflows, Page::Hotkeys]);

        let mut w = saved.clone();
        w.destinations.image = Some("imgur".into());
        assert_eq!(dirty_pages(&saved, &w), [Page::Uploaders]);
    }

    #[test]
    fn version_alone_is_not_a_change() {
        let saved = Settings::default();
        let mut w = saved.clone();
        w.version += 1;
        assert!(dirty_pages(&saved, &w).is_empty());
    }
}
