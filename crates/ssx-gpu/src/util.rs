//! Small GPU plumbing shared by the pipelines: alignment, readback, tiling.

use std::sync::mpsc;

use crate::{
    context::DeviceHandle,
    error::{GpuError, Result},
};

/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` as `usize`.
pub(crate) const ROW_ALIGN: usize = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;

/// Rounds `v` up to a multiple of `a` (`a > 0`).
pub(crate) const fn align_up(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

/// Overrides for how large a single GPU tile may be. Lower limits force more, smaller
/// tiles; they can only *reduce* what the device offers. Mostly useful for tests and for
/// machines where a huge staging allocation is unwelcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileLimits {
    /// Maximum tile width and height in pixels.
    pub max_dimension: u32,
    /// Maximum size in bytes of the per-tile readback buffer.
    pub max_buffer_bytes: u64,
}

impl Default for TileLimits {
    fn default() -> Self {
        Self { max_dimension: u32::MAX, max_buffer_bytes: 128 << 20 }
    }
}

/// A rectangle of the frame handled in one dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TileRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Splits `width` x `height` into row-major tiles of at most `tw` x `th`.
pub(crate) fn plan_tiles(width: u32, height: u32, tw: u32, th: u32) -> Vec<TileRect> {
    let mut out = Vec::new();
    let mut y = 0;
    while y < height {
        let h = th.min(height - y);
        let mut x = 0;
        while x < width {
            let w = tw.min(width - x);
            out.push(TileRect { x, y, w, h });
            x += w;
        }
        y += h;
    }
    out
}

/// Maps `len` bytes of `buffer` (which needs `MAP_READ`), waits for the GPU and runs `f`
/// on the bytes.
pub(crate) fn map_read<T>(
    h: &DeviceHandle,
    buffer: &wgpu::Buffer,
    len: u64,
    f: impl FnOnce(&[u8]) -> T,
) -> Result<T> {
    let slice = buffer.slice(0..len);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        // The receiver only disappears if we bailed out early; nothing to report then.
        let _ = tx.send(r);
    });
    h.device().poll(wgpu::PollType::wait_indefinitely())?;
    match rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(GpuError::Readback(e.to_string())),
        Err(_) => return Err(GpuError::Readback("map callback was dropped".into())),
    }
    let out = {
        let view = slice.get_mapped_range().map_err(|e| GpuError::Readback(e.to_string()))?;
        f(&view)
    };
    buffer.unmap();
    Ok(out)
}

/// Creates a buffer initialised with `data` (padded with zeros to a multiple of 4 bytes).
pub(crate) fn buffer_with_data(
    h: &DeviceHandle,
    label: &str,
    usage: wgpu::BufferUsages,
    data: &[u8],
) -> wgpu::Buffer {
    let size = align_up(data.len().max(4), 4) as u64;
    let buf = h.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    h.queue().write_buffer(&buf, 0, &pad4(data));
    buf
}

fn pad4(data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    v.resize(align_up(v.len().max(4), 4), 0);
    v
}

/// Compiles `wgsl` and builds a compute pipeline with an explicit bind group layout.
pub(crate) struct ComputeKernel {
    pub pipeline: wgpu::ComputePipeline,
    pub layout: wgpu::BindGroupLayout,
}

impl std::fmt::Debug for ComputeKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputeKernel").finish_non_exhaustive()
    }
}

/// Kinds of binding used by the kernels (all in bind group 0, compute stage only).
#[derive(Debug)]
#[derive(Clone, Copy)]
pub(crate) enum Bind {
    Uniform,
    StorageRead,
    StorageWrite,
    /// Sampled non-filterable float texture.
    Texture,
    /// Write-only storage texture of the given format.
    StorageTexture(wgpu::TextureFormat),
}

impl ComputeKernel {
    pub fn new(
        h: &DeviceHandle,
        label: &str,
        wgsl: &str,
        entry: &str,
        binds: &[(u32, Bind)],
    ) -> Result<Self> {
        h.scoped(|| {
            let device = h.device();
            let entries: Vec<wgpu::BindGroupLayoutEntry> = binds
                .iter()
                .map(|&(binding, ref b)| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: match *b {
                        Bind::Uniform => wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        Bind::StorageRead => wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        Bind::StorageWrite => wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: false },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        Bind::Texture => wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        Bind::StorageTexture(format) => wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                    },
                    count: None,
                })
                .collect();
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries: &entries,
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(wgsl.into()),
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            });
            Ok(Self { pipeline, layout })
        })
    }

    /// Builds a bind group from `(binding, resource)` pairs.
    pub fn bind_group(
        &self,
        h: &DeviceHandle,
        label: &str,
        resources: &[(u32, wgpu::BindingResource<'_>)],
    ) -> wgpu::BindGroup {
        let entries: Vec<wgpu::BindGroupEntry<'_>> = resources
            .iter()
            .map(|(binding, r)| wgpu::BindGroupEntry { binding: *binding, resource: r.clone() })
            .collect();
        h.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.layout,
            entries: &entries,
        })
    }
}

/// Number of workgroups of size `wg` needed to cover `n` items.
pub(crate) fn groups(n: u32, wg: u32) -> u32 {
    n.div_ceil(wg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align() {
        assert_eq!(align_up(1, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        assert_eq!(align_up(0, 4), 0);
    }

    #[test]
    fn tiles_cover_exactly() {
        for (w, h, tw, th) in [(10, 7, 4, 3), (1, 1, 8, 8), (16, 16, 16, 16), (17, 5, 16, 2)] {
            let tiles = plan_tiles(w, h, tw, th);
            let mut covered = vec![0u8; (w * h) as usize];
            for t in &tiles {
                assert!(t.w <= tw && t.h <= th && t.w > 0 && t.h > 0);
                for y in t.y..t.y + t.h {
                    for x in t.x..t.x + t.w {
                        covered[(y * w + x) as usize] += 1;
                    }
                }
            }
            assert!(covered.iter().all(|&c| c == 1), "{w}x{h} by {tw}x{th}");
        }
        assert!(plan_tiles(0, 5, 4, 4).is_empty());
    }
}
