//! The synthetic test source: moving colour bars with a machine-readable frame counter.
//!
//! Every encoder and session test records this source and then decodes the result. The
//! picture is designed so a decoded frame can be *checked*, not just eyeballed:
//!
//! * top 60 %: eight colour bars that scroll 4 px per frame (motion, and exact colours to
//!   compare within codec tolerance);
//! * next 20 %: the frame number in decimal (human readable, 5x7 font);
//! * bottom 20 %: the frame number as 16 large black/white blocks, MSB first. Blocks are
//!   big enough to survive any lossy codec, so [`read_counter`] recovers the exact index
//!   from a decoded frame (this is how tests assert drop/dup accounting and A/V sync).
//!
//! Two time modes: [`TimeMode::Realtime`] paces against the session clock like a real
//! screen; [`TimeMode::Virtual`] stamps frame `n` with exactly `n / fps` and never
//! sleeps, which makes encoder tests fast and deterministic.

use std::{ops::Range, time::Duration};

use half::f16;
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

use super::{FrameSource, SourceEvent, SourceInfo, VideoFrame};
use crate::{
    error::SourceError,
    time::{Clock, Fps},
};

/// How the synthetic source treats time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeMode {
    /// Frames are produced at real time on the session clock.
    #[default]
    Realtime,
    /// Frame `n` is stamped `n / fps`; no sleeping. For deterministic encoder tests.
    Virtual,
}

/// Configuration of a [`SyntheticSource`].
#[derive(Debug, Clone)]
pub struct SyntheticConfig {
    /// Frame size.
    pub size: Size,
    /// Frame rate of the pattern.
    pub fps: Fps,
    /// Pixel format: `Bgra8`, `Rgba8` or `Rgba16F` (HDR scRGB, to exercise tone mapping).
    pub format: PixelFormat,
    /// Time mode.
    pub time: TimeMode,
    /// Stop after this many frames (`None`: endless).
    pub max_frames: Option<u64>,
    /// Frame indices that are *not* delivered while time keeps advancing: simulates an
    /// idle screen of a damage-driven source.
    pub skip: Option<Range<u64>>,
    /// SDR white level for `Rgba16F` output.
    pub sdr_white_nits: f32,
}

impl SyntheticConfig {
    /// A realtime `Bgra8` source.
    pub fn new(width: u32, height: u32, fps: Fps) -> Self {
        Self {
            size: Size::new(width, height),
            fps,
            format: PixelFormat::Bgra8,
            time: TimeMode::Realtime,
            max_frames: None,
            skip: None,
            sdr_white_nits: 203.0,
        }
    }

    /// Sets the time mode.
    pub fn time(mut self, time: TimeMode) -> Self {
        self.time = time;
        self
    }

    /// Stops after `n` frames.
    pub fn max_frames(mut self, n: u64) -> Self {
        self.max_frames = Some(n);
        self
    }

    /// Sets the pixel format.
    pub fn format(mut self, format: PixelFormat) -> Self {
        self.format = format;
        self
    }

    /// Does not deliver frames with index in `range` (idle screen).
    pub fn skip(mut self, range: Range<u64>) -> Self {
        self.skip = Some(range);
        self
    }
}

/// The eight bar colours (sRGB): white, yellow, cyan, green, magenta, red, blue, black.
pub const BAR_COLORS: [[u8; 3]; 8] = [
    [235, 235, 235],
    [235, 235, 16],
    [16, 235, 235],
    [16, 235, 16],
    [235, 16, 235],
    [235, 16, 16],
    [16, 16, 235],
    [16, 16, 16],
];

/// Pixels the bars move per frame.
pub const SCROLL_PER_FRAME: u32 = 4;

const GLYPHS: [[u8; 7]; 10] = [
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
];

/// Geometry of the three bands for a frame of height `h`: `(bars_end, digits_end)`.
fn bands(h: u32) -> (u32, u32) {
    (h * 6 / 10, h * 8 / 10)
}

/// The colour of bar pixel column `x` in frame `n` of a `w`-wide picture.
pub fn bar_color_at(n: u64, x: u32, w: u32) -> [u8; 3] {
    let shift = (n.wrapping_mul(u64::from(SCROLL_PER_FRAME)) % u64::from(w.max(1))) as u32;
    let bar_w = (w / 8).max(1);
    let idx = (((x + shift) % w.max(1)) / bar_w).min(7);
    BAR_COLORS[idx as usize]
}

