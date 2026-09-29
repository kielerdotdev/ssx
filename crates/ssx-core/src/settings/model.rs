//! The plain settings sections. Every struct is `#[serde(default)]`, so a file that omits
//! a key (older version, hand-written) silently gets the default for exactly that key.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ssx_types::{EncodeOptions, ImageFormat};

/// Image container used when ssx saves a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormatKind {
    /// Lossless PNG.
    Png,
    /// JPEG (`image_quality` applies).
    #[serde(alias = "jpeg")]
    Jpg,
    /// Lossless WebP.
    Webp,
}

impl ImageFormatKind {
    /// Extension without the dot.
    pub const fn extension(self) -> &'static str {
        self.to_types().extension()
    }

    /// The equivalent [`ssx_types::ImageFormat`].
    pub const fn to_types(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpg => ImageFormat::Jpeg,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

/// Names of the per-type subfolders of the save folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TypeSubfolders {
    /// Screenshots.
    pub image: String,
    /// Recordings.
    pub video: String,
    /// Text snippets.
    pub text: String,
    /// Other files.
    pub file: String,
}

impl Default for TypeSubfolders {
    fn default() -> Self {
        Self {
            image: "Screenshots".to_owned(),
            video: "Recordings".to_owned(),
            text: "Text".to_owned(),
            file: "Files".to_owned(),
        }
    }
}

/// Saving, naming and formats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Root save folder. `None` = `<Pictures>/ssx` (resolved at runtime, see
    /// [`General::resolve_save_dir`]).
    pub save_dir: Option<PathBuf>,
    /// Put files into per-type subfolders ([`TypeSubfolders`]).
    pub use_type_subfolders: bool,
    /// Names of those subfolders.
    pub subfolders: TypeSubfolders,
    /// Pattern for an extra dated sub-folder (`%y-%mo`); empty for none.
    pub folder_pattern: String,
    /// File-name pattern without extension.
    pub file_name_pattern: String,
    /// Maximum stem length in characters (grapheme clusters); 0 = only the OS limit.
    pub max_file_name_len: usize,
    /// Maximum `%t` (window title) length in characters; 0 = unlimited.
    pub max_title_len: usize,
    /// Format for screenshots.
    pub image_format: ImageFormatKind,
    /// JPEG quality, 1-100 (ignored for lossless formats).
    pub image_quality: u8,
    /// Show desktop notifications for finished workflows.
    pub show_notifications: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            save_dir: None,
            use_type_subfolders: true,
            subfolders: TypeSubfolders::default(),
            folder_pattern: "%y-%mo".to_owned(),
            file_name_pattern: "Screenshot_%y-%mo-%d_%h-%mi-%s".to_owned(),
            max_file_name_len: 100,
            max_title_len: 50,
            image_format: ImageFormatKind::Png,
            image_quality: 90,
            show_notifications: true,
        }
    }
}

impl General {
    /// The save folder: the configured one, else `<Pictures>/ssx`, else `<home>/ssx`,
    /// else `./ssx`.
    pub fn resolve_save_dir(&self) -> PathBuf {
        if let Some(d) = self.save_dir.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            return d.clone();
        }
        let user = directories::UserDirs::new();
        user.as_ref()
            .and_then(|u| u.picture_dir().map(std::path::Path::to_path_buf))
            .or_else(|| user.as_ref().map(|u| u.home_dir().to_path_buf()))
            .unwrap_or_default()
            .join("ssx")
    }

    /// Encoder options for saved screenshots.
    pub fn encode_options(&self) -> EncodeOptions {
        EncodeOptions {
            format: self.image_format.to_types(),
            jpeg_quality: self.image_quality.clamp(1, 100),
            ..EncodeOptions::default()
        }
    }
}

/// HDR to SDR tone-mapping operator (see PLAN.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TonemapOperator {
    /// Hard clip: the exact SDR look, blows highlights.
    Clip,
    /// Extended Reinhard; the default.
    ReinhardExtended,
    /// ITU-R BT.2390 EETF.
    Bt2390,
    /// ACES filmic curve fit.
    AcesFit,
}

