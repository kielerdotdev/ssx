//! The convert stage: source pixels to whatever the encoder wants.
//!
//! ```text
//!   HDR Rgba16F scRGB ──┬─ GPU (ssx-gpu): tonemap + NV12/I420 in one dispatch ─────────┐
//!                       └─ CPU: ssx_hdr::to_sdr8 ─┐                                     │
//!   SDR Bgra8/Rgba8 ────────────────────────────┴─ swscale (BT.709, limited) ─────────┴─► planar frame
//! ```
//!
//! * **HDR** frames are always tone-mapped exactly like screenshots ([`ssx_hdr`], with the
//!   GPU shader as a parity-tested accelerator), so a recorded HDR desktop looks like its
//!   screenshot. The GPU path applies when the frame already has the output size; a
//!   frame that must also be rescaled takes the CPU path (tonemap, then swscale scaling).
//! * **SDR** frames go through `swscale` with the BT.709 limited-range matrix, the same
//!   maths the GPU shader uses and the same tags the encoder writes into the stream, so
//!   both paths decode to the same colours.
//! * Frames whose size differs from the output size (a window that was resized, a region
//!   that changed) are scaled to the fixed output size instead of changing the encoder
//!   mid-stream.

use ssx_hdr::TonemapSettings;
use ssx_types::{Frame, PixelFormat, Size};

use crate::{
    encode::{EncoderInput, InputKind},
    error::{RecordError, Result},
};

/// Whether the GPU path is used for HDR frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuMode {
    /// Never use the GPU.
    Off,
    /// Use the GPU for HDR frames when an adapter is available (default).
    #[default]
    Auto,
}

/// Convert stage configuration.
#[derive(Debug, Clone, Copy)]
pub struct ConvertConfig {
    /// Output picture size (even for 4:2:0).
    pub out_size: Size,
    /// What the encoder wants.
    pub kind: InputKind,
    /// GPU policy.
    pub gpu: GpuMode,
    /// HDR tone mapping.
    pub tonemap: TonemapSettings,
}

/// Converts frames; owns cached swscale contexts and the GPU converter.
pub struct Converter {
    cfg: ConvertConfig,
    #[cfg(feature = "ffmpeg")]
    sws: Option<sws::Scaler>,
    #[cfg(feature = "gpu")]
    gpu: Option<gpu::GpuPath>,
    #[cfg(feature = "gpu")]
    gpu_tried: bool,
    /// Frames converted on the GPU / CPU (for the session report and tests).
    pub gpu_frames: u64,
    /// Frames tone-mapped on the CPU.
    pub cpu_hdr_frames: u64,
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("cfg", &self.cfg)
            .field("gpu_frames", &self.gpu_frames)
            .field("cpu_hdr_frames", &self.cpu_hdr_frames)
            .finish_non_exhaustive()
    }
}

impl Converter {
    /// Creates a converter. Nothing heavy happens until the first frame.
    pub fn new(cfg: ConvertConfig) -> Self {
        Self {
            cfg,
            #[cfg(feature = "ffmpeg")]
            sws: None,
            #[cfg(feature = "gpu")]
            gpu: None,
            #[cfg(feature = "gpu")]
            gpu_tried: false,
            gpu_frames: 0,
            cpu_hdr_frames: 0,
        }
    }

    /// `true` once the GPU path has been initialised and is in use.
    pub fn gpu_active(&self) -> bool {
        #[cfg(feature = "gpu")]
        {
            self.gpu.is_some()
        }
        #[cfg(not(feature = "gpu"))]
        {
            false
        }
    }

    /// Converts one frame.
    pub fn convert(&mut self, frame: &Frame) -> Result<EncoderInput> {
        let hdr = frame.format().is_float();
        match self.cfg.kind {
            InputKind::Rgba8 => self.to_rgba(frame),
            InputKind::Yuv420p | InputKind::Nv12 => {
                if hdr && frame.size() == self.cfg.out_size {
                    #[cfg(feature = "gpu")]
                    if let Some(planar) = self.try_gpu(frame) {
                        return Ok(EncoderInput::Planar(planar));
                    }
                }
                self.to_planar_cpu(frame)
            }
        }
    }

    /// 8-bit sRGB RGBA at the source size (GIF; gifski scales itself).
    fn to_rgba(&mut self, frame: &Frame) -> Result<EncoderInput> {
        if frame.format().is_float() {
            self.cpu_hdr_frames += 1;
        }
        let sdr = ssx_hdr::to_sdr8(frame, &self.cfg.tonemap)
            .map_err(|e| RecordError::Convert(e.to_string()))?;
        Ok(EncoderInput::Rgba(sdr))
    }

