//! Encoders: turning constant-rate frames (and audio) into a file.
//!
//! The [`Encoder`] trait is what the session drives from its encoder thread. Two
//! implementations exist:
//!
//! * [`ffmpeg::FfmpegEncoder`] (feature `ffmpeg`, enabled by `system` and `static`):
//!   H.264 / HEVC / AV1 / VP9 into MP4, `WebM` or MKV, with hardware encoders first and
//!   software fallbacks. Which encoder is used is decided by [`select`] against a
//!   [`select::Prober`] that really tries to open each candidate.
//! * [`gif::GifEncoder`] (feature `gif`): high quality animated GIF through `gifski`.
//!
//! Encoders receive frames in the pixel layout they asked for ([`InputKind`]): planar
//! 4:2:0 for video codecs (converted from RGB by the pipeline's convert stage, on the GPU
//! for HDR sources or with `swscale`), RGBA for GIF.

use std::{path::PathBuf, sync::Arc, time::Duration};

use ssx_types::{Frame, Size};

use crate::{error::Result, time::Fps};

#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;
#[cfg(feature = "gif")]
pub mod gif;
pub mod select;
pub mod settings;

pub use select::{
    CachingProber, Candidate, EncoderKind, HwApi, ProbeResult, Prober, Selection, SelectionRequest,
    candidates, select,
};
pub use settings::{
    AudioCodec, AudioSettings, Codec, Container, GifSettings, HwPolicy, Mp4Mode, Quality,
    QualityPreset, SpeedPreset, VideoSettings,
};

/// Pixel layout an encoder wants to be fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// Planar Y, U, V 4:2:0 (software encoders).
    Yuv420p,
    /// Y plane plus interleaved UV (hardware encoders).
    Nv12,
    /// 8-bit sRGB RGBA/BGRA (GIF).
    Rgba8,
}

/// A 4:2:0 frame in one contiguous buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanarFrame {
    /// Layout.
    pub kind: InputKind,
    /// Visible width (even).
    pub width: u32,
    /// Visible height (even).
    pub height: u32,
    /// All planes back to back.
    pub data: Vec<u8>,
}

impl PlanarFrame {
    /// A zeroed frame. `kind` must be `Yuv420p` or `Nv12`.
    pub fn new(kind: InputKind, width: u32, height: u32) -> Self {
        let n = Self::byte_len(kind, width, height);
        Self { kind, width, height, data: vec![0; n] }
    }

    /// Total bytes of a tightly packed frame.
    pub fn byte_len(kind: InputKind, width: u32, height: u32) -> usize {
        let luma = width as usize * height as usize;
        match kind {
            InputKind::Yuv420p | InputKind::Nv12 => luma + luma / 2,
            InputKind::Rgba8 => luma * 4,
        }
    }

    /// Number of planes.
    pub fn plane_count(&self) -> usize {
        match self.kind {
            InputKind::Yuv420p => 3,
            InputKind::Nv12 => 2,
            InputKind::Rgba8 => 1,
        }
    }

    /// `(offset, stride, rows, row_bytes)` of plane `i` (tightly packed).
    pub fn plane_layout(&self, i: usize) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (self.width as usize, self.height as usize);
        let luma = w * h;
        match (self.kind, i) {
            (InputKind::Yuv420p | InputKind::Nv12, 0) => Some((0, w, h, w)),
            (InputKind::Yuv420p, 1) => Some((luma, w / 2, h / 2, w / 2)),
            (InputKind::Yuv420p, 2) => Some((luma + luma / 4, w / 2, h / 2, w / 2)),
            (InputKind::Nv12, 1) => Some((luma, w, h / 2, w)),
            _ => None,
        }
    }

    /// The bytes of plane `i`.
    pub fn plane(&self, i: usize) -> Option<&[u8]> {
        let (off, stride, rows, row_bytes) = self.plane_layout(i)?;
        self.data.get(off..off + stride * (rows.saturating_sub(1)) + row_bytes)
    }
}

