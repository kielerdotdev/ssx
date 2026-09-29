//! HDR (`Rgba16F` scRGB) to SDR (`Rgba8` sRGB) tonemapping on the GPU.
//!
//! The per-pixel maths is the WGSL port of [`ssx_hdr`]'s reference (see
//! `shaders/tonemap_common.wgsl`); this module is the plumbing: upload, tiling, readback
//! and the reusable [`TonemapPass`] for callers that keep frames on the GPU.
//!
//! # Upload and readback
//!
//! The half-float frame is uploaded as an `Rgba16Float` texture. `Queue::write_texture`
//! takes the *frame's own stride* as `bytes_per_row` (it has no 256-byte alignment
//! requirement and repacks internally), so padded capture buffers are uploaded without an
//! intermediate copy. The result is written to an `Rgba8Unorm` storage texture and copied
//! into a `MAP_READ` staging buffer whose rows *are* padded to 256 bytes
//! (`COPY_BYTES_PER_ROW_ALIGNMENT`); rows are un-padded while copying into the output
//! frame.
//!
//! # Tiling
//!
//! Frames larger than the device's `max_texture_dimension_2d` (or than the staging-buffer
//! budget, see [`TileLimits`]) are processed in tiles. The dither noise is a function of
//! the *frame-global* pixel coordinate, so tiled and untiled results are identical.
//!
//! # Caching
//!
//! Textures, staging buffer, uniform buffers and bind group are cached per allocation
//! size (sizes are rounded up to a multiple of 64 so a window being resized does not
//! reallocate every frame). A call takes a cached set out of the cache while it runs, so
//! concurrent calls never block each other; they just use separate sets.

use std::sync::{Arc, Mutex, PoisonError};

use bytemuck::{Pod, Zeroable};
use ssx_hdr::{GpuParams, PixelParams, TonemapSettings};
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

use crate::{
    context::{DeviceHandle, GpuContext},
    error::{GpuError, Result},
    shaders::TONEMAP_WGSL,
    util::{
        Bind, ComputeKernel, ROW_ALIGN, TileLimits, TileRect, align_up, groups, map_read,
        plan_tiles,
    },
};

/// Byte image of [`GpuParams`] (which is not `Pod` because it lives in another crate).
/// The layout test in this module verifies both have identical size and field offsets.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct ParamsBytes {
    pub scale: f32,
    pub knee: f32,
    pub peak: f32,
    pub headroom: f32,
    pub mode: u32,
    pub dither: u32,
    pub width: f32,
    pub pad: f32,
    pub c: [f32; 4],
}

impl From<GpuParams> for ParamsBytes {
    fn from(g: GpuParams) -> Self {
        Self {
            scale: g.scale,
            knee: g.knee,
            peak: g.peak,
            headroom: g.headroom,
            mode: g.mode,
            dither: g.dither,
            width: g.width,
            pad: g.pad,
            c: g.c,
        }
    }
}

/// Per-dispatch tile uniform (`struct Tile` in `tonemap.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct TileBytes {
    pub origin: [u32; 2],
    pub size: [u32; 2],
}

/// Resolves settings + white level to the uniform bytes, validating both.
pub(crate) fn params_bytes(
    settings: &TonemapSettings,
    sdr_white_nits: Option<f32>,
) -> Result<ParamsBytes> {
    settings.validate()?;
    let nits = match sdr_white_nits {
        Some(n) if n.is_finite() && n > 0.0 => n,
        Some(n) => return Err(GpuError::InvalidSdrWhite(n)),
        None => {
            tracing::warn!("HDR frame has no sdr_white_nits; assuming 80 nits");
            80.0
        }
    };
    Ok(ParamsBytes::from(PixelParams::new(settings, nits).to_gpu()))
}

#[derive(Debug)]
struct Pipeline {
    kernel: ComputeKernel,
    generation: u64,
}

/// A tonemap dispatch bound to a fixed pair of textures, for callers that convert many
/// frames through the same textures (the recording path).
///
/// Create it once with [`GpuTonemapper::create_pass`], then per frame call
/// [`set_params`](TonemapPass::set_params) (only when settings changed) and
/// [`record`](TonemapPass::record) into your command encoder. Nothing is allocated per
/// frame.
#[derive(Debug)]
pub struct TonemapPass {
    pipeline: Arc<Pipeline>,
    handle: Arc<DeviceHandle>,
    bind_group: wgpu::BindGroup,
    params: wgpu::Buffer,
    tile: wgpu::Buffer,
}

