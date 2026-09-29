//! Pixel-buffer handling (pure logic): staging-row de-padding, rotation and [`Frame`]
//! construction.
//!
//! GPU staging textures are mapped with a *row pitch* that is usually larger than
//! `width * bytes_per_pixel`, and the mapped range is only guaranteed for
//! `pitch * (height - 1) + row_bytes` bytes. [`depad_rows`] validates against exactly that
//! so it never reads past the mapping, whatever the driver reports. [`frame_from_packed`]
//! then attaches origin, scale and HDR metadata.

use ssx_types::{Frame, HdrInfo, Point, Size};

use crate::{error::WinError, hdr::CaptureFormat};

/// Bytes a mapped surface must expose for `height` rows of `row_bytes` at `pitch`.
/// `None` on overflow.
pub(crate) fn mapped_len(pitch: usize, row_bytes: usize, height: u32) -> Option<usize> {
    match height {
        0 => Some(0),
        h => pitch.checked_mul(h as usize - 1)?.checked_add(row_bytes),
    }
}

/// Copies a pitched surface into a tightly packed buffer (`width * bpp` bytes per row).
///
/// # Errors
/// If `pitch` is smaller than one row, or `src` is shorter than [`mapped_len`].
pub(crate) fn depad_rows(
    src: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<Vec<u8>, WinError> {
    let row = (width as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| WinError::Other("frame row size overflows".into()))?;
    if pitch < row {
        return Err(WinError::Other(format!(
            "mapped row pitch {pitch} is smaller than one row ({row} bytes)"
        )));
    }
    let need = mapped_len(pitch, row, height)
        .ok_or_else(|| WinError::Other("frame size overflows".into()))?;
    if src.len() < need {
        return Err(WinError::Other(format!(
            "mapped surface too small: need {need} bytes, have {}",
            src.len()
        )));
    }
    if pitch == row {
        return Ok(src[..need].to_vec());
    }
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * pitch;
        out.extend_from_slice(&src[start..start + row]);
    }
    Ok(out)
}

/// Where a captured frame sits and how it was produced; copied onto the [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placement {
    /// Top-left on the virtual desktop, in physical pixels.
    pub(crate) origin: Point,
    /// UI scale factor of the display the pixels came from.
    pub(crate) scale_factor: f64,
    /// HDR state of that display, used to set `sdr_white_nits` on float frames.
    pub(crate) hdr: Option<HdrInfo>,
}

/// Wraps tightly packed pixels into a [`Frame`]: sets origin, scale, `sdr_white_nits` and
/// forces alpha opaque (capture APIs leave it undefined, and the desktop is opaque).
pub(crate) fn frame_from_packed(
    data: Vec<u8>,
    size: Size,
    format: CaptureFormat,
    placement: &Placement,
) -> Result<Frame, WinError> {
    let stride = size.width as usize * format.bytes_per_pixel();
    let mut frame =
        Frame::from_raw(size, stride, format.pixel_format(), format.color_space(), data)?;
    frame.origin = placement.origin;
    frame.scale_factor = placement.scale_factor;
    frame.sdr_white_nits = format.frame_sdr_white_nits(placement.hdr);
    frame.set_opaque();
    Ok(frame)
}

/// Display rotation as reported by DXGI (`DXGI_MODE_ROTATION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rotation {
    Identity,
    /// The texture must be rotated 90° clockwise to match the display orientation.
    Rotate90,
    Rotate180,
    /// The texture must be rotated 90° counter-clockwise.
    Rotate270,
}

impl Rotation {
    /// Maps a raw `DXGI_MODE_ROTATION` (`0` unspecified, `1` identity, `2` 90°, `3` 180°,
    /// `4` 270°). Unknown values are treated as identity.
    pub(crate) fn from_dxgi(raw: i32) -> Self {
        match raw {
            2 => Rotation::Rotate90,
            3 => Rotation::Rotate180,
            4 => Rotation::Rotate270,
            _ => Rotation::Identity,
        }
    }

