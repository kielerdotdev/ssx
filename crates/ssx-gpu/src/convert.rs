//! GPU conversion to NV12 / I420 (and back, for tests).
//!
//! Three paths share one integer-arithmetic core (`shaders/yuv_core.wgsl`):
//!
//! * 8-bit RGBA/BGRA texture to YUV,
//! * HDR scRGB texture to YUV in **one dispatch** (tonemap fused into the fetch, so the
//!   result equals "tonemap to 8 bit, then convert" without a round trip through memory),
//! * YUV to RGBA (test/preview helper).
//!
//! Planes are written by compute shaders into one storage buffer (`array<u32>`, four
//! bytes per invocation write, because R8 storage textures are not baseline WebGPU) and
//! read back with a single staging copy. Frames taller than the buffer budget are
//! converted in horizontal *bands* of even height; bands are independent because the
//! chroma filter only spans rows `2cy, 2cy+1`.
//!
//! Output rows are padded to 4 bytes on the GPU and un-padded on readback, so the
//! returned [`YuvFrame`] is tightly packed at the even "coded" size.

use std::sync::{Arc, Mutex, PoisonError};

use bytemuck::{Pod, Zeroable};
use ssx_hdr::TonemapSettings;
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

use crate::{
    context::{DeviceHandle, GpuContext},
    error::{GpuError, Result},
    shaders::{RGB_TO_YUV_WGSL, TONEMAP_TO_YUV_WGSL, YUV_TO_RGB_WGSL},
    tonemap::{ParamsBytes, params_bytes},
    util::{Bind, ComputeKernel, TileLimits, align_up, groups, map_read},
    yuv::{YuvFrame, YuvLayout, YuvOptions, even},
};

/// What the source texture of a [`YuvPass`] contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvInput {
    /// `Rgba8Unorm` texture holding 8-bit sRGB.
    Rgba8,
    /// `Bgra8Unorm` texture holding 8-bit sRGB.
    Bgra8,
    /// `Rgba16Float` texture holding scRGB; tonemapped in the same dispatch.
    HdrScRgb,
}

impl YuvInput {
    /// The texture format this input kind expects.
    pub fn texture_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
            Self::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
            Self::HdrScRgb => wgpu::TextureFormat::Rgba16Float,
        }
    }

    fn is_hdr(self) -> bool {
        self == Self::HdrScRgb
    }
}

/// Where each plane lives in the output storage buffer of a [`YuvPass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Plane layout.
    pub layout: YuvLayout,
    /// Width of the planes (even).
    pub coded_width: u32,
    /// Height of the planes (even).
    pub coded_height: u32,
    /// Bytes per Y row (padded to a multiple of 4).
    pub y_stride: u32,
    /// Bytes per chroma row (padded to a multiple of 4): NV12's interleaved UV rows or
    /// I420's U and V rows.
    pub chroma_stride: u32,
    /// Byte offset of the Y plane (always 0).
    pub y_offset: u32,
    /// Byte offset of the UV plane (NV12) or U plane (I420).
    pub u_offset: u32,
    /// Byte offset of the V plane (I420; equals `u_offset` for NV12).
    pub v_offset: u32,
    /// Total bytes.
    pub total_bytes: u64,
}

impl PlaneLayout {
    /// Layout for a band of `src` visible pixels.
    pub fn new(src: Size, layout: YuvLayout) -> Self {
        let (cw, ch) = (even(src.width), even(src.height));
        let y_stride = align_up(cw as usize, 4);
        let chroma_stride = match layout {
            YuvLayout::Nv12 => y_stride,
            YuvLayout::I420 => align_up(cw as usize / 2, 4),
        };
        let y_size = y_stride * ch as usize;
        let c_size = chroma_stride * (ch as usize / 2);
        let (v_offset, total) = match layout {
            YuvLayout::Nv12 => (y_size, y_size + c_size),
            YuvLayout::I420 => (y_size + c_size, y_size + 2 * c_size),
        };
        Self {
            layout,
            coded_width: cw,
            coded_height: ch,
            y_stride: y_stride as u32,
            chroma_stride: chroma_stride as u32,
            y_offset: 0,
            u_offset: y_size as u32,
            v_offset: v_offset as u32,
            total_bytes: total as u64,
        }
    }
}

