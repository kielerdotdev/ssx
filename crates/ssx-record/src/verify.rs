//! Decode-back verification of recordings (tests, the example's `--verify`, diagnostics).
//!
//! [`inspect`] fully decodes a file with FFmpeg and reports what a *player* would see:
//! container and stream properties, every video frame's timestamp and (for the synthetic
//! test pattern) its burnt-in frame counter, the first and last picture as RGB, the audio
//! samples as `f32`, whether an MP4 has its `moov` atom in front (`faststart`), and
//! whether seeking works. Tests assert on this instead of trusting the muxer.

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use ffmpeg_next::{
    self as ff, ChannelLayout, Rational,
    codec::context::Context as CodecContext,
    format::{self, Pixel, Sample, sample::Type as SampleType},
    frame, media,
    software::{
        resampling,
        scaling::{Context as Sws, Flags},
    },
};

use crate::source::synthetic::read_counter;

/// An RGB24 picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 3` bytes, tightly packed.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// The pixel at `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// 0-255 brightness at `(x, y)`.
    pub fn luma(&self, x: u32, y: u32) -> u8 {
        let p = self.pixel(x, y);
        ((u32::from(p[0]) * 2126 + u32::from(p[1]) * 7152 + u32::from(p[2]) * 722) / 10_000) as u8
    }
}

/// What was decoded from the video stream.
#[derive(Debug, Clone)]
pub struct VideoReport {
    /// Decoder name (`h264`, `vp9`, `gif`, ...).
    pub codec: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The stream's average frame rate as reported by the container.
    pub avg_fps: f64,
    /// Presentation time of every decoded frame, in seconds, in decode order sorted by pts.
    pub pts: Vec<f64>,
    /// The synthetic pattern's counter read from every decoded frame.
    pub counters: Vec<u16>,
    /// Stream duration in seconds (container value).
    pub duration: f64,
    /// First decoded picture.
    pub first: Option<RgbImage>,
    /// Last decoded picture.
    pub last: Option<RgbImage>,
    /// Colour matrix tag of the stream.
    pub color_space: String,
}

impl VideoReport {
    /// Number of decoded frames.
    pub fn frame_count(&self) -> usize {
        self.pts.len()
    }

    /// Mean of the differences between consecutive pts (the frame period), in seconds.
    pub fn mean_frame_period(&self) -> f64 {
        if self.pts.len() < 2 {
            return 0.0;
        }
        (self.pts[self.pts.len() - 1] - self.pts[0]) / (self.pts.len() - 1) as f64
    }
}

/// What was decoded from the audio stream.
#[derive(Debug, Clone)]
pub struct AudioReport {
    /// Decoder name (`aac`, `opus`, ...).
    pub codec: String,
    /// Sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Decoded samples, interleaved `f32`.
    pub samples: Vec<f32>,
    /// Presentation time of the first decoded frame, seconds.
    pub start: f64,
    /// Stream duration in seconds (container value).
    pub duration: f64,
}

impl AudioReport {
    /// Decoded length in seconds.
    pub fn decoded_seconds(&self) -> f64 {
        self.samples.len() as f64
            / f64::from(self.channels.max(1))
            / f64::from(self.sample_rate.max(1))
    }

    /// Time in seconds (relative to the first decoded sample) at which the signal first
    /// exceeds `threshold` (linear amplitude), or `None`.
    pub fn onset(&self, threshold: f32) -> Option<f64> {
        let ch = usize::from(self.channels.max(1));
        let idx = self.samples.chunks(ch).position(|f| f.iter().any(|s| s.abs() > threshold))?;
        Some(idx as f64 / f64::from(self.sample_rate))
    }

    /// RMS amplitude in `[from, to)` seconds (relative to the first decoded sample).
    pub fn rms(&self, from: f64, to: f64) -> f32 {
        let ch = usize::from(self.channels.max(1));
        let rate = f64::from(self.sample_rate);
        let a = ((from * rate) as usize * ch).min(self.samples.len());
        let b = ((to * rate) as usize * ch).min(self.samples.len());
        if b <= a {
            return 0.0;
        }
        let sum: f64 = self.samples[a..b].iter().map(|s| f64::from(*s).powi(2)).sum();
        (sum / (b - a) as f64).sqrt() as f32
    }
}

/// Layout of the top-level MP4 boxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4Layout {
    /// Top-level box types in file order (`ftyp`, `moov`, `mdat`, ...).
    pub boxes: Vec<String>,
}

impl Mp4Layout {
    /// A `moov` atom exists at all (the file was finalised).
    pub fn has_moov(&self) -> bool {
        self.boxes.iter().any(|b| b == "moov")
    }

