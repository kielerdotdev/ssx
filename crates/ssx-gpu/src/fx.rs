//! Editor effects on the GPU: Gaussian blur, pixelate and resize (bilinear / Lanczos-3).
//!
//! All three operate on 8-bit sRGB frames (`Rgba8`/`Bgra8`, result `Rgba8`) and work in
//! the *encoded* sRGB domain, channel by channel, alpha included: that is what
//! redaction blur/pixelate in screenshot editors does, it is cheap, and it keeps opaque
//! screenshots opaque. (A linear-light blur would darken/brighten text differently and
//! needs a 16-bit intermediate; not worth it for redaction.)
//!
//! Blur and resize are separable and driven by *weight tables* built on the CPU
//! ([`Taps`]); the shader (`resample.wgsl`) only does the weighted sums. The CPU
//! references in [`cpu`] use the same tables, so parity is limited only by float
//! accumulation order (at most one code value; measured in `tests/fx_parity.rs`).
//! Pixelate uses integer sums and is bit-exact.
//!
//! A region is processed in isolation: samples are clamped to the region's edges, so
//! nothing outside the region leaks into a redaction.
//!
//! There is no tiling: the region (and the intermediate `Rgba32Float` texture) must fit
//! `max_texture_dimension_2d`; larger requests fail with [`GpuError::TooLarge`]. These
//! effects run on interaction-sized regions, not on 16K frames.

use std::sync::{Arc, Mutex, PoisonError};

use bytemuck::{Pod, Zeroable};
use ssx_types::{Frame, PixelFormat, Rect};

use crate::{
    context::{DeviceHandle, GpuContext},
    error::{GpuError, Result},
    shaders::{PIXELATE_WGSL, RESAMPLE_WGSL},
    util::{Bind, ComputeKernel, ROW_ALIGN, align_up, buffer_with_data, groups, map_read},
};

/// Largest Gaussian sigma accepted (radius 3 sigma taps: 3 * 256 * 2 + 1 taps).
pub const MAX_SIGMA: f32 = 256.0;
/// Largest pixelate block size accepted.
pub const MAX_BLOCK: u32 = 1024;

/// Resampling filter for [`GpuFx::resize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ResizeFilter {
    /// Triangle filter (bilinear when upscaling, area-aware when downscaling).
    Bilinear,
    /// Lanczos with a = 3. Sharper; the default for thumbnails.
    #[default]
    Lanczos3,
}

/// Per-output-index filter taps for one axis.
#[derive(Debug, Clone, PartialEq)]
pub struct Taps {
    /// Number of taps per output index (rows are zero padded to this length).
    pub taps: u32,
    /// First source index of each output index (may be negative or beyond the source for
    /// edge-replicating filters; the shader clamps).
    pub starts: Vec<i32>,
    /// `starts.len() * taps` weights, each row summing to one.
    pub weights: Vec<f32>,
}

impl Taps {
    /// Number of output samples along the axis.
    pub fn out_len(&self) -> usize {
        self.starts.len()
    }

    fn row(&self, i: usize) -> &[f32] {
        let n = self.taps as usize;
        &self.weights[i * n..(i + 1) * n]
    }
}

/// Gaussian blur taps for an axis of `len` samples: radius `ceil(3 sigma)`, weights
/// normalised, indices clamped at the edges by the consumer.
pub fn gaussian_taps(len: u32, sigma: f32) -> Result<Taps> {
    if !(sigma.is_finite() && sigma > 0.0 && sigma <= MAX_SIGMA) {
        return Err(GpuError::invalid(
            "sigma",
            format!("must be in (0, {MAX_SIGMA}], got {sigma}"),
        ));
    }
    let radius = ((3.0 * f64::from(sigma)).ceil() as i32).max(1);
    let two_s2 = 2.0 * f64::from(sigma) * f64::from(sigma);
    let raw: Vec<f64> = (-radius..=radius).map(|i| (-f64::from(i * i) / two_s2).exp()).collect();
    let sum: f64 = raw.iter().sum();
    let row: Vec<f32> = raw.iter().map(|w| (w / sum) as f32).collect();
    let taps = row.len() as u32;
    let mut weights = Vec::with_capacity(len as usize * row.len());
    let mut starts = Vec::with_capacity(len as usize);
    for i in 0..len as i32 {
        starts.push(i - radius);
        weights.extend_from_slice(&row);
    }
    Ok(Taps { taps, starts, weights })
}

