//! Small persistent state of the editor window: geometry, last tool, per-tool styles, recent
//! colours and a handful of toggles.
//!
//! It lives in `editor-ui.json` inside the ssx config directory (`SSX_CONFIG_DIR` overrides the
//! platform default, the same convention as `ssx-core`). It is deliberately *state*, not
//! settings: nothing here is documented as hand-editable, and a broken file must never stop the
//! editor from starting, so loading falls back to defaults (moving the bad file aside) and
//! saving is atomic (write to a temporary file, then rename).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use ssx_editor::{Color, Preset, StyleMemory};

use crate::tools::ToolId;

/// Environment variable that relocates the config directory.
pub const CONFIG_DIR_ENV: &str = "SSX_CONFIG_DIR";
/// File name inside the config directory.
pub const FILE_NAME: &str = "editor-ui.json";
/// How many recent colours are remembered.
pub const MAX_RECENT_COLORS: usize = 14;

/// Window geometry in egui points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPrefs {
    /// Inner width.
    pub width: f32,
    /// Inner height.
    pub height: f32,
    /// Outer x position (absent on Wayland, where clients cannot know it).
    pub x: Option<f32>,
    /// Outer y position.
    pub y: Option<f32>,
    /// Was the window maximised?
    pub maximized: bool,
}

impl Default for WindowPrefs {
    fn default() -> Self {
        Self { width: 1280.0, height: 800.0, x: None, y: None, maximized: false }
    }
}

impl WindowPrefs {
    /// Clamps absurd values (a file from another machine, a disconnected monitor) so the window
    /// is always a sensible size and never far off-screen.
    pub fn sanitized(mut self) -> Self {
        let ok = |v: f32| v.is_finite();
        if !ok(self.width) || !ok(self.height) {
            return Self::default();
        }
        self.width = self.width.clamp(640.0, 8192.0);
        self.height = self.height.clamp(420.0, 8192.0);
        for p in [&mut self.x, &mut self.y] {
            if let Some(v) = *p {
                *p = (v.is_finite() && (-4000.0..=16000.0).contains(&v)).then_some(v);
            }
        }
        if self.x.is_none() || self.y.is_none() {
            self.x = None;
            self.y = None;
        }
        self
    }
}

/// Everything remembered between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Window geometry.
    pub window: WindowPrefs,
    /// The tool selected when the editor was last closed.
    pub last_tool: ToolId,
    /// Last used style of every tool.
    pub styles: StyleMemory,
    /// Most recently used colours, newest first.
    pub recent_colors: Vec<Color>,
    /// Object list panel visible.
    pub show_layers: bool,
    /// Draw the pixel grid when zoomed in.
    pub pixel_grid: bool,
    /// JPEG quality for the save dialog (1-100).
    pub jpeg_quality: u8,
    /// Use fast (larger) PNG compression.
    pub png_fast: bool,
    /// Folder of the last open/save dialog.
    pub last_dir: Option<PathBuf>,
    /// Variant chosen in the blur/pixelate toolbar slot.
    pub blur_variant: ToolId,
    /// Variant chosen in the highlighter toolbar slot.
    pub highlight_variant: ToolId,
    /// Remembered style of the plain text tool (the engine has one Text preset; the toolbar's
    /// "text with outline and background" button is a second identity that keeps its own).
    pub text_plain: Option<Preset>,
    /// Remembered style of the boxed text tool.
    pub text_boxed: Option<Preset>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            window: WindowPrefs::default(),
            last_tool: ToolId::Select,
            styles: StyleMemory::new(),
            recent_colors: Vec::new(),
            show_layers: false,
            pixel_grid: true,
            jpeg_quality: 90,
            png_fast: false,
            last_dir: None,
            blur_variant: ToolId::Blur,
            highlight_variant: ToolId::Highlight,
            text_plain: None,
            text_boxed: None,
        }
    }
}

/// Errors reading or writing the state file.
#[derive(Debug, thiserror::Error)]
pub enum PrefsError {
    /// Filesystem trouble.
    #[error("cannot access the editor state file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file is not valid JSON for this version.
    #[error("the editor state file {path} is unreadable ({source}); it was moved aside")]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: serde_json::Error,
    },
    /// No config directory could be determined.
    #[error("no per-user config directory; set {CONFIG_DIR_ENV} to a writable folder")]
    NoConfigDir,
}

/// The directory that holds the state file.
pub fn config_dir_with(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    match env(CONFIG_DIR_ENV) {
        Some(v) if !v.to_string_lossy().trim().is_empty() => Some(PathBuf::from(v)),
        Some(_) => None,
        None => directories::ProjectDirs::from("", "", "ssx").map(|d| d.config_dir().to_path_buf()),
    }
}

/// The state file path for the running process.
pub fn default_path() -> Option<PathBuf> {
    config_dir_with(|k| std::env::var_os(k)).map(|d| d.join(FILE_NAME))
}

