//! `wl_shm` pixel formats and their conversion to the `Frame` layout.
//!
//! Compositors pick the shm format, not us, and different compositors/GPUs pick
//! different ones (`Xrgb8888` on pixman, `Xbgr2101010` on 10-bit scanout, `Abgr16161616f`
//! on some HDR paths). Everything is normalised here to tightly packed 8-bit **BGRA**
//! (`PixelFormat::Bgra8`, the byte order of `Argb8888`/`Xrgb8888` on little-endian hosts)
//! so that frames from different outputs can always be stitched together. Alpha is forced
//! opaque: capture buffers carry meaningless alpha (`X` formats) or premultiplied
//! desktop alpha that the user never wants in a screenshot.
//!
//! Wider formats are reduced with round-to-nearest (`v * 255 / max`), not by truncating
//! low bits, so full white stays exactly 255 and mid-tones do not drift dark.

use ssx_capture::{CaptureError, Result};

/// `wl_shm` format codes (`Argb8888`/`Xrgb8888` are special, the rest are DRM fourcc).
const fn fourcc(s: [u8; 4]) -> u32 {
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

/// A shm format this crate can convert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShmFormat {
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
    Rgba8888,
    Rgbx8888,
    Bgra8888,
    Bgrx8888,
    Rgb888,
    Bgr888,
    Rgb565,
    Argb2101010,
    Xrgb2101010,
    Abgr2101010,
    Xbgr2101010,
    /// 16-bit unsigned normalised, memory order R, G, B, A.
    Abgr16161616,
    Xbgr16161616,
    /// IEEE half floats, memory order R, G, B, A. Treated as already sRGB-encoded.
    Abgr16161616f,
    Xbgr16161616f,
}

impl ShmFormat {
    /// Every supported format, most preferred first (cheapest to convert and most
    /// common first, wide formats last).
    pub const PREFERENCE: [ShmFormat; 19] = [
        ShmFormat::Xrgb8888,
        ShmFormat::Argb8888,
        ShmFormat::Xbgr8888,
        ShmFormat::Abgr8888,
        ShmFormat::Bgrx8888,
        ShmFormat::Bgra8888,
        ShmFormat::Rgbx8888,
        ShmFormat::Rgba8888,
        ShmFormat::Xrgb2101010,
        ShmFormat::Xbgr2101010,
        ShmFormat::Argb2101010,
        ShmFormat::Abgr2101010,
        ShmFormat::Bgr888,
        ShmFormat::Rgb888,
        ShmFormat::Xbgr16161616,
        ShmFormat::Abgr16161616,
        ShmFormat::Xbgr16161616f,
        ShmFormat::Abgr16161616f,
        ShmFormat::Rgb565,
    ];

