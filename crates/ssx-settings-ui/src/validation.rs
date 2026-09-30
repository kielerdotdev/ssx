//! Maps the validator's findings onto the fields that show them.
//!
//! `ssx_core::settings::Settings::validate` reports `ValidationIssue`s with a dotted path such
//! as `general.image_quality` or `workflows[2].after_capture[1]`. A widget asks for the issues
//! of *its* path with [`Issues::at`]; the match is "the same path, or something inside it", so
//! a group (a whole workflow) also shows what is wrong with its children, while a sibling with
//! a similar prefix (`workflows[2]` vs `workflows[20]`) is never confused with it.

use ssx_core::settings::{Settings, Severity, ValidationIssue};

use crate::nav::{Page, page_for_path};

/// `true` if `issue_path` is `field` or lies inside it (`field.` / `field[` prefix).
pub fn path_matches(issue_path: &str, field: &str) -> bool {
    match issue_path.strip_prefix(field) {
        Some("") => true,
        Some(rest) => rest.starts_with('.') || rest.starts_with('['),
        None => false,
    }
}

/// The path of the `index`-th workflow.
pub fn workflow_path(index: usize) -> String {
    format!("workflows[{index}]")
}

/// The findings for one state of the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issues(Vec<ValidationIssue>);

impl Issues {
    /// Runs the validator over `settings`. Cheap enough to call every frame something changed.
    pub fn of(settings: &Settings) -> Self {
        Self(settings.validate())
    }

    /// Drops the validator's hint that an uploader "has no [uploaders.x] section" for names
    /// `is_known` recognises (built-in destinations and imported `.sxcu` files need no table).
    pub fn without_known_uploader_hints(mut self, is_known: impl Fn(&str) -> bool) -> Self {
        self.0.retain(|i| {
            let name = i
                .message
                .strip_prefix("uploader \"")
                .filter(|_| i.severity == Severity::Warning)
                .and_then(|rest| rest.split_once('"'))
                .filter(|(_, tail)| tail.starts_with(" has no [uploaders."))
                .map(|(n, _)| n);
            !name.is_some_and(&is_known)
        });
        self
    }

    /// Wraps already computed issues.
    pub fn from_vec(v: Vec<ValidationIssue>) -> Self {
        Self(v)
    }

    /// Every issue.
    pub fn all(&self) -> &[ValidationIssue] {
        &self.0
    }

    /// `true` if there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Issues at `field` or inside it, errors first.
    pub fn at(&self, field: &str) -> Vec<&ValidationIssue> {
        let mut v: Vec<&ValidationIssue> =
            self.0.iter().filter(|i| path_matches(&i.path, field)).collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.severity));
        v
    }

    /// Issues at exactly `field` (not its children).
    pub fn exactly_at(&self, field: &str) -> Vec<&ValidationIssue> {
        self.0.iter().filter(|i| i.path == field).collect()
    }

    /// The most severe issue at or inside `field`.
    pub fn worst_at(&self, field: &str) -> Option<Severity> {
        self.at(field).first().map(|i| i.severity)
    }

    /// Blocking findings.
    pub fn errors(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.0.iter().filter(|i| i.severity == Severity::Error)
    }

    /// Non-blocking findings.
    pub fn warnings(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.0.iter().filter(|i| i.severity == Severity::Warning)
    }

    /// Whether Save must be refused.
    pub fn blocks_save(&self) -> bool {
        self.errors().next().is_some()
    }

    /// `(errors, warnings)` that belong to `page`.
    pub fn counts_for(&self, page: Page) -> (usize, usize) {
        let mut e = 0;
        let mut w = 0;
        for i in self.0.iter().filter(|i| page_for_path(&i.path) == page) {
            match i.severity {
                Severity::Error => e += 1,
                Severity::Warning => w += 1,
            }
        }
        (e, w)
    }

    /// The first page that has an error, in navigation order (where "fix it" should go).
    pub fn first_page_with_error(&self) -> Option<Page> {
        Page::ALL.into_iter().find(|p| self.counts_for(*p).0 > 0)
    }

    /// Total `(errors, warnings)`.
    pub fn totals(&self) -> (usize, usize) {
        Page::ALL.iter().map(|p| self.counts_for(*p)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }
}