    /// `moov` comes before `mdat` (`faststart`).
    pub fn moov_first(&self) -> bool {
        let moov = self.boxes.iter().position(|b| b == "moov");
        let mdat = self.boxes.iter().position(|b| b == "mdat");
        matches!((moov, mdat), (Some(a), Some(b)) if a < b)
    }
}

/// Everything [`inspect`] learned about a file.
#[derive(Debug, Clone)]
pub struct MediaReport {
    /// FFmpeg demuxer name (`mov,mp4,m4a,3gp,3g2,mj2`, `matroska,webm`, `gif`).
    pub format: String,
    /// Container duration in seconds.
    pub duration: f64,
    /// Video stream report.
    pub video: Option<VideoReport>,
    /// Audio stream report.
    pub audio: Option<AudioReport>,
    /// Seeking to the middle and decoding a frame works.
    pub seekable: bool,
    /// Top-level MP4 boxes (`None` for other containers).
    pub mp4: Option<Mp4Layout>,
    /// File size in bytes.
    pub size: u64,
}

fn rational_f64(r: Rational) -> f64 {
    if r.denominator() == 0 { 0.0 } else { f64::from(r.numerator()) / f64::from(r.denominator()) }
}

/// Parses the top-level boxes of an ISO base media file.
pub fn mp4_layout(path: &Path) -> std::io::Result<Mp4Layout> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos = 0u64;
    let mut boxes = Vec::new();
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr)?;
        let size32 = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
        boxes.push(String::from_utf8_lossy(&hdr[4..8]).into_owned());
        let size = match size32 {
            0 => len - pos,
            1 => {
                let mut big = [0u8; 8];
                f.read_exact(&mut big)?;
                u64::from_be_bytes(big)
            }
            n => n,
        };
        if size < 8 {
            break;
        }
        pos += size;
    }
    Ok(Mp4Layout { boxes })
}

