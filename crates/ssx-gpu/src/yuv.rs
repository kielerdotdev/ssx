//! Video pixel formats (NV12 / I420): types, colour-matrix coefficients and the CPU
//! reference conversions that the GPU shaders are tested against.
//!
//! # Design decisions
//!
//! * **Integer maths, bit-exact parity.** All conversions use Q16 fixed-point
//!   coefficients and `i32` arithmetic, which WGSL and Rust evaluate identically, so the
//!   GPU output equals the CPU reference *exactly* for the same 8-bit input (no ulp
//!   tolerance needed). The coefficient rows are forced to sum to exactly the ideal value
//!   (chroma rows to zero) so grey stays exactly neutral (`U = V = 128`).
//! * **Odd sizes.** 4:2:0 needs even dimensions. Frames are padded to the next even
//!   size by *replicating the last column/row* (the "coded" size); the visible size is
//!   kept in [`YuvFrame`] so the encoder can crop (`crop_right`/`crop_bottom`) if it
//!   supports that. Planes are stored tightly packed at the coded size.
//! * **Chroma siting.** [`ChromaSiting::Center`] averages each 2x2 block (chroma sample
//!   between the four luma samples, as in JPEG/MPEG-1 and what "2x2 averaging" means).
//!   [`ChromaSiting::Left`] is MPEG-2/H.264/H.265's default siting (chroma co-sited
//!   horizontally with the even luma column, vertically centred), implemented as a
//!   `[1 2 1]` horizontal filter over two rows. Tell the encoder which one was used
//!   (`chroma_sample_location`), otherwise players assume `left` and a `Center` stream is
//!   shifted by half a luma pixel (invisible except on one-pixel coloured edges).
//! * **Colour matrices.** BT.709 (default, what players assume for HD) and BT.601, in
//!   limited (16-235 / 16-240, default) or full range.

use std::time::Duration;

use ssx_types::Size;

use crate::error::{GpuError, Result};

/// Plane layout of a 4:2:0 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum YuvLayout {
    /// Y plane followed by one interleaved UVUV... plane (what hardware encoders want).
    #[default]
    Nv12,
    /// Y, U and V planes (`yuv420p`, what x264/x265 want).
    I420,
}

/// RGB to YUV matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorMatrix {
    /// ITU-R BT.709 (HD video). Default.
    #[default]
    Bt709,
    /// ITU-R BT.601 (SD video).
    Bt601,
}

impl ColorMatrix {
    /// `(Kr, Kb)` luma weights.
    fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt601 => (0.299, 0.114),
        }
    }
}

/// Quantisation range of the YUV codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum YuvRange {
    /// Studio range: Y 16-235, chroma 16-240. Default.
    #[default]
    Limited,
    /// Full range 0-255 ("JPEG" range).
    Full,
}

/// Where the chroma samples sit relative to the luma grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChromaSiting {
    /// Centre of the 2x2 luma block (plain 2x2 average). Default.
    #[default]
    Center,
    /// Co-sited with the even luma column, vertically centred (H.264/H.265 default).
    Left,
}

/// Conversion options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct YuvOptions {
    /// Output plane layout.
    pub layout: YuvLayout,
    /// Colour matrix.
    pub matrix: ColorMatrix,
    /// Code range.
    pub range: YuvRange,
    /// Chroma siting.
    pub siting: ChromaSiting,
}

/// Fixed-point coefficients derived from [`YuvOptions`]; identical on CPU and GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Coeffs {
    /// `[r, g, b, bias]` in Q16 for Y, U, V (bias includes the +0.5 rounding term).
    pub ky: [i32; 4],
    pub ku: [i32; 4],
    pub kv: [i32; 4],
    /// `[y_min, y_max, c_min, c_max]`.
    pub range: [i32; 4],
    /// Column weights for source columns `2cx-1, 2cx, 2cx+1`.
    pub taps: [i32; 3],
    /// log2 of the weight sum (columns x 2 rows).
    pub shift: u32,
    /// Inverse (YUV to RGB): `[y_scale, rv, gu, gv]` and `[bu, y_offset, 0, 0]`, Q16.
    pub inv_a: [i32; 4],
    pub inv_b: [i32; 4],
}

fn q16(v: f64) -> i32 {
    (v * 65536.0).round() as i32
}

