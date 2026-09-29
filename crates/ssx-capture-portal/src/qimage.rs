//! Conversion of KWin's raw `QImage` pixel dumps into [`Frame`]s.
//!
//! `org.kde.KWin.ScreenShot2` writes the *in-memory bytes* of a `QImage` to a pipe and
//! reports the `QImage::Format` enum value plus stride. Qt's 32-bit formats are packed
//! **native-endian** `u32`s, so the byte order depends on the host; decoding through
//! `u32::from_ne_bytes` keeps this correct on big-endian machines too.
//!
//! Everything is normalised to straight-alpha 8-bit sRGB RGBA (tightly packed). The
//! premultiplied formats are un-premultiplied because `Frame` (and every image file
//! format) expects straight alpha. Formats with no sensible sRGB-8 interpretation
//! (floating point, indexed, 16-bit RGB) are rejected instead of guessed at.

use ssx_types::{Frame, FrameError};

/// A `QImage::Format` value we know how to read, keyed by the enum's numeric value
/// (stable ABI: see `qimage.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `0xAARRGGBB` native-endian u32 (`RGB32`, `ARGB32`, `ARGB32_Premultiplied`).
    Argb32(Alpha),
    /// Bytes R, G, B.
    Rgb888,
    /// Bytes B, G, R.
    Bgr888,
    /// Bytes R, G, B, X (RGBX8888).
    Rgbx8888,
    /// Bytes R, G, B, A (RGBA8888 and premultiplied variant).
    Rgba8888(Alpha),
    /// 2-bit alpha, 10-bit channels in a native-endian u32, red in the high bits.
    Rgb30(Alpha),
    /// As [`Layout::Rgb30`] but blue in the high bits.
    Bgr30(Alpha),
    /// 8-bit luminance.
    Gray8,
    /// Four native-endian u16 channels R, G, B, A.
    Rgba64(Alpha),
}

/// How the alpha channel of a format is to be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alpha {
    /// Format has no alpha (or it is padding): output is opaque.
    Opaque,
    /// Straight (non-premultiplied) alpha.
    Straight,
    /// Colour channels are multiplied by alpha.
    Premultiplied,
}

/// Why a raw image could not be converted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum QImageError {
    /// A `QImage::Format` this crate does not decode.
    #[error("unsupported QImage format {0} (only 8/10/16-bit integer RGB(A) and gray are decoded)")]
    UnsupportedFormat(u32),
    /// Geometry inconsistent with the pixel data.
    #[error("invalid raw image: {0}")]
    Invalid(String),
    /// `Frame` construction failed.
    #[error(transparent)]
    Frame(#[from] FrameError),
}

fn layout_for(format: u32) -> Option<Layout> {
    Some(match format {
        4 => Layout::Argb32(Alpha::Opaque),
        5 => Layout::Argb32(Alpha::Straight),
        6 => Layout::Argb32(Alpha::Premultiplied),
        13 => Layout::Rgb888,
        16 => Layout::Rgbx8888,
        17 => Layout::Rgba8888(Alpha::Straight),
        18 => Layout::Rgba8888(Alpha::Premultiplied),
        19 => Layout::Bgr30(Alpha::Opaque),
        20 => Layout::Bgr30(Alpha::Premultiplied),
        21 => Layout::Rgb30(Alpha::Opaque),
        22 => Layout::Rgb30(Alpha::Premultiplied),
        24 => Layout::Gray8,
        25 => Layout::Rgba64(Alpha::Opaque),
        26 => Layout::Rgba64(Alpha::Straight),
        27 => Layout::Rgba64(Alpha::Premultiplied),
        29 => Layout::Bgr888,
        _ => return None,
    })
}

impl Layout {
    const fn bytes_per_pixel(self) -> usize {
        match self {
            Layout::Argb32(_)
            | Layout::Rgbx8888
            | Layout::Rgba8888(_)
            | Layout::Rgb30(_)
            | Layout::Bgr30(_) => 4,
            Layout::Rgb888 | Layout::Bgr888 => 3,
            Layout::Gray8 => 1,
            Layout::Rgba64(_) => 8,
        }
    }
}

