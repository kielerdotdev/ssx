//! GPU NV12 / I420 conversion vs the CPU references in `ssx_gpu::yuv::cpu`.
//!
//! RGB to YUV uses integer maths on both sides, so the parity here is *exact* (0 code
//! differences) for the same 8-bit input. The fused HDR path is compared to
//! "CPU tonemap, then CPU convert", where the tonemap tolerance of the README applies.

mod common;

use common::{Rng, colour_bars, gpu, gradient, hdr_frame, rgba_frame};
use ssx_gpu::yuv::cpu;
use ssx_gpu::{
    ChromaSiting, ColorMatrix, GpuError, GpuTonemapper, TileLimits, YuvConverter, YuvFrame,
    YuvInput, YuvLayout, YuvOptions, YuvRange,
};
use ssx_hdr::TonemapSettings;
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

fn converter() -> Option<YuvConverter> {
    gpu().map(|c| YuvConverter::new(c).expect("converter"))
}

fn all_options() -> Vec<YuvOptions> {
    let mut v = Vec::new();
    for layout in [YuvLayout::Nv12, YuvLayout::I420] {
        for matrix in [ColorMatrix::Bt709, ColorMatrix::Bt601] {
            for range in [YuvRange::Limited, YuvRange::Full] {
                for siting in [ChromaSiting::Center, ChromaSiting::Left] {
                    v.push(YuvOptions { layout, matrix, range, siting });
                }
            }
        }
    }
    v
}

fn noise_frame(w: u32, h: u32, pad: usize, bgra: bool, seed: u64) -> Frame {
    let mut rng = Rng(seed);
    let px: Vec<[u8; 4]> = (0..w * h)
        .map(|_| [rng.next_u32() as u8, rng.next_u32() as u8, rng.next_u32() as u8, 255])
        .collect();
    rgba_frame(w, h, pad, bgra, |x, y| px[(y * w + x) as usize])
}

fn assert_same(label: &str, gpu: &YuvFrame, cpu: &YuvFrame) {
    assert_eq!(gpu.size(), cpu.size(), "{label}: size");
    assert_eq!(gpu.options(), cpu.options(), "{label}: options");
    assert_eq!(gpu.data().len(), cpu.data().len(), "{label}: length");
    if gpu.data() != cpu.data() {
        let i = gpu.data().iter().zip(cpu.data()).position(|(a, b)| a != b).unwrap();
        panic!(
            "{label}: first difference at byte {i} (y plane is {} bytes): gpu {} vs cpu {}",
            gpu.y().len(),
            gpu.data()[i],
            cpu.data()[i]
        );
    }
}

#[test]
fn rgb_to_yuv_is_bit_exact_for_every_option_combination() {
    let Some(c) = converter() else { return };
    let mut compared = 0;
    for opts in all_options() {
        for (w, h) in [(64u32, 48u32), (33, 17)] {
            for (name, bgra) in [("bars", false), ("gradient", true)] {
                let f = if name == "bars" {
                    rgba_frame(w, h, 4, bgra, |x, y| colour_bars(x, y, w, h))
                } else {
                    rgba_frame(w, h, 0, bgra, |x, y| gradient(x, y, w, h))
                };
                let g = c.rgba_to_yuv(&f, &opts).unwrap();
                assert_same(
                    &format!("{opts:?} {w}x{h} {name} bgra={bgra}"),
                    &g,
                    &cpu::rgba_to_yuv(&f, &opts).unwrap(),
                );
                compared += 1;
            }
        }
    }
    eprintln!(
        "RGB->YUV: {compared} GPU/CPU comparisons (2 layouts x 2 matrices x 2 ranges x 2 sitings x sizes x bars/gradient x rgba/bgra): 0 differences"
    );
}