    /// Maps a `wl_shm.format` value, or `None` if we cannot convert it.
    pub const fn from_wl(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Argb8888,
            1 => Self::Xrgb8888,
            c if c == fourcc(*b"AB24") => Self::Abgr8888,
            c if c == fourcc(*b"XB24") => Self::Xbgr8888,
            c if c == fourcc(*b"RA24") => Self::Rgba8888,
            c if c == fourcc(*b"RX24") => Self::Rgbx8888,
            c if c == fourcc(*b"BA24") => Self::Bgra8888,
            c if c == fourcc(*b"BX24") => Self::Bgrx8888,
            c if c == fourcc(*b"RG24") => Self::Rgb888,
            c if c == fourcc(*b"BG24") => Self::Bgr888,
            c if c == fourcc(*b"RG16") => Self::Rgb565,
            c if c == fourcc(*b"AR30") => Self::Argb2101010,
            c if c == fourcc(*b"XR30") => Self::Xrgb2101010,
            c if c == fourcc(*b"AB30") => Self::Abgr2101010,
            c if c == fourcc(*b"XB30") => Self::Xbgr2101010,
            c if c == fourcc(*b"AB48") => Self::Abgr16161616,
            c if c == fourcc(*b"XB48") => Self::Xbgr16161616,
            c if c == fourcc(*b"AB4H") => Self::Abgr16161616f,
            c if c == fourcc(*b"XB4H") => Self::Xbgr16161616f,
            _ => return None,
        })
    }

    /// The `wl_shm.format` value.
    pub const fn to_wl(self) -> u32 {
        match self {
            Self::Argb8888 => 0,
            Self::Xrgb8888 => 1,
            Self::Abgr8888 => fourcc(*b"AB24"),
            Self::Xbgr8888 => fourcc(*b"XB24"),
            Self::Rgba8888 => fourcc(*b"RA24"),
            Self::Rgbx8888 => fourcc(*b"RX24"),
            Self::Bgra8888 => fourcc(*b"BA24"),
            Self::Bgrx8888 => fourcc(*b"BX24"),
            Self::Rgb888 => fourcc(*b"RG24"),
            Self::Bgr888 => fourcc(*b"BG24"),
            Self::Rgb565 => fourcc(*b"RG16"),
            Self::Argb2101010 => fourcc(*b"AR30"),
            Self::Xrgb2101010 => fourcc(*b"XR30"),
            Self::Abgr2101010 => fourcc(*b"AB30"),
            Self::Xbgr2101010 => fourcc(*b"XB30"),
            Self::Abgr16161616 => fourcc(*b"AB48"),
            Self::Xbgr16161616 => fourcc(*b"XB48"),
            Self::Abgr16161616f => fourcc(*b"AB4H"),
            Self::Xbgr16161616f => fourcc(*b"XB4H"),
        }
    }

    /// Bytes per pixel in the shm buffer.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb888 | Self::Bgr888 => 3,
            Self::Rgb565 => 2,
            Self::Abgr16161616 | Self::Xbgr16161616 | Self::Abgr16161616f | Self::Xbgr16161616f => {
                8
            }
            _ => 4,
        }
    }

    /// Picks the best format out of the codes a compositor offered, optionally limited to
    /// those `wl_shm` advertised (`supported` empty means "unknown, do not filter").
    pub fn choose(offered: &[u32], supported: &[u32]) -> Option<Self> {
        offered
            .iter()
            .filter(|c| supported.is_empty() || supported.contains(c))
            .filter_map(|c| Self::from_wl(*c))
            .min_by_key(|f| Self::PREFERENCE.iter().position(|p| p == f))
    }
}

/// Rounds a `bits`-wide unsigned value to 8 bits: `round(v * 255 / (2^bits - 1))`.
const fn to_u8(v: u32, bits: u32) -> u8 {
    let max = (1u32 << bits) - 1;
    ((v * 255 + max / 2) / max) as u8
}