/// Tone-mapping parameters. A plain struct on purpose: `ssx-core` must not depend on
/// `ssx-hdr`; the app converts this into the HDR crate's own configuration type.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HdrConfig {
    /// Roll-off curve.
    pub operator: TonemapOperator,
    /// Scene peak that maps to display white, as a multiple of SDR white (1.0 = SDR white,
    /// 5.0 = five times brighter, e.g. 1000 nits on a 200-nit SDR white level). `>= 1`.
    pub peak: f32,
    /// Where the highlight roll-off starts, as a fraction (0-1) of SDR white.
    pub knee: f32,
    /// Dither before quantising to 8 bit to avoid banding.
    pub dither: bool,
    /// Exposure adjustment in stops (EV), -10..=10.
    pub exposure: f32,
}

impl Default for HdrConfig {
    fn default() -> Self {
        Self {
            operator: TonemapOperator::ReinhardExtended,
            peak: 4.0,
            knee: 0.75,
            dither: true,
            exposure: 0.0,
        }
    }
}

/// Capture behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureSettings {
    /// Include the mouse cursor.
    pub show_cursor: bool,
    /// Delay before capturing, in milliseconds (max one minute).
    pub delay_ms: u32,
    /// Default HDR tone mapping.
    pub hdr: HdrConfig,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self { show_cursor: false, delay_ms: 0, hdr: HdrConfig::default() }
    }
}

/// What `post_file` does with folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderPolicy {
    /// Zip the folder (through the [`Zipper`](crate::workflow::Zipper) service) and upload
    /// the archive.
    Zip,
    /// Fail that entry with an explanatory error; other entries proceed.
    Error,
}

/// `post_file` behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostFileSettings {
    /// Send image files through the editor first (only if the workflow has `open_editor`).
    pub images_through_editor: bool,
    /// Folder handling.
    pub folders: FolderPolicy,
    /// Maximum simultaneous uploads for multi-file posts (1-16).
    pub max_parallel_uploads: u32,
}

impl Default for PostFileSettings {
    fn default() -> Self {
        Self { images_through_editor: false, folders: FolderPolicy::Zip, max_parallel_uploads: 3 }
    }
}

/// History database behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    /// Record finished workflows.
    pub enabled: bool,
    /// Keep at most this many entries (0 = unlimited).
    pub max_entries: u32,
    /// Drop entries older than this many days (0 = keep forever).
    pub max_age_days: u32,
    /// Longest edge of stored thumbnails in pixels (16-1024).
    pub thumbnail_max_edge: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self { enabled: true, max_entries: 10_000, max_age_days: 0, thumbnail_max_edge: 256 }
    }
}

/// Global hotkeys that are not tied to a workflow. Workflows carry their own hotkey in
/// [`Trigger`](super::Trigger).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Open the history window.
    pub open_history: Option<String>,
    /// Open the settings window.
    pub open_settings: Option<String>,
    /// Pause / resume a running recording.
    pub pause_recording: Option<String>,
}

impl Hotkeys {
    /// `(setting name, accelerator)` for every hotkey that is set.
    pub fn entries(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("hotkeys.open_history", self.open_history.as_deref()),
            ("hotkeys.open_settings", self.open_settings.as_deref()),
            ("hotkeys.pause_recording", self.pause_recording.as_deref()),
        ]
        .into_iter()
        .filter_map(|(n, v)| v.map(|v| (n, v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mapping() {
        assert_eq!(ImageFormatKind::Png.extension(), "png");
        assert_eq!(ImageFormatKind::Jpg.extension(), "jpg");
        assert_eq!(ImageFormatKind::Webp.extension(), "webp");
        let g =
            General { image_format: ImageFormatKind::Jpg, image_quality: 0, ..General::default() };
        let o = g.encode_options();
        assert_eq!(o.format, ImageFormat::Jpeg);
        assert_eq!(o.jpeg_quality, 1, "clamped");
    }

    #[test]
    fn save_dir_resolution() {
        let g = General { save_dir: Some(PathBuf::from("/tmp/x")), ..General::default() };
        assert_eq!(g.resolve_save_dir(), PathBuf::from("/tmp/x"));
        let g = General { save_dir: Some(PathBuf::new()), ..General::default() };
        assert!(g.resolve_save_dir().ends_with("ssx"));
        assert!(General::default().resolve_save_dir().ends_with("ssx"));
    }

    #[test]
    fn jpeg_alias_accepted() {
        let g: General = toml::from_str("image_format = \"jpeg\"").unwrap();
        assert_eq!(g.image_format, ImageFormatKind::Jpg);
    }
}