/// Bytes per pixel of `format`, if it is one we decode. Used to bound-check a KWin reply
/// before the pixel data has been read.
pub(crate) fn bytes_per_pixel(format: u32) -> Option<usize> {
    layout_for(format).map(Layout::bytes_per_pixel)
}

/// Undoes alpha premultiplication of one 8-bit channel, rounding to nearest.
pub(crate) fn unpremultiply(c: u8, a: u8) -> u8 {
    match a {
        255 => c,
        0 => 0,
        _ => {
            let v = (u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a);
            v.min(255) as u8
        }
    }
}

/// Scales a 10-bit channel to 8 bits with rounding.
fn ten_to_eight(v: u32) -> u8 {
    ((v & 0x3ff) * 255 + 511).div_euclid(1023) as u8
}

fn finish(rgb: [u8; 3], a: u8, alpha: Alpha) -> [u8; 4] {
    match alpha {
        Alpha::Opaque => [rgb[0], rgb[1], rgb[2], 255],
        Alpha::Straight => [rgb[0], rgb[1], rgb[2], a],
        Alpha::Premultiplied => {
            [unpremultiply(rgb[0], a), unpremultiply(rgb[1], a), unpremultiply(rgb[2], a), a]
        }
    }
}

fn decode_pixel(layout: Layout, px: &[u8]) -> [u8; 4] {
    match layout {
        Layout::Argb32(alpha) => {
            let v = u32::from_ne_bytes([px[0], px[1], px[2], px[3]]);
            finish([(v >> 16) as u8, (v >> 8) as u8, v as u8], (v >> 24) as u8, alpha)
        }
        // RGBX8888 carries a padding byte in place of alpha.
        Layout::Rgb888 | Layout::Rgbx8888 => [px[0], px[1], px[2], 255],
        Layout::Bgr888 => [px[2], px[1], px[0], 255],
        Layout::Rgba8888(alpha) => finish([px[0], px[1], px[2]], px[3], alpha),
        Layout::Rgb30(alpha) | Layout::Bgr30(alpha) => {
            let v = u32::from_ne_bytes([px[0], px[1], px[2], px[3]]);
            let hi = ten_to_eight(v >> 20);
            let mid = ten_to_eight(v >> 10);
            let lo = ten_to_eight(v);
            let rgb =
                if matches!(layout, Layout::Rgb30(_)) { [hi, mid, lo] } else { [lo, mid, hi] };
            // 2-bit alpha: 0, 85, 170, 255
            finish(rgb, ((v >> 30) as u8) * 85, alpha)
        }
        Layout::Gray8 => [px[0], px[0], px[0], 255],
        Layout::Rgba64(alpha) => {
            let ch = |i: usize| (u16::from_ne_bytes([px[2 * i], px[2 * i + 1]]) >> 8) as u8;
            finish([ch(0), ch(1), ch(2)], ch(3), alpha)
        }
    }
}