/// Uniform block of `yuv_core.wgsl` (`struct YuvParams`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct YuvParamsBytes {
    dims: [u32; 4],
    strides: [u32; 4],
    misc: [u32; 4],
    taps: [i32; 4],
    ky: [i32; 4],
    ku: [i32; 4],
    kv: [i32; 4],
    range: [i32; 4],
}

/// Uniform block of `yuv_to_rgb.wgsl` (`struct P`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct InverseParamsBytes {
    dims: [u32; 4],
    offs: [u32; 4],
    strides: [u32; 4],
    inv_a: [i32; 4],
    inv_b: [i32; 4],
}

#[derive(Debug)]
struct Pair {
    luma: ComputeKernel,
    chroma: ComputeKernel,
}

#[derive(Debug, Default)]
struct Kernels {
    generation: u64,
    rgb: Option<Arc<Pair>>,
    hdr: Option<Arc<Pair>>,
    inverse: Option<Arc<ComputeKernel>>,
}

/// Reusable "texture to planes" dispatch: bound to one input texture and owning the output
/// storage buffer. See [`YuvConverter::create_pass`].
#[derive(Debug)]
pub struct YuvPass {
    kernels: Arc<Pair>,
    handle: Arc<DeviceHandle>,
    input_kind: YuvInput,
    opts: YuvOptions,
    bind_luma: wgpu::BindGroup,
    bind_chroma: wgpu::BindGroup,
    yuv_params: wgpu::Buffer,
    tonemap_params: Option<wgpu::Buffer>,
    origin: Option<wgpu::Buffer>,
    out: wgpu::Buffer,
    capacity: u64,
}

impl YuvPass {
    /// The storage buffer receiving the planes (`STORAGE | COPY_SRC`); copy it to a staging
    /// buffer or hand it to an encoder that can import buffers.
    pub fn output(&self) -> &wgpu::Buffer {
        &self.out
    }

    /// The plane layout for a region of `size` visible pixels.
    pub fn plane_layout(&self, size: Size) -> PlaneLayout {
        PlaneLayout::new(size, self.opts.layout)
    }

    /// Sets the region converted by the next [`record`](Self::record): `size` visible
    /// pixels of the input texture, whose top-left pixel is at frame-global `origin`
    /// (dither phase for HDR input). Returns the resulting plane layout.
    pub fn set_region(&self, size: Size, origin: [u32; 2]) -> Result<PlaneLayout> {
        let l = self.plane_layout(size);
        if l.total_bytes > self.capacity {
            return Err(GpuError::TooLarge {
                width: size.width,
                height: size.height,
                reason: format!(
                    "planes need {} bytes, pass capacity is {}",
                    l.total_bytes, self.capacity
                ),
            });
        }
        let k = self.opts.coeffs();
        let p = YuvParamsBytes {
            dims: [size.width, size.height, l.coded_width, l.coded_height],
            strides: [l.y_stride, l.chroma_stride, l.y_offset, l.u_offset],
            misc: [l.v_offset, u32::from(self.opts.layout == YuvLayout::I420), k.shift, 0],
            taps: [k.taps[0], k.taps[1], k.taps[2], 0],
            ky: k.ky,
            ku: k.ku,
            kv: k.kv,
            range: k.range,
        };
        let q = self.handle.queue();
        q.write_buffer(&self.yuv_params, 0, bytemuck::bytes_of(&p));
        if let Some(o) = &self.origin {
            q.write_buffer(o, 0, bytemuck::bytes_of(&[origin[0], origin[1], 0u32, 0u32]));
        }
        Ok(l)
    }