/// A short, human label for a validator path, for the "N problems" list
/// (`general.image_quality` becomes "Image quality").
pub fn field_label(path: &str) -> String {
    let last = path.rsplit(['.', ']']).find(|s| !s.is_empty()).unwrap_or(path);
    let base = last.split('[').next().unwrap_or(last);
    let mut out = String::new();
    for (i, w) in base.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if i == 0 {
            let mut c = w.chars();
            if let Some(f) = c.next() {
                out.extend(f.to_uppercase());
                out.push_str(c.as_str());
            }
        } else {
            out.push_str(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ssx_core::settings::Workflow;

    use super::*;

    #[test]
    fn matching_is_exact_or_inside_never_a_sibling() {
        assert!(path_matches("workflows[2]", "workflows[2]"));
        assert!(path_matches("workflows[2].trigger.hotkey", "workflows[2]"));
        assert!(path_matches("workflows[2].after_capture[1]", "workflows[2].after_capture"));
        assert!(!path_matches("workflows[20].trigger.hotkey", "workflows[2]"));
        assert!(!path_matches("general.image_quality", "general.image"));
        assert!(!path_matches("general", "general.image_quality"));
        assert!(path_matches("general.image_quality", "general"));
    }

    #[test]
    fn defaults_have_no_issues() {
        let i = Issues::of(&Settings::default());
        assert!(i.is_empty());
        assert!(!i.blocks_save());
        assert_eq!(i.totals(), (0, 0));
        assert_eq!(i.first_page_with_error(), None);
    }

    #[test]
    fn errors_are_attached_to_their_field_and_page() {
        let mut s = Settings::default();
        s.general.image_quality = 0;
        s.capture.hdr.knee = 3.0;
        s.workflows[1].trigger.hotkey = Some("ctrl+banana".into());
        let i = Issues::of(&s);
        assert!(i.blocks_save());
        assert_eq!(i.at("general.image_quality").len(), 1);
        assert!(i.at("general.image_quality")[0].message.contains("1 to 100"));
        assert_eq!(i.worst_at("capture.hdr.knee"), Some(Severity::Error));
        assert_eq!(i.at("workflows[1]").len(), 1, "children count for the group");
        assert!(i.at("workflows[0]").is_empty());
        assert_eq!(i.counts_for(Page::General), (1, 0));
        assert_eq!(i.counts_for(Page::Capture), (1, 0));
        assert_eq!(i.counts_for(Page::Workflows), (1, 0));
        assert_eq!(i.first_page_with_error(), Some(Page::General));
        assert_eq!(i.totals(), (3, 0));
    }

    #[test]
    fn warnings_do_not_block_and_sort_after_errors() {
        let mut s = Settings::default();
        s.general.file_name_pattern = "%hh-%foo".into();
        let i = Issues::of(&s);
        assert!(!i.blocks_save());
        assert!(i.warnings().count() >= 2);
        assert_eq!(i.worst_at("general.file_name_pattern"), Some(Severity::Warning));
        let mut w = Workflow { id: "a".into(), name: "A".into(), ..Workflow::default() };
        w.trigger.hotkey = Some("Ctrl+A".into());
        let mut s = Settings { workflows: vec![w.clone(), w], ..Settings::default() };
        s.general.file_name_pattern = "%foo".into();
        let i = Issues::of(&s);
        let all = i.at("general");
        assert!(all.iter().all(|x| x.severity == Severity::Warning));
    }

    #[test]
    fn hints_about_missing_tables_are_dropped_for_known_uploaders_only() {
        let mut s = Settings::default();
        s.destinations.image = Some("is.gd".into());
        s.destinations.file = Some("mystery".into());
        let all = Issues::of(&s);
        assert_eq!(all.warnings().count(), 2, "{:?}", all.all());
        let filtered = all.without_known_uploader_hints(|n| n == "is.gd");
        let left: Vec<_> = filtered.warnings().collect();
        assert_eq!(left.len(), 1);
        assert!(left[0].message.contains("mystery"));
        // errors are never dropped, whatever the name
        let mut s = Settings::default();
        s.destinations.image = Some("bad name".into());
        let f = Issues::of(&s).without_known_uploader_hints(|_| true);
        assert_eq!(f.errors().count(), 1);
    }

    #[test]
    fn labels_are_readable() {
        assert_eq!(field_label("general.image_quality"), "Image quality");
        assert_eq!(field_label("workflows[2].trigger.hotkey"), "Hotkey");
        assert_eq!(field_label("workflows[2].after_capture[1]"), "After capture");
        assert_eq!(field_label("hotkeys.open_history"), "Open history");
        assert_eq!(field_label("uploaders"), "Uploaders");
    }
}
