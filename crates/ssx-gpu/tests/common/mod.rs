//! Shared helpers for the integration tests.
#![allow(dead_code)] // each test binary uses a different subset

use std::sync::OnceLock;

use half::f16;
use ssx_gpu::GpuContext;
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

/// The shared context, or `None` (after printing why) when the machine has no adapter.
///
/// Set `SSX_GPU_REQUIRE=1` (CI does) to turn "no adapter" into a hard failure so a broken
/// runner cannot silently skip every test.
pub fn gpu() -> Option<&'static GpuContext> {
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| match GpuContext::new_default() {
        Ok(c) => {
            eprintln!("ssx-gpu tests: using {c:?}");
            Some(c)
        }
        Err(e) => {
            assert!(
                std::env::var_os("SSX_GPU_REQUIRE").is_none(),
                "SSX_GPU_REQUIRE is set but no GPU is available: {e}"
            );
            eprintln!("SKIPPED: no usable GPU adapter ({e})");
            None
        }
    });
    ctx.as_ref()
}

/// Deterministic xorshift64* generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Uniform in `[0, 1)`.
    pub fn f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }
}

/// Builds an `Rgba16F` scRGB frame from `f32` pixels with `pad` extra bytes per row
/// (filled with a recognisable pattern so any accidental read of padding shows up).
pub fn hdr_frame(w: u32, h: u32, pad: usize, sdr_white: f32, px: &[[f32; 4]]) -> Frame {
    assert_eq!(px.len(), (w * h) as usize);
    let stride = w as usize * 8 + pad;
    let mut data = vec![0xA5u8; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = px[y * w as usize + x];
            for (c, v) in p.iter().enumerate() {
                let o = y * stride + x * 8 + c * 2;
                data[o..o + 2].copy_from_slice(&f16::from_f32(*v).to_le_bytes());
            }
        }
    }
    let mut f = Frame::from_raw(
        Size::new(w, h),
        stride,
        PixelFormat::Rgba16F,
        ColorSpace::ScRgbLinear,
        data,
    )
    .unwrap();
    f.sdr_white_nits = Some(sdr_white);
    f
}

/// Same but from raw half-float bit patterns (for NaN/Inf tests).
pub fn hdr_frame_bits(w: u32, h: u32, sdr_white: f32, bits: &[[u16; 4]]) -> Frame {
    let mut data = Vec::new();
    for p in bits {
        for b in p {
            data.extend_from_slice(&b.to_le_bytes());
        }
    }
    let mut f = Frame::from_raw(
        Size::new(w, h),
        w as usize * 8,
        PixelFormat::Rgba16F,
        ColorSpace::ScRgbLinear,
        data,
    )
    .unwrap();
    f.sdr_white_nits = Some(sdr_white);
    f
}

/// Result of comparing two RGBA8 frames.
#[derive(Debug, Default, Clone, Copy)]
pub struct Diff {
    pub pixels: usize,
    pub channels_differing: usize,
    pub pixels_differing: usize,
    #[allow(clippy::struct_field_names)] // `d.max_diff` reads best at the call sites
    pub max_diff: u32,
    /// Pixels differing by more than one code.
    pub pixels_over_one: usize,
}

impl Diff {
    /// Fraction of pixels within one code.
    pub fn within_one(&self) -> f64 {
        1.0 - self.pixels_over_one as f64 / self.pixels.max(1) as f64
    }
}

pub fn diff_rgba(a: &Frame, b: &Frame) -> Diff {
    assert_eq!(a.size(), b.size());
    let mut d = Diff { pixels: (a.width() * a.height()) as usize, ..Diff::default() };
    for y in 0..a.height() {
        for (pa, pb) in a.row(y).chunks_exact(4).zip(b.row(y).chunks_exact(4)) {
            let mut any = false;
            let mut worst = 0;
            for c in 0..4 {
                let dd = u32::from(pa[c].abs_diff(pb[c]));
                if dd > 0 {
                    d.channels_differing += 1;
                    any = true;
                }
                worst = worst.max(dd);
            }
            d.pixels_differing += usize::from(any);
            d.pixels_over_one += usize::from(worst > 1);
            d.max_diff = d.max_diff.max(worst);
        }
    }
    d
}

/// An 8-bit sRGB frame (`Rgba8`, or `Bgra8` when `bgra`) with `pad` extra bytes per row.
pub fn rgba_frame(
    w: u32,
    h: u32,
    pad: usize,
    bgra: bool,
    f: impl Fn(u32, u32) -> [u8; 4],
) -> Frame {
    let stride = w as usize * 4 + pad;
    let mut data = vec![0x5Au8; stride * h as usize];
    for y in 0..h {
        for x in 0..w {
            let p = f(x, y);
            let px = if bgra { [p[2], p[1], p[0], p[3]] } else { p };
            let o = y as usize * stride + x as usize * 4;
            data[o..o + 4].copy_from_slice(&px);
        }
    }
    let format = if bgra { PixelFormat::Bgra8 } else { PixelFormat::Rgba8 };
    Frame::from_raw(Size::new(w, h), stride, format, ColorSpace::Srgb, data).unwrap()
}

/// SMPTE-style colour bars over a grey ramp: top 2/3 bars, bottom third ramp.
pub fn colour_bars(x: u32, y: u32, w: u32, h: u32) -> [u8; 4] {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    if y * 3 >= h * 2 {
        let v = (x * 255 / w.max(2).saturating_sub(1).max(1)).min(255) as u8;
        return [v, v, v, 255];
    }
    let b = BARS[(x * 8 / w.max(1)).min(7) as usize];
    [b[0], b[1], b[2], 255]
}

/// Smooth 2-D colour gradient.
pub fn gradient(x: u32, y: u32, w: u32, h: u32) -> [u8; 4] {
    let fx = x as f32 / w.max(2).saturating_sub(1).max(1) as f32;
    let fy = y as f32 / h.max(2).saturating_sub(1).max(1) as f32;
    [(fx * 255.0) as u8, (fy * 255.0) as u8, (((fx + fy) * 0.5) * 255.0) as u8, 255]
}