/// Renders frame `n`. HDR output scales linear light by `sdr_white_nits / 80`.
pub fn render(cfg: &SyntheticConfig, n: u64) -> Frame {
    let (w, h) = (cfg.size.width, cfg.size.height);
    let bpp = cfg.format.bytes_per_pixel();
    let stride = w as usize * bpp;
    let mut data = vec![0u8; stride * h as usize];
    let (bars_end, digits_end) = bands(h);
    let hdr_scale = cfg.sdr_white_nits / 80.0;
    let lut: Vec<[u8; 2]> = (0..=255u8)
        .map(|v| {
            let lin = srgb_to_linear(f32::from(v) / 255.0) * hdr_scale;
            f16::from_f32(lin).to_le_bytes()
        })
        .collect();
    let one = f16::from_f32(1.0).to_le_bytes();

    let put = |row: &mut [u8], x: u32, rgb: [u8; 3]| {
        let o = x as usize * bpp;
        match cfg.format {
            PixelFormat::Rgba8 => row[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]),
            PixelFormat::Bgra8 => row[o..o + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]),
            PixelFormat::Rgba16F => {
                row[o..o + 2].copy_from_slice(&lut[rgb[0] as usize]);
                row[o + 2..o + 4].copy_from_slice(&lut[rgb[1] as usize]);
                row[o + 4..o + 6].copy_from_slice(&lut[rgb[2] as usize]);
                row[o + 6..o + 8].copy_from_slice(&one);
            }
        }
    };

    // Bars: one row, copied.
    if bars_end > 0 {
        let (first, rest) = data.split_at_mut(stride);
        for x in 0..w {
            put(&mut first[..stride], x, bar_color_at(n, x, w));
        }
        for y in 1..bars_end as usize {
            rest[(y - 1) * stride..y * stride].copy_from_slice(&first[..stride]);
        }
    }

    // Digits band: dark background with the decimal frame number.
    let digits: Vec<u8> = n.to_string().bytes().map(|b| b - b'0').collect();
    let band_h = digits_end.saturating_sub(bars_end);
    let scale = (band_h / 9).max(1);
    let glyph_w = 6 * scale;
    for y in bars_end..digits_end {
        let row = &mut data[y as usize * stride..(y as usize + 1) * stride];
        let gy = (y - bars_end).saturating_sub(scale) / scale;
        for x in 0..w {
            let mut rgb = [32, 32, 32];
            let tx = x.saturating_sub(4);
            if x >= 4 && gy < 7 && (y - bars_end) >= scale {
                let ci = (tx / glyph_w) as usize;
                let cx = (tx % glyph_w) / scale;
                if let Some(&d) = digits.get(ci)
                    && cx < 5
                    && (GLYPHS[d as usize][gy as usize] >> (4 - cx)) & 1 == 1
                {
                    rgb = [235, 235, 235];
                }
            }
            put(row, x, rgb);
        }
    }

    // Bits band: one row, copied.
    if digits_end < h {
        let y0 = digits_end as usize;
        for x in 0..w {
            let bit_idx = ((u64::from(x) * 16) / u64::from(w.max(1))) as u32;
            let bit = (n >> (15 - bit_idx.min(15))) & 1;
            let v = if bit == 1 { 255 } else { 0 };
            put(&mut data[y0 * stride..(y0 + 1) * stride], x, [v, v, v]);
        }
        for y in y0 + 1..h as usize {
            data.copy_within(y0 * stride..(y0 + 1) * stride, y * stride);
        }
    }

    let space = if cfg.format.is_float() { ColorSpace::ScRgbLinear } else { ColorSpace::Srgb };
    // The buffer is exactly `stride * h` bytes by construction.
    let mut frame = Frame::from_raw(cfg.size, stride, cfg.format, space, data)
        .unwrap_or_else(|_| Frame::new(cfg.size, cfg.format, space));
    if cfg.format.is_float() {
        frame.sdr_white_nits = Some(cfg.sdr_white_nits);
    }
    frame
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// Reads the 16-bit counter out of a decoded picture by sampling the centre of each bit
/// block of the bottom band. `luma(x, y)` returns 0-255 brightness; the threshold is the
/// mid-point so any codec that keeps black and white apart works.
pub fn read_counter(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> u16 {
    let (_, digits_end) = bands(height);
    let y = digits_end + (height - digits_end) / 2;
    let mut v = 0u16;
    for i in 0..16u32 {
        let x = (i * 2 + 1) * width / 32;
        v = (v << 1) | u16::from(luma(x.min(width - 1), y.min(height - 1)) > 128);
    }
    v
}

/// The synthetic [`FrameSource`]. See the module docs.
#[derive(Debug)]
pub struct SyntheticSource {
    cfg: SyntheticConfig,
    clock: Option<Clock>,
    origin: Duration,
    next: u64,
    started: bool,
}

impl SyntheticSource {
    /// Creates the source; nothing runs until `start`.
    pub fn new(cfg: SyntheticConfig) -> Self {
        Self { cfg, clock: None, origin: Duration::ZERO, next: 0, started: false }
    }

    fn build(&self, index: u64, ts: Duration) -> VideoFrame {
        let mut frame = render(&self.cfg, index);
        frame.timestamp = Some(ts);
        VideoFrame { frame, timestamp: ts }
    }
}

impl FrameSource for SyntheticSource {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn start(&mut self, clock: Clock) -> Result<(), SourceError> {
        if self.cfg.size.is_empty() {
            return Err(SourceError::Unsupported("synthetic source needs a non-empty size".into()));
        }
        if !matches!(
            self.cfg.format,
            PixelFormat::Bgra8 | PixelFormat::Rgba8 | PixelFormat::Rgba16F
        ) {
            return Err(SourceError::Unsupported("unknown synthetic pixel format".into()));
        }
        self.origin = clock.now();
        self.clock = Some(clock);
        self.next = 0;
        self.started = true;
        Ok(())
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            name: "synthetic",
            size: self.cfg.size,
            format: self.cfg.format,
            color_space: if self.cfg.format.is_float() {
                ColorSpace::ScRgbLinear
            } else {
                ColorSpace::Srgb
            },
            hdr: self.cfg.format.is_float(),
            damage_driven: self.cfg.skip.is_some(),
            realtime: self.cfg.time == TimeMode::Realtime,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<SourceEvent, SourceError> {
        let Some(clock) = self.clock else {
            return Err(SourceError::backend("synthetic", "next_frame before start"));
        };
        if !self.started {
            return Ok(SourceEvent::Ended);
        }
        // Skip the indices of the idle window without delivering them.
        while self.cfg.skip.as_ref().is_some_and(|r| r.contains(&self.next)) {
            if self.cfg.time == TimeMode::Virtual {
                self.next += 1;
                continue;
            }
            // Realtime: nothing to deliver until the window is over, so wait it out.
            let resume_at =
                self.origin + self.cfg.fps.slot_time(self.cfg.skip.as_ref().map_or(0, |r| r.end));
            let deadline = clock.now() + timeout;
            if resume_at <= deadline {
                clock.sleep_until(resume_at);
                self.next = self.cfg.skip.as_ref().map_or(self.next, |r| r.end);
            } else {
                clock.sleep_until(deadline);
                return Ok(SourceEvent::Timeout);
            }
        }
        if self.cfg.max_frames.is_some_and(|m| self.next >= m) {
            return Ok(SourceEvent::Ended);
        }
        match self.cfg.time {
            TimeMode::Virtual => {
                let index = self.next;
                self.next += 1;
                Ok(SourceEvent::Frame(self.build(index, self.cfg.fps.slot_time(index))))
            }
            TimeMode::Realtime => {
                let due = self.origin + self.cfg.fps.slot_time(self.next);
                let deadline = clock.now() + timeout;
                if due > deadline {
                    clock.sleep_until(deadline);
                    return Ok(SourceEvent::Timeout);
                }
                clock.sleep_until(due);
                let now = clock.now();
                // A slow consumer skips ahead like a real screen would: the burnt-in
                // counter always equals the timeline slot at capture time.
                let index = self.next.max(self.cfg.fps.slot_for(now.saturating_sub(self.origin)));
                self.next = index + 1;
                Ok(SourceEvent::Frame(self.build(index, now)))
            }
        }
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma_of(frame: &Frame) -> impl Fn(u32, u32) -> u8 + '_ {
        move |x, y| {
            let row = frame.row(y);
            let px = &row[x as usize * 4..x as usize * 4 + 4];
            match frame.format() {
                PixelFormat::Rgba8 => px[0],
                _ => px[2],
            }
        }
    }

    #[test]
    fn counter_round_trips_for_many_sizes() {
        for (w, h) in [(64, 64), (320, 240), (641, 361), (1920, 1080)] {
            let cfg = SyntheticConfig::new(w, h, Fps::FPS_30);
            for n in [0u64, 1, 2, 255, 256, 4095, 12345, 65535] {
                let f = render(&cfg, n);
                assert_eq!(read_counter(w, h, luma_of(&f)), n as u16, "{w}x{h} n={n}");
            }
        }
    }

    #[test]
    fn bars_scroll_between_frames() {
        let cfg = SyntheticConfig::new(320, 240, Fps::FPS_30);
        let a = render(&cfg, 0);
        let b = render(&cfg, 1);
        assert_ne!(a.row(10), b.row(10));
        // Pixel (x + 4) of frame 0 is pixel x of frame 1... i.e. the pattern moved left.
        assert_eq!(&a.row(10)[16..20], &b.row(10)[12..16]);
    }

    #[test]
    fn formats_have_matching_layout() {
        let base = SyntheticConfig::new(64, 48, Fps::FPS_30);
        let bgra = render(&base.clone().format(PixelFormat::Bgra8), 3);
        let rgba = render(&base.clone().format(PixelFormat::Rgba8), 3);
        assert_eq!(bgra.row(0)[0], rgba.row(0)[2]);
        assert_eq!(bgra.row(0)[2], rgba.row(0)[0]);
        let hdr = render(&base.format(PixelFormat::Rgba16F), 3);
        assert_eq!(hdr.color_space(), ColorSpace::ScRgbLinear);
        assert_eq!(hdr.sdr_white_nits, Some(203.0));
        assert_eq!(hdr.stride(), 64 * 8);
    }

    #[test]
    fn hdr_frame_tonemaps_back_to_the_sdr_pattern() {
        // SDR-representable content in an HDR frame comes back byte-exact (ssx-hdr contract).
        let sdr_cfg = SyntheticConfig::new(96, 64, Fps::FPS_30);
        let hdr_cfg = sdr_cfg.clone().format(PixelFormat::Rgba16F);
        let sdr = render(&sdr_cfg.clone().format(PixelFormat::Rgba8), 5);
        let hdr = render(&hdr_cfg, 5);
        let mapped = ssx_hdr::to_sdr8(&hdr, &ssx_hdr::TonemapSettings::default()).expect("tonemap");
        let mut worst = 0i32;
        for y in 0..64 {
            for (a, b) in sdr.row(y).iter().zip(mapped.row(y)) {
                worst = worst.max((i32::from(*a) - i32::from(*b)).abs());
            }
        }
        assert!(worst <= 1, "worst channel difference {worst}");
    }

    #[test]
    fn virtual_time_is_exact_and_ends() {
        let mut s = SyntheticSource::new(
            SyntheticConfig::new(64, 64, Fps::FPS_30).time(TimeMode::Virtual).max_frames(3),
        );
        s.start(Clock::start()).unwrap();
        let mut ts = Vec::new();
        while let SourceEvent::Frame(f) = s.next_frame(Duration::from_secs(1)).unwrap() {
            ts.push(f.timestamp);
        }
        assert_eq!(ts, vec![Duration::ZERO, Fps::FPS_30.slot_time(1), Fps::FPS_30.slot_time(2)]);
        assert!(matches!(s.next_frame(Duration::ZERO).unwrap(), SourceEvent::Ended));
    }

    #[test]
    fn realtime_paces_to_the_clock() {
        let mut s = SyntheticSource::new(SyntheticConfig::new(64, 64, Fps::FPS_60));
        let clock = Clock::start();
        s.start(clock).unwrap();
        let mut stamps = Vec::new();
        while stamps.len() < 12 {
            if let SourceEvent::Frame(f) = s.next_frame(Duration::from_millis(100)).unwrap() {
                stamps.push(f.timestamp);
            }
        }
        assert!(stamps.windows(2).all(|w| w[1] >= w[0]));
        let span = stamps[11] - stamps[0];
        // 11 intervals of 16.67 ms.
        assert!(
            span >= Duration::from_millis(170) && span < Duration::from_millis(230),
            "{span:?}"
        );
    }

    #[test]
    fn realtime_reports_timeout_when_no_frame_is_due() {
        let mut s = SyntheticSource::new(SyntheticConfig::new(64, 64, Fps::from_int(2)));
        s.start(Clock::start()).unwrap();
        // First frame is due immediately, the second only after 500 ms.
        assert!(matches!(s.next_frame(Duration::from_millis(10)).unwrap(), SourceEvent::Frame(_)));
        assert!(matches!(s.next_frame(Duration::from_millis(20)).unwrap(), SourceEvent::Timeout));
    }

    #[test]
    fn skip_window_produces_a_gap_in_frame_indices() {
        let mut s = SyntheticSource::new(
            SyntheticConfig::new(64, 64, Fps::FPS_30)
                .time(TimeMode::Virtual)
                .max_frames(10)
                .skip(3..7),
        );
        s.start(Clock::start()).unwrap();
        let mut idx = Vec::new();
        while let SourceEvent::Frame(f) = s.next_frame(Duration::from_secs(1)).unwrap() {
            idx.push((f.timestamp.as_secs_f64() * 30.0).round() as u64);
        }
        assert_eq!(idx, vec![0, 1, 2, 7, 8, 9]);
    }
}