impl Prefs {
    /// Loads the state from `path`. A missing file is not an error (defaults); a corrupt one is
    /// moved to `<file>.bad` and reported so the caller can log it and carry on with defaults.
    pub fn load_from(path: &Path) -> Result<Prefs, PrefsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Prefs::default()),
            Err(source) => return Err(PrefsError::Io { path: path.to_path_buf(), source }),
        };
        match serde_json::from_str::<Prefs>(&text) {
            Ok(mut p) => {
                p.sanitize();
                Ok(p)
            }
            Err(source) => {
                let bad = path.with_extension("json.bad");
                let _ = std::fs::rename(path, bad);
                Err(PrefsError::Corrupt { path: path.to_path_buf(), source })
            }
        }
    }

    /// Loads from the default location, falling back to defaults on any problem (logged).
    pub fn load() -> Prefs {
        let Some(path) = default_path() else { return Prefs::default() };
        Prefs::load_from(&path).unwrap_or_else(|e| {
            tracing::warn!("{e}");
            Prefs::default()
        })
    }

    /// Writes the state atomically to `path`, creating the directory if needed.
    pub fn save_to(&self, path: &Path) -> Result<(), PrefsError> {
        let io = |source| PrefsError::Io { path: path.to_path_buf(), source };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|source| PrefsError::Corrupt { path: path.to_path_buf(), source })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    /// Saves to the default location; failures are logged, never fatal.
    pub fn save(&self) {
        let Some(path) = default_path() else { return };
        if let Err(e) = self.save_to(&path) {
            tracing::warn!("{e}");
        }
    }

    /// Repairs values that would misbehave.
    pub fn sanitize(&mut self) {
        self.window = self.window.sanitized();
        self.jpeg_quality = self.jpeg_quality.clamp(1, 100);
        self.recent_colors.truncate(MAX_RECENT_COLORS);
        // The variants must be the tools the slots actually hold.
        if !matches!(self.blur_variant, ToolId::Blur | ToolId::Pixelate) {
            self.blur_variant = ToolId::Blur;
        }
        if !matches!(self.highlight_variant, ToolId::Highlight | ToolId::HighlightPen) {
            self.highlight_variant = ToolId::Highlight;
        }
    }

    /// Records `c` as most recently used (opaque duplicates move to the front, transparent
    /// colours are not worth remembering).
    pub fn remember_color(&mut self, c: Color) {
        if c.is_transparent() {
            return;
        }
        self.recent_colors.retain(|x| *x != c);
        self.recent_colors.insert(0, c);
        self.recent_colors.truncate(MAX_RECENT_COLORS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(FILE_NAME);
        let mut p = Prefs {
            window: WindowPrefs {
                width: 1000.0,
                height: 700.0,
                x: Some(10.0),
                y: Some(20.0),
                maximized: true,
            },
            ..Prefs::default()
        };
        p.last_tool = ToolId::TextBoxed;
        p.remember_color(Color::rgb(1, 2, 3));
        p.last_dir = Some(PathBuf::from("/tmp/x"));
        let mut preset = ssx_editor::Tool::Arrow.preset().unwrap();
        preset.style.stroke_width = 11.0;
        p.styles.remember(ssx_editor::Tool::Arrow, preset);
        p.save_to(&path).unwrap();
        let back = Prefs::load_from(&path).unwrap();
        assert_eq!(back, p);
        assert!(!path.with_extension("json.tmp").exists(), "temp file is renamed away");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = Prefs::load_from(&dir.path().join("nope.json")).unwrap();
        assert_eq!(p, Prefs::default());
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = Prefs::load_from(&path).unwrap_err();
        assert!(matches!(err, PrefsError::Corrupt { .. }));
        assert!(err.to_string().contains("moved aside"));
        assert!(!path.exists());
        assert!(dir.path().join("editor-ui.json.bad").exists());
        // The next load starts clean.
        assert_eq!(Prefs::load_from(&path).unwrap(), Prefs::default());
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, r#"{"jpeg_quality": 55, "from_the_future": [1,2,3]}"#).unwrap();
        let p = Prefs::load_from(&path).unwrap();
        assert_eq!(p.jpeg_quality, 55);
        assert_eq!(p.window, WindowPrefs::default());
    }

    #[test]
    fn insane_values_are_repaired() {
        let w =
            WindowPrefs { width: 1.0, height: 1e9, x: Some(1e9), y: Some(5.0), maximized: false }
                .sanitized();
        assert_eq!((w.width, w.height), (640.0, 8192.0));
        assert_eq!((w.x, w.y), (None, None), "a half-valid position is dropped");
        let w = WindowPrefs { width: f32::NAN, ..WindowPrefs::default() }.sanitized();
        assert_eq!(w, WindowPrefs::default());

        let mut p = Prefs { jpeg_quality: 0, blur_variant: ToolId::Rectangle, ..Prefs::default() };
        p.recent_colors = (0..40).map(|i| Color::rgb(i, 0, 0)).collect();
        p.sanitize();
        assert_eq!(p.jpeg_quality, 1);
        assert_eq!(p.blur_variant, ToolId::Blur);
        assert_eq!(p.recent_colors.len(), MAX_RECENT_COLORS);
    }

    #[test]
    fn recent_colours_are_mru_and_deduplicated() {
        let mut p = Prefs::default();
        let (a, b, c) = (Color::rgb(1, 0, 0), Color::rgb(2, 0, 0), Color::rgb(3, 0, 0));
        for x in [a, b, c, a] {
            p.remember_color(x);
        }
        assert_eq!(p.recent_colors, vec![a, c, b]);
        p.remember_color(Color::TRANSPARENT);
        assert_eq!(p.recent_colors.len(), 3);
        for i in 0..40 {
            p.remember_color(Color::rgb(0, i, 0));
        }
        assert_eq!(p.recent_colors.len(), MAX_RECENT_COLORS);
        assert_eq!(p.recent_colors[0], Color::rgb(0, 39, 0));
    }

    #[test]
    fn env_override_wins_and_empty_is_rejected() {
        let dir = config_dir_with(|k| (k == CONFIG_DIR_ENV).then(|| OsString::from("/x/y")));
        assert_eq!(dir, Some(PathBuf::from("/x/y")));
        assert_eq!(config_dir_with(|_| Some(OsString::from("  "))), None);
    }
}