#[test]
fn odd_tiny_and_awkward_sizes() {
    let Some(c) = converter() else { return };
    let sizes = [
        (1u32, 1u32),
        (1, 2),
        (2, 1),
        (3, 3),
        (3, 2),
        (5, 7),
        (7, 5),
        (9, 4),
        (15, 15),
        (16, 16),
        (17, 9),
        (31, 33),
        (33, 2),
        (2, 33),
        (63, 65),
        (100, 1),
        (1, 100),
        (255, 17),
        (257, 3),
    ];
    for (w, h) in sizes {
        for opts in [
            YuvOptions::default(),
            YuvOptions { layout: YuvLayout::I420, ..YuvOptions::default() },
            YuvOptions {
                siting: ChromaSiting::Left,
                matrix: ColorMatrix::Bt601,
                range: YuvRange::Full,
                layout: YuvLayout::I420,
            },
        ] {
            for pad in [0usize, 3, 100] {
                let f = noise_frame(w, h, pad, pad % 2 == 1, u64::from(w) * 1000 + u64::from(h));
                let g = c.rgba_to_yuv(&f, &opts).unwrap();
                assert_eq!(
                    (g.coded_width(), g.coded_height()),
                    (w.div_ceil(2) * 2, h.div_ceil(2) * 2)
                );
                assert_same(
                    &format!("{w}x{h} pad {pad} {opts:?}"),
                    &g,
                    &cpu::rgba_to_yuv(&f, &opts).unwrap(),
                );
            }
        }
    }
    eprintln!("odd/tiny sizes: {} sizes x 3 option sets x 3 paddings: 0 differences", sizes.len());
}

