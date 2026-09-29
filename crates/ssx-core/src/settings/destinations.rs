//! Which uploader handles which kind of content.
//!
//! Uploaders are referenced **by name** (`"imgur"`, `"my-s3"`, …). The names are resolved by
//! whoever implements [`crate::workflow::Uploaders`]; this crate never needs to know what an
//! uploader is. Configuration for an uploader lives in the opaque `[uploaders.<name>]`
//! tables of the settings file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The kind of destination a piece of content is sent to (ShareX "destination types").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationType {
    /// Screenshots and other images.
    Image,
    /// Text snippets (OCR results, clipboard text).
    Text,
    /// Arbitrary files.
    File,
    /// Screen recordings. Falls back to [`DestinationType::File`] when unset.
    Video,
    /// URL shortening services.
    UrlShortener,
    /// URL sharing services (post a link to social media, chat, …).
    UrlSharing,
}

impl DestinationType {
    /// All destination types, in a stable order.
    pub const ALL: [DestinationType; 6] = [
        DestinationType::Image,
        DestinationType::Text,
        DestinationType::File,
        DestinationType::Video,
        DestinationType::UrlShortener,
        DestinationType::UrlSharing,
    ];
}

/// Per-workflow destination overrides. `None` means "use the global default".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DestinationOverride {
    /// Image uploader.
    pub image: Option<String>,
    /// Text uploader.
    pub text: Option<String>,
    /// File uploader.
    pub file: Option<String>,
    /// Video uploader.
    pub video: Option<String>,
    /// URL shortener.
    pub url_shortener: Option<String>,
    /// URL sharing service.
    pub url_sharing: Option<String>,
}

impl DestinationOverride {
    /// The override for `ty`, if set.
    pub fn get(&self, ty: DestinationType) -> Option<&str> {
        match ty {
            DestinationType::Image => self.image.as_deref(),
            DestinationType::Text => self.text.as_deref(),
            DestinationType::File => self.file.as_deref(),
            DestinationType::Video => self.video.as_deref(),
            DestinationType::UrlShortener => self.url_shortener.as_deref(),
            DestinationType::UrlSharing => self.url_sharing.as_deref(),
        }
    }

    /// All `(type, name)` pairs that are set.
    pub fn entries(&self) -> impl Iterator<Item = (DestinationType, &str)> {
        DestinationType::ALL.into_iter().filter_map(|t| self.get(t).map(|n| (t, n)))
    }
}

/// The global destination defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Destinations {
    /// Default image uploader.
    pub image: Option<String>,
    /// Default text uploader.
    pub text: Option<String>,
    /// Default file uploader.
    pub file: Option<String>,
    /// Default video uploader; falls back to `file` (ShareX sends video through its file
    /// uploader) so that most users never need to set it.
    pub video: Option<String>,
    /// Default URL shortener.
    pub url_shortener: Option<String>,
    /// Default URL sharing service.
    pub url_sharing: Option<String>,
    /// File-extension overrides, e.g. `zip = "my-s3"`, applied to *uploads* of files with
    /// that (lower-case, dot-less) extension regardless of type. Beats the global default,
    /// loses against a workflow override.
    pub extension_overrides: BTreeMap<String, String>,
}

impl Destinations {
    /// The configured default for `ty`, without any fallback.
    pub fn default_for(&self, ty: DestinationType) -> Option<&str> {
        match ty {
            DestinationType::Image => self.image.as_deref(),
            DestinationType::Text => self.text.as_deref(),
            DestinationType::File => self.file.as_deref(),
            DestinationType::Video => self.video.as_deref(),
            DestinationType::UrlShortener => self.url_shortener.as_deref(),
            DestinationType::UrlSharing => self.url_sharing.as_deref(),
        }
    }