    /// Sets tonemap parameters (HDR input only; ignored otherwise).
    pub fn set_tonemap(
        &self,
        settings: &TonemapSettings,
        sdr_white_nits: Option<f32>,
    ) -> Result<()> {
        let p = params_bytes(settings, sdr_white_nits)?;
        if let Some(buf) = &self.tonemap_params {
            self.handle.queue().write_buffer(buf, 0, bytemuck::bytes_of(&p));
        }
        Ok(())
    }

    /// Records both dispatches (Y, then chroma) for the region set by
    /// [`set_region`](Self::set_region).
    pub fn record(&self, encoder: &mut wgpu::CommandEncoder, size: Size) {
        let l = self.plane_layout(size);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ssx yuv"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.kernels.luma.pipeline);
        pass.set_bind_group(0, &self.bind_luma, &[]);
        pass.dispatch_workgroups(groups(l.y_stride / 4, 8), groups(l.coded_height, 8), 1);
        pass.set_pipeline(&self.kernels.chroma.pipeline);
        pass.set_bind_group(0, &self.bind_chroma, &[]);
        pass.dispatch_workgroups(groups(l.chroma_stride / 4, 8), groups(l.coded_height / 2, 8), 1);
    }

    /// The input kind this pass was created for.
    pub fn input_kind(&self) -> YuvInput {
        self.input_kind
    }
}

/// Cached objects for banded conversions of one allocation size.
#[derive(Debug)]
struct Band {
    generation: u64,
    kind: YuvInput,
    opts_layout: YuvLayout,
    alloc: (u32, u32),
    texture: wgpu::Texture,
    staging: wgpu::Buffer,
    /// Only the layout of the pass's options is baked into the buffers; the coefficients
    /// are replaced per call (`YuvPass::opts`), so any matrix/range/siting can reuse it.
    pass: YuvPass,
}

/// GPU RGB/HDR to NV12/I420 converter. `Send + Sync`; share one instance.
#[derive(Debug)]
pub struct YuvConverter {
    ctx: GpuContext,
    limits: Mutex<TileLimits>,
    kernels: Mutex<Kernels>,
    bands: Mutex<Vec<Band>>,
}

const BAND_SETS: usize = 4;

