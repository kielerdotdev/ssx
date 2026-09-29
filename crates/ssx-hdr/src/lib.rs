//! CPU reference implementation of HDR → SDR conversion for Windows scRGB screenshots.
//!
//! When Windows HDR is on, capture yields `Rgba16F` / `ScRgbLinear` frames: linear light,
//! Rec.709 primaries, **1.0 = 80 nits**, highlights above 1.0, wide-gamut colours as
//! negative channels. Ordinary SDR content (UI, apps) is composited at the user's "SDR
//! content brightness" (`Frame::sdr_white_nits`, typically ~200 nits = 2.5 in scRGB), so
//! neither clipping nor a plain sRGB encode of the raw values looks right.
//! [`to_sdr8`] converts such a frame to an 8-bit sRGB [`Frame`]:
//!
//! 1. scale so SDR white is 1.0 (and apply exposure),
//! 2. luminance-preserving gamut mapping of negative channels,
//! 3. hue-preserving highlight roll-off (selectable [`TonemapOperator`]),
//! 4. piecewise sRGB OETF (IEC 61966-2-1),
//! 5. optional deterministic dither before quantisation.
//!
//! The per-pixel maths lives in [`params`] (pipeline and the exact formula of every
//! operator, with references), [`srgb`] and [`dither`]. It is written as pure `f32`
//! functions plus a uniform-buffer mirror ([`GpuParams`]) because this crate is the
//! *reference* that the WGSL shader must match.
//!
//! # Guarantees
//!
//! * **SDR content is untouched.** Every pixel whose channels are all within `[0, knee]`
//!   after scaling (default knee 1.0 = SDR white) is a plain sRGB encode of the colour,
//!   for every operator. A solid-colour UI region is byte-identical to the same region in
//!   a non-HDR screenshot, also with dither enabled (see [`dither`] for the dead-zone
//!   rule). 8-bit sRGB input is passed through unchanged.
//! * **Robust input.** `NaN → 0`, `+Inf → peak`, `-Inf → 0`; no panics on any bit pattern.
//! * **Deterministic.** Output depends only on the input pixels and settings, never on
//!   thread count or scheduling.
//!
//! # Performance
//!
//! Rows are converted in parallel with `rayon`. For pixels whose channels are all in the
//! pass-through range (i.e. nearly all desktop content) the result is looked up from a
//! 65536-entry table indexed by the raw half-float bits, built once per call from the very
//! same functions the slow path uses, so both paths agree bit for bit. Other pixels run
//! the full pipeline. `cargo test --release -- --ignored timing --nocapture` prints the
//! measured time for a 3840×2160 frame.

#![forbid(unsafe_code)]

pub mod dither;
pub mod params;
pub mod settings;
pub mod srgb;

use rayon::prelude::*;
use ssx_types::{ColorSpace, Frame, PixelFormat};

pub use dither::{DITHER_DEAD_ZONE, dither_noise, encode_channel, quantize};
pub use params::{
    GpuParams, PixelParams, ShoulderMode, gamut_map, luminance, rolloff, scale_channel,
    tonemap_rgb, tonemap_scaled,
};
pub use settings::{HdrError, TonemapOperator, TonemapSettings};
pub use srgb::{srgb_eotf, srgb_oetf};

/// Number of distinct half-float bit patterns.
const F16_VALUES: usize = 1 << 16;

/// `true` if `frame` must be tonemapped before it can be encoded as an image, i.e. it is
/// not already 8-bit sRGB.
pub fn frame_needs_tonemap(frame: &Frame) -> bool {
    !frame.is_sdr8()
}

/// Converts `frame` to tightly packed 8-bit sRGB RGBA.
///
/// * `Rgba16F` + `ScRgbLinear`: full HDR → SDR conversion using `frame.sdr_white_nits`
///   (80 nits, with a warning, when unset). Alpha is forced to 255.
/// * `Rgba8` / `Bgra8` + `Srgb`: returned as RGBA8 with no colour change (alpha kept).
///
/// `origin`, `scale_factor` and `timestamp` are preserved. `settings` are validated first.
pub fn to_sdr8(frame: &Frame, settings: &TonemapSettings) -> Result<Frame, HdrError> {
    settings.validate()?;
    match (frame.format(), frame.color_space()) {
        (PixelFormat::Rgba16F, ColorSpace::ScRgbLinear) => tonemap_frame(frame, settings),
        (PixelFormat::Rgba8 | PixelFormat::Bgra8, ColorSpace::Srgb) => {
            Ok(frame.clone().into_rgba8()?)
        }
        (format, space) => Err(HdrError::UnsupportedFrame(format, space)),
    }
}

/// Like [`to_sdr8`] but consumes the frame, avoiding a copy for 8-bit sRGB input.
pub fn into_sdr8(frame: Frame, settings: &TonemapSettings) -> Result<Frame, HdrError> {
    if frame.is_sdr8() {
        settings.validate()?;
        return Ok(frame.into_rgba8()?);
    }
    to_sdr8(&frame, settings)
}

