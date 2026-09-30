//! The audio half of the `FFmpeg` muxer: AAC / Opus encoding of the mixed `f32` stream.
//!
//! The pipeline delivers interleaved `f32` at the encoder's sample rate; this module
//! only buffers to the encoder's fixed frame size (AAC wants exactly 1024 samples per
//! frame, Opus 960), converts to whatever sample format the encoder accepts, and stamps
//! `pts` from a running sample counter, which is exactly the recording timeline because
//! the mixer fills every gap with silence.

use ff::{
    ChannelLayout, Packet, Rational, codec, encoder, format,
    format::sample::{Sample, Type},
    frame,
};
use ffmpeg_next as ff;

use crate::encode::{
    AudioParams,
    settings::{AudioCodec, AudioSettings, Container},
};

/// `FFmpeg` encoder names to try, in order.
pub(crate) fn codec_names(container: Container, pref: AudioCodec) -> Vec<&'static str> {
    match (pref, container) {
        (AudioCodec::Aac, _) => vec!["aac"],
        (AudioCodec::Opus, _) => vec!["libopus", "opus"],
        (AudioCodec::Auto, Container::WebM) => vec!["libopus", "libvorbis"],
        (AudioCodec::Auto, _) => vec!["aac", "libopus"],
    }
}

/// Picks the best sample format the codec supports for `f32` input.
fn pick_format(codec: ff::Codec) -> Option<Sample> {
    let audio = codec.audio().ok()?;
    let supported: Vec<Sample> = audio.formats()?.collect();
    [
        Sample::F32(Type::Planar),
        Sample::F32(Type::Packed),
        Sample::I16(Type::Packed),
        Sample::I16(Type::Planar),
    ]
    .into_iter()
    .find(|f| supported.contains(f))
}

pub(crate) struct AudioEncoder {
    enc: encoder::Audio,
    pub(crate) stream_index: usize,
    enc_tb: Rational,
    pub(crate) out_tb: Rational,
    frame_size: usize,
    fmt: Sample,
    channels: usize,
    rate: u32,
    layout: ChannelLayout,
    pending: Vec<f32>,
    next_pts: i64,
    pub(crate) written: u64,
    pub(crate) name: &'static str,
}