    #[cfg(feature = "gpu")]
    fn try_gpu(&mut self, frame: &Frame) -> Option<crate::encode::PlanarFrame> {
        if self.cfg.gpu == GpuMode::Off {
            return None;
        }
        if !self.gpu_tried {
            self.gpu_tried = true;
            match gpu::GpuPath::new() {
                Ok(g) => {
                    tracing::info!(adapter = %g.adapter, "HDR frames are tone-mapped on the GPU");
                    self.gpu = Some(g);
                }
                Err(e) => tracing::warn!(error = %e, "GPU tone mapping unavailable; using the CPU"),
            }
        }
        let g = self.gpu.as_ref()?;
        match g.convert(frame, &self.cfg.tonemap, self.cfg.kind) {
            Ok(p) => {
                self.gpu_frames += 1;
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "GPU conversion failed; switching to the CPU path");
                self.gpu = None;
                None
            }
        }
    }

    #[cfg(feature = "ffmpeg")]
    fn to_planar_cpu(&mut self, frame: &Frame) -> Result<EncoderInput> {
        let sdr;
        let src = if frame.format().is_float() {
            self.cpu_hdr_frames += 1;
            sdr = ssx_hdr::to_sdr8(frame, &self.cfg.tonemap)
                .map_err(|e| RecordError::Convert(e.to_string()))?;
            &sdr
        } else {
            frame
        };
        let scaler = match &mut self.sws {
            Some(s) => s,
            slot => slot.insert(sws::Scaler::new()),
        };
        let planar = scaler
            .to_planar(src, self.cfg.out_size, self.cfg.kind)
            .map_err(RecordError::Convert)?;
        Ok(EncoderInput::Planar(planar))
    }

    #[cfg(not(feature = "ffmpeg"))]
    fn to_planar_cpu(&mut self, _frame: &Frame) -> Result<EncoderInput> {
        Err(RecordError::NoFfmpeg)
    }
}

/// The output size for a source of `src` pixels: even dimensions (4:2:0), at least 16, and
/// scaled down (aspect preserved) to fit `max` if given.
pub fn output_size(src: Size, max: Option<Size>) -> Size {
    let mut w = f64::from(src.width.max(1));
    let mut h = f64::from(src.height.max(1));
    if let Some(m) = max {
        let mut s = 1.0f64;
        if m.width > 0 && src.width > m.width {
            s = s.min(f64::from(m.width) / w);
        }
        if m.height > 0 && src.height > m.height {
            s = s.min(f64::from(m.height) / h);
        }
        w = (w * s).round();
        h = (h * s).round();
    }
    let even = |v: f64| ((v as u32) & !1).max(16);
    Size::new(even(w), even(h))
}

/// Returns `PixelFormat` names for messages.
#[allow(dead_code)] // used in error messages by the swscale path only
fn format_name(f: PixelFormat) -> &'static str {
    match f {
        PixelFormat::Rgba8 => "rgba",
        PixelFormat::Bgra8 => "bgra",
        PixelFormat::Rgba16F => "rgba64f",
    }
}

#[cfg(feature = "ffmpeg")]
pub(crate) mod sws {
    //! swscale RGB to planar YUV, with the BT.709 matrix.

    #![allow(unsafe_code)] // one FFI call to set the colour matrix; see `set_matrix`.

    use ffmpeg_next::{
        ffi,
        format::Pixel,
        frame,
        software::scaling::{Context, Flags},
    };
    use ssx_types::{Frame, PixelFormat, Size};

    use crate::encode::{InputKind, PlanarFrame};

    /// Caches one swscale context per (source, destination) description.
    pub(crate) struct Scaler {
        ctx: Option<(Key, Context)>,
    }

    #[derive(PartialEq, Eq, Clone, Copy)]
    struct Key {
        src: (Pixel, u32, u32),
        dst: (Pixel, u32, u32),
    }