impl TonemapPass {
    /// Uploads new tonemap parameters. Takes effect for command buffers submitted after
    /// this call (queue writes are ordered before later submissions), so do not change the
    /// parameters between `record` and `submit` of the same frame.
    pub fn set_params(
        &self,
        settings: &TonemapSettings,
        sdr_white_nits: Option<f32>,
    ) -> Result<()> {
        let p = params_bytes(settings, sdr_white_nits)?;
        self.handle.queue().write_buffer(&self.params, 0, bytemuck::bytes_of(&p));
        Ok(())
    }

    /// Sets the region processed by the next dispatch: `origin` is the frame-global
    /// position of the region (the dither phase), `size` its extent in pixels.
    pub fn set_region(&self, origin: [u32; 2], size: Size) {
        let t = TileBytes { origin, size: [size.width, size.height] };
        self.handle.queue().write_buffer(&self.tile, 0, bytemuck::bytes_of(&t));
    }

    /// Records the dispatch for a region of `size` pixels (must match
    /// [`set_region`](TonemapPass::set_region)).
    pub fn record(&self, encoder: &mut wgpu::CommandEncoder, size: Size) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ssx tonemap"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline.kernel.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.dispatch_workgroups(groups(size.width, 8), groups(size.height, 8), 1);
    }
}

/// Cached GPU objects for one allocation size.
#[derive(Debug)]
struct Scratch {
    generation: u64,
    alloc: (u32, u32),
    input: wgpu::Texture,
    output: wgpu::Texture,
    staging: wgpu::Buffer,
    pass: TonemapPass,
}

#[derive(Debug, Default)]
struct Cache {
    pipeline: Option<Arc<Pipeline>>,
    scratch: Vec<Scratch>,
}

/// How many scratch sets are kept alive between calls.
const CACHE_SETS: usize = 4;

/// GPU HDR to SDR tonemapper. `Send + Sync`; share one instance.
#[derive(Debug)]
pub struct GpuTonemapper {
    ctx: GpuContext,
    limits: Mutex<TileLimits>,
    cache: Mutex<Cache>,
}