impl AudioEncoder {
    /// Opens the first working audio encoder and adds its stream to `octx`.
    pub(crate) fn open(
        octx: &mut format::context::Output,
        container: Container,
        params: AudioParams,
        settings: AudioSettings,
        global_header: bool,
    ) -> Result<Self, String> {
        let mut errors = Vec::new();
        for name in codec_names(container, settings.codec) {
            match Self::open_one(octx, name, params, settings, global_header) {
                Ok(a) => return Ok(a),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Err(errors.join("; "))
    }

    fn open_one(
        octx: &mut format::context::Output,
        name: &'static str,
        params: AudioParams,
        settings: AudioSettings,
        global_header: bool,
    ) -> Result<Self, String> {
        let codec = encoder::find_by_name(name).ok_or("not built into this FFmpeg")?;
        let fmt = pick_format(codec).ok_or("no usable sample format")?;
        let layout = match params.channels {
            1 => ChannelLayout::MONO,
            2 => ChannelLayout::STEREO,
            n => return Err(format!("{n} channels are not supported")),
        };
        let ctx = codec::context::Context::new_with_codec(codec);
        let mut a = ctx.encoder().audio().map_err(|e| e.to_string())?;
        a.set_rate(params.sample_rate as i32);
        a.set_channel_layout(layout);
        a.set_format(fmt);
        a.set_bit_rate(settings.bitrate_kbps as usize * 1000);
        let enc_tb = Rational(1, params.sample_rate as i32);
        a.set_time_base(enc_tb);
        if global_header {
            a.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let opened = a.open().map_err(|e| e.to_string())?;
        let frame_size = match opened.frame_size() {
            0 => 1024,
            n => n as usize,
        };
        let mut stream = octx.add_stream(codec).map_err(|e| e.to_string())?;
        stream.set_parameters(&opened);
        stream.set_time_base(enc_tb);
        let stream_index = stream.index();
        Ok(Self {
            enc: opened,
            stream_index,
            enc_tb,
            out_tb: enc_tb,
            frame_size,
            fmt,
            channels: usize::from(params.channels),
            rate: params.sample_rate,
            layout,
            pending: Vec::new(),
            next_pts: 0,
            written: 0,
            name,
        })
    }

    /// Buffers `samples` (interleaved `f32`) and encodes every complete frame.
    pub(crate) fn write(
        &mut self,
        octx: &mut format::context::Output,
        samples: &[f32],
    ) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        let need = self.frame_size * self.channels;
        while self.pending.len() >= need {
            let chunk: Vec<f32> = self.pending.drain(..need).collect();
            self.encode_chunk(octx, &chunk)?;
        }
        Ok(())
    }

    fn encode_chunk(
        &mut self,
        octx: &mut format::context::Output,
        chunk: &[f32],
    ) -> Result<(), String> {
        let n = chunk.len() / self.channels;
        let mut f = frame::Audio::new(self.fmt, n, self.layout);
        f.set_rate(self.rate);
        fill_frame(&mut f, self.fmt, chunk, self.channels);
        f.set_pts(Some(self.next_pts));
        self.next_pts += n as i64;
        self.written += n as u64;
        self.enc.send_frame(&f).map_err(|e| e.to_string())?;
        self.drain(octx)
    }

    /// Pads the last partial frame with silence, drains the encoder.
    pub(crate) fn finish(&mut self, octx: &mut format::context::Output) -> Result<(), String> {
        if !self.pending.is_empty() {
            let need = self.frame_size * self.channels;
            let mut chunk = std::mem::take(&mut self.pending);
            let real = chunk.len() / self.channels;
            chunk.resize(need, 0.0);
            self.encode_chunk(octx, &chunk)?;
            // Only the real samples count as written audio.
            self.written = self.written.saturating_sub((self.frame_size - real) as u64);
        }
        self.enc.send_eof().map_err(|e| e.to_string())?;
        self.drain(octx)
    }

    fn drain(&mut self, octx: &mut format::context::Output) -> Result<(), String> {
        let mut pkt = Packet::empty();
        loop {
            match self.enc.receive_packet(&mut pkt) {
                Ok(()) => {
                    pkt.set_stream(self.stream_index);
                    pkt.rescale_ts(self.enc_tb, self.out_tb);
                    pkt.write_interleaved(octx).map_err(|e| e.to_string())?;
                }
                Err(ff::Error::Eof) => return Ok(()),
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {
                    return Ok(());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Copies interleaved `f32` samples into `f` in the encoder's sample format.
fn fill_frame(f: &mut frame::Audio, fmt: Sample, samples: &[f32], channels: usize) {
    let n = samples.len() / channels.max(1);
    match fmt {
        Sample::F32(Type::Planar) => {
            for c in 0..channels {
                let plane = f.plane_mut::<f32>(c);
                for (i, out) in plane.iter_mut().take(n).enumerate() {
                    *out = samples[i * channels + c];
                }
            }
        }
        Sample::F32(Type::Packed) => {
            let plane = f.plane_mut::<f32>(0);
            let len = samples.len().min(plane.len());
            plane[..len].copy_from_slice(&samples[..len]);
        }
        Sample::I16(Type::Packed) => {
            let plane = f.plane_mut::<i16>(0);
            for (out, s) in plane.iter_mut().zip(samples) {
                *out = to_i16(*s);
            }
        }
        Sample::I16(Type::Planar) => {
            for c in 0..channels {
                let plane = f.plane_mut::<i16>(c);
                for (i, out) in plane.iter_mut().take(n).enumerate() {
                    *out = to_i16(samples[i * channels + c]);
                }
            }
        }
        _ => {}
    }
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_choice_follows_container() {
        assert_eq!(codec_names(Container::Mp4, AudioCodec::Auto), ["aac", "libopus"]);
        assert_eq!(codec_names(Container::WebM, AudioCodec::Auto), ["libopus", "libvorbis"]);
        assert_eq!(codec_names(Container::Mkv, AudioCodec::Opus)[0], "libopus");
        assert_eq!(codec_names(Container::WebM, AudioCodec::Aac), ["aac"]);
    }

    #[test]
    fn i16_conversion_clamps() {
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(1.0), 32767);
        assert_eq!(to_i16(-1.0), -32767);
        assert_eq!(to_i16(5.0), 32767);
        assert_eq!(to_i16(-5.0), -32767);
    }
}
