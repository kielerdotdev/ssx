//! One opened `FFmpeg` video encoder plus the per-encoder tuning table.
//!
//! [`VideoEncoder::open`] is shared by the runtime prober and the real encoder, so a
//! probe exercises exactly the code path a recording will use (options, pixel format,
//! hardware upload) and cannot pass where a recording would then fail.

use ff::{Dictionary, Packet, Rational, codec, encoder, format::Pixel, frame};
use ffmpeg_next as ff;

use super::hw::HwUpload;
use crate::encode::{
    InputKind, PlanarFrame, VideoParams,
    select::{Candidate, EncoderKind, HwApi},
    settings::{Codec, Quality, QualityPreset, SpeedPreset, VideoSettings},
};

/// Encoder options derived from user settings for one candidate.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Tuning {
    /// `AVOptions` passed to `avcodec_open2`.
    pub opts: Vec<(String, String)>,
    /// Target bitrate in bit/s (for rate-controlled encoders).
    pub bit_rate: Option<usize>,
    /// Keyframe interval in frames.
    pub gop: u32,
    /// Worker threads (0 = leave the encoder default).
    pub threads: usize,
    /// Use slice threading instead of frame threading (native mpeg4).
    pub slice_threading: bool,
}

impl Tuning {
    fn opt(&mut self, k: &str, v: impl ToString) {
        self.opts.push((k.to_owned(), v.to_string()));
    }

    #[cfg(test)]
    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.opts.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
}

fn preset_index(p: QualityPreset) -> usize {
    match p {
        QualityPreset::Low => 0,
        QualityPreset::Medium => 1,
        QualityPreset::High => 2,
        QualityPreset::Max => 3,
    }
}

/// Constant-quality value on each encoder's native scale for a preset.
fn crf_for(name: &str, p: QualityPreset) -> Option<u32> {
    let i = preset_index(p);
    let table: [u32; 4] = match name {
        "libx264" => [28, 23, 19, 15],
        "libx265" => [32, 28, 24, 20],
        "libvpx-vp9" => [40, 33, 27, 20],
        "libsvtav1" | "libaom-av1" => [42, 35, 28, 22],
        "h264_nvenc" | "hevc_nvenc" | "av1_nvenc" | "h264_amf" | "hevc_amf" | "av1_amf"
        | "h264_vaapi" | "hevc_vaapi" | "av1_vaapi" | "vp9_vaapi" => [30, 25, 21, 17],
        "h264_qsv" | "hevc_qsv" | "av1_qsv" | "vp9_qsv" => [32, 26, 22, 18],
        _ => return None,
    };
    Some(table[i])
}

/// Bitrate in kbit/s for encoders without a constant-quality mode: bits per pixel per
/// frame by preset, scaled by how efficient the codec is.
pub(crate) fn estimate_bitrate_kbps(codec: Codec, params: &VideoParams, p: QualityPreset) -> u32 {
    let bpp = [0.05, 0.09, 0.15, 0.30][preset_index(p)];
    let eff = match codec {
        Codec::Hevc | Codec::Vp9 => 0.75,
        Codec::Av1 => 0.6,
        Codec::Mpeg4 => 1.7,
        _ => 1.0,
    };
    let px_per_s = params.size.area() as f64 * params.fps.as_f64();
    ((px_per_s * bpp * eff / 1000.0).round() as u32).max(200)
}

