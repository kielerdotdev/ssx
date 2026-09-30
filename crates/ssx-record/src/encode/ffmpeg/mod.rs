//! FFmpeg encoder backend (`ffmpeg-next`): H.264 / HEVC / AV1 / VP9 into MP4, WebM, MKV.
//!
//! Linking: the `system` feature links the distribution's `libav*` dynamically (development
//! default), the `static` feature compiles FFmpeg from source and links it statically
//! (see `build/README.md`). The Rust code is identical for both.
//!
//! Everything happens on one thread (the session's encoder thread): FFmpeg contexts are
//! not `Send`, so the encoder is *created* on that thread too.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Once},
    time::Duration,
};

use ff::{Dictionary, Packet, Rational, format};
use ffmpeg_next as ff;

mod audio;
mod hw;
mod video;

use audio::AudioEncoder;
use video::VideoEncoder;

use super::{
    EncodeSummary, Encoder, EncoderInput, InputKind, OutputSpec, PlanarFrame, VideoParams,
    select::{Candidate, ProbeResult, Prober, Selection},
    settings::{Container, Mp4Mode, SpeedPreset, VideoSettings},
};
use crate::{
    error::{RecordError, Result},
    time::Fps,
};

static INIT: Once = Once::new();

/// Initialises FFmpeg once per process (log level, network off). FFmpeg's log output is
/// limited to errors so probing hardware encoders does not spam the console.
pub fn init() {
    INIT.call_once(|| {
        if ff::init().is_err() {
            tracing::warn!("ffmpeg initialisation reported an error");
        }
        ff::log::set_level(ff::log::Level::Error);
    });
}

/// Names of the video encoders compiled into the linked FFmpeg that ssx knows about,
/// for diagnostics (`available` means "listed", not "works": use the prober for that).
pub fn listed_encoders() -> Vec<&'static str> {
    init();
    use super::select::{Platform, candidates};
    let mut names: Vec<&'static str> = Vec::new();
    for platform in [Platform::Windows, Platform::Linux, Platform::MacOs] {
        for codec in [
            super::Codec::H264,
            super::Codec::Hevc,
            super::Codec::Av1,
            super::Codec::Vp9,
            super::Codec::Mpeg4,
        ] {
            for c in candidates(&super::SelectionRequest {
                container: Container::Mkv,
                codec,
                hw: super::HwPolicy::PreferHardware,
                allow_gpl: true,
                allow_codec_fallback: false,
                platform,
                encoder_override: None,
            }) {
                if ff::encoder::find_by_name(c.name).is_some() && !names.contains(&c.name) {
                    names.push(c.name);
                }
            }
        }
    }
    names
}

