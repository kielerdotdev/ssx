//! Animated GIF output through `gifski` (pngquant-quality palettes per frame).
//!
//! `gifski` runs a small pipeline of its own (resize, denoise, quantise, LZW) on worker
//! threads and streams the finished file to a writer, so this encoder only has to:
//!
//! * **cap the frame rate** (GIF delays are centiseconds; 15 fps is the default cap) by
//!   skipping frames closer together than the minimum interval;
//! * **drop exact duplicate frames** (an idle screen) instead of feeding them through the
//!   quantiser: `gifski` computes each frame's delay from the *next* frame's timestamp, so
//!   skipping a duplicate simply lengthens the previous frame, exactly what is wanted;
//! * **scale down** to the configured maximum size (aspect kept), which is also the main
//!   lever against huge files;
//! * keep the **final still picture's duration**: `gifski` gives the last frame the
//!   duration of the previous gap, which is too short after an idle tail, so
//!   [`Encoder::finish`] re-adds the last picture half way to the end.
//!
//! The output size is tracked with a counting writer so the session's `max_bytes` guard
//! works for GIFs too.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use imgref::ImgVec;
use rgb::RGBA8;
use ssx_types::{Frame, PixelFormat};

use super::{EncodeSummary, Encoder, EncoderInput, InputKind, OutputSpec, settings::GifSettings};
use crate::{
    error::{RecordError, Result},
    time::Fps,
};

const NAME: &str = "gifski";

struct CountingWriter {
    inner: BufWriter<File>,
    count: Arc<AtomicU64>,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The GIF [`Encoder`].
pub struct GifEncoder {
    collector: Option<gifski::Collector>,
    writer: Option<JoinHandle<std::result::Result<(), String>>>,
    path: PathBuf,
    fps: Fps,
    settings: GifSettings,
    bytes: Arc<AtomicU64>,
    /// The last frame handed to gifski, and its presentation time.
    last: Option<(Frame, Duration)>,
    /// Most recent slot seen (even if skipped), for the recorded duration.
    last_slot: Option<u64>,
    next_index: usize,
    frames_written: u64,
}

impl std::fmt::Debug for GifEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GifEncoder")
            .field("path", &self.path)
            .field("frames", &self.frames_written)
            .finish_non_exhaustive()
    }
}

impl GifEncoder {
    /// Creates the file and starts the `gifski` writer thread.
    pub fn open(spec: &OutputSpec) -> Result<Self> {
        let s = spec.gif;
        let out = s.output_size(spec.video.size);
        let file = File::create(&spec.path)
            .map_err(|source| RecordError::Io { path: spec.path.clone(), source })?;
        let bytes = Arc::new(AtomicU64::new(0));
        let sink = CountingWriter { inner: BufWriter::new(file), count: Arc::clone(&bytes) };
        let resize = out != spec.video.size;
        let (collector, writer) = gifski::new(gifski::Settings {
            width: resize.then_some(out.width),
            height: resize.then_some(out.height),
            quality: s.quality.clamp(1, 100),
            fast: s.fast,
            repeat: if s.repeat { gifski::Repeat::Infinite } else { gifski::Repeat::Finite(0) },
        })
        .map_err(|e| RecordError::encoder(NAME, e.to_string()))?;
        let handle = std::thread::Builder::new()
            .name("ssx-gifski-writer".into())
            .spawn(move || {
                let mut progress = gifski::progress::NoProgress {};
                writer.write(sink, &mut progress).map_err(|e| e.to_string())
            })
            .map_err(|e| {
                RecordError::encoder(NAME, format!("cannot start the writer thread: {e}"))
            })?;
        Ok(Self {
            collector: Some(collector),
            writer: Some(handle),
            path: spec.path.clone(),
            fps: spec.video.fps,
            settings: s,
            bytes,
            last: None,
            last_slot: None,
            next_index: 0,
            frames_written: 0,
        })
    }

    fn min_interval(&self) -> Duration {
        let cap = f64::from(self.settings.max_fps.clamp(0.5, 100.0));
        // A frame exactly on the cap boundary must be accepted, so allow 5% slack.
        Duration::from_secs_f64(0.95 / cap)
    }

