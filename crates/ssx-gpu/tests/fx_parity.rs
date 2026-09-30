//! GPU blur / pixelate / resize vs their CPU references (`ssx_gpu::fx::cpu`).
//!
//! Pixelate is integer maths on both sides (bit-exact). Blur and resize share weight
//! tables with the CPU; the GPU accumulates in a different order and precision, so they
//! must agree within one code value.

mod common;

use common::{Rng, colour_bars, diff_rgba, gpu, gradient, hdr_frame, rgba_frame};
use ssx_gpu::fx::{self, cpu};
use ssx_gpu::{GpuError, GpuFx, ResizeFilter};
use ssx_types::{Frame, Rect};

fn fx() -> Option<GpuFx> {
    gpu().map(|c| GpuFx::new(c).expect("fx"))
}

fn noise(w: u32, h: u32, pad: usize, bgra: bool, seed: u64) -> Frame {
    let mut rng = Rng(seed);
    let px: Vec<[u8; 4]> = (0..w * h)
        .map(|_| {
            [rng.next_u32() as u8, rng.next_u32() as u8, rng.next_u32() as u8, rng.next_u32() as u8]
        })
        .collect();
    rgba_frame(w, h, pad, bgra, |x, y| px[(y * w + x) as usize])
}

fn report(label: &str, gpu: &Frame, cpu: &Frame, allowed: u32) {
    let d = diff_rgba(gpu, cpu);
    eprintln!("{label}: {} px, {} differ, max diff {}", d.pixels, d.pixels_differing, d.max_diff);
    assert!(d.max_diff <= allowed, "{label}: max diff {} > {allowed}", d.max_diff);
}

#[test]
fn gaussian_blur_matches_cpu() {
    let Some(fx) = fx() else { return };
    let (w, h) = (97u32, 61u32);
    for (name, f) in [
        ("noise rgba", noise(w, h, 0, false, 1)),
        ("noise bgra padded", noise(w, h, 12, true, 2)),
        ("bars", rgba_frame(w, h, 4, false, |x, y| colour_bars(x, y, w, h))),
    ] {
        for region in [
            Rect::new(0, 0, w, h),
            Rect::new(10, 7, 40, 30),
            Rect::new(0, 0, 1, 1),
            Rect::new(96, 60, 1, 1),
            Rect::new(5, 5, 1, 50),
            Rect::new(50, 0, 47, 3),
            Rect::new(3, 3, 2, 2),
        ] {
            for sigma in [0.4f32, 1.0, 2.5, 6.0, 30.0] {
                let g = fx.gaussian_blur_region(&f, region, sigma).unwrap();
                let c = cpu::gaussian_blur_region(&f, region, sigma).unwrap();
                report(&format!("blur {name} {region:?} sigma {sigma}"), &g, &c, 1);
            }
        }
    }
}

