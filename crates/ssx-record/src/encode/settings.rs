//! User-facing encoder settings: container, codec, quality, speed, keyframes.

use std::time::Duration;

use ssx_types::Size;

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// MP4 (H.264 / HEVC / AV1, AAC).
    Mp4,
    /// WebM (VP9 / AV1, Opus).
    WebM,
    /// Matroska (anything).
    Mkv,
    /// Animated GIF (no audio).
    Gif,
}

impl Container {
    /// File extension without the dot.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::WebM => "webm",
            Self::Mkv => "mkv",
            Self::Gif => "gif",
        }
    }

    /// FFmpeg muxer name (`None` for GIF, which does not use FFmpeg).
    pub const fn muxer(self) -> Option<&'static str> {
        match self {
            Self::Mp4 => Some("mp4"),
            Self::WebM => Some("webm"),
            Self::Mkv => Some("matroska"),
            Self::Gif => None,
        }
    }

    /// Container for a file extension (case-insensitive).
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" | "mov" => Some(Self::Mp4),
            "webm" => Some(Self::WebM),
            "mkv" => Some(Self::Mkv),
            "gif" => Some(Self::Gif),
            _ => None,
        }
    }

    /// Video codecs this container can hold.
    pub const fn supports(self, codec: Codec) -> bool {
        match self {
            Self::Mp4 => !matches!(codec, Codec::Auto),
            Self::WebM => matches!(codec, Codec::Vp9 | Codec::Av1),
            Self::Mkv => !matches!(codec, Codec::Auto),
            Self::Gif => false,
        }
    }

    /// The codec `Codec::Auto` resolves to.
    pub const fn default_codec(self) -> Codec {
        match self {
            Self::Mp4 | Self::Mkv | Self::Gif => Codec::H264,
            Self::WebM => Codec::Vp9,
        }
    }
}

/// Video codec family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// Pick by container (H.264 for MP4/MKV, VP9 for WebM).
    #[default]
    Auto,
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
    /// MPEG-4 part 2 (the always-available last resort of FFmpeg's native encoders).
    Mpeg4,
}

impl Codec {
    /// Display name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Mpeg4 => "mpeg4",
        }
    }

    /// Resolves `Auto` for a container.
    pub const fn resolve(self, container: Container) -> Codec {
        match self {
            Self::Auto => container.default_codec(),
            c => c,
        }
    }
}

/// Whether hardware encoders are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwPolicy {
    /// Try hardware first, fall back to software.
    #[default]
    PreferHardware,
    /// Software encoders only (predictable, no driver surprises).
    SoftwareOnly,
    /// Hardware only; fail if none works.
    HardwareOnly,
}

/// Coarse quality choice, mapped to each encoder's own scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QualityPreset {
    /// Small files.
    Low,
    /// Balanced (default).
    #[default]
    Medium,
    /// Visually transparent for screen content.
    High,
    /// Near lossless, large files.
    Max,
}

/// Rate control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// A preset, translated to CRF / CQ / QP for the chosen encoder.
    Preset(QualityPreset),
    /// An explicit constant-rate-factor in the chosen encoder's native scale
    /// (x264: 0-51, lower is better).
    Crf(u8),
    /// Target average bitrate in kbit/s.
    BitrateKbps(u32),
}

impl Default for Quality {
    fn default() -> Self {
        Self::Preset(QualityPreset::Medium)
    }
}

/// Encoder speed / efficiency trade-off. Screen recording runs in real time next to the
/// thing being recorded, so the default is a fast preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeedPreset {
    /// x264 `ultrafast`: lowest CPU, biggest files.
    Fastest,
    /// x264 `veryfast` (default).
    #[default]
    Fast,
    /// x264 `medium`.
    Balanced,
    /// x264 `slow`: best compression, for offline conversion only.
    Small,
}

/// MP4 layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mp4Mode {
    /// Classic MP4 with the `moov` atom moved to the front on finish (`faststart`):
    /// streams and seeks immediately, but a crash mid-recording leaves an unplayable file.
    #[default]
    Faststart,
    /// Fragmented MP4 (`frag_keyframe+empty_moov`): playable up to the last fragment even
    /// after a crash, slightly less compatible with old players.
    Fragmented,
}

