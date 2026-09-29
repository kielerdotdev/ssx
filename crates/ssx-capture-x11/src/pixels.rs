//! Decoding X11 `ZPixmap` image data into 8-bit BGRA.
//!
//! The wire layout of `GetImage` data is *not* fixed: it depends on the pixmap format the
//! server advertises for the drawable's depth (bits per pixel, scanline padding), the
//! server's image byte order, and the visual's channel masks. Real servers mostly use
//! little-endian 32 bpp with `0xff0000/0x00ff00/0x0000ff` masks, but 16-bit (RGB565, 555),
//! packed 24 bpp, 30-bit (10 bpc) and big-endian servers all exist, so this module decodes
//! the general case and keeps a fast path for the common one. It is pure (no X
//! connection), which is what lets the odd layouts be unit-tested on any machine.
//!
//! Alpha is deliberately discarded: on a depth-24 screen the fourth byte is padding, and
//! on a depth-32 screen it is premultiplied alpha of the *window*, which is not what the
//! user saw (the compositor blended it over the desktop). Output alpha is always 255,
//! which equals compositing over black for a premultiplied source.

use x11rb::protocol::xproto::{Format, VisualClass, Visualtype};

use crate::error::{X11Error, X11Result};

/// One colour channel of a visual.
#[derive(Debug, Clone)]
struct Channel {
    mask: u32,
    shift: u32,
    /// Scales a raw channel value (`bits` wide) to 0..=255.
    lut: Vec<u8>,
}

impl Channel {
    fn new(mask: u32, what: &str) -> X11Result<Self> {
        if mask == 0 {
            return Err(X11Error::UnsupportedVisual(format!("{what} mask is empty")));
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).trailing_ones();
        if (mask >> shift) >> bits != 0 {
            return Err(X11Error::UnsupportedVisual(format!(
                "{what} mask {mask:#x} is not contiguous"
            )));
        }
        if bits > 16 {
            return Err(X11Error::UnsupportedVisual(format!("{what} channel is {bits} bits wide")));
        }
        let max = (1u32 << bits) - 1;
        let lut = (0..=max).map(|v| ((v * 255 + max / 2) / max) as u8).collect();
        Ok(Self { mask, shift, lut })
    }

    #[inline]
    fn extract(&self, pixel: u32) -> u8 {
        // Masked and shifted value is at most `bits` wide, which the table covers; the
        // fallback is unreachable but keeps this panic-free.
        self.lut.get(((pixel & self.mask) >> self.shift) as usize).copied().unwrap_or(255)
    }
}

/// How pixels of one drawable depth are laid out in `GetImage` replies.
#[derive(Debug, Clone)]
pub(crate) struct PixelLayout {
    bits_per_pixel: u32,
    scanline_pad: u32,
    msb_first: bool,
    red: Channel,
    green: Channel,
    blue: Channel,
}

impl PixelLayout {
    /// Builds the layout for a drawable of `format.depth` using `visual`'s masks.
    pub(crate) fn new(format: &Format, msb_first: bool, visual: &Visualtype) -> X11Result<Self> {
        if !matches!(visual.class, VisualClass::TRUE_COLOR | VisualClass::DIRECT_COLOR) {
            return Err(X11Error::UnsupportedVisual(format!(
                "visual class {:?} (depth {}); only TrueColor/DirectColor screens can be captured",
                visual.class, format.depth
            )));
        }
        let bpp = u32::from(format.bits_per_pixel);
        if !matches!(bpp, 16 | 24 | 32) {
            return Err(X11Error::UnsupportedVisual(format!("{bpp} bits per pixel")));
        }
        let scanline_pad = u32::from(format.scanline_pad);
        if !matches!(scanline_pad, 8 | 16 | 32) {
            return Err(X11Error::UnsupportedVisual(format!("scanline pad {scanline_pad}")));
        }
        Ok(Self {
            bits_per_pixel: bpp,
            scanline_pad,
            msb_first,
            red: Channel::new(visual.red_mask, "red")?,
            green: Channel::new(visual.green_mask, "green")?,
            blue: Channel::new(visual.blue_mask, "blue")?,
        })
    }

    fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel / 8) as usize
    }

    /// Bytes per scanline on the wire (`width` pixels rounded up to the scanline pad).
    pub(crate) fn row_stride(&self, width: usize) -> usize {
        let bits = width * self.bits_per_pixel as usize;
        let pad = self.scanline_pad as usize;
        bits.div_ceil(pad) * pad / 8
    }

    #[inline]
    fn read_pixel(&self, px: &[u8]) -> u32 {
        // `px` is exactly `bytes_per_pixel()` long (guaranteed by `chunks_exact`).
        let mut v = 0u32;
        if self.msb_first {
            for &b in px {
                v = (v << 8) | u32::from(b);
            }
        } else {
            for &b in px.iter().rev() {
                v = (v << 8) | u32::from(b);
            }
        }
        v
    }

    fn is_bgra8_le(&self) -> bool {
        self.bits_per_pixel == 32
            && !self.msb_first
            && self.red.mask == 0x00ff_0000
            && self.green.mask == 0x0000_ff00
            && self.blue.mask == 0x0000_00ff
    }

    /// Decodes `rows` scanlines of `data` (wire layout) into `out` as tightly packed BGRA
    /// with opaque alpha. `out` must hold exactly `width * rows * 4` bytes.
    pub(crate) fn decode_rows(
        &self,
        data: &[u8],
        width: usize,
        rows: usize,
        out: &mut [u8],
    ) -> X11Result<()> {
        let stride = self.row_stride(width);
        let bpp = self.bytes_per_pixel();
        let row_bytes = width * bpp;
        let out_row = width * 4;
        let need = if rows == 0 { 0 } else { stride * (rows - 1) + row_bytes };
        if data.len() < need || out.len() != out_row * rows {
            return Err(X11Error::Malformed("image data shorter than its geometry implies"));
        }
        if rows == 0 || width == 0 {
            return Ok(());
        }
        let fast = self.is_bgra8_le();
        for (src_row, dst_row) in data.chunks(stride).zip(out.chunks_exact_mut(out_row)).take(rows)
        {
            let src_row = src_row.get(..row_bytes).ok_or(X11Error::Malformed("short scanline"))?;
            if fast {
                dst_row.copy_from_slice(src_row);
                for px in dst_row.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            } else {
                for (px, dst) in src_row.chunks_exact(bpp).zip(dst_row.chunks_exact_mut(4)) {
                    let v = self.read_pixel(px);
                    dst.copy_from_slice(&[
                        self.blue.extract(v),
                        self.green.extract(v),
                        self.red.extract(v),
                        255,
                    ]);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual(class: VisualClass, r: u32, g: u32, b: u32) -> Visualtype {
        Visualtype {
            visual_id: 0x21,
            class,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: r,
            green_mask: g,
            blue_mask: b,
        }
    }

    fn fmt(depth: u8, bpp: u8, pad: u8) -> Format {
        Format { depth, bits_per_pixel: bpp, scanline_pad: pad }
    }

    fn decode(layout: &PixelLayout, data: &[u8], w: usize, rows: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * rows * 4];
        layout.decode_rows(data, w, rows, &mut out).expect("decode");
        out
    }

    #[test]
    fn depth24_le_fast_path_forces_opaque_alpha() {
        let l = PixelLayout::new(
            &fmt(24, 32, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff),
        )
        .unwrap();
        assert!(l.is_bgra8_le());
        // B,G,R,padding(0x00 or garbage)
        let out = decode(&l, &[1, 2, 3, 0, 4, 5, 6, 0x7f], 2, 1);
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn depth32_alpha_channel_is_ignored() {
        let l = PixelLayout::new(
            &fmt(32, 32, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff),
        )
        .unwrap();
        let out = decode(&l, &[10, 20, 30, 0x40], 1, 1);
        assert_eq!(out, [10, 20, 30, 255]);
    }

    #[test]
    fn big_endian_server_32bpp() {
        // Value 0x00RRGGBB stored most significant byte first.
        let l = PixelLayout::new(
            &fmt(24, 32, 32),
            true,
            &visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff),
        )
        .unwrap();
        assert!(!l.is_bgra8_le());
        let out = decode(&l, &[0x00, 0xaa, 0xbb, 0xcc], 1, 1);
        assert_eq!(out, [0xcc, 0xbb, 0xaa, 255]);
    }

    #[test]
    fn rgb_ordered_visual_is_swizzled() {
        // Masks reversed (RGBA-in-memory servers): red in the low byte.
        let l = PixelLayout::new(
            &fmt(24, 32, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xff, 0xff00, 0xff0000),
        )
        .unwrap();
        let out = decode(&l, &[0xaa, 0xbb, 0xcc, 0], 1, 1);
        assert_eq!(out, [0xcc, 0xbb, 0xaa, 255]);
    }

    #[test]
    fn packed_24bpp_both_byte_orders() {
        let v = visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff);
        let le = PixelLayout::new(&fmt(24, 24, 32), false, &v).unwrap();
        // Row stride: 2 px * 24 bit = 48 bit, padded to 64 bit = 8 bytes.
        assert_eq!(le.row_stride(2), 8);
        let data = [3, 2, 1, 6, 5, 4, 0xee, 0xee, /* row 2 */ 9, 8, 7, 12, 11, 10, 0xee, 0xee];
        assert_eq!(decode(&le, &data, 2, 2), [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]);
        let be = PixelLayout::new(&fmt(24, 24, 32), true, &v).unwrap();
        // MSB first: bytes are R,G,B.
        assert_eq!(decode(&be, &[1, 2, 3, 4, 5, 6, 0, 0], 2, 1), [3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn rgb565_expands_to_full_range() {
        let l = PixelLayout::new(
            &fmt(16, 16, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xf800, 0x07e0, 0x001f),
        )
        .unwrap();
        // Row of 3 px = 48 bit padded to 64 bit.
        assert_eq!(l.row_stride(3), 8);
        let px = |v: u16| v.to_le_bytes();
        let mut data = Vec::new();
        for v in [0xffffu16, 0x0000, 0xf800] {
            data.extend_from_slice(&px(v));
        }
        data.extend_from_slice(&[0, 0]);
        let out = decode(&l, &data, 3, 1);
        assert_eq!(out, [255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn rgb555_middle_values_round() {
        let l = PixelLayout::new(
            &fmt(15, 16, 16),
            false,
            &visual(VisualClass::TRUE_COLOR, 0x7c00, 0x03e0, 0x001f),
        )
        .unwrap();
        // 5-bit 16 -> 16*255/31 = 131.6 -> 132.
        let v: u16 = (16 << 10) | (16 << 5) | 16;
        let out = decode(&l, &v.to_le_bytes(), 1, 1);
        assert_eq!(out, [132, 132, 132, 255]);
    }

    #[test]
    fn ten_bit_channels_scale_down() {
        let l = PixelLayout::new(
            &fmt(30, 32, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0x3ff0_0000, 0x000f_fc00, 0x0000_03ff),
        )
        .unwrap();
        let v: u32 = (0x3ff << 20) | (0x200 << 10) | 0;
        let out = decode(&l, &v.to_le_bytes(), 1, 1);
        assert_eq!(out, [0, 128, 255, 255]);
    }

    #[test]
    fn palette_visuals_are_rejected_with_a_clear_error() {
        let err = PixelLayout::new(&fmt(8, 8, 8), false, &visual(VisualClass::PSEUDO_COLOR, 0, 0, 0))
            .unwrap_err();
        assert!(err.to_string().contains("TrueColor"), "{err}");
    }

    #[test]
    fn bad_masks_are_rejected() {
        let v = visual(VisualClass::TRUE_COLOR, 0xf0f0, 0xff00, 0xff);
        assert!(PixelLayout::new(&fmt(24, 32, 32), false, &v).is_err());
        let v = visual(VisualClass::TRUE_COLOR, 0, 0xff00, 0xff);
        assert!(PixelLayout::new(&fmt(24, 32, 32), false, &v).is_err());
        let v = visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff);
        assert!(PixelLayout::new(&fmt(24, 8, 8), false, &v).is_err(), "8 bpp unsupported");
        assert!(PixelLayout::new(&fmt(24, 32, 64), false, &v).is_err());
    }

    #[test]
    fn short_data_is_an_error_not_a_panic() {
        let l = PixelLayout::new(
            &fmt(24, 32, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff),
        )
        .unwrap();
        let mut out = vec![0u8; 8];
        assert!(l.decode_rows(&[0; 7], 2, 1, &mut out).is_err());
        let mut wrong = vec![0u8; 4];
        assert!(l.decode_rows(&[0; 8], 2, 1, &mut wrong).is_err(), "output size mismatch");
        assert!(l.decode_rows(&[], 0, 0, &mut []).is_ok());
    }

    #[test]
    fn last_row_may_omit_padding() {
        let l = PixelLayout::new(
            &fmt(24, 24, 32),
            false,
            &visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff),
        )
        .unwrap();
        // 1 px wide: stride 4, row bytes 3; two rows need 4 + 3 bytes.
        let out = decode(&l, &[1, 2, 3, 0, 4, 5, 6], 1, 2);
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn stride_math_for_odd_widths() {
        let v = visual(VisualClass::TRUE_COLOR, 0xff0000, 0xff00, 0xff);
        let l = PixelLayout::new(&fmt(24, 24, 32), false, &v).unwrap();
        assert_eq!(l.row_stride(1), 4);
        assert_eq!(l.row_stride(3), 12);
        assert_eq!(l.row_stride(0), 0);
        let l = PixelLayout::new(&fmt(24, 24, 8), false, &v).unwrap();
        assert_eq!(l.row_stride(3), 9);
    }
}
