//! Helpers shared by the integration tests.

#![allow(dead_code)] // each test binary uses a different subset

use std::{path::Path, sync::Arc, time::Duration};

use ssx_record::{
    convert::{ConvertConfig, Converter, GpuMode},
    encode::{
        self, AudioParams, AudioSettings, Container, EncodeSummary, GifSettings, HwPolicy,
        OutputSpec, Selection, SelectionRequest, VideoParams, VideoSettings, ffmpeg::FfmpegProber,
    },
    source::{
        FrameSource, SourceEvent,
        synthetic::{SyntheticConfig, SyntheticSource, TimeMode},
    },
    time::{Clock, Fps},
};
use ssx_types::Size;

/// Software-only settings (deterministic on any machine).
pub fn sw_settings() -> VideoSettings {
    VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() }
}

/// Selects an encoder with the real FFmpeg prober.
pub fn select(container: Container, settings: &VideoSettings) -> Selection {
    encode::select(&SelectionRequest::from_settings(container, settings), &FfmpegProber)
        .expect("an encoder must be available on the test machine")
}

/// Encodes `frames` frames of the synthetic pattern (virtual time, so exact timestamps)
/// plus optionally `audio_seconds` of a 440 Hz tone that starts at `beep_at`.
pub fn encode_synthetic(
    path: &Path,
    container: Container,
    settings: VideoSettings,
    size: Size,
    fps: Fps,
    frames: u64,
    audio: Option<(f32, f32)>,
) -> (EncodeSummary, Selection) {
    let selection =
        if container == Container::Gif { dummy_selection() } else { select(container, &settings) };
    let mut src = SyntheticSource::new(
        SyntheticConfig::new(size.width, size.height, fps)
            .time(TimeMode::Virtual)
            .max_frames(frames),
    );
    src.start(Clock::start()).unwrap();
    let audio_params = audio.map(|_| AudioParams { sample_rate: 48_000, channels: 2 });
    let spec = OutputSpec {
        path: path.to_owned(),
        container,
        video: VideoParams { size, fps },
        audio: audio_params,
        video_settings: settings,
        audio_settings: AudioSettings::default(),
        gif: GifSettings::default(),
    };
    let mut enc = encode::open(&spec, Some(&selection)).expect("open encoder");
    let mut conv = Converter::new(ConvertConfig {
        out_size: size,
        kind: enc.input_kind(),
        gpu: GpuMode::Off,
        tonemap: ssx_hdr::TonemapSettings::default(),
    });
    let mut last = Duration::ZERO;
    while let SourceEvent::Frame(f) = src.next_frame(Duration::from_secs(1)).unwrap() {
        let input = Arc::new(conv.convert(&f.frame).unwrap());
        let slot = fps.slot_for(f.timestamp);
        enc.write_video(slot, &input).unwrap();
        last = fps.slot_time(slot + 1);
    }
    if let Some((seconds, beep_at)) = audio {
        let rate = 48_000usize;
        let total = (seconds * rate as f32) as usize;
        let mut chunk = Vec::new();
        for i in 0..total {
            let t = i as f32 / rate as f32;
            let s =
                if t >= beep_at { (t * 440.0 * std::f32::consts::TAU).sin() * 0.5 } else { 0.0 };
            chunk.push(s);
            chunk.push(s);
            if chunk.len() >= 2048 {
                enc.write_audio(&chunk).unwrap();
                chunk.clear();
            }
        }
        enc.write_audio(&chunk).unwrap();
    }
    let summary = enc.finish(last).expect("finish");
    (summary, selection)
}

fn dummy_selection() -> Selection {
    use encode::select::{Candidate, EncoderKind};
    let c = Candidate {
        codec: encode::Codec::H264,
        name: "libx264",
        kind: EncoderKind::Software,
        gpl: true,
    };
    Selection { chosen: c, tried: vec![], remaining: vec![], codec_fallback: false }
}
