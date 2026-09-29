//! User-tunable behaviour of [`crate::X11Capture`].

/// How [`crate::X11Capture::capture_window`] obtains pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowCaptureMode {
    /// Use XComposite (`NameWindowPixmap`) when a compositing manager is running, so the
    /// window is captured even where other windows cover it; otherwise crop the window's
    /// rectangle out of the root window.
    #[default]
    Auto,
    /// Always crop from the root window: what is on screen, including anything overlapping
    /// the window and the window manager's decorations exactly as drawn.
    Root,
    /// Require XComposite; fail rather than fall back to the root window.
    Composite,
}

/// Options for connecting to and using an X server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11Config {
    /// Display name such as `":0"` or `"localhost:10.0"`; `None` uses `$DISPLAY`. Works
    /// unchanged for XWayland.
    pub display: Option<String>,
    /// Use MIT-SHM for `GetImage` when the server supports it (local connections only).
    /// Setting this to `false` forces the plain protocol path, which is what remote
    /// displays use anyway.
    pub use_shm: bool,
    /// Upper bound, in bytes, for one `GetImage` reply on the plain path and for the shared
    /// memory segment on the SHM path. `None` picks `min(server max request size, 4 MiB)`
    /// for plain requests and 16 MiB for SHM. Larger images are fetched in bands of rows.
    pub max_chunk_bytes: Option<usize>,
    /// See [`WindowCaptureMode`].
    pub window_capture: WindowCaptureMode,
}

impl Default for X11Config {
    fn default() -> Self {
        Self {
            display: None,
            use_shm: true,
            max_chunk_bytes: None,
            window_capture: WindowCaptureMode::Auto,
        }
    }
}

impl X11Config {
    /// Default settings for an explicit display name.
    pub fn for_display(display: impl Into<String>) -> Self {
        Self { display: Some(display.into()), ..Self::default() }
    }
}