    /// Size after rotating a `width` x `height` image.
    pub(crate) fn output_size(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Rotation::Identity | Rotation::Rotate180 => (width, height),
            Rotation::Rotate90 | Rotation::Rotate270 => (height, width),
        }
    }
}

/// Rotates a tightly packed image, returning the new pixels and dimensions.
///
/// Desktop Duplication returns the desktop in the *unrotated* orientation of a rotated
/// display; this brings it to what the user sees. The direction of `Rotate90` follows the
/// Microsoft desktop-duplication sample (`(x, y)` maps to `(height - 1 - y, x)`, i.e. a
/// clockwise turn). It has not been verified on a physically rotated monitor; the caller
/// cross-checks the result size against the monitor rectangle.
///
/// # Errors
/// If `data` is not exactly `width * height * bytes_per_pixel` bytes.
pub(crate) fn rotate(
    data: &[u8],
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    rotation: Rotation,
) -> Result<(Vec<u8>, u32, u32), WinError> {
    let (w, h, bpp) = (width as usize, height as usize, bytes_per_pixel);
    if w.checked_mul(h).and_then(|n| n.checked_mul(bpp)) != Some(data.len()) {
        return Err(WinError::Other(format!(
            "cannot rotate: {} bytes is not {width}x{height} at {bpp} bytes per pixel",
            data.len()
        )));
    }
    let (ow, oh) = rotation.output_size(width, height);
    if rotation == Rotation::Identity {
        return Ok((data.to_vec(), ow, oh));
    }
    let mut out = vec![0u8; data.len()];
    // Destination pixel (dx, dy) reads from source pixel (sx, sy).
    for dy in 0..oh as usize {
        for dx in 0..ow as usize {
            let (sx, sy) = match rotation {
                Rotation::Identity => (dx, dy),
                Rotation::Rotate90 => (dy, h - 1 - dx),
                Rotation::Rotate180 => (w - 1 - dx, h - 1 - dy),
                Rotation::Rotate270 => (w - 1 - dy, dx),
            };
            let s = (sy * w + sx) * bpp;
            let d = (dy * ow as usize + dx) * bpp;
            out[d..d + bpp].copy_from_slice(&data[s..s + bpp]);
        }
    }
    Ok((out, ow, oh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssx_types::{ColorSpace, PixelFormat};

    /// A `w`x`h` single-byte-per-pixel image where pixel (x, y) = y * w + x.
    fn ramp(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).map(|v| v as u8).collect()
    }

    #[test]
    fn depad_removes_row_padding_8bit() {
        // 3 px wide BGRA (12 bytes per row), pitch 16, 2 rows. Last row has no trailing pad.
        let mut src = vec![0xEE; 16 + 12];
        src[..12].copy_from_slice(&[1; 12]);
        src[16..28].copy_from_slice(&[2; 12]);
        let out = depad_rows(&src, 16, 3, 2, 4).unwrap();
        assert_eq!(out.len(), 24);
        assert_eq!(&out[..12], &[1; 12]);
        assert_eq!(&out[12..], &[2; 12]);
    }

    #[test]
    fn depad_removes_row_padding_float() {
        // 2 px wide RGBA16F (16 bytes per row), pitch 256 (typical GPU alignment), 3 rows.
        let (pitch, row, h) = (256usize, 16usize, 3u32);
        let mut src = vec![0xAA; pitch * (h as usize - 1) + row];
        for y in 0..h as usize {
            src[y * pitch..y * pitch + row].fill(y as u8 + 1);
        }
        let out = depad_rows(&src, pitch, 2, h, 8).unwrap();
        assert_eq!(out.len(), row * h as usize);
        for y in 0..h as usize {
            assert!(out[y * row..(y + 1) * row].iter().all(|b| *b == y as u8 + 1), "row {y}");
        }
    }

    #[test]
    fn depad_tight_pitch_is_a_plain_copy() {
        let src = ramp(4, 4);
        assert_eq!(depad_rows(&src, 4, 4, 4, 1).unwrap(), src);
    }

    #[test]
    fn depad_never_reads_past_the_mapping() {
        // Exactly pitch * (h - 1) + row bytes is enough...
        assert!(depad_rows(&[0; 16 + 12], 16, 3, 2, 4).is_ok());
        // ...one byte less is rejected rather than read out of bounds.
        assert!(depad_rows(&[0; 16 + 11], 16, 3, 2, 4).is_err());
    }

    #[test]
    fn depad_rejects_bad_pitch_and_handles_degenerate_sizes() {
        assert!(depad_rows(&[0; 100], 8, 3, 2, 4).is_err(), "pitch < row");
        assert_eq!(depad_rows(&[], 0, 0, 0, 4).unwrap(), Vec::<u8>::new());
        assert_eq!(depad_rows(&[], 16, 4, 0, 4).unwrap(), Vec::<u8>::new(), "zero rows");
        assert!(depad_rows(&[], usize::MAX, u32::MAX, u32::MAX, 8).is_err(), "no overflow panic");
    }

    #[test]
    fn mapped_len_edge_cases() {
        assert_eq!(mapped_len(16, 12, 0), Some(0));
        assert_eq!(mapped_len(16, 12, 1), Some(12));
        assert_eq!(mapped_len(16, 12, 3), Some(44));
        assert_eq!(mapped_len(usize::MAX, 1, 3), None);
    }

    #[test]
    fn frame_gets_placement_and_is_opaque() {
        let mut data = vec![0u8; 2 * 2 * 4];
        data.chunks_exact_mut(4).for_each(|p| p.copy_from_slice(&[10, 20, 30, 0]));
        let placement =
            Placement { origin: Point::new(-1920, 5), scale_factor: 1.5, hdr: Some(HdrInfo::SDR) };
        let f = frame_from_packed(data, Size::new(2, 2), CaptureFormat::Bgra8, &placement).unwrap();
        assert_eq!(f.format(), PixelFormat::Bgra8);
        assert_eq!(f.color_space(), ColorSpace::Srgb);
        assert_eq!(f.origin, Point::new(-1920, 5));
        assert!((f.scale_factor - 1.5).abs() < 1e-12);
        assert_eq!(f.sdr_white_nits, None);
        assert_eq!(f.row(1), &[10, 20, 30, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn float_frame_carries_sdr_white_and_opaque_alpha() {
        let hdr = HdrInfo { active: true, sdr_white_nits: 200.0, max_luminance_nits: None };
        let placement = Placement { origin: Point::default(), scale_factor: 2.0, hdr: Some(hdr) };
        // 1x2 float image: pixel rows padded to 32-byte pitch.
        let mut mapped = vec![0u8; 32 + 8];
        mapped[..8].copy_from_slice(&[1; 8]);
        mapped[32..40].copy_from_slice(&[2; 8]);
        let packed = depad_rows(&mapped, 32, 1, 2, 8).unwrap();
        let f =
            frame_from_packed(packed, Size::new(1, 2), CaptureFormat::Rgba16F, &placement).unwrap();
        assert_eq!(f.format(), PixelFormat::Rgba16F);
        assert_eq!(f.color_space(), ColorSpace::ScRgbLinear);
        assert_eq!(f.sdr_white_nits, Some(200.0));
        assert_eq!(f.stride(), 8);
        let one = half::f16::from_f32(1.0).to_le_bytes();
        assert_eq!(&f.row(0)[6..8], &one, "alpha is exactly 1.0");
        assert_eq!(&f.row(1)[6..8], &one);
        assert_eq!(&f.row(1)[..6], &[2; 6], "colour channels untouched");
    }

    #[test]
    fn frame_from_packed_reports_short_buffers() {
        let placement = Placement { origin: Point::default(), scale_factor: 1.0, hdr: None };
        let err = frame_from_packed(vec![0; 3], Size::new(2, 2), CaptureFormat::Bgra8, &placement);
        assert!(matches!(err, Err(WinError::Frame(_))));
    }

    #[test]
    fn rotation_from_dxgi() {
        assert_eq!(Rotation::from_dxgi(0), Rotation::Identity);
        assert_eq!(Rotation::from_dxgi(1), Rotation::Identity);
        assert_eq!(Rotation::from_dxgi(2), Rotation::Rotate90);
        assert_eq!(Rotation::from_dxgi(3), Rotation::Rotate180);
        assert_eq!(Rotation::from_dxgi(4), Rotation::Rotate270);
        assert_eq!(Rotation::from_dxgi(99), Rotation::Identity);
        assert_eq!(Rotation::Rotate90.output_size(1920, 1080), (1080, 1920));
        assert_eq!(Rotation::Rotate180.output_size(1920, 1080), (1920, 1080));
    }

    #[test]
    fn rotate90_is_clockwise() {
        // 3x2:  0 1 2
        //       3 4 5
        // Clockwise 90 -> 2x3:  3 0
        //                       4 1
        //                       5 2
        let (out, w, h) = rotate(&ramp(3, 2), 3, 2, 1, Rotation::Rotate90).unwrap();
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, vec![3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn rotate270_is_counter_clockwise() {
        // Counter-clockwise 90 of the 3x2 ramp -> 2x3:  2 5
        //                                               1 4
        //                                               0 3
        let (out, w, h) = rotate(&ramp(3, 2), 3, 2, 1, Rotation::Rotate270).unwrap();
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, vec![2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn rotate180_reverses_pixels() {
        let (out, w, h) = rotate(&ramp(3, 2), 3, 2, 1, Rotation::Rotate180).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(out, vec![5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn rotation_moves_whole_pixels_not_bytes() {
        // 2x1 image of 4-byte pixels A=[1,2,3,4], B=[5,6,7,8]; clockwise -> 1x2: A over B.
        let (out, w, h) = rotate(&[1, 2, 3, 4, 5, 6, 7, 8], 2, 1, 4, Rotation::Rotate90).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let (out, ..) = rotate(&[1, 2, 3, 4, 5, 6, 7, 8], 2, 1, 4, Rotation::Rotate270).unwrap();
        assert_eq!(out, vec![5, 6, 7, 8, 1, 2, 3, 4]);
    }

    #[test]
    fn rotations_compose_to_identity() {
        for (w, h) in [(1, 1), (1, 5), (4, 3), (7, 7)] {
            let img = ramp(w, h);
            let mut cur = (img.clone(), w, h);
            for _ in 0..4 {
                cur = rotate(&cur.0, cur.1, cur.2, 1, Rotation::Rotate90).unwrap();
            }
            assert_eq!(cur.0, img, "4 x 90 for {w}x{h}");
            let (a, aw, ah) = rotate(&img, w, h, 1, Rotation::Rotate90).unwrap();
            let (b, bw, bh) = rotate(&a, aw, ah, 1, Rotation::Rotate270).unwrap();
            assert_eq!((b, bw, bh), (img.clone(), w, h), "90 then 270 for {w}x{h}");
            let (a, aw, ah) = rotate(&img, w, h, 1, Rotation::Rotate180).unwrap();
            let (b, ..) = rotate(&a, aw, ah, 1, Rotation::Rotate180).unwrap();
            assert_eq!(b, img, "180 twice for {w}x{h}");
        }
    }

    #[test]
    fn rotate_identity_and_bad_input() {
        let img = ramp(3, 2);
        assert_eq!(rotate(&img, 3, 2, 1, Rotation::Identity).unwrap(), (img.clone(), 3, 2));
        assert!(rotate(&img, 3, 3, 1, Rotation::Rotate90).is_err(), "length mismatch");
        assert!(rotate(&[], u32::MAX, u32::MAX, 8, Rotation::Rotate90).is_err(), "overflow");
        assert_eq!(rotate(&[], 0, 0, 4, Rotation::Rotate90).unwrap().1, 0);
    }
}