/// Video encoder settings.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSettings {
    /// Codec family; `Auto` picks by container.
    pub codec: Codec,
    /// Rate control.
    pub quality: Quality,
    /// Speed preset for software encoders.
    pub speed: SpeedPreset,
    /// Hardware policy.
    pub hw: HwPolicy,
    /// Interval between keyframes (seek granularity). Default 2 s.
    pub keyframe_interval: Duration,
    /// Allow GPL-licensed encoders (libx264, libx265). Off leaves LGPL-only ones
    /// (libopenh264, hardware, native).
    pub allow_gpl: bool,
    /// If no encoder for `codec` works, try another codec the container can hold
    /// (H.264 to MPEG-4 to VP9...) instead of failing.
    pub allow_codec_fallback: bool,
    /// Force one FFmpeg encoder by name (skips selection; for debugging and tests).
    pub encoder_override: Option<String>,
    /// MP4 layout.
    pub mp4: Mp4Mode,
    /// Scale the output down so neither side exceeds this (aspect preserved).
    pub max_size: Option<Size>,
    /// Encoder thread count (`0` = number of CPUs).
    pub threads: usize,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: Codec::Auto,
            quality: Quality::default(),
            speed: SpeedPreset::default(),
            hw: HwPolicy::default(),
            keyframe_interval: Duration::from_secs(2),
            allow_gpl: cfg!(feature = "gpl"),
            allow_codec_fallback: true,
            encoder_override: None,
            mp4: Mp4Mode::default(),
            max_size: None,
            threads: 0,
        }
    }
}

/// Audio codec choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    /// AAC for MP4/MKV, Opus for WebM.
    #[default]
    Auto,
    /// AAC (FFmpeg's native encoder).
    Aac,
    /// Opus (`libopus`).
    Opus,
}

/// Audio encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSettings {
    /// Codec.
    pub codec: AudioCodec,
    /// Bitrate in kbit/s.
    pub bitrate_kbps: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { codec: AudioCodec::Auto, bitrate_kbps: 128 }
    }
}

/// GIF settings (`gifski`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GifSettings {
    /// Quality 1-100 (gifski's scale; 90+ is visually clean, 60 is small).
    pub quality: u8,
    /// Highest frame rate written; source frames in between are skipped. GIF delays are
    /// in centiseconds so rates above 50 fps cannot be represented anyway.
    pub max_fps: f32,
    /// Scale down so the picture is at most this wide (aspect preserved).
    pub max_width: Option<u32>,
    /// Scale down so the picture is at most this tall (aspect preserved).
    pub max_height: Option<u32>,
    /// Faster, lower quality encoding.
    pub fast: bool,
    /// Loop forever (otherwise play once).
    pub repeat: bool,
}

impl Default for GifSettings {
    fn default() -> Self {
        Self {
            quality: 90,
            max_fps: 15.0,
            max_width: Some(1280),
            max_height: None,
            fast: false,
            repeat: true,
        }
    }
}

impl GifSettings {
    /// Output size for a `src` picture, honouring `max_width`/`max_height` and keeping
    /// the aspect ratio (never upscales, never below 1x1).
    pub fn output_size(&self, src: Size) -> Size {
        let mut scale = 1.0f64;
        if let Some(w) = self.max_width
            && src.width > w
        {
            scale = scale.min(f64::from(w) / f64::from(src.width));
        }
        if let Some(h) = self.max_height
            && src.height > h
        {
            scale = scale.min(f64::from(h) / f64::from(src.height));
        }
        if scale >= 1.0 {
            return src;
        }
        Size::new(
            ((f64::from(src.width) * scale).round() as u32).max(1),
            ((f64::from(src.height) * scale).round() as u32).max(1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_codec_matrix() {
        assert!(Container::Mp4.supports(Codec::H264));
        assert!(Container::Mp4.supports(Codec::Mpeg4) && !Container::Mp4.supports(Codec::Auto));
        assert!(Container::WebM.supports(Codec::Vp9));
        assert!(!Container::WebM.supports(Codec::H264));
        assert!(Container::Mkv.supports(Codec::Vp9) && Container::Mkv.supports(Codec::H264));
        assert_eq!(Codec::Auto.resolve(Container::WebM), Codec::Vp9);
        assert_eq!(Codec::Auto.resolve(Container::Mp4), Codec::H264);
        assert_eq!(Codec::Hevc.resolve(Container::Mkv), Codec::Hevc);
    }

    #[test]
    fn container_from_extension() {
        assert_eq!(Container::from_extension("MP4"), Some(Container::Mp4));
        assert_eq!(Container::from_extension("webm"), Some(Container::WebM));
        assert_eq!(Container::from_extension("gif"), Some(Container::Gif));
        assert_eq!(Container::from_extension("avi"), None);
    }

    #[test]
    fn gif_size_keeps_aspect_and_never_upscales() {
        let g = GifSettings { max_width: Some(640), max_height: None, ..Default::default() };
        assert_eq!(g.output_size(Size::new(1920, 1080)), Size::new(640, 360));
        assert_eq!(g.output_size(Size::new(320, 200)), Size::new(320, 200));
        let both =
            GifSettings { max_width: Some(800), max_height: Some(200), ..Default::default() };
        assert_eq!(both.output_size(Size::new(1600, 800)), Size::new(400, 200));
        let tiny = GifSettings { max_width: Some(1), max_height: None, ..Default::default() };
        assert_eq!(tiny.output_size(Size::new(1000, 10)), Size::new(1, 1));
    }
}