/// Builds the encoder options for `cand`. Pure, so it is unit-tested without `FFmpeg`.
pub(crate) fn tuning(cand: &Candidate, s: &VideoSettings, p: &VideoParams) -> Tuning {
    let mut t = Tuning::default();
    let gop = (s.keyframe_interval.as_secs_f64() * p.fps.as_f64()).round() as u32;
    t.gop = gop.max(1);
    let threads = if s.threads == 0 {
        std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
    } else {
        s.threads
    };
    let preset = match s.quality {
        Quality::Preset(p) => p,
        _ => QualityPreset::Medium,
    };
    let crf = match s.quality {
        Quality::Crf(c) => Some(u32::from(c)),
        Quality::Preset(p) => crf_for(cand.name, p),
        Quality::BitrateKbps(_) => None,
    };
    let explicit_kbps = match s.quality {
        Quality::BitrateKbps(k) => Some(k),
        _ => None,
    };
    let use_crf = explicit_kbps.is_none() && crf.is_some();
    let kbps = explicit_kbps.unwrap_or_else(|| estimate_bitrate_kbps(cand.codec, p, preset));
    let x264_preset = match s.speed {
        SpeedPreset::Fastest => "ultrafast",
        SpeedPreset::Fast => "veryfast",
        SpeedPreset::Balanced => "medium",
        SpeedPreset::Small => "slow",
    };
    let speed_idx = match s.speed {
        SpeedPreset::Fastest => 0usize,
        SpeedPreset::Fast => 1,
        SpeedPreset::Balanced => 2,
        SpeedPreset::Small => 3,
    };

    match cand.name {
        "libx264" | "libx265" => {
            t.opt("preset", x264_preset);
            if use_crf {
                t.opt("crf", crf.unwrap_or(23));
            } else {
                t.bit_rate = Some(kbps as usize * 1000);
            }
            t.threads = threads;
            if cand.name == "libx265" {
                t.opt("x265-params", "log-level=error");
            }
        }
        "libopenh264" => {
            t.bit_rate = Some(kbps as usize * 1000);
            t.threads = threads;
        }
        "libvpx-vp9" => {
            if use_crf {
                t.opt("crf", crf.unwrap_or(33));
                t.bit_rate = Some(0);
            } else {
                t.bit_rate = Some(kbps as usize * 1000);
            }
            t.opt("deadline", if speed_idx <= 1 { "realtime" } else { "good" });
            t.opt("cpu-used", [8, 6, 4, 1][speed_idx]);
            t.opt("row-mt", 1);
            t.opt("tile-columns", 2);
            t.opt("lag-in-frames", if speed_idx <= 1 { 0 } else { 16 });
            t.threads = threads;
        }
        "libsvtav1" => {
            t.opt("preset", [10, 9, 7, 4][speed_idx]);
            if use_crf {
                t.opt("crf", crf.unwrap_or(35));
            } else {
                t.bit_rate = Some(kbps as usize * 1000);
            }
            t.threads = threads;
        }
        "libaom-av1" => {
            t.opt("cpu-used", [8, 7, 5, 2][speed_idx]);
            t.opt("row-mt", 1);
            if speed_idx <= 1 {
                t.opt("usage", "realtime");
            }
            if use_crf {
                t.opt("crf", crf.unwrap_or(35));
                t.bit_rate = Some(0);
            } else {
                t.bit_rate = Some(kbps as usize * 1000);
            }
            t.threads = threads;
        }
        "librav1e" => {
            t.bit_rate = Some(kbps as usize * 1000);
            t.opt("speed", [10, 9, 6, 3][speed_idx]);
            t.threads = threads;
        }
        "mpeg4" => {
            t.bit_rate = Some(kbps as usize * 1000);
            t.threads = threads;
            t.slice_threading = true;
        }
        "h264_nvenc" | "hevc_nvenc" | "av1_nvenc" => {
            t.opt("preset", ["p1", "p3", "p4", "p6"][speed_idx]);
            if use_crf {
                t.opt("rc", "vbr");
                t.opt("cq", crf.unwrap_or(25));
                t.bit_rate = Some(0);
            } else {
                t.opt("rc", "vbr");
                t.bit_rate = Some(kbps as usize * 1000);
            }
        }
        "h264_amf" | "hevc_amf" | "av1_amf" => {
            t.opt("quality", ["speed", "speed", "balanced", "quality"][speed_idx]);
            if use_crf {
                let q = crf.unwrap_or(25);
                t.opt("rc", "cqp");
                t.opt("qp_i", q);
                t.opt("qp_p", q);
            } else {
                t.opt("rc", "vbr_peak");
                t.bit_rate = Some(kbps as usize * 1000);
            }
        }
        "h264_qsv" | "hevc_qsv" | "av1_qsv" | "vp9_qsv" => {
            t.opt("preset", ["veryfast", "faster", "medium", "slow"][speed_idx]);
            if use_crf {
                t.opt("global_quality", crf.unwrap_or(26));
            } else {
                t.bit_rate = Some(kbps as usize * 1000);
            }
        }
        "h264_vaapi" | "hevc_vaapi" | "av1_vaapi" | "vp9_vaapi" => {
            if use_crf {
                t.opt("rc_mode", "CQP");
                t.opt("qp", crf.unwrap_or(25));
            } else {
                t.opt("rc_mode", "VBR");
                t.bit_rate = Some(kbps as usize * 1000);
            }
        }
        // Media Foundation and VideoToolbox have no portable constant-quality knob.
        "h264_mf" | "hevc_mf" => {
            t.opt("rate_control", "u_vbr");
            t.opt("scenario", "display_remoting");
            t.opt("hw_encoding", 1);
            t.bit_rate = Some(kbps as usize * 1000);
        }
        "h264_videotoolbox" | "hevc_videotoolbox" => {
            t.opt("realtime", 1);
            t.opt("allow_sw", 1);
            t.bit_rate = Some(kbps as usize * 1000);
        }
        _ => {
            t.bit_rate = Some(kbps as usize * 1000);
        }
    }
    t
}