/// What the convert stage hands to an encoder.
#[derive(Debug)]
pub enum EncoderInput {
    /// 4:2:0 planes.
    Planar(PlanarFrame),
    /// 8-bit sRGB RGBA or BGRA pixels.
    Rgba(Frame),
}

/// Video parameters fixed for the lifetime of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoParams {
    /// Coded size (even for 4:2:0 codecs).
    pub size: Size,
    /// Constant frame rate. Frame slot `k` has presentation time `k / fps`.
    pub fps: Fps,
}

/// Audio parameters of the mixed stream fed to the encoder (interleaved `f32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count (1 or 2).
    pub channels: u16,
}

/// Everything needed to open an output file.
#[derive(Debug, Clone)]
pub struct OutputSpec {
    /// Final path. The encoder creates (or truncates) it.
    pub path: PathBuf,
    /// Container.
    pub container: Container,
    /// Video parameters.
    pub video: VideoParams,
    /// Audio parameters; `None` for a silent recording.
    pub audio: Option<AudioParams>,
    /// Video encoder settings.
    pub video_settings: VideoSettings,
    /// Audio encoder settings.
    pub audio_settings: AudioSettings,
    /// GIF settings (used when the container is GIF).
    pub gif: GifSettings,
}

/// Result of finishing an encoder.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeSummary {
    /// The finished file.
    pub path: PathBuf,
    /// Human-readable encoder description (`libx264 (software)`).
    pub encoder: String,
    /// Video frames written.
    pub video_frames: u64,
    /// Audio sample frames (per channel) written.
    pub audio_samples: u64,
    /// File size in bytes.
    pub bytes: u64,
    /// Length of the video timeline.
    pub duration: Duration,
    /// Whether an audio track was written.
    pub has_audio: bool,
}

/// A file writer driven from one thread. See the module docs.
pub trait Encoder {
    /// Description of what is actually encoding (`h264_nvenc (hardware, nvenc)`).
    fn description(&self) -> String;

    /// The pixel layout `write_video` expects.
    fn input_kind(&self) -> InputKind;

    /// `true` if the file has an audio track that `write_audio` feeds.
    fn has_audio(&self) -> bool;

    /// Encodes the picture of timeline slot `slot` (presentation time `slot / fps`).
    /// Slots must be strictly increasing; gaps are allowed (dropped frames).
    fn write_video(&mut self, slot: u64, input: &Arc<EncoderInput>) -> Result<()>;

    /// Appends interleaved `f32` samples that continue the audio timeline; `samples`
    /// holds whole frames (`channels` values each).
    fn write_audio(&mut self, samples: &[f32]) -> Result<()>;

    /// Bytes written to the file so far (best effort; used by the size guard).
    fn bytes_written(&self) -> u64;

    /// Flushes, writes the trailer (MP4 `moov`, GIF terminator) and closes the file.
    /// `end` is the end of the recorded timeline, so a final still picture keeps its
    /// duration.
    fn finish(self: Box<Self>, end: Duration) -> Result<EncodeSummary>;

    /// Closes and **deletes** the partial file.
    fn abort(self: Box<Self>);
}

/// Opens the encoder for `spec`, using the already-selected `selection` for video
/// container formats (ignored for GIF).
pub fn open(spec: &OutputSpec, selection: Option<&Selection>) -> Result<Box<dyn Encoder>> {
    match spec.container {
        Container::Gif => {
            #[cfg(feature = "gif")]
            {
                Ok(Box::new(gif::GifEncoder::open(spec)?))
            }
            #[cfg(not(feature = "gif"))]
            {
                Err(crate::error::RecordError::Unsupported(
                    "this build has no GIF support (feature `gif`)".into(),
                ))
            }
        }
        _ => {
            #[cfg(feature = "ffmpeg")]
            {
                let selection = selection.ok_or_else(|| {
                    crate::error::RecordError::InvalidConfig(
                        "a video container needs an encoder selection".into(),
                    )
                })?;
                Ok(Box::new(ffmpeg::FfmpegEncoder::open(spec, selection)?))
            }
            #[cfg(not(feature = "ffmpeg"))]
            {
                let _ = selection;
                Err(crate::error::RecordError::NoFfmpeg)
            }
        }
    }
}