fn lanczos3(x: f64) -> f64 {
    let x = x.abs();
    if x < 1e-9 {
        1.0
    } else if x < 3.0 {
        let px = std::f64::consts::PI * x;
        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
    } else {
        0.0
    }
}

fn triangle(x: f64) -> f64 {
    (1.0 - x.abs()).max(0.0)
}

/// Resize taps mapping `src_len` samples to `dst_len`. The filter support is widened by
/// the scale factor when downscaling (so every source sample contributes, as in area
/// averaging) and taps outside the source are dropped and the rest renormalised.
pub fn resize_taps(src_len: u32, dst_len: u32, filter: ResizeFilter) -> Taps {
    let scale = f64::from(src_len) / f64::from(dst_len);
    let fscale = scale.max(1.0);
    let (support, kernel): (f64, fn(f64) -> f64) = match filter {
        ResizeFilter::Bilinear => (1.0, triangle),
        ResizeFilter::Lanczos3 => (3.0, lanczos3),
    };
    let radius = support * fscale;
    let mut rows: Vec<(i32, Vec<f64>)> = Vec::with_capacity(dst_len as usize);
    for i in 0..dst_len {
        let center = (f64::from(i) + 0.5) * scale;
        let lo = ((center - radius).floor() as i64).max(0);
        let hi = ((center + radius).ceil() as i64).min(i64::from(src_len));
        let mut w: Vec<f64> =
            (lo..hi).map(|j| kernel((j as f64 + 0.5 - center) / fscale)).collect();
        let sum: f64 = w.iter().sum();
        if w.is_empty() || sum.abs() < 1e-12 {
            // Degenerate (cannot happen for these kernels): nearest sample.
            let j = (center.floor() as i64).clamp(0, i64::from(src_len) - 1);
            rows.push((j as i32, vec![1.0]));
        } else {
            w.iter_mut().for_each(|v| *v /= sum);
            rows.push((lo as i32, w));
        }
    }
    let taps = rows.iter().map(|(_, w)| w.len()).max().unwrap_or(1);
    let mut starts = Vec::with_capacity(rows.len());
    let mut weights = Vec::with_capacity(rows.len() * taps);
    for (s, w) in &rows {
        starts.push(*s);
        weights.extend(w.iter().map(|v| *v as f32));
        weights.extend(std::iter::repeat_n(0.0, taps - w.len()));
    }
    Taps { taps: taps as u32, starts, weights }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct PassParams {
    dims: [u32; 4],
    taps: [u32; 4],
}

#[derive(Debug, Default)]
struct Kernels {
    generation: u64,
    pass_h: Option<Arc<ComputeKernel>>,
    pass_v: Option<Arc<ComputeKernel>>,
    pixelate: Option<Arc<ComputeKernel>>,
}

/// GPU blur / pixelate / resize. `Send + Sync`; share one instance.
#[derive(Debug)]
pub struct GpuFx {
    ctx: GpuContext,
    kernels: Mutex<Kernels>,
}

fn require_sdr8(frame: &Frame) -> Result<()> {
    if frame.is_sdr8() {
        Ok(())
    } else {
        Err(GpuError::UnsupportedFrame {
            format: frame.format(),
            space: frame.color_space(),
            expected: "8-bit sRGB (Rgba8/Bgra8); tonemap HDR frames first",
        })
    }
}

/// Validates `region` against the frame and returns `(x, y, w, h)` as `usize`.
fn check_region(frame: &Frame, region: Rect) -> Result<(usize, usize, u32, u32)> {
    let bounds = Rect::new(0, 0, frame.width(), frame.height());
    if region.is_empty() {
        return Err(GpuError::invalid("region", "must not be empty"));
    }
    if region.intersect(bounds) != Some(region) {
        return Err(GpuError::invalid(
            "region",
            format!("{region:?} is not inside the {}x{} frame", frame.width(), frame.height()),
        ));
    }
    Ok((region.x as usize, region.y as usize, region.width, region.height))
}

impl GpuFx {
    /// Creates the effects runner (kernels compile lazily).
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        ctx.handle()?;
        Ok(Self { ctx: ctx.clone(), kernels: Mutex::new(Kernels::default()) })
    }

    fn kernel(
        &self,
        h: &DeviceHandle,
        pick: impl FnOnce(&mut Kernels) -> &mut Option<Arc<ComputeKernel>>,
        make: impl FnOnce() -> Result<ComputeKernel>,
    ) -> Result<Arc<ComputeKernel>> {
        let mut k = self.kernels.lock().unwrap_or_else(PoisonError::into_inner);
        if k.generation != h.generation() {
            *k = Kernels { generation: h.generation(), ..Kernels::default() };
        }
        let slot = pick(&mut k);
        if let Some(p) = slot {
            return Ok(Arc::clone(p));
        }
        let p = Arc::new(make()?);
        *slot = Some(Arc::clone(&p));
        Ok(p)
    }

    /// Gaussian blur of `region` (frame-local pixels) with standard deviation `sigma`
    /// pixels. Returns the whole frame as `Rgba8` with only the region changed.
    pub fn gaussian_blur_region(&self, frame: &Frame, region: Rect, sigma: f32) -> Result<Frame> {
        require_sdr8(frame)?;
        let (_, _, w, hgt) = check_region(frame, region)?;
        let tx = gaussian_taps(w, sigma)?;
        let ty = gaussian_taps(hgt, sigma)?;
        self.with_retry(|h| {
            let px = self.separable(h, frame, region, &tx, &ty)?;
            paste(frame, region, &px)
        })
    }

    /// Pixelates `region` into `block` x `block` squares of their average colour (blocks
    /// are aligned to the region's top-left corner; edge blocks are clipped).
    pub fn pixelate_region(&self, frame: &Frame, region: Rect, block: u32) -> Result<Frame> {
        require_sdr8(frame)?;
        let (_, _, w, hgt) = check_region(frame, region)?;
        if block == 0 || block > MAX_BLOCK {
            return Err(GpuError::invalid("block", format!("must be in 1..={MAX_BLOCK}")));
        }
        self.with_retry(|h| {
            let kernel = self.kernel(
                h,
                |k| &mut k.pixelate,
                || {
                    ComputeKernel::new(
                        h,
                        "ssx pixelate",
                        PIXELATE_WGSL,
                        "main",
                        &[
                            (0, Bind::Uniform),
                            (1, Bind::Texture),
                            (2, Bind::StorageTexture(wgpu::TextureFormat::Rgba8Unorm)),
                        ],
                    )
                },
            )?;
            let px = h.scoped(|| {
                check_dims(h, w, hgt, frame)?;
                let src = upload_region(h, frame, region);
                let out = create_tex(
                    h,
                    "ssx pixelate out",
                    (w, hgt),
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                );
                let params = buffer_with_data(
                    h,
                    "ssx pixelate params",
                    wgpu::BufferUsages::UNIFORM,
                    bytemuck::bytes_of(&[w, hgt, block, 0u32]),
                );
                let (sv, ov) = (view(&src), view(&out));
                let bg = kernel.bind_group(
                    h,
                    "ssx pixelate",
                    &[
                        (0, params.as_entire_binding()),
                        (1, wgpu::BindingResource::TextureView(&sv)),
                        (2, wgpu::BindingResource::TextureView(&ov)),
                    ],
                );
                let mut enc = encoder(h, "ssx pixelate");
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("ssx pixelate"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(&kernel.pipeline);
                    pass.set_bind_group(0, &bg, &[]);
                    pass.dispatch_workgroups(
                        groups(groups(w, block), 8),
                        groups(groups(hgt, block), 8),
                        1,
                    );
                }
                read_texture(h, enc, &out, (w, hgt))
            })?;
            paste(frame, region, &px)
        })
    }

    /// Resizes the whole frame to `width` x `height` (`Rgba8` result).
    pub fn resize(
        &self,
        frame: &Frame,
        width: u32,
        height: u32,
        filter: ResizeFilter,
    ) -> Result<Frame> {
        require_sdr8(frame)?;
        if width == 0 || height == 0 || frame.width() == 0 || frame.height() == 0 {
            return Err(GpuError::invalid("size", "source and target must be non-empty"));
        }
        let tx = resize_taps(frame.width(), width, filter);
        let ty = resize_taps(frame.height(), height, filter);
        let whole = Rect::new(0, 0, frame.width(), frame.height());
        let px = self.with_retry(|h| self.separable(h, frame, whole, &tx, &ty))?;
        let mut f = Frame::from_rgba8(width, height, px)?;
        f.timestamp = frame.timestamp;
        Ok(f)
    }

    fn with_retry<T>(&self, mut f: impl FnMut(&Arc<DeviceHandle>) -> Result<T>) -> Result<T> {
        let mut attempt = 0;
        loop {
            let h = self.ctx.handle()?;
            match f(&h) {
                Err(e) if e.is_device_lost() && attempt == 0 => {
                    attempt += 1;
                    tracing::warn!("device lost during effect; retrying: {e}");
                }
                other => return other,
            }
        }
    }

    /// Runs both passes over `region` of `frame` and returns tightly packed RGBA8.
    fn separable(
        &self,
        h: &Arc<DeviceHandle>,
        frame: &Frame,
        region: Rect,
        tx: &Taps,
        ty: &Taps,
    ) -> Result<Vec<u8>> {
        let (sw, sh) = (region.width, region.height);
        let (dw, dh) = (tx.out_len() as u32, ty.out_len() as u32);
        let kh = self.kernel(
            h,
            |k| &mut k.pass_h,
            || make_pass_kernel(h, "pass_h", 4, wgpu::TextureFormat::Rgba32Float),
        )?;
        let kv = self.kernel(
            h,
            |k| &mut k.pass_v,
            || make_pass_kernel(h, "pass_v", 5, wgpu::TextureFormat::Rgba8Unorm),
        )?;
        h.scoped(|| {
            check_dims(h, sw, sh, frame)?;
            check_dims(h, dw, dh, frame)?;
            check_dims(h, dw, sh, frame)?;
            let limit = h.limits().max_storage_buffer_binding_size;
            for t in [tx, ty] {
                if (t.weights.len() * 4) as u64 > limit {
                    return Err(GpuError::TooLarge {
                        width: frame.width(),
                        height: frame.height(),
                        reason: "filter weight table exceeds the storage binding limit".into(),
                    });
                }
            }
            let src = upload_region(h, frame, region);
            let inter = create_tex(
                h,
                "ssx fx intermediate",
                (dw, sh),
                wgpu::TextureFormat::Rgba32Float,
                wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            );
            let out = create_tex(
                h,
                "ssx fx out",
                (dw, dh),
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            );
            let storage = wgpu::BufferUsages::STORAGE;
            let uniform = wgpu::BufferUsages::UNIFORM;
            let ph = PassParams { dims: [sw, sh, dw, sh], taps: [tx.taps, 0, 0, 0] };
            let pv = PassParams { dims: [dw, sh, dw, dh], taps: [ty.taps, 0, 0, 0] };
            let b_ph = buffer_with_data(h, "ssx fx params h", uniform, bytemuck::bytes_of(&ph));
            let b_pv = buffer_with_data(h, "ssx fx params v", uniform, bytemuck::bytes_of(&pv));
            let b_sx =
                buffer_with_data(h, "ssx fx starts x", storage, bytemuck::cast_slice(&tx.starts));
            let b_wx =
                buffer_with_data(h, "ssx fx weights x", storage, bytemuck::cast_slice(&tx.weights));
            let b_sy =
                buffer_with_data(h, "ssx fx starts y", storage, bytemuck::cast_slice(&ty.starts));
            let b_wy =
                buffer_with_data(h, "ssx fx weights y", storage, bytemuck::cast_slice(&ty.weights));
            let (v_src, v_inter, v_out) = (view(&src), view(&inter), view(&out));
            let bg_h = kh.bind_group(
                h,
                "ssx fx h",
                &[
                    (0, b_ph.as_entire_binding()),
                    (1, b_sx.as_entire_binding()),
                    (2, b_wx.as_entire_binding()),
                    (3, wgpu::BindingResource::TextureView(&v_src)),
                    (4, wgpu::BindingResource::TextureView(&v_inter)),
                ],
            );
            let bg_v = kv.bind_group(
                h,
                "ssx fx v",
                &[
                    (0, b_pv.as_entire_binding()),
                    (1, b_sy.as_entire_binding()),
                    (2, b_wy.as_entire_binding()),
                    (3, wgpu::BindingResource::TextureView(&v_inter)),
                    (5, wgpu::BindingResource::TextureView(&v_out)),
                ],
            );
            let mut enc = encoder(h, "ssx fx");
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("ssx fx"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&kh.pipeline);
                pass.set_bind_group(0, &bg_h, &[]);
                pass.dispatch_workgroups(groups(dw, 8), groups(sh, 8), 1);
                pass.set_pipeline(&kv.pipeline);
                pass.set_bind_group(0, &bg_v, &[]);
                pass.dispatch_workgroups(groups(dw, 8), groups(dh, 8), 1);
            }
            read_texture(h, enc, &out, (dw, dh))
        })
    }
}