/// Version string of the linked libavcodec, for logs and bug reports.
pub fn ffmpeg_version() -> String {
    init();
    let v = ff::codec::version();
    format!("libavcodec {}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// Opens each candidate with a small test picture and encodes a few frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct FfmpegProber;

impl Prober for FfmpegProber {
    fn probe(&self, cand: &Candidate) -> ProbeResult {
        init();
        let params = VideoParams { size: ssx_types::Size::new(640, 360), fps: Fps::FPS_30 };
        let settings = VideoSettings { speed: SpeedPreset::Fastest, ..VideoSettings::default() };
        let mut enc = match VideoEncoder::open(cand, &params, &settings, true) {
            Ok(e) => e,
            Err(why) => return ProbeResult::Unavailable(why),
        };
        let mut gray = PlanarFrame::new(cand.input_kind(), 640, 360);
        gray.data.fill(128);
        let mut packets = 0u32;
        let mut drain = |enc: &mut VideoEncoder| -> std::result::Result<(), String> {
            while enc.receive()?.is_some() {
                packets += 1;
            }
            Ok(())
        };
        for i in 0..5 {
            if let Err(e) = enc.send(&gray, i).and_then(|()| drain(&mut enc)) {
                return ProbeResult::Unavailable(e);
            }
        }
        if let Err(e) = enc.send_eof().and_then(|()| drain(&mut enc)) {
            return ProbeResult::Unavailable(e);
        }
        if packets == 0 {
            ProbeResult::Unavailable("the encoder opened but produced no packets".into())
        } else {
            ProbeResult::Usable
        }
    }
}

/// The FFmpeg-backed [`Encoder`].
pub struct FfmpegEncoder {
    octx: format::context::Output,
    video: VideoEncoder,
    video_stream: usize,
    video_out_tb: Rational,
    audio: Option<AudioEncoder>,
    path: PathBuf,
    fps: Fps,
    frames: u64,
    last_slot: Option<u64>,
    description: String,
}

impl std::fmt::Debug for FfmpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegEncoder")
            .field("path", &self.path)
            .field("encoder", &self.description)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

fn io_err(path: &Path, e: &std::io::Error) -> RecordError {
    RecordError::Io { path: path.to_owned(), source: std::io::Error::new(e.kind(), e.to_string()) }
}

impl FfmpegEncoder {
    /// Opens the output file with the selected encoder, falling back through
    /// `selection.remaining` if opening at the real resolution fails although the probe
    /// passed (hardware encoders have size limits the tiny probe frame does not hit).
    /// A missing audio encoder degrades to a silent file with a warning.
    pub fn open(spec: &OutputSpec, selection: &Selection) -> Result<Self> {
        init();
        let result = Self::open_inner(spec, selection);
        if result.is_err() {
            let _ = std::fs::remove_file(&spec.path);
        }
        result
    }

    fn open_inner(spec: &OutputSpec, selection: &Selection) -> Result<Self> {
        let muxer = spec.container.muxer().ok_or_else(|| {
            RecordError::InvalidConfig("GIF is not written through FFmpeg".into())
        })?;
        let mut octx = format::output_as(&spec.path, muxer).map_err(|e| {
            RecordError::encoder("ffmpeg", format!("cannot create {muxer} output: {e}"))
        })?;
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let mut chain = vec![selection.chosen];
        chain.extend(selection.remaining.iter().copied());
        let mut failures = Vec::new();
        let mut opened = None;
        for cand in chain {
            match VideoEncoder::open(&cand, &spec.video, &spec.video_settings, global_header) {
                Ok(v) => {
                    opened = Some(v);
                    break;
                }
                Err(e) => {
                    tracing::warn!(encoder = cand.name, error = %e, "encoder failed to open at the recording size; trying the next candidate");
                    failures.push(format!("{}: {e}", cand.name));
                }
            }
        }
        let video = opened.ok_or_else(|| RecordError::NoEncoder {
            codec: spec.video_settings.codec.name().to_owned(),
            container: spec.container.extension().to_owned(),
            tried: failures,
        })?;

        let mut ost = octx
            .add_stream(
                ff::encoder::find_by_name(video.candidate.name)
                    .ok_or_else(|| RecordError::encoder(video.candidate.name, "vanished"))?,
            )
            .map_err(|e| RecordError::encoder(video.candidate.name, e.to_string()))?;
        ost.set_parameters(&video.enc);
        ost.set_time_base(video.time_base);
        ost.set_avg_frame_rate(Rational(spec.video.fps.num() as i32, spec.video.fps.den() as i32));
        let video_stream = ost.index();

        let audio = match spec.audio {
            Some(params) => {
                match AudioEncoder::open(
                    &mut octx,
                    spec.container,
                    params,
                    &spec.audio_settings,
                    global_header,
                ) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!(error = %e, "no usable audio encoder; recording without audio");
                        None
                    }
                }
            }
            None => None,
        };

        let mut meta = Dictionary::new();
        meta.set("encoder", concat!("ssx-record ", env!("CARGO_PKG_VERSION")));
        octx.set_metadata(meta);

        let mut opts = Dictionary::new();
        if spec.container == Container::Mp4 {
            match spec.video_settings.mp4 {
                Mp4Mode::Faststart => opts.set("movflags", "+faststart"),
                Mp4Mode::Fragmented => {
                    opts.set("movflags", "+frag_keyframe+empty_moov+default_base_moof");
                }
            }
        }
        octx.write_header_with(opts).map_err(|e| {
            RecordError::encoder(video.candidate.name, format!("writing the header failed: {e}"))
        })?;

        // The muxer may have changed the stream time bases while writing the header.
        let video_out_tb = octx.stream(video_stream).map_or(video.time_base, |s| s.time_base());
        let mut audio = audio;
        if let Some(a) = &mut audio
            && let Some(s) = octx.stream(a.stream_index)
        {
            a.out_tb = s.time_base();
        }
        let description = format!(
            "{}{}",
            video.candidate.describe(),
            audio.as_ref().map_or_else(String::new, |a| format!(" + {}", a.name))
        );
        tracing::info!(encoder = %description, size = ?spec.video.size, fps = %spec.video.fps, "encoder opened");
        Ok(Self {
            octx,
            video,
            video_stream,
            video_out_tb,
            audio,
            path: spec.path.clone(),
            fps: spec.video.fps,
            frames: 0,
            last_slot: None,
            description,
        })
    }

    fn drain_video(&mut self) -> Result<()> {
        let name = self.video.candidate.name;
        loop {
            match self.video.receive().map_err(|e| RecordError::encoder(name, e))? {
                Some(mut pkt) => {
                    pkt.set_stream(self.video_stream);
                    pkt.rescale_ts(self.video.time_base, self.video_out_tb);
                    write_packet(&mut pkt, &mut self.octx, name)?;
                }
                None => return Ok(()),
            }
        }
    }
}