    /// Sets the swscale YUV matrix to BT.709 and the range flags (`src_full`/`dst_full`).
    /// `to_yuv` picks which side is YUV.
    pub(crate) fn set_matrix(ctx: &mut Context, to_yuv: bool) {
        let (src_range, dst_range) = if to_yuv { (1, 0) } else { (0, 1) };
        // SAFETY: `ctx.as_mut_ptr()` is the live SwsContext owned by `ctx`;
        // `sws_getCoefficients` returns pointers to static tables (valid forever) for the
        // documented constants; the remaining arguments are plain integers
        // (brightness 0, contrast and saturation 1.0 in 16.16 fixed point).
        unsafe {
            let bt709 = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709 as i32);
            let default = ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT as i32);
            let (inv, table) = if to_yuv { (default, bt709) } else { (bt709, default) };
            ffi::sws_setColorspaceDetails(
                ctx.as_mut_ptr(),
                inv,
                src_range,
                table,
                dst_range,
                0,
                1 << 16,
                1 << 16,
            );
        }
    }

    impl Scaler {
        pub(crate) fn new() -> Self {
            Self { ctx: None }
        }

        fn context(&mut self, key: Key) -> Result<&mut Context, String> {
            if self.ctx.as_ref().is_none_or(|(k, _)| *k != key) {
                let mut c = Context::get(
                    key.src.0,
                    key.src.1,
                    key.src.2,
                    key.dst.0,
                    key.dst.1,
                    key.dst.2,
                    Flags::BICUBIC,
                )
                .map_err(|e| format!("swscale context: {e}"))?;
                set_matrix(&mut c, true);
                self.ctx = Some((key, c));
            }
            self.ctx.as_mut().map(|(_, c)| c).ok_or_else(|| "swscale context vanished".into())
        }

        /// Converts an 8-bit RGB frame to planar YUV of `out` size.
        pub(crate) fn to_planar(
            &mut self,
            src: &Frame,
            out: Size,
            kind: InputKind,
        ) -> Result<PlanarFrame, String> {
            let src_fmt = match src.format() {
                PixelFormat::Bgra8 => Pixel::BGRA,
                PixelFormat::Rgba8 => Pixel::RGBA,
                other => {
                    return Err(format!("cannot convert {} directly", super::format_name(other)));
                }
            };
            let dst_fmt = match kind {
                InputKind::Nv12 => Pixel::NV12,
                InputKind::Yuv420p => Pixel::YUV420P,
                InputKind::Rgba8 => return Err("RGBA output does not use swscale".into()),
            };
            let key = Key {
                src: (src_fmt, src.width(), src.height()),
                dst: (dst_fmt, out.width, out.height),
            };
            let ctx = self.context(key)?;
            let mut input = frame::Video::new(src_fmt, src.width(), src.height());
            let dst_stride = input.stride(0);
            let row_bytes = src.width() as usize * 4;
            {
                let dst = input.data_mut(0);
                for y in 0..src.height() {
                    let d = dst
                        .get_mut(y as usize * dst_stride..y as usize * dst_stride + row_bytes)
                        .ok_or("AVFrame too small")?;
                    d.copy_from_slice(src.row(y));
                }
            }
            let mut output = frame::Video::empty();
            ctx.run(&input, &mut output).map_err(|e| format!("swscale: {e}"))?;
            let mut planar = PlanarFrame::new(kind, out.width, out.height);
            for i in 0..planar.plane_count() {
                let (off, stride, rows, row_bytes) = planar.plane_layout(i).ok_or("bad plane")?;
                let src_stride = output.stride(i);
                let plane = output.data(i);
                for r in 0..rows {
                    let s = plane
                        .get(r * src_stride..r * src_stride + row_bytes)
                        .ok_or("swscale output too small")?;
                    planar.data[off + r * stride..off + r * stride + row_bytes].copy_from_slice(s);
                }
            }
            Ok(planar)
        }
    }
}

#[cfg(feature = "gpu")]
mod gpu {
    //! The ssx-gpu path: tonemap and NV12/I420 conversion in one compute dispatch.

    use ssx_gpu::{
        ChromaSiting, ColorMatrix, GpuContext, YuvConverter, YuvLayout, YuvOptions, YuvRange,
    };
    use ssx_hdr::TonemapSettings;
    use ssx_types::Frame;

    use crate::encode::{InputKind, PlanarFrame};

    pub(super) struct GpuPath {
        conv: YuvConverter,
        pub(super) adapter: String,
    }

    impl GpuPath {
        pub(super) fn new() -> Result<Self, String> {
            let ctx = GpuContext::global().map_err(|e| e.to_string())?;
            let adapter = ctx.adapter_info().name;
            let conv = YuvConverter::new(ctx).map_err(|e| e.to_string())?;
            Ok(Self { conv, adapter })
        }

        pub(super) fn convert(
            &self,
            frame: &Frame,
            tonemap: &TonemapSettings,
            kind: InputKind,
        ) -> Result<PlanarFrame, String> {
            let opts = YuvOptions {
                layout: if kind == InputKind::Nv12 { YuvLayout::Nv12 } else { YuvLayout::I420 },
                matrix: ColorMatrix::Bt709,
                range: YuvRange::Limited,
                siting: ChromaSiting::Left,
            };
            let yuv = self.conv.tonemap_to_yuv(frame, tonemap, &opts).map_err(|e| e.to_string())?;
            let (width, height) = (yuv.width(), yuv.height());
            let mut planar = PlanarFrame::new(kind, width, height);
            let data = yuv.into_data();
            if data.len() != planar.data.len() {
                return Err(format!(
                    "GPU planes are {} bytes, expected {}",
                    data.len(),
                    planar.data.len()
                ));
            }
            planar.data = data;
            Ok(planar)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_is_even_and_bounded() {
        assert_eq!(output_size(Size::new(1920, 1080), None), Size::new(1920, 1080));
        assert_eq!(output_size(Size::new(1921, 1081), None), Size::new(1920, 1080));
        assert_eq!(output_size(Size::new(3, 3), None), Size::new(16, 16));
        assert_eq!(
            output_size(Size::new(3840, 2160), Some(Size::new(1920, 1080))),
            Size::new(1920, 1080)
        );
        assert_eq!(
            output_size(Size::new(3840, 1600), Some(Size::new(1920, 1080))),
            Size::new(1920, 800)
        );
        // Never upscales.
        assert_eq!(
            output_size(Size::new(800, 600), Some(Size::new(1920, 1080))),
            Size::new(800, 600)
        );
    }
}