#[test]
fn blur_properties() {
    let Some(fx) = fx() else { return };
    // A constant image is a fixed point (weights sum to one; rounding must not drift).
    let flat = rgba_frame(40, 30, 0, false, |_, _| [37, 200, 91, 255]);
    let out = fx.gaussian_blur_region(&flat, Rect::new(0, 0, 40, 30), 5.0).unwrap();
    assert_eq!(out.data(), flat.data());
    // Only the region changes; everything else is byte-identical.
    let f = noise(50, 40, 8, false, 3);
    let region = Rect::new(10, 12, 20, 15);
    let out = fx.gaussian_blur_region(&f, region, 3.0).unwrap();
    for y in 0..40u32 {
        for x in 0..50u32 {
            let inside = region.contains(ssx_types::Point::new(x as i32, y as i32));
            let (a, b) = (&f.row(y)[x as usize * 4..][..4], &out.row(y)[x as usize * 4..][..4]);
            if !inside {
                assert_eq!(a, b, "pixel {x},{y} outside the region changed");
            }
        }
    }
    // Blur smooths: variance of the region drops sharply.
    let var = |fr: &Frame| {
        let mut vals = Vec::new();
        for y in region.y as u32..region.y as u32 + region.height {
            for x in region.x as usize..(region.x as u32 + region.width) as usize {
                vals.push(f64::from(fr.row(y)[x * 4 + 1]));
            }
        }
        let m = vals.iter().sum::<f64>() / vals.len() as f64;
        vals.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / vals.len() as f64
    };
    assert!(var(&out) < var(&f) / 10.0);
    // Nothing outside the region leaks in: a white region in a black frame stays white at
    // its centre and the frame outside stays black.
    let mut data = vec![0u8; 30 * 30 * 4];
    for px in data.chunks_exact_mut(4) {
        px[3] = 255;
    }
    for y in 10..20usize {
        for x in 10..20usize {
            data[(y * 30 + x) * 4..][..3].copy_from_slice(&[255, 255, 255]);
        }
    }
    let f = Frame::from_rgba8(30, 30, data).unwrap();
    let out = fx.gaussian_blur_region(&f, Rect::new(10, 10, 10, 10), 4.0).unwrap();
    assert_eq!(&out.row(15)[15 * 4..][..3], &[255, 255, 255]);
    assert_eq!(&out.row(9)[15 * 4..][..3], &[0, 0, 0]);
}

#[test]
fn pixelate_is_bit_exact() {
    let Some(fx) = fx() else { return };
    let (w, h) = (83u32, 47u32);
    for (name, f) in
        [("noise", noise(w, h, 0, false, 5)), ("bgra padded", noise(w, h, 20, true, 6))]
    {
        for region in [
            Rect::new(0, 0, w, h),
            Rect::new(7, 3, 50, 30),
            Rect::new(0, 0, 1, 1),
            Rect::new(80, 44, 3, 3),
            Rect::new(1, 1, 2, 40),
        ] {
            for block in [1u32, 2, 3, 8, 16, 33, 200, 1024] {
                let g = fx.pixelate_region(&f, region, block).unwrap();
                let c = cpu::pixelate_region(&f, region, block).unwrap();
                assert_eq!(g.data(), c.data(), "pixelate {name} {region:?} block {block}");
            }
        }
    }
    // Known values: a 4x4 region of a 2x2 checker pixelated by 2 gives the block means.
    let f = rgba_frame(4, 4, 0, false, |x, y| {
        if (x + y) % 2 == 0 { [200, 100, 0, 255] } else { [100, 50, 0, 255] }
    });
    let out = fx.pixelate_region(&f, Rect::new(0, 0, 4, 4), 2).unwrap();
    for px in out.data().chunks_exact(4) {
        assert_eq!(px, &[150, 75, 0, 255]);
    }
    // Block 1 is the identity.
    let n = noise(20, 20, 0, false, 9);
    assert_eq!(fx.pixelate_region(&n, Rect::new(0, 0, 20, 20), 1).unwrap().data(), n.data());
    eprintln!("pixelate: 2 frames x 5 regions x 8 block sizes: 0 differences");
}

#[test]
fn resize_matches_cpu() {
    let Some(fx) = fx() else { return };
    let cases = [
        (200u32, 113u32, 100u32, 57u32),
        (200, 113, 37, 19),
        (200, 113, 1, 1),
        (200, 113, 400, 200),
        (17, 9, 64, 33),
        (64, 33, 17, 9),
        (5, 5, 5, 5),
        (1, 1, 7, 7),
        (301, 2, 43, 1),
        (2, 301, 1, 43),
        (1000, 600, 100, 60),
    ];
    for filter in [ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
        for (sw, sh, dw, dh) in cases {
            for (name, f) in [
                ("gradient", rgba_frame(sw, sh, 8, false, |x, y| gradient(x, y, sw, sh))),
                ("noise bgra", noise(sw, sh, 0, true, u64::from(sw))),
            ] {
                let g = fx.resize(&f, dw, dh, filter).unwrap();
                assert_eq!((g.width(), g.height()), (dw, dh));
                let c = cpu::resize(&f, dw, dh, filter).unwrap();
                report(&format!("resize {filter:?} {name} {sw}x{sh}->{dw}x{dh}"), &g, &c, 1);
            }
        }
    }
}