/// Converts the raw bytes of a `QImage` into an sRGB [`Frame`] (`Rgba8`, tightly packed,
/// straight alpha).
///
/// `stride` is the source's bytes per line and may exceed the row size (padding is
/// dropped). The final row need not include padding.
pub(crate) fn frame_from_raw(
    width: u32,
    height: u32,
    stride: usize,
    format: u32,
    data: &[u8],
) -> Result<Frame, QImageError> {
    let layout = layout_for(format).ok_or(QImageError::UnsupportedFormat(format))?;
    if width == 0 || height == 0 {
        return Err(QImageError::Invalid(format!("empty image {width}x{height}")));
    }
    let bpp = layout.bytes_per_pixel();
    let row = (width as usize)
        .checked_mul(bpp)
        .ok_or_else(|| QImageError::Invalid("row size overflows".into()))?;
    if stride < row {
        return Err(QImageError::Invalid(format!(
            "stride {stride} smaller than row of {row} bytes"
        )));
    }
    let need = stride
        .checked_mul(height as usize - 1)
        .and_then(|n| n.checked_add(row))
        .ok_or_else(|| QImageError::Invalid("image size overflows".into()))?;
    if data.len() < need {
        return Err(QImageError::Invalid(format!(
            "pixel data truncated: got {} of {need} bytes for {width}x{height} stride {stride}",
            data.len()
        )));
    }
    let out_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| QImageError::Invalid("output size overflows".into()))?;
    let mut out = Vec::with_capacity(out_len);
    for y in 0..height as usize {
        let src = &data[y * stride..y * stride + row];
        for px in src.chunks_exact(bpp) {
            out.extend_from_slice(&decode_pixel(layout, px));
        }
    }
    Ok(Frame::from_rgba8(width, height, out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px_at(f: &Frame, x: usize, y: u32) -> [u8; 4] {
        let r = f.row(y);
        [r[4 * x], r[4 * x + 1], r[4 * x + 2], r[4 * x + 3]]
    }

    fn argb(a: u8, r: u8, g: u8, b: u8) -> [u8; 4] {
        (u32::from(a) << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)).to_ne_bytes()
    }

    #[test]
    fn rgb32_ignores_alpha_byte() {
        let data = argb(0x00, 10, 20, 30);
        let f = frame_from_raw(1, 1, 4, 4, &data).unwrap();
        assert_eq!(px_at(&f, 0, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn argb32_straight_keeps_alpha() {
        let data = argb(128, 10, 20, 30);
        let f = frame_from_raw(1, 1, 4, 5, &data).unwrap();
        assert_eq!(px_at(&f, 0, 0), [10, 20, 30, 128]);
    }

    #[test]
    fn argb32_premultiplied_is_unpremultiplied() {
        // straight (200, 100, 50) at alpha 128 premultiplies to (100, 50, 25).
        let data = argb(128, 100, 50, 25);
        let f = frame_from_raw(1, 1, 4, 6, &data).unwrap();
        assert_eq!(px_at(&f, 0, 0), [199, 100, 50, 128]);
        // alpha 0 -> colour 0; alpha 255 untouched
        let z = frame_from_raw(1, 1, 4, 6, &argb(0, 9, 9, 9)).unwrap();
        assert_eq!(px_at(&z, 0, 0), [0, 0, 0, 0]);
        let o = frame_from_raw(1, 1, 4, 6, &argb(255, 9, 8, 7)).unwrap();
        assert_eq!(px_at(&o, 0, 0), [9, 8, 7, 255]);
    }

    #[test]
    fn unpremultiply_never_overflows_and_roundtrips_within_one() {
        for a in 1..=255u16 {
            for c in 0..=255u16 {
                let pre = ((c * a + 127) / 255) as u8;
                let back = unpremultiply(pre, a as u8);
                // premultiplying loses precision; the round trip may be off by up to 255/a.
                let tol = (255 / a).max(1) as i32;
                assert!((i32::from(back) - c as i32).abs() <= tol, "c={c} a={a} back={back}");
            }
        }
        // corrupt data where colour > alpha must clamp, not wrap
        assert_eq!(unpremultiply(255, 1), 255);
    }

    #[test]
    fn byte_order_formats() {
        let f = frame_from_raw(1, 1, 3, 13, &[1, 2, 3]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [1, 2, 3, 255]);
        let f = frame_from_raw(1, 1, 3, 29, &[1, 2, 3]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [3, 2, 1, 255]);
        let f = frame_from_raw(1, 1, 4, 16, &[1, 2, 3, 0]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [1, 2, 3, 255]);
        let f = frame_from_raw(1, 1, 4, 17, &[1, 2, 3, 77]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [1, 2, 3, 77]);
        let f = frame_from_raw(1, 1, 4, 18, &[50, 25, 12, 128]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [100, 50, 24, 128]);
        let f = frame_from_raw(1, 1, 1, 24, &[42]).unwrap();
        assert_eq!(px_at(&f, 0, 0), [42, 42, 42, 255]);
    }

    #[test]
    fn ten_bit_formats() {
        // RGB30: R=1023 G=512 B=0
        let v: u32 = 3 << 30 | 1023 << 20 | 512 << 10;
        let f = frame_from_raw(1, 1, 4, 21, &v.to_ne_bytes()).unwrap();
        assert_eq!(px_at(&f, 0, 0), [255, 128, 0, 255]);
        // BGR30 swaps R/B
        let v: u32 = 3 << 30 | 1023 << 20 | 512 << 10;
        let f = frame_from_raw(1, 1, 4, 19, &v.to_ne_bytes()).unwrap();
        assert_eq!(px_at(&f, 0, 0), [0, 128, 255, 255]);
        assert_eq!(ten_to_eight(0), 0);
        assert_eq!(ten_to_eight(1023), 255);
        // A2 premultiplied with alpha 2 bits = 1 -> 85
        let v: u32 = 1 << 30 | 341 << 20;
        let f = frame_from_raw(1, 1, 4, 22, &v.to_ne_bytes()).unwrap();
        assert_eq!(px_at(&f, 0, 0)[3], 85);
    }

    #[test]
    fn sixteen_bit_channels_use_high_byte() {
        let mut d = Vec::new();
        for v in [0xFF00u16, 0x8000, 0x0100, 0xFFFF] {
            d.extend_from_slice(&v.to_ne_bytes());
        }
        let f = frame_from_raw(1, 1, 8, 26, &d).unwrap();
        assert_eq!(px_at(&f, 0, 0), [255, 128, 1, 255]);
        let f = frame_from_raw(1, 1, 8, 25, &d).unwrap();
        assert_eq!(px_at(&f, 0, 0)[3], 255);
    }

    #[test]
    fn stride_padding_is_dropped_and_last_row_may_be_short() {
        // 2x3 RGB32, stride 12 (4 padding bytes per row), last row without padding.
        let mut data = Vec::new();
        for y in 0..3u8 {
            for x in 0..2u8 {
                data.extend_from_slice(&argb(255, x, y, 7));
            }
            if y < 2 {
                data.extend_from_slice(&[0xEE; 4]);
            }
        }
        assert_eq!(data.len(), 12 * 2 + 8);
        let f = frame_from_raw(2, 3, 12, 4, &data).unwrap();
        assert_eq!(f.stride(), 8);
        for y in 0..3u8 {
            for x in 0..2u8 {
                assert_eq!(px_at(&f, x as usize, u32::from(y)), [x, y, 7, 255]);
            }
        }
    }

    #[test]
    fn rejects_unsupported_and_malformed() {
        // float formats, indexed, mono, RGB16, Invalid
        for fmt in [0u32, 1, 2, 3, 7, 30, 31, 32, 33, 34, 35, 99] {
            assert!(matches!(
                frame_from_raw(1, 1, 16, fmt, &[0; 16]),
                Err(QImageError::UnsupportedFormat(f)) if f == fmt
            ));
        }
        assert!(matches!(frame_from_raw(0, 1, 4, 4, &[]), Err(QImageError::Invalid(_))));
        assert!(matches!(frame_from_raw(2, 1, 4, 4, &[0; 8]), Err(QImageError::Invalid(_))));
        assert!(matches!(frame_from_raw(2, 2, 8, 4, &[0; 15]), Err(QImageError::Invalid(_))));
        assert!(matches!(
            frame_from_raw(u32::MAX, u32::MAX, usize::MAX, 4, &[]),
            Err(QImageError::Invalid(_))
        ));
    }

    #[test]
    fn bytes_per_pixel_matches_layouts() {
        assert_eq!(bytes_per_pixel(4), Some(4));
        assert_eq!(bytes_per_pixel(13), Some(3));
        assert_eq!(bytes_per_pixel(24), Some(1));
        assert_eq!(bytes_per_pixel(26), Some(8));
        assert_eq!(bytes_per_pixel(31), None);
    }
}