    /// Resolves the uploader name for content of type `ty`.
    ///
    /// Precedence: workflow override for `ty` → extension override (upload types only) →
    /// global default for `ty` → for [`DestinationType::Video`], the file destination
    /// (workflow override first, then global). Empty names count as unset.
    pub fn resolve<'a>(
        &'a self,
        ty: DestinationType,
        workflow: &'a DestinationOverride,
        extension: Option<&str>,
    ) -> Option<&'a str> {
        let non_empty = |s: Option<&'a str>| s.map(str::trim).filter(|s| !s.is_empty());
        if let Some(n) = non_empty(workflow.get(ty)) {
            return Some(n);
        }
        let is_upload = matches!(
            ty,
            DestinationType::Image
                | DestinationType::Text
                | DestinationType::File
                | DestinationType::Video
        );
        if is_upload {
            if let Some(ext) = extension {
                let key = ext.trim_start_matches('.').to_ascii_lowercase();
                if let Some(n) = non_empty(self.extension_overrides.get(&key).map(String::as_str)) {
                    return Some(n);
                }
            }
        }
        if let Some(n) = non_empty(self.default_for(ty)) {
            return Some(n);
        }
        if ty == DestinationType::Video {
            return non_empty(workflow.file.as_deref()).or_else(|| non_empty(self.file.as_deref()));
        }
        None
    }

    /// All uploader names referenced by the defaults and extension overrides.
    pub fn referenced_names(&self) -> impl Iterator<Item = &str> {
        DestinationType::ALL
            .into_iter()
            .filter_map(|t| self.default_for(t))
            .chain(self.extension_overrides.values().map(String::as_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dests() -> Destinations {
        Destinations {
            image: Some("imgur".into()),
            file: Some("s3".into()),
            extension_overrides: BTreeMap::from([("zip".to_owned(), "ftp".to_owned())]),
            ..Destinations::default()
        }
    }

    #[test]
    fn global_default() {
        let d = dests();
        let none = DestinationOverride::default();
        assert_eq!(d.resolve(DestinationType::Image, &none, None), Some("imgur"));
        assert_eq!(d.resolve(DestinationType::Text, &none, None), None);
    }

    #[test]
    fn workflow_override_wins() {
        let d = dests();
        let o = DestinationOverride { image: Some("custom".into()), ..Default::default() };
        assert_eq!(d.resolve(DestinationType::Image, &o, None), Some("custom"));
        // and beats extension override
        let o = DestinationOverride { file: Some("mine".into()), ..Default::default() };
        assert_eq!(d.resolve(DestinationType::File, &o, Some("zip")), Some("mine"));
    }

    #[test]
    fn extension_override_beats_default() {
        let d = dests();
        let none = DestinationOverride::default();
        assert_eq!(d.resolve(DestinationType::File, &none, Some("zip")), Some("ftp"));
        assert_eq!(d.resolve(DestinationType::File, &none, Some(".ZIP")), Some("ftp"));
        assert_eq!(d.resolve(DestinationType::File, &none, Some("txt")), Some("s3"));
        assert_eq!(d.resolve(DestinationType::UrlShortener, &none, Some("zip")), None);
    }

    #[test]
    fn video_falls_back_to_file() {
        let d = dests();
        let none = DestinationOverride::default();
        assert_eq!(d.resolve(DestinationType::Video, &none, Some("mp4")), Some("s3"));
        let d2 = Destinations { video: Some("youtube".into()), ..dests() };
        assert_eq!(d2.resolve(DestinationType::Video, &none, Some("mp4")), Some("youtube"));
        let o = DestinationOverride { file: Some("wf-file".into()), ..Default::default() };
        assert_eq!(d.resolve(DestinationType::Video, &o, None), Some("wf-file"));
        let o = DestinationOverride { video: Some("wf-video".into()), ..Default::default() };
        assert_eq!(d2.resolve(DestinationType::Video, &o, None), Some("wf-video"));
    }

    #[test]
    fn empty_names_are_unset() {
        let d = Destinations { image: Some("  ".into()), ..Destinations::default() };
        let o = DestinationOverride { image: Some(String::new()), ..Default::default() };
        assert_eq!(d.resolve(DestinationType::Image, &o, None), None);
    }

    #[test]
    fn referenced_names_cover_everything() {
        let d = dests();
        let names: Vec<_> = d.referenced_names().collect();
        assert!(names.contains(&"imgur") && names.contains(&"s3") && names.contains(&"ftp"));
    }

    #[test]
    fn override_entries() {
        let o = DestinationOverride {
            video: Some("v".into()),
            url_sharing: Some("u".into()),
            ..Default::default()
        };
        let e: Vec<_> = o.entries().collect();
        assert_eq!(e, vec![(DestinationType::Video, "v"), (DestinationType::UrlSharing, "u")]);
    }
}