#[test]
fn resize_properties() {
    let Some(fx) = fx() else { return };
    // Constant images stay constant, whatever the filter and ratio.
    let flat = rgba_frame(90, 60, 0, false, |_, _| [12, 250, 130, 255]);
    for filter in [ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
        for (w, h) in [(30, 20), (180, 120), (7, 3), (1, 1)] {
            let out = fx.resize(&flat, w, h, filter).unwrap();
            assert!(
                out.data().chunks_exact(4).all(|p| p == [12, 250, 130, 255]),
                "{filter:?} {w}x{h}"
            );
        }
    }
    // Same-size resize is the identity.
    let n = noise(33, 21, 0, false, 12);
    for filter in [ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
        assert_eq!(fx.resize(&n, 33, 21, filter).unwrap().data(), n.data(), "{filter:?}");
    }
    // A symmetric filter reproduces a linear ramp exactly at the sample centres: pixel j
    // holds 2j, output pixel i is centred on input coordinate 2i + 0.5, so it is 4i + 1.
    let ramp = rgba_frame(64, 8, 0, false, |x, _| [(2 * x) as u8, 0, 0, 255]);
    for filter in [ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
        let out = fx.resize(&ramp, 32, 4, filter).unwrap();
        for i in 3..29u32 {
            let got = i32::from(out.row(1)[i as usize * 4]);
            assert!((got - (4 * i as i32 + 1)).abs() <= 1, "{filter:?} output {i}: {got}");
        }
    }
    // Timestamps carry over.
    let mut t = n.clone();
    t.timestamp = Some(std::time::Duration::from_secs(3));
    assert_eq!(fx.resize(&t, 10, 10, ResizeFilter::Lanczos3).unwrap().timestamp, t.timestamp);
}

#[test]
fn error_paths() {
    let Some(fx) = fx() else { return };
    let f = noise(20, 10, 0, false, 1);
    let ok = Rect::new(0, 0, 20, 10);
    // Regions.
    for bad in [
        Rect::new(0, 0, 0, 5),
        Rect::new(-1, 0, 5, 5),
        Rect::new(15, 0, 6, 5),
        Rect::new(0, 8, 5, 3),
        Rect::new(30, 30, 2, 2),
    ] {
        assert!(
            matches!(
                fx.gaussian_blur_region(&f, bad, 2.0),
                Err(GpuError::InvalidArgument { what: "region", .. })
            ),
            "{bad:?}"
        );
        assert!(
            matches!(
                fx.pixelate_region(&f, bad, 2),
                Err(GpuError::InvalidArgument { what: "region", .. })
            ),
            "{bad:?}"
        );
    }
    // Parameters.
    for s in [0.0f32, -1.0, f32::NAN, f32::INFINITY, fx::MAX_SIGMA + 1.0] {
        assert!(
            matches!(
                fx.gaussian_blur_region(&f, ok, s),
                Err(GpuError::InvalidArgument { what: "sigma", .. })
            ),
            "{s}"
        );
    }
    for b in [0u32, fx::MAX_BLOCK + 1] {
        assert!(
            matches!(
                fx.pixelate_region(&f, ok, b),
                Err(GpuError::InvalidArgument { what: "block", .. })
            ),
            "{b}"
        );
    }
    assert!(fx.resize(&f, 0, 5, ResizeFilter::Bilinear).is_err());
    assert!(fx.resize(&f, 5, 0, ResizeFilter::Bilinear).is_err());
    // HDR frames must be tonemapped first.
    let hdr = hdr_frame(4, 4, 0, 203.0, &[[1.0; 4]; 16]);
    assert!(matches!(
        fx.gaussian_blur_region(&hdr, Rect::new(0, 0, 4, 4), 1.0),
        Err(GpuError::UnsupportedFrame { .. })
    ));
    assert!(matches!(
        fx.pixelate_region(&hdr, Rect::new(0, 0, 4, 4), 2),
        Err(GpuError::UnsupportedFrame { .. })
    ));
    assert!(matches!(
        fx.resize(&hdr, 2, 2, ResizeFilter::Lanczos3),
        Err(GpuError::UnsupportedFrame { .. })
    ));
    // The CPU references validate identically.
    assert!(cpu::gaussian_blur_region(&f, Rect::new(0, 0, 0, 5), 1.0).is_err());
    assert!(cpu::pixelate_region(&f, ok, 0).is_err());
}