fn make_pass_kernel(
    h: &DeviceHandle,
    entry: &str,
    dst_binding: u32,
    dst_format: wgpu::TextureFormat,
) -> Result<ComputeKernel> {
    ComputeKernel::new(
        h,
        "ssx resample",
        RESAMPLE_WGSL,
        entry,
        &[
            (0, Bind::Uniform),
            (1, Bind::StorageRead),
            (2, Bind::StorageRead),
            (3, Bind::Texture),
            (dst_binding, Bind::StorageTexture(dst_format)),
        ],
    )
}

fn check_dims(h: &DeviceHandle, w: u32, hgt: u32, frame: &Frame) -> Result<()> {
    let max = h.limits().max_texture_dimension_2d;
    if w.max(hgt) > max {
        return Err(GpuError::TooLarge {
            width: frame.width(),
            height: frame.height(),
            reason: format!("a {w}x{hgt} texture exceeds the maximum {max}"),
        });
    }
    Ok(())
}

fn view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor::default())
}

fn encoder(h: &DeviceHandle, label: &str) -> wgpu::CommandEncoder {
    h.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) })
}

fn create_tex(
    h: &DeviceHandle,
    label: &str,
    (w, hgt): (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    h.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: hgt, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Uploads `region` of an 8-bit frame (rows copied with the frame's own stride).
fn upload_region(h: &DeviceHandle, frame: &Frame, region: Rect) -> wgpu::Texture {
    let format = if frame.format() == PixelFormat::Bgra8 {
        wgpu::TextureFormat::Bgra8Unorm
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let tex = create_tex(
        h,
        "ssx fx source",
        (region.width, region.height),
        format,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    let stride = frame.stride();
    let offset = region.y as usize * stride + region.x as usize * 4;
    // A stride that does not fit u32 is reported by wgpu as a validation error.
    h.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        frame.data().get(offset..).unwrap_or(&[]),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(u32::try_from(stride).unwrap_or(u32::MAX)),
            rows_per_image: Some(region.height),
        },
        wgpu::Extent3d { width: region.width, height: region.height, depth_or_array_layers: 1 },
    );
    tex
}

/// Copies `tex` (`Rgba8Unorm`) into a staging buffer, submits `enc` and returns tightly
/// packed RGBA8.
fn read_texture(
    h: &DeviceHandle,
    mut enc: wgpu::CommandEncoder,
    tex: &wgpu::Texture,
    (w, hgt): (u32, u32),
) -> Result<Vec<u8>> {
    let padded = align_up(w as usize * 4, ROW_ALIGN);
    let len = (padded * hgt as usize) as u64;
    let staging = h.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssx fx staging"),
        size: len,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
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
    let row = w as usize * 4;
    map_read(h, &staging, len, |bytes| {
        let mut out = Vec::with_capacity(row * hgt as usize);
        for src in bytes.chunks_exact(padded).take(hgt as usize) {
            out.extend_from_slice(&src[..row]);
        }
        out
    })
}

/// Returns `frame` as `Rgba8` with `pixels` (tightly packed RGBA8 of `region`'s size)
/// pasted at `region`.
fn paste(frame: &Frame, region: Rect, pixels: &[u8]) -> Result<Frame> {
    let mut out = frame.clone().into_rgba8()?;
    let row = region.width as usize * 4;
    for (r, src) in pixels.chunks_exact(row).enumerate() {
        let dst = out.row_mut(region.y as u32 + r as u32);
        let x = region.x as usize * 4;
        dst[x..x + row].copy_from_slice(src);
    }
    Ok(out)
}

/// CPU references for the effects (same tables and rounding as the shaders).
pub mod cpu {
    use ssx_types::{Frame, PixelFormat, Rect};

    use super::{
        GpuError, MAX_BLOCK, ResizeFilter, Result, Taps, check_region, gaussian_taps, paste,
        require_sdr8, resize_taps,
    };

    /// RGBA8 of `region` as tightly packed `f32` values 0..255 (Bgra swizzled to RGBA).
    fn region_pixels(frame: &Frame, region: Rect) -> Vec<[f32; 4]> {
        let mut v = Vec::with_capacity(region.width as usize * region.height as usize);
        for y in 0..region.height {
            let row = frame.row(region.y as u32 + y);
            for x in 0..region.width as usize {
                let o = (region.x as usize + x) * 4;
                let p = &row[o..o + 4];
                let px = if frame.format() == PixelFormat::Bgra8 {
                    [p[2], p[1], p[0], p[3]]
                } else {
                    [p[0], p[1], p[2], p[3]]
                };
                v.push(px.map(f32::from));
            }
        }
        v
    }

    fn filter_pass(
        src: &[[f32; 4]],
        (sw, sh): (usize, usize),
        t: &Taps,
        horizontal: bool,
    ) -> Vec<[f32; 4]> {
        let (dw, dh) = if horizontal { (t.out_len(), sh) } else { (sw, t.out_len()) };
        let mut out = Vec::with_capacity(dw * dh);
        for y in 0..dh {
            for x in 0..dw {
                let (i, last) = if horizontal { (x, sw as i32 - 1) } else { (y, sh as i32 - 1) };
                let (start, w) = (t.starts[i], t.row(i));
                let mut acc = [0.0f32; 4];
                for (k, wk) in w.iter().enumerate() {
                    let s = (start + k as i32).clamp(0, last) as usize;
                    let p = if horizontal { src[y * sw + s] } else { src[s * sw + x] };
                    for c in 0..4 {
                        acc[c] += wk * p[c];
                    }
                }
                out.push(acc);
            }
        }
        out
    }

    fn separable(frame: &Frame, region: Rect, tx: &Taps, ty: &Taps) -> Vec<u8> {
        let (sw, sh) = (region.width as usize, region.height as usize);
        let src = region_pixels(frame, region);
        let mid = filter_pass(&src, (sw, sh), tx, true);
        let out = filter_pass(&mid, (tx.out_len(), sh), ty, false);
        out.iter().flat_map(|p| p.map(|v| (v + 0.5).floor().clamp(0.0, 255.0) as u8)).collect()
    }

    /// CPU Gaussian blur of `region`; see [`GpuFx::gaussian_blur_region`](super::GpuFx).
    pub fn gaussian_blur_region(frame: &Frame, region: Rect, sigma: f32) -> Result<Frame> {
        require_sdr8(frame)?;
        let (_, _, w, h) = check_region(frame, region)?;
        let px = separable(frame, region, &gaussian_taps(w, sigma)?, &gaussian_taps(h, sigma)?);
        paste(frame, region, &px)
    }

    /// CPU pixelate of `region`; see [`GpuFx::pixelate_region`](super::GpuFx).
    pub fn pixelate_region(frame: &Frame, region: Rect, block: u32) -> Result<Frame> {
        require_sdr8(frame)?;
        let (_, _, w, h) = check_region(frame, region)?;
        if block == 0 || block > MAX_BLOCK {
            return Err(GpuError::invalid("block", format!("must be in 1..={MAX_BLOCK}")));
        }
        let src = region_pixels(frame, region);
        let mut out = vec![0u8; w as usize * h as usize * 4];
        for by in (0..h).step_by(block as usize) {
            for bx in (0..w).step_by(block as usize) {
                let (x1, y1) = ((bx + block).min(w), (by + block).min(h));
                let mut sum = [0u32; 4];
                for y in by..y1 {
                    for x in bx..x1 {
                        for c in 0..4 {
                            sum[c] += src[(y * w + x) as usize][c] as u32;
                        }
                    }
                }
                let n = (x1 - bx) * (y1 - by);
                let avg = sum.map(|s| ((s + n / 2) / n) as u8);
                for y in by..y1 {
                    for x in bx..x1 {
                        let o = (y * w + x) as usize * 4;
                        out[o..o + 4].copy_from_slice(&avg);
                    }
                }
            }
        }
        paste(frame, region, &out)
    }

    /// CPU resize; see [`GpuFx::resize`](super::GpuFx).
    pub fn resize(frame: &Frame, width: u32, height: u32, filter: ResizeFilter) -> Result<Frame> {
        require_sdr8(frame)?;
        if width == 0 || height == 0 || frame.width() == 0 || frame.height() == 0 {
            return Err(GpuError::invalid("size", "source and target must be non-empty"));
        }
        let tx = resize_taps(frame.width(), width, filter);
        let ty = resize_taps(frame.height(), height, filter);
        let whole = Rect::new(0, 0, frame.width(), frame.height());
        let mut f = Frame::from_rgba8(width, height, separable(frame, whole, &tx, &ty))?;
        f.timestamp = frame.timestamp;
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_rows_are_normalised_and_symmetric() {
        for sigma in [0.5f32, 1.0, 2.7, 10.0] {
            let t = gaussian_taps(5, sigma).unwrap();
            let row = t.row(2);
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-5, "{sigma}: {sum}");
            for i in 0..row.len() / 2 {
                assert!((row[i] - row[row.len() - 1 - i]).abs() < 1e-7);
            }
            assert_eq!(t.starts[2], 2 - (row.len() as i32 / 2));
        }
    }

    #[test]
    fn invalid_sigma_is_rejected() {
        for s in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_SIGMA + 1.0] {
            assert!(gaussian_taps(4, s).is_err(), "{s}");
        }
    }

    #[test]
    fn resize_rows_are_normalised_and_in_range() {
        for filter in [ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
            for (s, d) in [(100, 10), (10, 100), (7, 7), (1, 5), (5, 1), (4097, 64)] {
                let t = resize_taps(s, d, filter);
                assert_eq!(t.out_len(), d as usize);
                for i in 0..d as usize {
                    let sum: f32 = t.row(i).iter().sum();
                    assert!((sum - 1.0).abs() < 1e-4, "{filter:?} {s}->{d} row {i}: {sum}");
                    assert!(t.starts[i] >= 0 && (t.starts[i] as u32) < s);
                }
            }
        }
    }

    #[test]
    fn identity_resize_is_identity() {
        let t = resize_taps(9, 9, ResizeFilter::Lanczos3);
        for i in 0..9 {
            let row = t.row(i);
            let peak = row.iter().position(|w| *w > 0.99).expect("delta");
            assert_eq!(t.starts[i] + peak as i32, i as i32);
        }
    }
}