/// Decodes `path` completely. See the module docs.
pub fn inspect(path: &Path) -> Result<MediaReport, String> {
    crate::encode::ffmpeg::init();
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let mut ictx = format::input(&path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
    let format_name = ictx.format().name().to_owned();
    let duration = ictx.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE);

    let v_idx = ictx.streams().best(media::Type::Video).map(|s| s.index());
    let a_idx = ictx.streams().best(media::Type::Audio).map(|s| s.index());

    struct VState {
        dec: ff::decoder::Video,
        tb: Rational,
        sws: Option<Sws>,
        rep: VideoReport,
    }
    struct AState {
        dec: ff::decoder::Audio,
        tb: Rational,
        rs: Option<resampling::Context>,
        rep: AudioReport,
    }

    let mut v = match v_idx {
        Some(i) => {
            let st = ictx.stream(i).ok_or("video stream vanished")?;
            let dec = CodecContext::from_parameters(st.parameters())
                .and_then(|c| c.decoder().video())
                .map_err(|e| format!("video decoder: {e}"))?;
            let rep = VideoReport {
                codec: dec.id().name().to_owned(),
                width: dec.width(),
                height: dec.height(),
                avg_fps: rational_f64(st.avg_frame_rate()),
                pts: Vec::new(),
                counters: Vec::new(),
                duration: if st.duration() > 0 {
                    st.duration() as f64 * rational_f64(st.time_base())
                } else {
                    duration
                },
                first: None,
                last: None,
                color_space: format!("{:?}", dec.color_space()),
            };
            Some(VState { dec, tb: st.time_base(), sws: None, rep })
        }
        None => None,
    };
    let mut a = match a_idx {
        Some(i) => {
            let st = ictx.stream(i).ok_or("audio stream vanished")?;
            let dec = CodecContext::from_parameters(st.parameters())
                .and_then(|c| c.decoder().audio())
                .map_err(|e| format!("audio decoder: {e}"))?;
            let rep = AudioReport {
                codec: dec.id().name().to_owned(),
                sample_rate: dec.rate(),
                channels: dec.channels(),
                samples: Vec::new(),
                start: f64::NAN,
                duration: if st.duration() > 0 {
                    st.duration() as f64 * rational_f64(st.time_base())
                } else {
                    duration
                },
            };
            Some(AState { dec, tb: st.time_base(), rs: None, rep })
        }
        None => None,
    };

    let mut handle_video = |v: &mut VState| -> Result<(), String> {
        let mut f = frame::Video::empty();
        while v.dec.receive_frame(&mut f).is_ok() {
            let (w, h) = (f.width(), f.height());
            if v.sws.is_none() {
                let mut c = Sws::get(f.format(), w, h, Pixel::RGB24, w, h, Flags::BILINEAR)
                    .map_err(|e| e.to_string())?;
                // Every stream we write is BT.709 limited range (see the encoder tags).
                crate::convert::sws::set_matrix(&mut c, false);
                v.sws = Some(c);
            }
            let mut rgb = frame::Video::empty();
            v.sws.as_mut().ok_or("no scaler")?.run(&f, &mut rgb).map_err(|e| e.to_string())?;
            let stride = rgb.stride(0);
            let src = rgb.data(0);
            let mut data = Vec::with_capacity(w as usize * h as usize * 3);
            for y in 0..h as usize {
                data.extend_from_slice(&src[y * stride..y * stride + w as usize * 3]);
            }
            let img = RgbImage { width: w, height: h, data };
            let pts = f.pts().or(f.timestamp()).unwrap_or(0) as f64 * rational_f64(v.tb);
            v.rep.pts.push(pts);
            v.rep.counters.push(read_counter(w, h, |x, y| img.luma(x, y)));
            if v.rep.first.is_none() {
                v.rep.first = Some(img.clone());
            }
            v.rep.last = Some(img);
        }
        Ok(())
    };
    let mut handle_audio = |a: &mut AState| -> Result<(), String> {
        let mut f = frame::Audio::empty();
        while a.dec.receive_frame(&mut f).is_ok() {
            if a.rep.start.is_nan() {
                a.rep.start = f.pts().unwrap_or(0) as f64 * rational_f64(a.tb);
            }
            if a.rs.is_none() {
                let layout = if f.channel_layout().is_empty() {
                    ChannelLayout::default(i32::from(f.channels()))
                } else {
                    f.channel_layout()
                };
                a.rs = Some(
                    resampling::Context::get(
                        f.format(),
                        layout,
                        f.rate(),
                        Sample::F32(SampleType::Packed),
                        layout,
                        f.rate(),
                    )
                    .map_err(|e| e.to_string())?,
                );
                a.rep.channels = f.channels();
            }
            let mut out = frame::Audio::empty();
            let rs = a.rs.as_mut().ok_or("no resampler")?;
            if f.channel_layout().is_empty() {
                f.set_channel_layout(ChannelLayout::default(i32::from(f.channels())));
            }
            rs.run(&f, &mut out).map_err(|e| e.to_string())?;
            let n = out.samples() * usize::from(a.rep.channels);
            a.rep
                .samples
                .extend_from_slice(&out.plane::<f32>(0)[..n.min(out.plane::<f32>(0).len())]);
        }
        Ok(())
    };

    for (stream, packet) in ictx.packets() {
        let idx = stream.index();
        if Some(idx) == v_idx {
            if let Some(v) = &mut v {
                v.dec.send_packet(&packet).map_err(|e| format!("video decode: {e}"))?;
                handle_video(v)?;
            }
        } else if Some(idx) == a_idx
            && let Some(a) = &mut a
        {
            a.dec.send_packet(&packet).map_err(|e| format!("audio decode: {e}"))?;
            handle_audio(a)?;
        }
    }
    if let Some(v) = &mut v {
        v.dec.send_eof().map_err(|e| e.to_string())?;
        handle_video(v)?;
        // Sort by presentation time: B-frame streams decode out of order.
        let mut pairs: Vec<(f64, u16)> =
            v.rep.pts.iter().copied().zip(v.rep.counters.iter().copied()).collect();
        pairs.sort_by(|x, y| x.0.total_cmp(&y.0));
        v.rep.pts = pairs.iter().map(|p| p.0).collect();
        v.rep.counters = pairs.iter().map(|p| p.1).collect();
    }
    if let Some(a) = &mut a {
        a.dec.send_eof().map_err(|e| e.to_string())?;
        handle_audio(a)?;
    }

    let seekable = check_seek(path, duration);
    let mp4 = format_name.contains("mp4").then(|| mp4_layout(path).ok()).flatten();
    Ok(MediaReport {
        format: format_name,
        duration,
        video: v.map(|v| v.rep),
        audio: a.map(|a| a.rep),
        seekable,
        mp4,
        size,
    })
}

/// Seeks to the middle of the file and expects to demux a video packet afterwards.
fn check_seek(path: &Path, duration: f64) -> bool {
    let Ok(mut ictx) = format::input(&path) else { return false };
    let Some(v_idx) = ictx.streams().best(media::Type::Video).map(|s| s.index()) else {
        return false;
    };
    let target = (duration / 2.0 * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
    if ictx.seek(target, ..target).is_err() {
        return false;
    }
    ictx.packets().any(|(s, _)| s.index() == v_idx)
}