impl YuvOptions {
    pub(crate) fn coeffs(&self) -> Coeffs {
        let (kr, kb) = self.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, yo, cs, range) = match self.range {
            YuvRange::Limited => (219.0 / 255.0, 16, 224.0 / 255.0, [16, 235, 16, 240]),
            YuvRange::Full => (1.0, 0, 1.0, [0, 255, 0, 255]),
        };
        let bias = |offset: i32| (offset << 16) + 32768;

        // Row sums are forced to their exact ideal so white/grey hit exact codes.
        let ytotal = q16(ys);
        let (yr, yb) = (q16(kr * ys), q16(kb * ys));
        let ky = [yr, ytotal - yr - yb, yb, bias(yo)];

        let s_cb = cs / (2.0 * (1.0 - kb));
        let (ur, ub) = (q16(-kr * s_cb), q16(0.5 * cs));
        let ku = [ur, -ur - ub, ub, bias(128)];
        let s_cr = cs / (2.0 * (1.0 - kr));
        let (vr, vb) = (q16(0.5 * cs), q16(-kb * s_cr));
        let kv = [vr, -vr - vb, vb, bias(128)];

        let (taps, shift) = match self.siting {
            ChromaSiting::Center => ([0, 1, 1], 2),
            ChromaSiting::Left => ([1, 2, 1], 3),
        };
        let inv_a = [
            q16(1.0 / ys),
            q16(2.0 * (1.0 - kr) / cs),
            q16(-2.0 * kb * (1.0 - kb) / (kg * cs)),
            q16(-2.0 * kr * (1.0 - kr) / (kg * cs)),
        ];
        let inv_b = [q16(2.0 * (1.0 - kb) / cs), yo, 0, 0];
        Coeffs { ky, ku, kv, range, taps, shift, inv_a, inv_b }
    }
}

/// Rounds up to an even number (the coded size of a 4:2:0 frame).
pub const fn even(n: u32) -> u32 {
    n.div_ceil(2) * 2
}

/// A 4:2:0 frame with tightly packed planes at the coded (even) size.
#[derive(Clone, PartialEq)]
pub struct YuvFrame {
    layout: YuvLayout,
    matrix: ColorMatrix,
    range: YuvRange,
    siting: ChromaSiting,
    width: u32,
    height: u32,
    data: Vec<u8>,
    /// Capture timestamp carried over from the source frame.
    pub timestamp: Option<Duration>,
}

impl std::fmt::Debug for YuvFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YuvFrame")
            .field("layout", &self.layout)
            .field("size", &(self.width, self.height))
            .field("coded", &(self.coded_width(), self.coded_height()))
            .field("matrix", &self.matrix)
            .field("range", &self.range)
            .field("siting", &self.siting)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl YuvFrame {
    /// Total byte length of a frame of `width` x `height` visible pixels.
    pub fn byte_len(width: u32, height: u32) -> usize {
        even(width) as usize * even(height) as usize * 3 / 2
    }

    /// Wraps packed planes. `data` must be exactly [`YuvFrame::byte_len`] bytes.
    pub fn new(
        opts: &YuvOptions,
        width: u32,
        height: u32,
        data: Vec<u8>,
        timestamp: Option<Duration>,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(GpuError::invalid("size", "YUV frames cannot be empty"));
        }
        let need = Self::byte_len(width, height);
        if data.len() != need {
            return Err(GpuError::invalid(
                "data",
                format!("expected {need} bytes for {width}x{height}, got {}", data.len()),
            ));
        }
        Ok(Self {
            layout: opts.layout,
            matrix: opts.matrix,
            range: opts.range,
            siting: opts.siting,
            width,
            height,
            data,
            timestamp,
        })
    }

    /// The options this frame was produced with.
    pub fn options(&self) -> YuvOptions {
        YuvOptions {
            layout: self.layout,
            matrix: self.matrix,
            range: self.range,
            siting: self.siting,
        }
    }

    /// Plane layout.
    pub fn layout(&self) -> YuvLayout {
        self.layout
    }
    /// Visible width.
    pub fn width(&self) -> u32 {
        self.width
    }
    /// Visible height.
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Visible size.
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
    /// Width of the planes (visible width rounded up to even).
    pub fn coded_width(&self) -> u32 {
        even(self.width)
    }
    /// Height of the planes (visible height rounded up to even).
    pub fn coded_height(&self) -> u32 {
        even(self.height)
    }
    /// Bytes per row of the Y plane (tightly packed: the coded width).
    pub fn y_stride(&self) -> usize {
        self.coded_width() as usize
    }
    /// Bytes per row of the chroma plane(s): the coded width for NV12's interleaved UV
    /// plane, half of it for I420's U and V planes.
    pub fn chroma_stride(&self) -> usize {
        match self.layout {
            YuvLayout::Nv12 => self.coded_width() as usize,
            YuvLayout::I420 => self.coded_width() as usize / 2,
        }
    }
    /// Number of chroma rows.
    pub fn chroma_rows(&self) -> usize {
        self.coded_height() as usize / 2
    }
    /// All planes back to back.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    /// Consumes the frame, returning all planes back to back.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
    fn y_len(&self) -> usize {
        self.y_stride() * self.coded_height() as usize
    }
    /// The Y plane (`coded_width * coded_height` bytes).
    pub fn y(&self) -> &[u8] {
        &self.data[..self.y_len()]
    }
    /// NV12's interleaved UV plane; `None` for I420.
    pub fn uv(&self) -> Option<&[u8]> {
        (self.layout == YuvLayout::Nv12).then(|| &self.data[self.y_len()..])
    }
    /// I420's U plane; `None` for NV12.
    pub fn u(&self) -> Option<&[u8]> {
        let n = self.chroma_stride() * self.chroma_rows();
        (self.layout == YuvLayout::I420).then(|| &self.data[self.y_len()..self.y_len() + n])
    }
    /// I420's V plane; `None` for NV12.
    pub fn v(&self) -> Option<&[u8]> {
        let n = self.chroma_stride() * self.chroma_rows();
        (self.layout == YuvLayout::I420).then(|| &self.data[self.y_len() + n..self.y_len() + 2 * n])
    }
}