/// The HDR path of [`to_sdr8`].
fn tonemap_frame(frame: &Frame, settings: &TonemapSettings) -> Result<Frame, HdrError> {
    let sdr_white = match frame.sdr_white_nits {
        Some(n) if n.is_finite() && n > 0.0 => n,
        Some(n) => return Err(HdrError::InvalidSdrWhite(n)),
        None => {
            tracing::warn!(
                "HDR frame has no sdr_white_nits; assuming 80 nits (SDR white = scRGB 1.0)"
            );
            80.0
        }
    };
    let params = PixelParams::new(settings, sdr_white);
    let (w, h) = (frame.width() as usize, frame.height());
    let mut out = vec![0u8; w * 4 * h as usize];
    if !out.is_empty() {
        let table = build_table(&params);
        out.par_chunks_mut(w * 4).enumerate().with_min_len(8).for_each(|(y, dst)| {
            // `y < h`, so the row exists; `h` is a u32.
            convert_row(frame.row(y as u32), dst, y as u32, &params, &table);
        });
    }
    let mut res = Frame::from_raw(frame.size(), w * 4, PixelFormat::Rgba8, ColorSpace::Srgb, out)?;
    res.origin = frame.origin;
    res.scale_factor = frame.scale_factor;
    res.timestamp = frame.timestamp;
    // The output is SDR: the white level no longer applies.
    res.sdr_white_nits = None;
    Ok(res)
}

/// Sentinel in the fast-path table: this channel value needs the full pipeline.
const NEEDS_SLOW_PATH: f32 = -1.0;

/// Per half-float-bit-pattern cache: `[scaled, encoded]`.
///
/// * `scaled` is [`scale_channel`] of the value (exposure and SDR-white scaling plus
///   non-finite sanitising), so the slow path never converts half floats itself.
/// * `encoded` is the pre-quantisation sRGB code (0..=255 scale) for channels that pass
///   through the pipeline unchanged, else [`NEEDS_SLOW_PATH`].
///
/// A pixel takes the fast path only if all three channels have an `encoded` entry: then
/// every channel is in `[0, limit]` where `limit` is the pass-through bound, so gamut
/// mapping and roll-off are the identity and the pixel result is exactly the per-channel
/// encode. Both paths use the same functions, so they agree bit for bit.
fn build_table(p: &PixelParams) -> Vec<[f32; 2]> {
    let limit = if p.mode == ShoulderMode::ChannelClip { 1.0 } else { p.knee };
    (0..=u16::MAX)
        .map(|bits| {
            let x = scale_channel(half::f16::from_bits(bits).to_f32(), p);
            let enc =
                if (0.0..=limit).contains(&x) { 255.0 * srgb_oetf(x) } else { NEEDS_SLOW_PATH };
            [x, enc]
        })
        .collect()
}

/// Pixels processed per block in [`convert_row`]; the dither noise of a block is computed
/// in one tight loop the compiler can vectorise.
const BLOCK: usize = 64;

/// Converts one row of little-endian `Rgba16F` pixels to RGBA8.
fn convert_row(src: &[u8], dst: &mut [u8], y: u32, p: &PixelParams, table: &[[f32; 2]]) {
    let Ok(table) = <&[[f32; 2]; F16_VALUES]>::try_from(table) else { return };
    for (block, (s_blk, d_blk)) in src.chunks(BLOCK * 8).zip(dst.chunks_mut(BLOCK * 4)).enumerate()
    {
        let x0 = (block * BLOCK) as u32;
        let mut noise = [0.0f32; BLOCK];
        if p.dither {
            for (i, n) in noise.iter_mut().enumerate() {
                *n = dither_noise(x0 + i as u32, y);
            }
        }
        for (i, (s, d)) in s_blk.chunks_exact(8).zip(d_blk.chunks_exact_mut(4)).enumerate() {
            let e = [
                table[usize::from(u16::from_le_bytes([s[0], s[1]]))],
                table[usize::from(u16::from_le_bytes([s[2], s[3]]))],
                table[usize::from(u16::from_le_bytes([s[4], s[5]]))],
            ];
            let n = p.dither.then_some(noise[i]);
            if e[0][1] >= 0.0 && e[1][1] >= 0.0 && e[2][1] >= 0.0 {
                d[0] = quantize(e[0][1], n);
                d[1] = quantize(e[1][1], n);
                d[2] = quantize(e[2][1], n);
            } else {
                let out = tonemap_scaled([e[0][0], e[1][0], e[2][0]], p);
                d[0] = encode_channel(out[0], n);
                d[1] = encode_channel(out[1], n);
                d[2] = encode_channel(out[2], n);
            }
            d[3] = 255;
        }
    }
}

/// Reference (slow) per-pixel conversion used by tests to validate the fast path.
#[cfg(test)]
pub(crate) fn reference_pixel(rgb: [f32; 3], p: &PixelParams, x: u32, y: u32) -> [u8; 3] {
    let noise = p.dither.then(|| dither_noise(x, y));
    tonemap_rgb(rgb, p).map(|c| encode_channel(c, noise))
}

#[cfg(test)]
mod tests;