impl YuvConverter {
    /// Creates the converter (kernels are compiled lazily on first use of each path).
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        ctx.handle()?;
        Ok(Self {
            ctx: ctx.clone(),
            limits: Mutex::new(TileLimits::default()),
            kernels: Mutex::new(Kernels::default()),
            bands: Mutex::new(Vec::new()),
        })
    }

    /// Restricts band size, forcing banding of tall frames.
    pub fn set_tile_limits(&self, limits: TileLimits) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner) = limits;
        self.bands.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    fn pair(&self, h: &DeviceHandle, hdr: bool) -> Result<Arc<Pair>> {
        let mut k = self.kernels.lock().unwrap_or_else(PoisonError::into_inner);
        if k.generation != h.generation() {
            *k = Kernels { generation: h.generation(), ..Kernels::default() };
            self.bands.lock().unwrap_or_else(PoisonError::into_inner).clear();
        }
        let slot = if hdr { &mut k.hdr } else { &mut k.rgb };
        if let Some(p) = slot {
            return Ok(Arc::clone(p));
        }
        let (label, src) = if hdr {
            ("ssx tonemap+yuv", TONEMAP_TO_YUV_WGSL)
        } else {
            ("ssx rgb->yuv", RGB_TO_YUV_WGSL)
        };
        let mut binds = vec![(1, Bind::Uniform), (2, Bind::StorageWrite), (3, Bind::Texture)];
        if hdr {
            binds.insert(0, (0, Bind::Uniform));
            binds.push((4, Bind::Uniform));
        }
        let pair = Arc::new(Pair {
            luma: ComputeKernel::new(h, label, src, "luma", &binds)?,
            chroma: ComputeKernel::new(h, label, src, "chroma", &binds)?,
        });
        *slot = Some(Arc::clone(&pair));
        Ok(pair)
    }

    /// Binds a reusable pass to an existing `input` texture view of kind `kind` (see
    /// [`YuvInput::texture_format`]) able to convert regions up to `size` pixels. Nothing
    /// is allocated per frame afterwards: call [`YuvPass::set_region`] (and
    /// [`YuvPass::set_tonemap`] for HDR) then [`YuvPass::record`].
    pub fn create_pass(
        &self,
        input: &wgpu::TextureView,
        kind: YuvInput,
        size: Size,
        opts: &YuvOptions,
    ) -> Result<YuvPass> {
        let h = self.ctx.handle()?;
        let pair = self.pair(&h, kind.is_hdr())?;
        let cap = PlaneLayout::new(size, opts.layout).total_bytes;
        let limit = h.limits().max_storage_buffer_binding_size.min(h.limits().max_buffer_size);
        if cap > limit {
            return Err(GpuError::TooLarge {
                width: size.width,
                height: size.height,
                reason: format!("planes need {cap} bytes, device limit {limit}; convert in bands"),
            });
        }
        h.scoped(|| Ok(make_pass(&h, pair, input, kind, *opts, cap)))
    }

    /// Converts an 8-bit sRGB frame (`Rgba8`/`Bgra8`) to 4:2:0 on the GPU.
    pub fn rgba_to_yuv(&self, frame: &Frame, opts: &YuvOptions) -> Result<YuvFrame> {
        if !frame.is_sdr8() {
            return Err(GpuError::UnsupportedFrame {
                format: frame.format(),
                space: frame.color_space(),
                expected: "8-bit sRGB (Rgba8/Bgra8); use tonemap_to_yuv for HDR frames",
            });
        }
        self.convert(frame, None, opts)
    }

    /// Tonemaps an HDR `Rgba16F` scRGB frame and converts it in one dispatch.
    pub fn tonemap_to_yuv(
        &self,
        frame: &Frame,
        settings: &TonemapSettings,
        opts: &YuvOptions,
    ) -> Result<YuvFrame> {
        self.convert(frame, Some(settings), opts)
    }

    /// Converts any supported frame: 8-bit sRGB directly, HDR with `settings` (defaults
    /// when `None`). A lost device is recreated and the call retried once.
    pub fn convert(
        &self,
        frame: &Frame,
        settings: Option<&TonemapSettings>,
        opts: &YuvOptions,
    ) -> Result<YuvFrame> {
        let kind = match (frame.format(), frame.color_space()) {
            (PixelFormat::Rgba8, ColorSpace::Srgb) => YuvInput::Rgba8,
            (PixelFormat::Bgra8, ColorSpace::Srgb) => YuvInput::Bgra8,
            (PixelFormat::Rgba16F, ColorSpace::ScRgbLinear) => YuvInput::HdrScRgb,
            (format, space) => {
                return Err(GpuError::UnsupportedFrame {
                    format,
                    space,
                    expected: "Rgba8/Bgra8 sRGB or Rgba16F scRGB",
                });
            }
        };
        if frame.width() == 0 || frame.height() == 0 {
            return Err(GpuError::invalid("frame", "cannot convert an empty frame"));
        }
        let default_settings = TonemapSettings::default();
        let tonemap = if kind.is_hdr() {
            Some(params_bytes(settings.unwrap_or(&default_settings), frame.sdr_white_nits)?)
        } else {
            None
        };
        let mut attempt = 0;
        loop {
            let h = self.ctx.handle()?;
            match self.convert_once(&h, frame, kind, tonemap.as_ref(), *opts) {
                Err(e) if e.is_device_lost() && attempt == 0 => {
                    attempt += 1;
                    tracing::warn!("device lost during YUV conversion; retrying: {e}");
                }
                other => return other,
            }
        }
    }

    fn convert_once(
        &self,
        h: &Arc<DeviceHandle>,
        frame: &Frame,
        kind: YuvInput,
        tonemap: Option<&ParamsBytes>,
        opts: YuvOptions,
    ) -> Result<YuvFrame> {
        let (w, height) = (frame.width(), frame.height());
        let limits = *self.limits.lock().unwrap_or_else(PoisonError::into_inner);
        let max_dim = h.limits().max_texture_dimension_2d.min(limits.max_dimension);
        if w > max_dim {
            return Err(GpuError::TooLarge {
                width: w,
                height,
                reason: format!("width exceeds the maximum texture size {max_dim}"),
            });
        }
        let max_bytes = h
            .limits()
            .max_storage_buffer_binding_size
            .min(h.limits().max_buffer_size)
            .min(limits.max_buffer_bytes);
        // Rows per band: even, within the texture and buffer limits.
        let one_pair = PlaneLayout::new(Size::new(w, 2), opts.layout).total_bytes;
        let pairs = max_bytes / one_pair;
        if pairs == 0 {
            return Err(GpuError::TooLarge {
                width: w,
                height,
                reason: format!("two rows of planes need {one_pair} bytes, limit {max_bytes}"),
            });
        }
        let band_h = u32::try_from(pairs.saturating_mul(2))
            .unwrap_or(u32::MAX)
            .min(max_dim & !1)
            .min(even(height))
            .max(2);

        let (cw, ch) = (even(w) as usize, even(height) as usize);
        let mut data = vec![0u8; cw * ch * 3 / 2];
        let alloc = (
            (w.div_ceil(64) * 64).min(max_dim).max(w),
            (band_h.div_ceil(64) * 64).min(max_dim).max(band_h),
        );
        let mut band = self.take_band(h, kind, opts, alloc)?;
        let result = h.scoped(|| {
            if let (Some(t), Some(buf)) = (tonemap, band.pass.tonemap_params.as_ref()) {
                h.queue().write_buffer(buf, 0, bytemuck::bytes_of(t));
            }
            band.pass.opts = opts;
            let mut y0 = 0;
            while y0 < height {
                let rows = band_h.min(height - y0);
                run_band(h, &band, frame, y0, rows, &mut data, (cw, ch))?;
                y0 += rows;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.put_band(band);
                YuvFrame::new(&opts, w, height, data, frame.timestamp)
            }
            Err(e) => Err(e),
        }
    }

    fn take_band(
        &self,
        h: &Arc<DeviceHandle>,
        kind: YuvInput,
        opts: YuvOptions,
        alloc: (u32, u32),
    ) -> Result<Band> {
        let pair = self.pair(h, kind.is_hdr())?;
        {
            let mut bands = self.bands.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(i) = bands.iter().position(|b| {
                b.generation == h.generation()
                    && b.kind == kind
                    && b.opts_layout == opts.layout
                    && b.alloc == alloc
            }) {
                return Ok(bands.swap_remove(i));
            }
        }
        h.scoped(|| {
            let texture = h.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("ssx yuv input"),
                size: wgpu::Extent3d { width: alloc.0, height: alloc.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: kind.texture_format(),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            // Capacity for a full-width band of the allocated height.
            let cap = PlaneLayout::new(Size::new(alloc.0, alloc.1), opts.layout).total_bytes;
            let pass = make_pass(
                h,
                pair,
                &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                kind,
                opts,
                cap,
            );
            let staging = h.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("ssx yuv staging"),
                size: cap,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            Ok(Band {
                generation: h.generation(),
                kind,
                opts_layout: opts.layout,
                alloc,
                texture,
                staging,
                pass,
            })
        })
    }

    fn put_band(&self, b: Band) {
        let mut bands = self.bands.lock().unwrap_or_else(PoisonError::into_inner);
        if bands.len() >= BAND_SETS {
            bands.remove(0);
        }
        bands.push(b);
    }

    /// Converts 4:2:0 planes back to an `Rgba8` frame (nearest-neighbour chroma). Intended
    /// for tests and previews; no tiling, so the planes must fit one storage binding.
    pub fn yuv_to_rgba(&self, yuv: &YuvFrame) -> Result<Frame> {
        let h = self.ctx.handle()?;
        let kernel = {
            let mut k = self.kernels.lock().unwrap_or_else(PoisonError::into_inner);
            if k.generation != h.generation() {
                *k = Kernels { generation: h.generation(), ..Kernels::default() };
                self.bands.lock().unwrap_or_else(PoisonError::into_inner).clear();
            }
            if let Some(p) = &k.inverse {
                Arc::clone(p)
            } else {
                let p = Arc::new(ComputeKernel::new(
                    &h,
                    "ssx yuv->rgb",
                    YUV_TO_RGB_WGSL,
                    "main",
                    &[
                        (0, Bind::Uniform),
                        (1, Bind::StorageRead),
                        (2, Bind::StorageTexture(wgpu::TextureFormat::Rgba8Unorm)),
                    ],
                )?);
                k.inverse = Some(Arc::clone(&p));
                p
            }
        };
        let (w, hgt) = (yuv.width(), yuv.height());
        let limits = h.limits();
        if w.max(hgt) > limits.max_texture_dimension_2d
            || yuv.data().len() as u64 > limits.max_storage_buffer_binding_size
        {
            return Err(GpuError::TooLarge {
                width: w,
                height: hgt,
                reason: "planes or image exceed the device limits".into(),
            });
        }
        let k = yuv.options().coeffs();
        let (cw, ch) = (yuv.coded_width(), yuv.coded_height());
        let y_len = yuv.y_stride() * ch as usize;
        let c_len = yuv.chroma_stride() * yuv.chroma_rows();
        let p = InverseParamsBytes {
            dims: [w, hgt, cw, ch],
            offs: [
                0,
                y_len as u32,
                (y_len + c_len) as u32,
                u32::from(yuv.layout() == YuvLayout::I420),
            ],
            strides: [yuv.y_stride() as u32, yuv.chroma_stride() as u32, 0, 0],
            inv_a: k.inv_a,
            inv_b: k.inv_b,
        };
        let mut out = vec![0u8; w as usize * 4 * hgt as usize];
        h.scoped(|| {
            let dev = h.device();
            let params = crate::util::buffer_with_data(
                &h,
                "ssx yuv->rgb params",
                wgpu::BufferUsages::UNIFORM,
                bytemuck::bytes_of(&p),
            );
            let planes = crate::util::buffer_with_data(
                &h,
                "ssx yuv->rgb planes",
                wgpu::BufferUsages::STORAGE,
                yuv.data(),
            );
            let tex = dev.create_texture(&wgpu::TextureDescriptor {
                label: Some("ssx yuv->rgb out"),
                size: wgpu::Extent3d { width: w, height: hgt, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bg = kernel.bind_group(
                &h,
                "ssx yuv->rgb",
                &[
                    (0, params.as_entire_binding()),
                    (1, planes.as_entire_binding()),
                    (2, wgpu::BindingResource::TextureView(&view)),
                ],
            );
            let padded = align_up(w as usize * 4, crate::util::ROW_ALIGN);
            let staging = dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ssx yuv->rgb staging"),
                size: (padded * hgt as usize) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ssx yuv->rgb"),
            });
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("ssx yuv->rgb"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&kernel.pipeline);
                pass.set_bind_group(0, &bg, &[]);
                pass.dispatch_workgroups(groups(w, 8), groups(hgt, 8), 1);
            }
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded as u32),
                        rows_per_image: Some(hgt),
                    },
                },
                wgpu::Extent3d { width: w, height: hgt, depth_or_array_layers: 1 },
            );
            h.queue().submit([enc.finish()]);
            map_read(&h, &staging, (padded * hgt as usize) as u64, |bytes| {
                for (dst, src) in
                    out.chunks_exact_mut(w as usize * 4).zip(bytes.chunks_exact(padded))
                {
                    dst.copy_from_slice(&src[..w as usize * 4]);
                }
            })
        })?;
        let mut f = Frame::from_rgba8(w, hgt, out)?;
        f.timestamp = yuv.timestamp;
        Ok(f)
    }
}