/// CPU reference conversions. Slow, exact, used by the tests and as a fallback.
pub mod cpu {
    use ssx_types::{Frame, PixelFormat};

    use super::{Coeffs, GpuError, Result, YuvFrame, YuvLayout, YuvOptions, even};

    /// RGB of pixel `(x, y)` of an 8-bit frame with coordinates clamped to the frame.
    fn rgb_at(frame: &Frame, x: i32, y: i32) -> [i32; 3] {
        let x = x.clamp(0, frame.width() as i32 - 1) as usize;
        let y = y.clamp(0, frame.height() as i32 - 1) as u32;
        let p = &frame.row(y)[x * 4..x * 4 + 4];
        match frame.format() {
            PixelFormat::Bgra8 => [i32::from(p[2]), i32::from(p[1]), i32::from(p[0])],
            _ => [i32::from(p[0]), i32::from(p[1]), i32::from(p[2])],
        }
    }

    fn luma(k: &Coeffs, rgb: [i32; 3]) -> u8 {
        let v = (k.ky[0] * rgb[0] + k.ky[1] * rgb[1] + k.ky[2] * rgb[2] + k.ky[3]) >> 16;
        v.clamp(k.range[0], k.range[1]) as u8
    }

    fn chroma(k: &Coeffs, sum: [i32; 3]) -> [u8; 2] {
        let f = |c: &[i32; 4]| {
            let v = (c[0] * sum[0] + c[1] * sum[1] + c[2] * sum[2] + (c[3] << k.shift))
                >> (16 + k.shift);
            v.clamp(k.range[2], k.range[3]) as u8
        };
        [f(&k.ku), f(&k.kv)]
    }