    fn add(&mut self, frame: &Frame, at: Duration) -> Result<()> {
        let Some(collector) = &self.collector else {
            return Err(RecordError::encoder(NAME, "encoder already finished"));
        };
        let img = to_rgba_img(frame)?;
        collector
            .add_frame_rgba(self.next_index, img, at.as_secs_f64())
            .map_err(|e| RecordError::encoder(NAME, format!("{e} (the GIF writer stopped)")))?;
        self.next_index += 1;
        self.frames_written += 1;
        Ok(())
    }

    fn join_writer(&mut self) -> Result<()> {
        drop(self.collector.take());
        if let Some(h) = self.writer.take() {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(RecordError::encoder(NAME, e)),
                Err(_) => return Err(RecordError::encoder(NAME, "the GIF writer thread panicked")),
            }
        }
        Ok(())
    }
}

/// Copies a `Bgra8`/`Rgba8` frame into gifski's pixel type.
fn to_rgba_img(frame: &Frame) -> Result<ImgVec<RGBA8>> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let mut px = Vec::with_capacity(w * h);
    for y in 0..frame.height() {
        let row = frame.row(y);
        match frame.format() {
            PixelFormat::Rgba8 => {
                px.extend(row.chunks_exact(4).map(|p| RGBA8::new(p[0], p[1], p[2], 255)));
            }
            PixelFormat::Bgra8 => {
                px.extend(row.chunks_exact(4).map(|p| RGBA8::new(p[2], p[1], p[0], 255)));
            }
            PixelFormat::Rgba16F => {
                return Err(RecordError::Convert("GIF needs tone-mapped 8-bit frames".into()));
            }
        }
    }
    Ok(ImgVec::new(px, w, h))
}

/// `true` if both frames have identical pixels (row by row, ignoring stride padding).
fn same_pixels(a: &Frame, b: &Frame) -> bool {
    a.size() == b.size()
        && a.format() == b.format()
        && (0..a.height()).all(|y| a.row(y) == b.row(y))
}

impl Encoder for GifEncoder {
    fn description(&self) -> String {
        format!("gifski (GIF, q{} max {} fps)", self.settings.quality, self.settings.max_fps)
    }

    fn input_kind(&self) -> InputKind {
        InputKind::Rgba8
    }

    fn has_audio(&self) -> bool {
        false
    }

    fn write_video(&mut self, slot: u64, input: &Arc<EncoderInput>) -> Result<()> {
        let EncoderInput::Rgba(frame) = &**input else {
            return Err(RecordError::encoder(NAME, "expected an RGBA frame"));
        };
        self.last_slot = Some(slot);
        let at = self.fps.slot_time(slot);
        if let Some((last, last_at)) = &self.last {
            if at.saturating_sub(*last_at) < self.min_interval() {
                return Ok(());
            }
            if same_pixels(last, frame) {
                return Ok(());
            }
        }
        // The first frame must be at t = 0 for gifski's delay maths.
        let at = if self.next_index == 0 { Duration::ZERO } else { at };
        self.add(frame, at)?;
        self.last = Some((frame.clone(), at));
        Ok(())
    }

    fn write_audio(&mut self, _samples: &[f32]) -> Result<()> {
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<EncodeSummary> {
        // Keep the duration of a final still picture (see the module docs).
        if let Some((frame, at)) = self.last.clone() {
            let tail = end.saturating_sub(at);
            if tail > self.min_interval() * 2 {
                self.add(&frame, at + tail / 2)?;
            }
        }
        if self.frames_written == 0 {
            let _ = self.join_writer();
            let _ = std::fs::remove_file(&self.path);
            return Err(RecordError::Empty);
        }
        self.join_writer()?;
        let path = self.path.clone();
        let bytes = std::fs::metadata(&path)
            .map_err(|source| RecordError::Io { path: path.clone(), source })?
            .len();
        Ok(EncodeSummary {
            path,
            encoder: self.description(),
            video_frames: self.frames_written,
            audio_samples: 0,
            bytes,
            duration: end,
            has_audio: false,
        })
    }

    fn abort(mut self: Box<Self>) {
        let _ = self.join_writer();
        let _ = std::fs::remove_file(&self.path);
    }
}