#[test]
fn banding_is_transparent() {
    let Some(ctx) = gpu() else { return };
    for opts in [
        YuvOptions::default(),
        YuvOptions { layout: YuvLayout::I420, siting: ChromaSiting::Left, ..YuvOptions::default() },
    ] {
        for (w, h) in [(37u32, 41u32), (64, 64), (5, 9)] {
            let f = noise_frame(w, h, 0, false, 99);
            let reference = cpu::rgba_to_yuv(&f, &opts).unwrap();
            // Budgets that force bands of 2, 4 and 10 rows (and a tiny texture limit).
            for limits in [
                TileLimits { max_dimension: u32::MAX, max_buffer_bytes: 1 },
                TileLimits { max_dimension: 4, max_buffer_bytes: 128 << 20 },
                TileLimits { max_dimension: 10, max_buffer_bytes: 128 << 20 },
            ] {
                let c = YuvConverter::new(ctx).unwrap();
                c.set_tile_limits(limits);
                match c.rgba_to_yuv(&f, &opts) {
                    Ok(g) => assert_same(&format!("{w}x{h} {limits:?} {opts:?}"), &g, &reference),
                    // A width above the (artificial) texture limit or a budget that cannot
                    // hold two rows is a clean error.
                    Err(GpuError::TooLarge { .. }) => {
                        assert!(
                            limits.max_buffer_bytes == 1 || w > limits.max_dimension,
                            "{w}x{h} {limits:?}"
                        );
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            // Bands that do split the frame: width fits, rows do not.
            let c = YuvConverter::new(ctx).unwrap();
            let per_pair = ssx_gpu::PlaneLayout::new(Size::new(w, 2), opts.layout).total_bytes;
            c.set_tile_limits(TileLimits {
                max_dimension: u32::MAX,
                max_buffer_bytes: per_pair * 2,
            });
            assert_same(
                &format!("{w}x{h} 4-row bands {opts:?}"),
                &c.rgba_to_yuv(&f, &opts).unwrap(),
                &reference,
            );
            c.set_tile_limits(TileLimits { max_dimension: u32::MAX, max_buffer_bytes: per_pair });
            assert_same(
                &format!("{w}x{h} 2-row bands {opts:?}"),
                &c.rgba_to_yuv(&f, &opts).unwrap(),
                &reference,
            );
        }
    }
}

#[test]
fn plane_accessors_and_byte_lengths() {
    let Some(c) = converter() else { return };
    let f = noise_frame(7, 5, 0, false, 1);
    let nv = c.rgba_to_yuv(&f, &YuvOptions::default()).unwrap();
    assert_eq!((nv.width(), nv.height(), nv.coded_width(), nv.coded_height()), (7, 5, 8, 6));
    assert_eq!(nv.y().len(), 8 * 6);
    assert_eq!(nv.uv().unwrap().len(), 8 * 3);
    assert!(nv.u().is_none() && nv.v().is_none());
    assert_eq!(nv.data().len(), YuvFrame::byte_len(7, 5));
    let i4 = c
        .rgba_to_yuv(&f, &YuvOptions { layout: YuvLayout::I420, ..YuvOptions::default() })
        .unwrap();
    assert_eq!(i4.u().unwrap().len(), 4 * 3);
    assert_eq!(i4.v().unwrap().len(), 4 * 3);
    assert!(i4.uv().is_none());
    assert_eq!((i4.y_stride(), i4.chroma_stride(), i4.chroma_rows()), (8, 4, 3));
}

#[test]
fn yuv_to_rgba_matches_cpu_and_round_trips() {
    let Some(c) = converter() else { return };
    for opts in all_options() {
        for (w, h) in [(64u32, 48u32), (33, 17), (5, 3)] {
            let f = rgba_frame(w, h, 0, false, |x, y| colour_bars(x, y, w, h));
            let yuv = cpu::rgba_to_yuv(&f, &opts).unwrap();
            let gpu_rgb = c.yuv_to_rgba(&yuv).unwrap();
            let cpu_rgb = cpu::yuv_to_rgba(&yuv).unwrap();
            assert_eq!(gpu_rgb.data(), cpu_rgb.data(), "yuv->rgb {opts:?} {w}x{h}");
        }
    }
    // Round trip of smooth content (chroma subsampling loses little on a gradient) and of
    // flat colour bars away from the bar edges.
    let (w, h) = (128u32, 96u32);
    for opts in all_options() {
        let f = rgba_frame(w, h, 0, false, |x, y| gradient(x, y, w, h));
        let back = c.yuv_to_rgba(&c.rgba_to_yuv(&f, &opts).unwrap()).unwrap();
        let worst = f
            .data()
            .chunks_exact(4)
            .zip(back.data().chunks_exact(4))
            .flat_map(|(a, b)| (0..3).map(move |i| i32::from(a[i]).abs_diff(i32::from(b[i]))))
            .max()
            .unwrap();
        assert!(worst <= 4, "gradient round trip {opts:?}: worst error {worst}");
        let bars = rgba_frame(w, h, 0, false, |x, y| colour_bars(x, y, w, h));
        let back = c.yuv_to_rgba(&c.rgba_to_yuv(&bars, &opts).unwrap()).unwrap();
        let mut worst = 0;
        for y in 0..h * 2 / 3 {
            for x in 0..w {
                // Skip pixels near a bar boundary (chroma is shared by 2x2 / [1 2 1] pixels).
                let edge = (x % (w / 8)).min(w / 8 - x % (w / 8)) < 3;
                if !edge {
                    let (a, b) =
                        (&bars.row(y)[x as usize * 4..][..3], &back.row(y)[x as usize * 4..][..3]);
                    worst = worst.max(
                        (0..3).map(|i| i32::from(a[i]).abs_diff(i32::from(b[i]))).max().unwrap(),
                    );
                }
            }
        }
        assert!(worst <= 4, "colour bar round trip {opts:?}: worst error {worst}");
    }
}

#[test]
fn chroma_siting_differs_only_where_chroma_changes() {
    let Some(c) = converter() else { return };
    // Flat colour: both sitings identical. One-pixel vertical stripe: they must differ.
    let flat = rgba_frame(16, 8, 0, false, |_, _| [200, 40, 90, 255]);
    let a = c.rgba_to_yuv(&flat, &YuvOptions::default()).unwrap();
    let b = c
        .rgba_to_yuv(&flat, &YuvOptions { siting: ChromaSiting::Left, ..YuvOptions::default() })
        .unwrap();
    assert_eq!(a.data(), b.data());
    let stripe =
        rgba_frame(16, 8, 0, false, |x, _| if x == 5 { [255, 0, 0, 255] } else { [0, 0, 0, 255] });
    let a = c.rgba_to_yuv(&stripe, &YuvOptions::default()).unwrap();
    let b = c
        .rgba_to_yuv(&stripe, &YuvOptions { siting: ChromaSiting::Left, ..YuvOptions::default() })
        .unwrap();
    assert_ne!(a.uv(), b.uv());
    assert_eq!(a.y(), b.y(), "siting never touches luma");
}

#[test]
fn hdr_tonemap_and_convert_in_one_dispatch() {
    let Some(ctx) = gpu() else { return };
    let c = YuvConverter::new(ctx).unwrap();
    let t = GpuTonemapper::new(ctx).unwrap();
    let mut rng = Rng(321);
    let (w, h) = (129u32, 71u32);
    let px: Vec<[f32; 4]> = (0..w * h)
        .map(|i| {
            let x = (i % w) as f32 / w as f32;
            if i % 3 == 0 {
                [x * 6.0, x * 2.0, rng.range(-0.3, 1.0), 1.0]
            } else {
                [rng.range(0.0, 2.5), rng.range(0.0, 2.5), rng.range(0.0, 2.5), 1.0]
            }
        })
        .collect();
    let f = hdr_frame(w, h, 16, 203.0, &px);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let mut worst_total = 0u32;
    let mut differing = 0usize;
    let mut bytes = 0usize;
    for opts in [
        YuvOptions::default(),
        YuvOptions { layout: YuvLayout::I420, siting: ChromaSiting::Left, ..YuvOptions::default() },
        YuvOptions { matrix: ColorMatrix::Bt601, range: YuvRange::Full, ..YuvOptions::default() },
    ] {
        let fused = c.tonemap_to_yuv(&f, &s, &opts).unwrap();
        // 1. vs the CPU pipeline "tonemap, then convert".
        let reference = cpu::tonemap_to_yuv(&f, &s, &opts).unwrap();
        // 2. vs the GPU pipeline "tonemap, then convert" (same GPU tonemap code).
        let two_step = c.rgba_to_yuv(&t.tonemap(&f, &s).unwrap(), &opts).unwrap();
        assert_eq!(fused.data(), two_step.data(), "fused vs two-step GPU {opts:?}");
        for (a, b) in fused.data().iter().zip(reference.data()) {
            let d = u32::from(a.abs_diff(*b));
            worst_total = worst_total.max(d);
            differing += usize::from(d != 0);
        }
        bytes += fused.data().len();
    }
    eprintln!("fused tonemap+YUV vs CPU: {differing} of {bytes} bytes differ, worst {worst_total}");
    assert!(worst_total <= 2, "fused path off by {worst_total}");
    assert!(differing * 1000 <= bytes, "{differing} of {bytes} bytes differ");

    // The `convert` entry point accepts HDR with default settings, and rgba_to_yuv refuses it.
    assert!(c.convert(&f, None, &YuvOptions::default()).is_ok());
    assert!(matches!(
        c.rgba_to_yuv(&f, &YuvOptions::default()),
        Err(GpuError::UnsupportedFrame { .. })
    ));
}

#[test]
fn hdr_sdr_content_stays_exact_through_the_fused_path() {
    let Some(c) = converter() else { return };
    // 8-bit colours expressed in scRGB at 203 nits must give the same YUV as converting the
    // 8-bit colours directly.
    let (w, h) = (48u32, 32u32);
    let k = 203.0 / 80.0;
    let mut rng = Rng(6);
    let codes: Vec<[u8; 4]> = (0..w * h)
        .map(|_| [rng.next_u32() as u8, rng.next_u32() as u8, rng.next_u32() as u8, 255])
        .collect();
    let px: Vec<[f32; 4]> = codes
        .iter()
        .map(|c| {
            let l = [c[0], c[1], c[2]].map(|v| ssx_hdr::srgb_eotf(f32::from(v) / 255.0) * k);
            [l[0], l[1], l[2], 1.0]
        })
        .collect();
    let hdr = hdr_frame(w, h, 0, 203.0, &px);
    let sdr = rgba_frame(w, h, 0, false, |x, y| codes[(y * w + x) as usize]);
    for opts in
        [YuvOptions::default(), YuvOptions { layout: YuvLayout::I420, ..YuvOptions::default() }]
    {
        let a = c.tonemap_to_yuv(&hdr, &TonemapSettings::default(), &opts).unwrap();
        let b = c.rgba_to_yuv(&sdr, &opts).unwrap();
        assert_eq!(a.data(), b.data(), "{opts:?}");
    }
}

#[test]
fn deterministic() {
    let Some(c) = converter() else { return };
    let f = noise_frame(50, 30, 0, false, 4);
    let a = c.rgba_to_yuv(&f, &YuvOptions::default()).unwrap();
    assert_eq!(a.data(), c.rgba_to_yuv(&f, &YuvOptions::default()).unwrap().data());
}

#[test]
fn timestamps_and_errors() {
    let Some(c) = converter() else { return };
    let mut f = noise_frame(4, 4, 0, false, 4);
    f.timestamp = Some(std::time::Duration::from_millis(40));
    assert_eq!(c.rgba_to_yuv(&f, &YuvOptions::default()).unwrap().timestamp, f.timestamp);
    let empty =
        Frame::from_raw(Size::new(0, 0), 0, PixelFormat::Rgba8, ColorSpace::Srgb, vec![]).unwrap();
    assert!(matches!(
        c.rgba_to_yuv(&empty, &YuvOptions::default()),
        Err(GpuError::InvalidArgument { .. })
    ));
    let weird =
        Frame::from_raw(Size::new(1, 1), 8, PixelFormat::Rgba16F, ColorSpace::Srgb, vec![0; 8])
            .unwrap();
    assert!(matches!(
        c.convert(&weird, None, &YuvOptions::default()),
        Err(GpuError::UnsupportedFrame { .. })
    ));
    let mut hdr = hdr_frame(2, 2, 0, 203.0, &[[1.0; 4]; 4]);
    hdr.sdr_white_nits = Some(-1.0);
    assert!(matches!(
        c.tonemap_to_yuv(&hdr, &TonemapSettings::default(), &YuvOptions::default()),
        Err(GpuError::InvalidSdrWhite(_))
    ));
    let bad = TonemapSettings { peak: 0.1, ..TonemapSettings::default() };
    assert!(matches!(
        c.tonemap_to_yuv(&hdr_frame(2, 2, 0, 203.0, &[[1.0; 4]; 4]), &bad, &YuvOptions::default()),
        Err(GpuError::Settings(_))
    ));
    // Width beyond the texture limit cannot be banded.
    let big = noise_frame(64, 2, 0, false, 1);
    let limited = YuvConverter::new(gpu().unwrap()).unwrap();
    limited.set_tile_limits(TileLimits { max_dimension: 32, max_buffer_bytes: 1 << 20 });
    assert!(matches!(
        limited.rgba_to_yuv(&big, &YuvOptions::default()),
        Err(GpuError::TooLarge { .. })
    ));
    assert!(YuvFrame::new(&YuvOptions::default(), 2, 2, vec![0; 5], None).is_err());
}

#[test]
fn device_loss_is_recovered() {
    let Ok(ctx) = ssx_gpu::GpuContext::new_default() else {
        eprintln!("SKIPPED: no usable GPU adapter");
        return;
    };
    let c = YuvConverter::new(&ctx).unwrap();
    let f = noise_frame(20, 10, 0, false, 8);
    let opts = YuvOptions::default();
    let before = c.rgba_to_yuv(&f, &opts).unwrap();
    ctx.simulate_device_loss();
    assert_eq!(c.rgba_to_yuv(&f, &opts).unwrap().data(), before.data());
}

#[test]
fn reusable_pass_on_existing_textures() {
    let Some(ctx) = gpu() else { return };
    let c = YuvConverter::new(ctx).unwrap();
    let handle = ctx.handle().unwrap();
    let (w, h) = (21u32, 13u32);
    let tex = handle.device().create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: YuvInput::Bgra8.texture_format(),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let opts = YuvOptions { layout: YuvLayout::I420, ..YuvOptions::default() };
    let pass = c
        .create_pass(
            &tex.create_view(&wgpu::TextureViewDescriptor::default()),
            YuvInput::Bgra8,
            Size::new(w, h),
            &opts,
        )
        .unwrap();
    for seed in [1u64, 2] {
        let f = noise_frame(w, h, 0, true, seed);
        handle.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            f.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let layout = pass.set_region(Size::new(w, h), [0, 0]).unwrap();
        let staging = handle.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: layout.total_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc =
            handle.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        pass.record(&mut enc, Size::new(w, h));
        enc.copy_buffer_to_buffer(pass.output(), 0, &staging, 0, layout.total_bytes);
        handle.queue().submit([enc.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
        handle.device().poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let bytes = slice.get_mapped_range().unwrap().to_vec();
        let reference = cpu::rgba_to_yuv(&f, &opts).unwrap();
        // Compare the Y plane row by row (the pass output rows are padded to 4 bytes).
        for y in 0..reference.coded_height() as usize {
            let cw = reference.coded_width() as usize;
            assert_eq!(
                &bytes[y * layout.y_stride as usize..][..cw],
                &reference.y()[y * cw..][..cw],
                "Y row {y}"
            );
        }
        let cw = reference.coded_width() as usize / 2;
        for r in 0..reference.chroma_rows() {
            let u = &bytes[layout.u_offset as usize + r * layout.chroma_stride as usize..][..cw];
            let v = &bytes[layout.v_offset as usize + r * layout.chroma_stride as usize..][..cw];
            assert_eq!(u, &reference.u().unwrap()[r * cw..][..cw], "U row {r}");
            assert_eq!(v, &reference.v().unwrap()[r * cw..][..cw], "V row {r}");
        }
    }
    // Oversized regions are rejected instead of overrunning the buffer.
    assert!(matches!(
        pass.set_region(Size::new(w * 4, h * 4), [0, 0]),
        Err(GpuError::TooLarge { .. })
    ));
}

/// `cargo test --release -p ssx-gpu -- --ignored bench --nocapture`
#[test]
#[ignore = "benchmark"]
fn bench_yuv_4k() {
    let Some(ctx) = gpu() else { return };
    let c = YuvConverter::new(ctx).unwrap();
    let (w, h) = (3840u32, 2160u32);
    let sdr = noise_frame(w, h, 0, true, 1);
    let px: Vec<[f32; 4]> = (0..w * h)
        .map(|i| [(i % 7) as f32 * 0.4, (i % 5) as f32 * 0.5, (i % 3) as f32, 1.0])
        .collect();
    let hdr = hdr_frame(w, h, 0, 203.0, &px);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let info = ctx.adapter_info();
    for opts in
        [YuvOptions::default(), YuvOptions { layout: YuvLayout::I420, ..YuvOptions::default() }]
    {
        let _ = c.rgba_to_yuv(&sdr, &opts).unwrap();
        let n = 5;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = c.rgba_to_yuv(&sdr, &opts).unwrap();
        }
        let sdr_t = t0.elapsed() / n;
        let _ = c.tonemap_to_yuv(&hdr, &s, &opts).unwrap();
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = c.tonemap_to_yuv(&hdr, &s, &opts).unwrap();
        }
        let hdr_t = t0.elapsed() / n;
        let t0 = std::time::Instant::now();
        let _ = cpu::rgba_to_yuv(&sdr, &opts).unwrap();
        let cpu_t = t0.elapsed();
        eprintln!(
            "BENCH 4K {:?} on {} ({:?}): BGRA->YUV {sdr_t:.2?}, HDR tonemap+YUV {hdr_t:.2?} (upload+dispatch+readback); CPU reference RGB->YUV {cpu_t:.2?}",
            opts.layout, info.name, info.device_type
        );
    }
}