impl GpuTonemapper {
    /// Creates the tonemapper and compiles its pipeline (so shader problems surface here).
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        let t = Self {
            ctx: ctx.clone(),
            limits: Mutex::new(TileLimits::default()),
            cache: Mutex::new(Cache::default()),
        };
        let h = ctx.handle()?;
        t.pipeline(&h)?;
        Ok(t)
    }

    /// Restricts the tile size, forcing tiling of large frames (see [`TileLimits`]).
    pub fn set_tile_limits(&self, limits: TileLimits) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner) = limits;
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).scratch.clear();
    }

    /// The context this tonemapper runs on.
    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    fn pipeline(&self, h: &Arc<DeviceHandle>) -> Result<Arc<Pipeline>> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = &cache.pipeline {
            if p.generation == h.generation() {
                return Ok(Arc::clone(p));
            }
        }
        cache.scratch.clear();
        let kernel = ComputeKernel::new(
            h,
            "ssx tonemap",
            TONEMAP_WGSL,
            "main",
            &[
                (0, Bind::Uniform),
                (1, Bind::Uniform),
                (2, Bind::Texture),
                (3, Bind::StorageTexture(wgpu::TextureFormat::Rgba8Unorm)),
            ],
        )?;
        let p = Arc::new(Pipeline { kernel, generation: h.generation() });
        cache.pipeline = Some(Arc::clone(&p));
        Ok(p)
    }

    /// Binds a pass to `input` (`Rgba16Float`, `TEXTURE_BINDING`) and `output`
    /// (`Rgba8Unorm`, `STORAGE_BINDING`). Both textures must belong to this context's
    /// current device.
    pub fn create_pass(
        &self,
        input: &wgpu::TextureView,
        output: &wgpu::TextureView,
    ) -> Result<TonemapPass> {
        let h = self.ctx.handle()?;
        let pipeline = self.pipeline(&h)?;
        h.scoped(|| Ok(make_pass(&h, pipeline, input, output)))
    }

    /// Converts `input` to `output` entirely on the GPU and submits the work. `input` must
    /// be `Rgba16Float` with `TEXTURE_BINDING`, `output` `Rgba8Unorm` with
    /// `STORAGE_BINDING`; the smaller of the two sizes is converted. Returns once the work
    /// is *submitted*, not finished; use the queue's ordering (or
    /// `Queue::on_submitted_work_done`) to synchronise.
    ///
    /// For a per-frame loop prefer [`create_pass`](Self::create_pass) so nothing is
    /// allocated per frame.
    pub fn tonemap_texture(
        &self,
        input: &wgpu::Texture,
        output: &wgpu::Texture,
        sdr_white_nits: f32,
        settings: &TonemapSettings,
    ) -> Result<()> {
        let params = params_bytes(settings, Some(sdr_white_nits))?;
        let h = self.ctx.handle()?;
        let pipeline = self.pipeline(&h)?;
        h.scoped(|| {
            let pass = make_pass(
                &h,
                pipeline,
                &input.create_view(&wgpu::TextureViewDescriptor::default()),
                &output.create_view(&wgpu::TextureViewDescriptor::default()),
            );
            let (a, b) = (input.size(), output.size());
            let size = Size::new(a.width.min(b.width), a.height.min(b.height));
            h.queue().write_buffer(&pass.params, 0, bytemuck::bytes_of(&params));
            pass.set_region([0, 0], size);
            let mut enc = h
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ssx tonemap") });
            pass.record(&mut enc, size);
            h.queue().submit([enc.finish()]);
            Ok(())
        })
    }

    /// Tonemaps `frame` on the GPU and reads the result back.
    ///
    /// Same contract as [`ssx_hdr::to_sdr8`]: `Rgba16F`/`ScRgbLinear` frames are converted
    /// (alpha forced to 255), 8-bit sRGB frames are returned as RGBA8 unchanged, origin,
    /// scale factor and timestamp are preserved. A lost device is recreated and the call
    /// retried once.
    pub fn tonemap(&self, frame: &Frame, settings: &TonemapSettings) -> Result<Frame> {
        settings.validate()?;
        match (frame.format(), frame.color_space()) {
            (PixelFormat::Rgba16F, ColorSpace::ScRgbLinear) => {
                let params = params_bytes(settings, frame.sdr_white_nits)?;
                let mut attempt = 0;
                loop {
                    let h = self.ctx.handle()?;
                    match self.tonemap_hdr(&h, frame, &params) {
                        Err(e) if e.is_device_lost() && attempt == 0 => {
                            attempt += 1;
                            tracing::warn!("device lost during tonemap; retrying: {e}");
                        }
                        other => return other,
                    }
                }
            }
            (PixelFormat::Rgba8 | PixelFormat::Bgra8, ColorSpace::Srgb) => {
                Ok(frame.clone().into_rgba8()?)
            }
            (format, space) => Err(GpuError::UnsupportedFrame {
                format,
                space,
                expected: "Rgba16F/ScRgbLinear or 8-bit sRGB",
            }),
        }
    }

    fn tonemap_hdr(
        &self,
        h: &Arc<DeviceHandle>,
        frame: &Frame,
        params: &ParamsBytes,
    ) -> Result<Frame> {
        let (w, hgt) = (frame.width(), frame.height());
        let mut out = vec![0u8; w as usize * 4 * hgt as usize];
        if !out.is_empty() {
            let pipeline = self.pipeline(h)?;
            let limits = *self.limits.lock().unwrap_or_else(PoisonError::into_inner);
            let (tw, th) = tile_size(h, &limits, w, hgt)?;
            let alloc = alloc_size(h, &limits, (w, hgt), (tw, th));
            let scratch = self.take_scratch(h, &pipeline, alloc)?;
            let result = h.scoped(|| {
                h.queue().write_buffer(&scratch.pass.params, 0, bytemuck::bytes_of(params));
                for tile in plan_tiles(w, hgt, tw, th) {
                    run_tile(h, &scratch, frame, tile, &mut out)?;
                }
                Ok(())
            });
            match result {
                Ok(()) => self.put_scratch(scratch),
                Err(e) => return Err(e),
            }
        }
        let mut res = Frame::from_raw(
            frame.size(),
            w as usize * 4,
            PixelFormat::Rgba8,
            ColorSpace::Srgb,
            out,
        )?;
        res.origin = frame.origin;
        res.scale_factor = frame.scale_factor;
        res.timestamp = frame.timestamp;
        res.sdr_white_nits = None;
        Ok(res)
    }

    fn take_scratch(
        &self,
        h: &Arc<DeviceHandle>,
        pipeline: &Arc<Pipeline>,
        alloc: (u32, u32),
    ) -> Result<Scratch> {
        {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(i) = cache
                .scratch
                .iter()
                .position(|s| s.alloc == alloc && s.generation == h.generation())
            {
                return Ok(cache.scratch.swap_remove(i));
            }
        }
        h.scoped(|| Ok(make_scratch(h, Arc::clone(pipeline), alloc)))
    }

    fn put_scratch(&self, s: Scratch) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.scratch.len() >= CACHE_SETS {
            cache.scratch.remove(0);
        }
        cache.scratch.push(s);
    }
}

fn make_pass(
    h: &Arc<DeviceHandle>,
    pipeline: Arc<Pipeline>,
    input: &wgpu::TextureView,
    output: &wgpu::TextureView,
) -> TonemapPass {
    let params = h.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssx tonemap params"),
        size: size_of::<ParamsBytes>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let tile = h.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssx tonemap tile"),
        size: size_of::<TileBytes>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = pipeline.kernel.bind_group(
        h,
        "ssx tonemap",
        &[
            (0, params.as_entire_binding()),
            (1, tile.as_entire_binding()),
            (2, wgpu::BindingResource::TextureView(input)),
            (3, wgpu::BindingResource::TextureView(output)),
        ],
    );
    TonemapPass { pipeline, handle: Arc::clone(h), bind_group, params, tile }
}