fn make_pass(
    h: &Arc<DeviceHandle>,
    kernels: Arc<Pair>,
    input: &wgpu::TextureView,
    kind: YuvInput,
    opts: YuvOptions,
    capacity: u64,
) -> YuvPass {
    let dev = h.device();
    let uniform = |label, size: usize| {
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    let yuv_params = uniform("ssx yuv params", size_of::<YuvParamsBytes>());
    let (tonemap_params, origin) = if kind.is_hdr() {
        (
            Some(uniform("ssx tonemap params", size_of::<ParamsBytes>())),
            Some(uniform("ssx band origin", 16)),
        )
    } else {
        (None, None)
    };
    let out = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssx yuv planes"),
        size: capacity.max(4),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mut res: Vec<(u32, wgpu::BindingResource<'_>)> = vec![
        (1, yuv_params.as_entire_binding()),
        (2, out.as_entire_binding()),
        (3, wgpu::BindingResource::TextureView(input)),
    ];
    if let (Some(t), Some(o)) = (&tonemap_params, &origin) {
        res.push((0, t.as_entire_binding()));
        res.push((4, o.as_entire_binding()));
    }
    let bind_luma = kernels.luma.bind_group(h, "ssx yuv luma", &res);
    let bind_chroma = kernels.chroma.bind_group(h, "ssx yuv chroma", &res);
    drop(res);
    YuvPass {
        kernels,
        handle: Arc::clone(h),
        input_kind: kind,
        opts,
        bind_luma,
        bind_chroma,
        yuv_params,
        tonemap_params,
        origin,
        out,
        capacity,
    }
}

/// Uploads rows `[y0, y0 + rows)`, converts them and copies the planes into `data` (packed
/// at the coded size `(cw, ch)` of the whole frame).
fn run_band(
    h: &Arc<DeviceHandle>,
    band: &Band,
    frame: &Frame,
    y0: u32,
    rows: u32,
    data: &mut [u8],
    (cw, ch): (usize, usize),
) -> Result<()> {
    let stride = frame.stride();
    let bytes_per_row = u32::try_from(stride).map_err(|_| GpuError::TooLarge {
        width: frame.width(),
        height: frame.height(),
        reason: format!("row stride {stride} does not fit in 32 bits"),
    })?;
    let src = frame.data().get(y0 as usize * stride..).ok_or_else(|| {
        GpuError::invalid("frame", "pixel buffer shorter than its stride implies")
    })?;
    let size = Size::new(frame.width(), rows);
    h.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &band.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        src,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(rows),
        },
        wgpu::Extent3d { width: size.width, height: rows, depth_or_array_layers: 1 },
    );
    let layout = band.pass.set_region(size, [0, y0])?;
    let mut enc = h
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ssx yuv band") });
    band.pass.record(&mut enc, size);
    enc.copy_buffer_to_buffer(band.pass.output(), 0, &band.staging, 0, layout.total_bytes);
    h.queue().submit([enc.finish()]);

    let (bcw, bch) = (layout.coded_width as usize, layout.coded_height as usize);
    let row0 = y0 as usize;
    map_read(h, &band.staging, layout.total_bytes, |bytes| {
        let ys = layout.y_stride as usize;
        for r in 0..bch {
            let dst = (row0 + r) * cw;
            data[dst..dst + bcw].copy_from_slice(&bytes[r * ys..r * ys + bcw]);
        }
        let cs = layout.chroma_stride as usize;
        let uv_base = cw * ch;
        match layout.layout {
            YuvLayout::Nv12 => {
                let off = layout.u_offset as usize;
                for r in 0..bch / 2 {
                    let dst = uv_base + (row0 / 2 + r) * cw;
                    data[dst..dst + bcw].copy_from_slice(&bytes[off + r * cs..off + r * cs + bcw]);
                }
            }
            YuvLayout::I420 => {
                let (u_off, v_off) = (layout.u_offset as usize, layout.v_offset as usize);
                let (pw, plane) = (bcw / 2, cw / 2 * (ch / 2));
                for r in 0..bch / 2 {
                    let dst = (row0 / 2 + r) * (cw / 2);
                    let du = uv_base + dst;
                    let dv = uv_base + plane + dst;
                    data[du..du + pw].copy_from_slice(&bytes[u_off + r * cs..u_off + r * cs + pw]);
                    data[dv..dv + pw].copy_from_slice(&bytes[v_off + r * cs..v_off + r * cs + pw]);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;

    /// Member names/offsets and span of a WGSL struct.
    fn wgsl_members(src: &str, name: &str) -> (Vec<(String, u32)>, u32) {
        let module = naga::front::wgsl::parse_str(src).expect("shader parses");
        for (_, ty) in module.types.iter() {
            if ty.name.as_deref() == Some(name)
                && let naga::TypeInner::Struct { members, span } = &ty.inner
            {
                let m = members
                    .iter()
                    .map(|m| (m.name.clone().unwrap_or_default(), m.offset))
                    .collect();
                return (m, *span);
            }
        }
        panic!("struct {name} not found");
    }

    #[test]
    fn yuv_params_layout_matches_wgsl() {
        let (m, span) = wgsl_members(RGB_TO_YUV_WGSL, "YuvParams");
        let names: Vec<_> = m.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["dims", "strides", "misc", "taps", "ky", "ku", "kv", "range"]);
        for (i, (_, off)) in m.iter().enumerate() {
            assert_eq!(*off as usize, i * 16);
        }
        assert_eq!(span as usize, size_of::<YuvParamsBytes>());
    }

    #[test]
    fn inverse_params_layout_matches_wgsl() {
        let (m, span) = wgsl_members(YUV_TO_RGB_WGSL, "P");
        let names: Vec<_> = m.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["dims", "offs", "strides", "inv_a", "inv_b"]);
        assert_eq!(span as usize, size_of::<InverseParamsBytes>());
    }

    #[test]
    fn plane_layout_arithmetic() {
        let l = PlaneLayout::new(Size::new(5, 3), YuvLayout::Nv12);
        assert_eq!((l.coded_width, l.coded_height), (6, 4));
        assert_eq!((l.y_stride, l.chroma_stride), (8, 8));
        assert_eq!(l.total_bytes, 8 * 4 + 8 * 2);
        let l = PlaneLayout::new(Size::new(6, 4), YuvLayout::I420);
        assert_eq!((l.y_stride, l.chroma_stride), (8, 4));
        assert_eq!(l.u_offset, 32);
        assert_eq!(l.v_offset, 32 + 4 * 2);
        assert_eq!(l.total_bytes, 32 + 16);
        for w in 1..40 {
            for h in 1..12 {
                for layout in [YuvLayout::Nv12, YuvLayout::I420] {
                    let l = PlaneLayout::new(Size::new(w, h), layout);
                    assert_eq!(l.y_stride % 4, 0);
                    assert_eq!(l.chroma_stride % 4, 0);
                    assert_eq!(l.u_offset % 4, 0);
                    assert_eq!(l.v_offset % 4, 0);
                }
            }
        }
    }
}