/// Decodes an IEEE binary16 value to `f32` (no `half` dependency for one function).
fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let frac = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        0x1f => {
            if frac == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// Unit float (clamped, NaN → 0) to 8 bit with rounding.
fn unit_to_u8(v: f32) -> u8 {
    if v.is_nan() { 0 } else { (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 }
}

/// Converts one pixel (`px` holds exactly `bytes_per_pixel` bytes) to `[b, g, r]`.
fn pixel_bgr(fmt: ShmFormat, px: &[u8]) -> [u8; 3] {
    let u32le = |p: &[u8]| u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let u16le = |p: &[u8], i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
    match fmt {
        ShmFormat::Argb8888 | ShmFormat::Xrgb8888 | ShmFormat::Rgb888 => [px[0], px[1], px[2]],
        ShmFormat::Abgr8888 | ShmFormat::Xbgr8888 | ShmFormat::Bgr888 => [px[2], px[1], px[0]],
        ShmFormat::Rgba8888 | ShmFormat::Rgbx8888 => [px[1], px[2], px[3]],
        ShmFormat::Bgra8888 | ShmFormat::Bgrx8888 => [px[3], px[2], px[1]],
        ShmFormat::Rgb565 => {
            let v = u32::from(u16le(px, 0));
            [to_u8(v & 0x1f, 5), to_u8((v >> 5) & 0x3f, 6), to_u8((v >> 11) & 0x1f, 5)]
        }
        ShmFormat::Argb2101010 | ShmFormat::Xrgb2101010 => {
            let v = u32le(px);
            [to_u8(v & 0x3ff, 10), to_u8((v >> 10) & 0x3ff, 10), to_u8((v >> 20) & 0x3ff, 10)]
        }
        ShmFormat::Abgr2101010 | ShmFormat::Xbgr2101010 => {
            let v = u32le(px);
            [to_u8((v >> 20) & 0x3ff, 10), to_u8((v >> 10) & 0x3ff, 10), to_u8(v & 0x3ff, 10)]
        }
        ShmFormat::Abgr16161616 | ShmFormat::Xbgr16161616 => [
            to_u8(u32::from(u16le(px, 4)), 16),
            to_u8(u32::from(u16le(px, 2)), 16),
            to_u8(u32::from(u16le(px, 0)), 16),
        ],
        ShmFormat::Abgr16161616f | ShmFormat::Xbgr16161616f => [
            unit_to_u8(f16_to_f32(u16le(px, 4))),
            unit_to_u8(f16_to_f32(u16le(px, 2))),
            unit_to_u8(f16_to_f32(u16le(px, 0))),
        ],
    }
}

/// Converts a shm buffer to tightly packed opaque BGRA.
///
/// * `stride` is the row pitch in bytes and may exceed `width * bpp`.
/// * `y_invert` means row 0 of the buffer is the bottom of the image (wlr-screencopy
///   sets this flag for GL-rendered buffers); rows are emitted bottom-up to undo it.
pub fn to_bgra(
    fmt: ShmFormat,
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    y_invert: bool,
) -> Result<Vec<u8>> {
    let bpp = fmt.bytes_per_pixel();
    let (w, h) = (width as usize, height as usize);
    let row_len = w
        .checked_mul(bpp)
        .ok_or_else(|| CaptureError::backend("wayland", "buffer width overflows"))?;
    if stride < row_len {
        return Err(CaptureError::backend(
            "wayland",
            format!("compositor stride {stride} is smaller than one row ({row_len} bytes)"),
        ));
    }
    let need = if h == 0 { 0 } else { stride.saturating_mul(h - 1).saturating_add(row_len) };
    if src.len() < need {
        return Err(CaptureError::backend(
            "wayland",
            format!("shm buffer too small: {} bytes, need {need}", src.len()),
        ));
    }
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let sy = if y_invert { h - 1 - y } else { y };
        let s = &src[sy * stride..sy * stride + row_len];
        let d = &mut out[y * w * 4..(y + 1) * w * 4];
        match fmt {
            // Fast paths: pure byte moves.
            ShmFormat::Argb8888 | ShmFormat::Xrgb8888 => {
                d.copy_from_slice(s);
                for px in d.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
            _ => {
                for (sp, dp) in s.chunks_exact(bpp).zip(d.chunks_exact_mut(4)) {
                    let [b, g, r] = pixel_bgr(fmt, sp);
                    dp.copy_from_slice(&[b, g, r, 255]);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // compares exactly representable constants
mod tests {
    use super::*;

    fn one(fmt: ShmFormat, px: &[u8]) -> [u8; 4] {
        let out = to_bgra(fmt, px, 1, 1, px.len(), false).unwrap();
        [out[0], out[1], out[2], out[3]]
    }

    #[test]
    fn wl_codes_round_trip() {
        for f in ShmFormat::PREFERENCE {
            assert_eq!(ShmFormat::from_wl(f.to_wl()), Some(f), "{f:?}");
        }
        assert_eq!(ShmFormat::Argb8888.to_wl(), 0);
        assert_eq!(ShmFormat::Xrgb8888.to_wl(), 1);
        // Well known fourcc values from drm_fourcc.h.
        assert_eq!(ShmFormat::Xbgr8888.to_wl(), 0x3432_4258);
        assert_eq!(ShmFormat::Xrgb2101010.to_wl(), 0x3033_5258);
        assert_eq!(ShmFormat::from_wl(0xdead_beef), None);
    }

    #[test]
    fn eight_bit_orders() {
        // Colour under test: R=10 G=20 B=30. Expected BGRA out: [30,20,10,255].
        let want = [30, 20, 10, 255];
        assert_eq!(one(ShmFormat::Argb8888, &[30, 20, 10, 7]), want);
        assert_eq!(one(ShmFormat::Xrgb8888, &[30, 20, 10, 0]), want);
        assert_eq!(one(ShmFormat::Abgr8888, &[10, 20, 30, 7]), want);
        assert_eq!(one(ShmFormat::Xbgr8888, &[10, 20, 30, 0]), want);
        assert_eq!(one(ShmFormat::Rgba8888, &[7, 30, 20, 10]), want);
        assert_eq!(one(ShmFormat::Rgbx8888, &[0, 30, 20, 10]), want);
        assert_eq!(one(ShmFormat::Bgra8888, &[7, 10, 20, 30]), want);
        assert_eq!(one(ShmFormat::Bgrx8888, &[0, 10, 20, 30]), want);
        assert_eq!(one(ShmFormat::Rgb888, &[30, 20, 10]), want);
        assert_eq!(one(ShmFormat::Bgr888, &[10, 20, 30]), want);
    }

    #[test]
    fn ten_bit_is_rounded_not_truncated() {
        // XRGB2101010: R in [29:20], G in [19:10], B in [9:0].
        let px = |r: u32, g: u32, b: u32| ((r << 20) | (g << 10) | b).to_le_bytes();
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(1023, 1023, 1023)), [255, 255, 255, 255]);
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(0, 0, 0)), [0, 0, 0, 255]);
        // 512/1023*255 = 127.6 -> 128 (truncating the low two bits would give 128 as
        // well, so also probe values where the two differ).
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(512, 512, 512))[0], 128);
        // 3/1023*255 = 0.75 -> 1, but `3 >> 2` = 0.
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(3, 3, 3))[0], 1);
        // 1022/1023*255 = 254.75 -> 255, but `1022 >> 2` = 255 too; 1019 differs:
        // 1019*255/1023 = 254.0 -> 254 (`1019 >> 2` = 254).
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(1019, 2, 1))[2], 254);
        // Channel placement: distinct values in each channel.
        assert_eq!(one(ShmFormat::Xrgb2101010, &px(1023, 0, 512)), [128, 0, 255, 255]);
        // Alpha bits are ignored and the output is opaque.
        let with_alpha = (0b01u32 << 30 | (1023 << 20)).to_le_bytes();
        assert_eq!(one(ShmFormat::Argb2101010, &with_alpha), [0, 0, 255, 255]);
    }

    #[test]
    fn ten_bit_bgr_order_swaps_channels() {
        // XBGR2101010: B in [29:20], G in [19:10], R in [9:0].
        let px = ((1023u32 << 20) | (512 << 10) | 3).to_le_bytes();
        assert_eq!(one(ShmFormat::Xbgr2101010, &px), [255, 128, 1, 255]);
        assert_eq!(one(ShmFormat::Abgr2101010, &px), [255, 128, 1, 255]);
    }

    #[test]
    fn to_u8_matches_reference_rounding_for_every_10_bit_value() {
        for v in 0..1024u32 {
            let want = (f64::from(v) * 255.0 / 1023.0).round() as u8;
            assert_eq!(to_u8(v, 10), want, "v={v}");
        }
        for v in 0..32u32 {
            assert_eq!(to_u8(v, 5), (f64::from(v) * 255.0 / 31.0).round() as u8);
        }
    }

    #[test]
    fn rgb565_expands_to_full_range() {
        assert_eq!(one(ShmFormat::Rgb565, &0xffffu16.to_le_bytes()), [255, 255, 255, 255]);
        // R=31 only.
        assert_eq!(one(ShmFormat::Rgb565, &(31u16 << 11).to_le_bytes()), [0, 0, 255, 255]);
        // G=63 only.
        assert_eq!(one(ShmFormat::Rgb565, &(63u16 << 5).to_le_bytes()), [0, 255, 0, 255]);
    }

    #[test]
    fn sixteen_bit_formats() {
        let mut px = Vec::new();
        for v in [0xffffu16, 0x8000, 0x0000, 0x1234] {
            px.extend_from_slice(&v.to_le_bytes());
        }
        // R=0xffff G=0x8000 B=0 -> [b,g,r] = [0,128,255]
        assert_eq!(one(ShmFormat::Abgr16161616, &px), [0, 128, 255, 255]);

        let half = |f: f32| -> [u8; 2] {
            // Only need exactly representable values: 0, 0.5, 1, 2.
            let bits: u16 = match f.to_bits() {
                0 => 0x0000,
                x if x == 0.5f32.to_bits() => 0x3800,
                x if x == 1.0f32.to_bits() => 0x3c00,
                x if x == 2.0f32.to_bits() => 0x4000,
                _ => unreachable!(),
            };
            bits.to_le_bytes()
        };
        let mut fpx = Vec::new();
        for v in [1.0, 0.5, 0.0, 1.0] {
            fpx.extend_from_slice(&half(v));
        }
        assert_eq!(one(ShmFormat::Abgr16161616f, &fpx), [0, 128, 255, 255]);
        // Over-range clamps, no wrap-around.
        let mut over = Vec::new();
        for v in [2.0, 0.0, 0.0, 1.0] {
            over.extend_from_slice(&half(v));
        }
        assert_eq!(one(ShmFormat::Xbgr16161616f, &over)[2], 255);
    }

    #[test]
    fn f16_decoding_edge_cases() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(unit_to_u8(f32::NAN), 0);
        assert_eq!(unit_to_u8(-1.0), 0);
        assert_eq!(unit_to_u8(f32::INFINITY), 255);
    }

    #[test]
    fn stride_padding_is_skipped() {
        // 2x2 Xrgb8888 with a 12 byte stride (4 bytes of padding per row).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xaa, 0xaa, 0xaa, 0xaa, //
            7, 8, 9, 0, 10, 11, 12, 0, 0xbb, 0xbb, 0xbb, 0xbb,
        ];
        let out = to_bgra(ShmFormat::Xrgb8888, &src, 2, 2, 12, false).unwrap();
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]);
    }

    #[test]
    fn y_invert_flips_rows() {
        let src = [1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 0];
        let out = to_bgra(ShmFormat::Xrgb8888, &src, 1, 3, 4, true).unwrap();
        assert_eq!(out, [3, 3, 3, 255, 2, 2, 2, 255, 1, 1, 1, 255]);
    }

    #[test]
    fn last_row_needs_no_trailing_padding() {
        // Buffer that ends right after the last pixel row (no stride padding at the end).
        let src = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0];
        assert!(to_bgra(ShmFormat::Xrgb8888, &src, 1, 2, 8, false).is_ok());
    }

    #[test]
    fn malformed_buffers_are_errors_not_panics() {
        assert!(to_bgra(ShmFormat::Xrgb8888, &[0; 8], 2, 1, 4, false).is_err(), "stride < row");
        assert!(to_bgra(ShmFormat::Xrgb8888, &[0; 7], 2, 1, 8, false).is_err(), "short buffer");
        assert!(to_bgra(ShmFormat::Xrgb8888, &[], 0, 0, 0, false).unwrap().is_empty());
        assert!(to_bgra(ShmFormat::Xrgb8888, &[], u32::MAX, 5, usize::MAX, false).is_err());
    }

    #[test]
    fn choose_prefers_cheap_formats_and_respects_wl_shm() {
        let offered = [ShmFormat::Xbgr2101010.to_wl(), ShmFormat::Xrgb8888.to_wl()];
        assert_eq!(ShmFormat::choose(&offered, &[]), Some(ShmFormat::Xrgb8888));
        // wl_shm claims it only supports the 10-bit one.
        assert_eq!(
            ShmFormat::choose(&offered, &[ShmFormat::Xbgr2101010.to_wl()]),
            Some(ShmFormat::Xbgr2101010)
        );
        assert_eq!(ShmFormat::choose(&[0x1234_5678], &[]), None);
        assert_eq!(ShmFormat::choose(&[], &[]), None);
    }
}