fn make_scratch(h: &Arc<DeviceHandle>, pipeline: Arc<Pipeline>, alloc: (u32, u32)) -> Scratch {
    let extent = wgpu::Extent3d { width: alloc.0, height: alloc.1, depth_or_array_layers: 1 };
    let tex = |label, format, usage| {
        h.device().create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let input = tex(
        "ssx tonemap input",
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    let output = tex(
        "ssx tonemap output",
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
    );
    let staging = h.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssx tonemap staging"),
        size: (align_up(alloc.0 as usize * 4, ROW_ALIGN) * alloc.1 as usize) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let pass = make_pass(
        h,
        pipeline,
        &input.create_view(&wgpu::TextureViewDescriptor::default()),
        &output.create_view(&wgpu::TextureViewDescriptor::default()),
    );
    Scratch { generation: h.generation(), alloc, input, output, staging, pass }
}

/// Largest tile (in pixels) that respects the device and user limits.
fn tile_size(h: &DeviceHandle, limits: &TileLimits, w: u32, hgt: u32) -> Result<(u32, u32)> {
    let max_dim = h.limits().max_texture_dimension_2d.min(limits.max_dimension).max(1);
    let max_bytes = h.limits().max_buffer_size.min(limits.max_buffer_bytes);
    let tw = w.min(max_dim);
    let row = align_up(tw as usize * 4, ROW_ALIGN) as u64;
    if row > max_bytes {
        return Err(GpuError::TooLarge {
            width: w,
            height: hgt,
            reason: format!("one padded row of a {tw}-pixel tile needs {row} bytes, limit {max_bytes}"),
        });
    }
    let rows_by_bytes = u32::try_from(max_bytes / row).unwrap_or(u32::MAX);
    Ok((tw, hgt.min(max_dim).min(rows_by_bytes).max(1)))
}

/// Texture allocation size for tiles of `tile` inside a frame of `frame` pixels: rounded
/// up to multiples of 64 when that still respects the limits.
fn alloc_size(h: &DeviceHandle, limits: &TileLimits, _frame: (u32, u32), tile: (u32, u32)) -> (u32, u32) {
    let max_dim = h.limits().max_texture_dimension_2d.min(limits.max_dimension).max(1);
    let max_bytes = h.limits().max_buffer_size.min(limits.max_buffer_bytes);
    let aw = (tile.0.div_ceil(64) * 64).min(max_dim);
    let ah = (tile.1.div_ceil(64) * 64).min(max_dim);
    let bytes = (align_up(aw as usize * 4, ROW_ALIGN) * ah as usize) as u64;
    if aw >= tile.0 && ah >= tile.1 && bytes <= max_bytes { (aw, ah) } else { tile }
}

fn run_tile(
    h: &Arc<DeviceHandle>,
    s: &Scratch,
    frame: &Frame,
    tile: TileRect,
    out: &mut [u8],
) -> Result<()> {
    let stride = frame.stride();
    let bytes_per_row = u32::try_from(stride).map_err(|_| GpuError::TooLarge {
        width: frame.width(),
        height: frame.height(),
        reason: format!("row stride {stride} does not fit in 32 bits"),
    })?;
    let offset = tile.y as usize * stride + tile.x as usize * 8;
    let data = frame
        .data()
        .get(offset..)
        .ok_or_else(|| GpuError::invalid("frame", "pixel buffer shorter than its stride implies"))?;
    let extent = wgpu::Extent3d { width: tile.w, height: tile.h, depth_or_array_layers: 1 };
    h.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &s.input,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(tile.h),
        },
        extent,
    );
    let size = Size::new(tile.w, tile.h);
    s.pass.set_region([tile.x, tile.y], size);
    let padded = align_up(tile.w as usize * 4, ROW_ALIGN);
    let mut enc = h
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ssx tonemap tile") });
    s.pass.record(&mut enc, size);
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &s.output,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &s.staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(tile.h),
            },
        },
        extent,
    );
    h.queue().submit([enc.finish()]);

    let out_stride = frame.width() as usize * 4;
    let row_bytes = tile.w as usize * 4;
    let len = (padded * tile.h as usize) as u64;
    map_read(h, &s.staging, len, |bytes| {
        for (r, src) in bytes.chunks_exact(padded).take(tile.h as usize).enumerate() {
            let dst = (tile.y as usize + r) * out_stride + tile.x as usize * 4;
            out[dst..dst + row_bytes].copy_from_slice(&src[..row_bytes]);
        }
    })
}