/// Pixel format the encoder context is opened with.
fn context_pixel_format(cand: &Candidate) -> Pixel {
    match cand.kind {
        EncoderKind::Hardware(HwApi::Vaapi) => Pixel::VAAPI,
        EncoderKind::Hardware(_) => Pixel::NV12,
        _ => Pixel::YUV420P,
    }
}

fn sw_pixel_format(kind: InputKind) -> Pixel {
    match kind {
        InputKind::Nv12 => Pixel::NV12,
        _ => Pixel::YUV420P,
    }
}

/// An opened encoder with the state needed to feed it planar frames.
pub(crate) struct VideoEncoder {
    pub(crate) enc: encoder::Video,
    pub(crate) candidate: Candidate,
    pub(crate) time_base: Rational,
    pub(crate) size: (u32, u32),
    hw: Option<HwUpload>,
    input: InputKind,
}

impl VideoEncoder {
    /// Opens `cand` for `params`. `global_header` must match the muxer's requirement.
    pub(crate) fn open(
        cand: &Candidate,
        params: &VideoParams,
        settings: &VideoSettings,
        global_header: bool,
    ) -> Result<Self, String> {
        let codec = encoder::find_by_name(cand.name)
            .ok_or_else(|| "not built into this FFmpeg".to_owned())?;
        let tune = tuning(cand, settings, params);
        let mut ctx = codec::context::Context::new_with_codec(codec);
        if tune.threads > 0 {
            let kind = if tune.slice_threading {
                ff::threading::Type::Slice
            } else {
                ff::threading::Type::Frame
            };
            ctx.set_threading(ff::threading::Config { kind, count: tune.threads });
        }
        let mut video = ctx.encoder().video().map_err(|e| format!("not a video encoder: {e}"))?;
        let time_base = Rational(params.fps.den() as i32, params.fps.num() as i32);
        video.set_width(params.size.width);
        video.set_height(params.size.height);
        video.set_format(context_pixel_format(cand));
        video.set_time_base(time_base);
        video.set_frame_rate(Some(Rational(params.fps.num() as i32, params.fps.den() as i32)));
        video.set_gop(tune.gop);
        video.set_aspect_ratio(Rational(1, 1));
        if let Some(br) = tune.bit_rate {
            video.set_bit_rate(br);
        }
        // Screen content is sRGB drawn in BT.709 primaries; convert.rs encodes with the
        // BT.709 matrix, limited range, so tag exactly that (players otherwise guess).
        video.set_colorspace(ff::color::Space::BT709);
        video.set_color_range(ff::color::Range::MPEG);
        video.set_color_primaries(ff::color::Primaries::BT709);
        video.set_color_transfer_characteristic(ff::color::TransferCharacteristic::BT709);
        if global_header {
            video.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        if cand.codec == Codec::Hevc {
            // `hvc1` (not `hev1`) is what QuickTime/Safari and Windows accept in MP4.
            super::hw::set_codec_tag(&mut video, *b"hvc1");
        }
        if cand.name == "mpeg4" {
            // Keep the quantiser range sane; the default max (31) is very blocky.
            video.set_qmin(2);
            video.set_qmax(12);
        }

        let hw = if let EncoderKind::Hardware(HwApi::Vaapi) = cand.kind {
            Some(HwUpload::vaapi(&mut video, params.size.width, params.size.height)?)
        } else {
            None
        };

        let mut dict = Dictionary::new();
        for (k, v) in &tune.opts {
            dict.set(k, v);
        }
        let enc = video.open_with(dict).map_err(|e| e.to_string())?;
        let input =
            if cand.input_kind() == InputKind::Nv12 { InputKind::Nv12 } else { InputKind::Yuv420p };
        Ok(Self {
            enc,
            candidate: *cand,
            time_base,
            size: (params.size.width, params.size.height),
            hw,
            input,
        })
    }

    /// Converts a planar frame into an `AVFrame` (uploading to the GPU if needed).
    fn make_frame(&mut self, planar: &PlanarFrame, pts: i64) -> Result<frame::Video, String> {
        if planar.kind != self.input || (planar.width, planar.height) != self.size {
            return Err(format!(
                "frame is {:?} {}x{} but the encoder expects {:?} {}x{}",
                planar.kind, planar.width, planar.height, self.input, self.size.0, self.size.1
            ));
        }
        let mut f = frame::Video::new(sw_pixel_format(planar.kind), planar.width, planar.height);
        for i in 0..planar.plane_count() {
            let (off, stride, rows, row_bytes) = planar.plane_layout(i).ok_or("bad plane index")?;
            let dst_stride = f.stride(i);
            let dst = f.data_mut(i);
            for r in 0..rows {
                let src = planar
                    .data
                    .get(off + r * stride..off + r * stride + row_bytes)
                    .ok_or("frame buffer too short")?;
                let d = dst
                    .get_mut(r * dst_stride..r * dst_stride + row_bytes)
                    .ok_or("AVFrame plane too short")?;
                d.copy_from_slice(src);
            }
        }
        f.set_color_space(ff::color::Space::BT709);
        f.set_color_range(ff::color::Range::MPEG);
        f.set_pts(Some(pts));
        match &mut self.hw {
            Some(hw) => hw.upload(&f),
            None => Ok(f),
        }
    }

    /// Sends one frame (`pts` in the encoder time base = slot index).
    pub(crate) fn send(&mut self, planar: &PlanarFrame, pts: i64) -> Result<(), String> {
        let f = self.make_frame(planar, pts)?;
        self.enc.send_frame(&f).map_err(|e| e.to_string())
    }

    /// Signals end of stream.
    pub(crate) fn send_eof(&mut self) -> Result<(), String> {
        self.enc.send_eof().map_err(|e| e.to_string())
    }

    /// Receives one packet; `Ok(None)` when the encoder needs more input or is drained.
    pub(crate) fn receive(&mut self) -> Result<Option<Packet>, String> {
        let mut pkt = Packet::empty();
        match self.enc.receive_packet(&mut pkt) {
            Ok(()) => Ok(Some(pkt)),
            Err(ff::Error::Eof) => Ok(None),
            Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ssx_types::Size;

    use super::*;
    use crate::{encode::select::Platform, time::Fps};

    fn cand(name: &'static str, codec: Codec, kind: EncoderKind) -> Candidate {
        Candidate { codec, name, kind, gpl: false }
    }

    fn params() -> VideoParams {
        VideoParams { size: Size::new(1920, 1080), fps: Fps::FPS_30 }
    }

    #[test]
    fn x264_uses_preset_and_crf() {
        let t = tuning(
            &cand("libx264", Codec::H264, EncoderKind::Software),
            &VideoSettings::default(),
            &params(),
        );
        assert_eq!(t.get("preset"), Some("veryfast"));
        assert_eq!(t.get("crf"), Some("23"));
        assert_eq!(t.gop, 60, "2 s at 30 fps");
        assert!(t.bit_rate.is_none());
    }

    #[test]
    fn explicit_bitrate_replaces_crf() {
        let s = VideoSettings { quality: Quality::BitrateKbps(5000), ..Default::default() };
        let t = tuning(&cand("libx264", Codec::H264, EncoderKind::Software), &s, &params());
        assert_eq!(t.get("crf"), None);
        assert_eq!(t.bit_rate, Some(5_000_000));
    }

    #[test]
    fn quality_presets_are_monotonic() {
        for name in ["libx264", "libx265", "libvpx-vp9", "libsvtav1", "h264_nvenc", "h264_qsv"] {
            let v: Vec<u32> = [
                QualityPreset::Low,
                QualityPreset::Medium,
                QualityPreset::High,
                QualityPreset::Max,
            ]
            .iter()
            .map(|p| crf_for(name, *p).unwrap())
            .collect();
            assert!(v.windows(2).all(|w| w[0] > w[1]), "{name}: {v:?} must get lower = better");
        }
        let br: Vec<u32> = [QualityPreset::Low, QualityPreset::Medium, QualityPreset::High]
            .iter()
            .map(|p| estimate_bitrate_kbps(Codec::Mpeg4, &params(), *p))
            .collect();
        assert!(br.windows(2).all(|w| w[0] < w[1]), "{br:?}");
    }

    #[test]
    fn keyframe_interval_and_speed_map_to_options() {
        let s = VideoSettings {
            keyframe_interval: Duration::from_secs(1),
            speed: SpeedPreset::Fastest,
            ..Default::default()
        };
        let t = tuning(&cand("libx264", Codec::H264, EncoderKind::Software), &s, &params());
        assert_eq!(t.gop, 30);
        assert_eq!(t.get("preset"), Some("ultrafast"));
        let v = tuning(&cand("libvpx-vp9", Codec::Vp9, EncoderKind::Software), &s, &params());
        assert_eq!(v.get("deadline"), Some("realtime"));
        assert_eq!(v.get("cpu-used"), Some("8"));
        assert_eq!(v.bit_rate, Some(0), "vp9 constant-quality mode needs b:v 0");
        // A sub-frame interval still yields one frame per GOP.
        let s = VideoSettings { keyframe_interval: Duration::ZERO, ..Default::default() };
        assert_eq!(
            tuning(&cand("libx264", Codec::H264, EncoderKind::Software), &s, &params()).gop,
            1
        );
    }

    #[test]
    fn mpeg4_gets_a_bitrate_and_slice_threads() {
        let t = tuning(
            &cand("mpeg4", Codec::Mpeg4, EncoderKind::Native),
            &VideoSettings::default(),
            &params(),
        );
        assert!(t.slice_threading);
        assert!(t.bit_rate.unwrap() > 1_000_000);
    }

    #[test]
    fn hardware_encoders_get_their_own_rate_control() {
        let s = VideoSettings::default();
        let n = tuning(
            &cand("h264_nvenc", Codec::H264, EncoderKind::Hardware(HwApi::Nvenc)),
            &s,
            &params(),
        );
        assert_eq!((n.get("rc"), n.get("cq"), n.bit_rate), (Some("vbr"), Some("25"), Some(0)));
        let v = tuning(
            &cand("h264_vaapi", Codec::H264, EncoderKind::Hardware(HwApi::Vaapi)),
            &s,
            &params(),
        );
        assert_eq!((v.get("rc_mode"), v.get("qp")), (Some("CQP"), Some("25")));
        let a = tuning(
            &cand("h264_amf", Codec::H264, EncoderKind::Hardware(HwApi::Amf)),
            &s,
            &params(),
        );
        assert_eq!((a.get("rc"), a.get("qp_i")), (Some("cqp"), Some("25")));
        let q = tuning(
            &cand("h264_qsv", Codec::H264, EncoderKind::Hardware(HwApi::Qsv)),
            &s,
            &params(),
        );
        assert_eq!(q.get("global_quality"), Some("26"));
        let _ = Platform::current();
    }
}