    /// Converts an `Rgba8`/`Bgra8` sRGB frame to 4:2:0.
    pub fn rgba_to_yuv(frame: &Frame, opts: &YuvOptions) -> Result<YuvFrame> {
        if !frame.is_sdr8() {
            return Err(GpuError::UnsupportedFrame {
                format: frame.format(),
                space: frame.color_space(),
                expected: "8-bit sRGB (Rgba8/Bgra8); tonemap HDR frames first",
            });
        }
        if frame.width() == 0 || frame.height() == 0 {
            return Err(GpuError::invalid("frame", "cannot convert an empty frame"));
        }
        let k = opts.coeffs();
        let (cw, ch) = (even(frame.width()) as usize, even(frame.height()) as usize);
        let mut data = Vec::with_capacity(cw * ch * 3 / 2);
        for y in 0..ch {
            for x in 0..cw {
                data.push(luma(&k, rgb_at(frame, x as i32, y as i32)));
            }
        }
        let chroma_at = |cx: usize, cy: usize| {
            let mut sum = [0i32; 3];
            for r in 0..2 {
                for (t, w) in k.taps.iter().enumerate() {
                    if *w != 0 {
                        let p = rgb_at(frame, (2 * cx + t) as i32 - 1, (2 * cy + r) as i32);
                        for c in 0..3 {
                            sum[c] += w * p[c];
                        }
                    }
                }
            }
            chroma(&k, sum)
        };
        match opts.layout {
            YuvLayout::Nv12 => {
                for cy in 0..ch / 2 {
                    for cx in 0..cw / 2 {
                        data.extend_from_slice(&chroma_at(cx, cy));
                    }
                }
            }
            YuvLayout::I420 => {
                let mut v_plane = Vec::with_capacity(cw * ch / 4);
                for cy in 0..ch / 2 {
                    for cx in 0..cw / 2 {
                        let [u, v] = chroma_at(cx, cy);
                        data.push(u);
                        v_plane.push(v);
                    }
                }
                data.extend_from_slice(&v_plane);
            }
        }
        YuvFrame::new(opts, frame.width(), frame.height(), data, frame.timestamp)
    }

    /// Tonemaps an HDR frame with [`ssx_hdr::to_sdr8`] then converts it (what the fused GPU
    /// shader computes).
    pub fn tonemap_to_yuv(
        frame: &Frame,
        settings: &ssx_hdr::TonemapSettings,
        opts: &YuvOptions,
    ) -> Result<YuvFrame> {
        let sdr = ssx_hdr::to_sdr8(frame, settings)?;
        rgba_to_yuv(&sdr, opts)
    }