fn write_packet(pkt: &mut Packet, octx: &mut format::context::Output, name: &str) -> Result<()> {
    pkt.write_interleaved(octx)
        .map_err(|e| RecordError::encoder(name, format!("muxing failed: {e}")))
}

impl Encoder for FfmpegEncoder {
    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_kind(&self) -> InputKind {
        self.video.candidate.input_kind()
    }

    fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn write_video(&mut self, slot: u64, input: &Arc<EncoderInput>) -> Result<()> {
        let EncoderInput::Planar(planar) = &**input else {
            return Err(RecordError::encoder(
                self.video.candidate.name,
                "expected a planar YUV frame",
            ));
        };
        if self.last_slot.is_some_and(|l| slot <= l) {
            return Err(RecordError::encoder(
                self.video.candidate.name,
                format!("slot {slot} is not after slot {:?}", self.last_slot),
            ));
        }
        self.video
            .send(planar, i64::try_from(slot).unwrap_or(i64::MAX))
            .map_err(|e| RecordError::encoder(self.video.candidate.name, e))?;
        self.last_slot = Some(slot);
        self.frames += 1;
        self.drain_video()
    }

    fn write_audio(&mut self, samples: &[f32]) -> Result<()> {
        if let Some(a) = &mut self.audio {
            a.write(&mut self.octx, samples).map_err(|e| RecordError::encoder(a.name, e))?;
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |m| m.len())
    }

    fn finish(mut self: Box<Self>, _end: Duration) -> Result<EncodeSummary> {
        let name = self.video.candidate.name;
        self.video.send_eof().map_err(|e| RecordError::encoder(name, e))?;
        self.drain_video()?;
        if let Some(a) = &mut self.audio {
            a.finish(&mut self.octx).map_err(|e| RecordError::encoder(a.name, e))?;
        }
        self.octx
            .write_trailer()
            .map_err(|e| RecordError::encoder(name, format!("finalising the file failed: {e}")))?;
        let path = self.path.clone();
        let summary = EncodeSummary {
            path: path.clone(),
            encoder: self.description.clone(),
            video_frames: self.frames,
            audio_samples: self.audio.as_ref().map_or(0, |a| a.written),
            bytes: 0,
            duration: self.last_slot.map_or(Duration::ZERO, |s| self.fps.slot_time(s + 1)),
            has_audio: self.audio.is_some(),
        };
        // Dropping the context closes the file (flushing any buffered output).
        drop(self);
        let bytes = std::fs::metadata(&path).map_err(|e| io_err(&path, &e))?.len();
        Ok(EncodeSummary { bytes, ..summary })
    }

    fn abort(self: Box<Self>) {
        let path = self.path.clone();
        drop(self);
        let _ = std::fs::remove_file(path);
    }
}