#[test]
fn deterministic_and_device_loss() {
    let Ok(ctx) = ssx_gpu::GpuContext::new_default() else {
        eprintln!("SKIPPED: no usable GPU adapter");
        return;
    };
    let fx = GpuFx::new(&ctx).unwrap();
    let f = noise(40, 30, 0, false, 1);
    let r = Rect::new(2, 2, 30, 20);
    let a = fx.gaussian_blur_region(&f, r, 3.0).unwrap();
    assert_eq!(a.data(), fx.gaussian_blur_region(&f, r, 3.0).unwrap().data());
    ctx.simulate_device_loss();
    assert_eq!(a.data(), fx.gaussian_blur_region(&f, r, 3.0).unwrap().data());
    ctx.simulate_device_loss();
    assert!(fx.pixelate_region(&f, r, 4).is_ok());
    ctx.simulate_device_loss();
    assert!(fx.resize(&f, 10, 10, ResizeFilter::Lanczos3).is_ok());
}

/// `cargo test --release -p ssx-gpu -- --ignored bench --nocapture`
#[test]
#[ignore = "benchmark"]
fn bench_fx() {
    let Some(ctx) = gpu() else { return };
    let fx = GpuFx::new(ctx).unwrap();
    let info = ctx.adapter_info();
    let f4k = noise(3840, 2160, 0, false, 1);
    let hd = noise(1920, 1080, 0, false, 2);
    let time = |label: &str, run: &dyn Fn(), cpu_run: &dyn Fn()| {
        run();
        let t0 = std::time::Instant::now();
        for _ in 0..3 {
            run();
        }
        let g = t0.elapsed() / 3;
        let t0 = std::time::Instant::now();
        cpu_run();
        let c = t0.elapsed();
        eprintln!(
            "BENCH {label} on {} ({:?}): GPU {g:.2?}, CPU reference {c:.2?}",
            info.name, info.device_type
        );
    };
    let region = Rect::new(0, 0, 1920, 1080);
    time(
        "gaussian blur 1080p sigma 8",
        &|| drop(fx.gaussian_blur_region(&hd, region, 8.0).unwrap()),
        &|| drop(cpu::gaussian_blur_region(&hd, region, 8.0).unwrap()),
    );
    time(
        "pixelate 1080p block 16",
        &|| drop(fx.pixelate_region(&hd, region, 16).unwrap()),
        &|| drop(cpu::pixelate_region(&hd, region, 16).unwrap()),
    );
    time(
        "Lanczos3 4K -> 480x270",
        &|| drop(fx.resize(&f4k, 480, 270, ResizeFilter::Lanczos3).unwrap()),
        &|| drop(cpu::resize(&f4k, 480, 270, ResizeFilter::Lanczos3).unwrap()),
    );
    time(
        "bilinear 4K -> 1920x1080",
        &|| drop(fx.resize(&f4k, 1920, 1080, ResizeFilter::Bilinear).unwrap()),
        &|| drop(cpu::resize(&f4k, 1920, 1080, ResizeFilter::Bilinear).unwrap()),
    );
}