    /// Converts back to an `Rgba8` frame of the visible size, with nearest-neighbour
    /// chroma upsampling.
    pub fn yuv_to_rgba(yuv: &YuvFrame) -> Result<Frame> {
        let k = yuv.options().coeffs();
        let (w, h) = (yuv.width() as usize, yuv.height() as usize);
        let cs = yuv.chroma_stride();
        let cw = yuv.coded_width() as usize / 2;
        let chroma_rows = yuv.chroma_rows();
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let yy = i32::from(yuv.y()[y * yuv.y_stride() + x]);
                let (cx, cy) = (x / 2, (y / 2).min(chroma_rows - 1));
                let (u, v) = match yuv.layout() {
                    YuvLayout::Nv12 => {
                        let o = cy * cs + cx * 2;
                        let uv = yuv.uv().unwrap_or(&[]);
                        (i32::from(uv[o]), i32::from(uv[o + 1]))
                    }
                    YuvLayout::I420 => {
                        let o = cy * cw + cx;
                        (i32::from(yuv.u().unwrap_or(&[])[o]), i32::from(yuv.v().unwrap_or(&[])[o]))
                    }
                };
                out.extend_from_slice(&yuv_pixel(&k, yy, u, v));
                out.push(255);
            }
        }
        let mut f = Frame::from_rgba8(yuv.width(), yuv.height(), out)?;
        f.timestamp = yuv.timestamp;
        Ok(f)
    }

    /// One YUV sample to RGB (the shader does exactly this).
    pub(crate) fn yuv_pixel(k: &Coeffs, y: i32, u: i32, v: i32) -> [u8; 3] {
        let (ys, rv, gu, gv) = (k.inv_a[0], k.inv_a[1], k.inv_a[2], k.inv_a[3]);
        let (bu, yo) = (k.inv_b[0], k.inv_b[1]);
        let yy = ys * (y - yo);
        let (du, dv) = (u - 128, v - 128);
        let r = (yy + rv * dv + 32768) >> 16;
        let g = (yy + gu * du + gv * dv + 32768) >> 16;
        let b = (yy + bu * du + 32768) >> 16;
        [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssx_types::Frame;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Frame {
        let data = (0..w * h).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
        Frame::from_rgba8(w, h, data).unwrap()
    }

    #[test]
    fn coefficient_rows_sum_exactly() {
        for matrix in [ColorMatrix::Bt709, ColorMatrix::Bt601] {
            for range in [YuvRange::Limited, YuvRange::Full] {
                let k = YuvOptions { matrix, range, ..Default::default() }.coeffs();
                assert_eq!(k.ku[0] + k.ku[1] + k.ku[2], 0);
                assert_eq!(k.kv[0] + k.kv[1] + k.kv[2], 0);
                let ys = if range == YuvRange::Limited { 219.0 / 255.0 } else { 1.0 };
                assert_eq!(k.ky[0] + k.ky[1] + k.ky[2], q16(ys));
            }
        }
    }

    #[test]
    fn known_colours_bt709_limited() {
        let o = YuvOptions::default();
        // (r,g,b) -> (Y,U,V) from the BT.709 limited-range definition.
        for (rgb, want) in [
            ([0, 0, 0], [16, 128, 128]),
            ([255, 255, 255], [235, 128, 128]),
            ([255, 0, 0], [63, 102, 240]),
            ([0, 255, 0], [173, 42, 26]),
            ([0, 0, 255], [32, 240, 118]),
            ([128, 128, 128], [126, 128, 128]),
        ] {
            let y = cpu::rgba_to_yuv(&solid(4, 4, rgb), &o).unwrap();
            let got = [y.y()[0], y.uv().unwrap()[0], y.uv().unwrap()[1]];
            for c in 0..3 {
                assert!((i32::from(got[c]) - want[c]).abs() <= 1, "{rgb:?}: {got:?} vs {want:?}");
            }
        }
    }

    #[test]
    fn full_range_and_bt601_known_values() {
        let o =
            YuvOptions { matrix: ColorMatrix::Bt601, range: YuvRange::Full, ..Default::default() };
        let y = cpu::rgba_to_yuv(&solid(2, 2, [255, 0, 0]), &o).unwrap();
        // Y = 0.299*255 = 76.2, Cb = 128 - 43.0 = 85, Cr = 255 (clamped 255.5)
        assert!((i32::from(y.y()[0]) - 76).abs() <= 1);
        assert!((i32::from(y.uv().unwrap()[0]) - 85).abs() <= 1);
        assert_eq!(y.uv().unwrap()[1], 255);
    }

    #[test]
    fn odd_sizes_are_padded_by_replication() {
        let f = solid(3, 5, [10, 200, 30]);
        for layout in [YuvLayout::Nv12, YuvLayout::I420] {
            let y = cpu::rgba_to_yuv(&f, &YuvOptions { layout, ..Default::default() }).unwrap();
            assert_eq!((y.coded_width(), y.coded_height()), (4, 6));
            assert_eq!(y.data().len(), 4 * 6 * 3 / 2);
            assert!(y.y().iter().all(|&v| v == y.y()[0]));
        }
    }

    #[test]
    fn i420_planes_match_nv12() {
        let data: Vec<u8> = (0..8 * 6 * 4).map(|i| (i * 37 % 251) as u8).collect();
        let f = Frame::from_rgba8(8, 6, data).unwrap();
        let a = cpu::rgba_to_yuv(&f, &YuvOptions::default()).unwrap();
        let b = cpu::rgba_to_yuv(&f, &YuvOptions { layout: YuvLayout::I420, ..Default::default() })
            .unwrap();
        assert_eq!(a.y(), b.y());
        let uv: Vec<u8> =
            b.u().unwrap().iter().zip(b.v().unwrap()).flat_map(|(u, v)| [*u, *v]).collect();
        assert_eq!(uv, a.uv().unwrap());
    }

    #[test]
    fn round_trip_of_flat_colours_is_close() {
        for matrix in [ColorMatrix::Bt709, ColorMatrix::Bt601] {
            for range in [YuvRange::Limited, YuvRange::Full] {
                let o = YuvOptions { matrix, range, ..Default::default() };
                for rgb in [[0, 0, 0], [255, 255, 255], [200, 30, 90], [12, 240, 130], [90, 90, 90]]
                {
                    let back = cpu::yuv_to_rgba(&cpu::rgba_to_yuv(&solid(4, 4, rgb), &o).unwrap())
                        .unwrap();
                    for c in 0..3 {
                        let d = i32::from(back.data()[c]) - i32::from(rgb[c]);
                        assert!(d.abs() <= 3, "{o:?} {rgb:?} -> {:?}", &back.data()[..3]);
                    }
                }
            }
        }
    }

    #[test]
    fn yuv_frame_validates_length() {
        let o = YuvOptions::default();
        assert!(YuvFrame::new(&o, 4, 4, vec![0; 23], None).is_err());
        assert!(YuvFrame::new(&o, 4, 4, vec![0; 24], None).is_ok());
        assert!(YuvFrame::new(&o, 0, 4, vec![], None).is_err());
    }
}
