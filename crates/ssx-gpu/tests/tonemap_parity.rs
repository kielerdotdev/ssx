//! GPU tonemap vs the `ssx-hdr` CPU reference.
//!
//! Parity contract (see README):
//!
//! * pixels that are SDR-representable are *byte-identical* to the CPU result (and to the
//!   original 8-bit value);
//! * every other pixel is within one code value of the CPU result;
//! * at most a tiny fraction of pixels differ at all (last-bit differences in `pow` or
//!   fused multiply-add flipping a dither decision).
//!
//! Every test prints its measured numbers (`cargo test -- --nocapture`).

mod common;

use std::thread;

use common::{Diff, Rng, diff_rgba, gpu, hdr_frame, hdr_frame_bits};
use half::f16;
use ssx_gpu::{GpuContext, GpuError, GpuOptions, GpuTonemapper, TileLimits};
use ssx_hdr::{TonemapOperator, TonemapSettings, srgb_eotf, to_sdr8};
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

const OPERATORS: [TonemapOperator; 4] = [
    TonemapOperator::Clip,
    TonemapOperator::ReinhardExtended,
    TonemapOperator::Bt2390,
    TonemapOperator::AcesFit,
];

/// Maximum fraction of pixels allowed to differ from the CPU at all (each by one code),
/// with a floor of a few pixels for tiny samples. Measured values are below this; see
/// the README.
const MAX_DIFFERING_FRACTION: f64 = 1e-4;
const MIN_ALLOWED_DIFFERING: f64 = 3.0;

fn settings(
    op: TonemapOperator,
    dither: bool,
    knee: f32,
    peak: f32,
    exposure: f32,
) -> TonemapSettings {
    TonemapSettings { operator: op, peak, knee, dither, exposure }
}

fn tonemapper() -> Option<GpuTonemapper> {
    gpu().map(|c| GpuTonemapper::new(c).expect("tonemapper"))
}

/// Runs GPU and CPU on `f` and returns the diff.
fn compare(t: &GpuTonemapper, f: &Frame, s: &TonemapSettings) -> Diff {
    let g = t.tonemap(f, s).expect("gpu tonemap");
    let c = to_sdr8(f, s).expect("cpu tonemap");
    assert_eq!(g.size(), f.size());
    assert_eq!(g.format(), PixelFormat::Rgba8);
    assert_eq!(g.color_space(), ColorSpace::Srgb);
    diff_rgba(&g, &c)
}

fn check(label: &str, d: &Diff) {
    eprintln!(
        "{label}: {} px, {} differ ({:.5}%), max diff {}, >1: {}",
        d.pixels,
        d.pixels_differing,
        100.0 * d.pixels_differing as f64 / d.pixels.max(1) as f64,
        d.max_diff,
        d.pixels_over_one
    );
    assert!(d.max_diff <= 1, "{label}: max diff {} (>1 code)", d.max_diff);
    assert!(
        d.pixels_differing as f64
            <= (MAX_DIFFERING_FRACTION * d.pixels as f64).max(MIN_ALLOWED_DIFFERING),
        "{label}: {} of {} pixels differ",
        d.pixels_differing,
        d.pixels
    );
}

/// Sums several diffs.
fn merge(a: &mut Diff, b: &Diff) {
    a.pixels += b.pixels;
    a.channels_differing += b.channels_differing;
    a.pixels_differing += b.pixels_differing;
    a.pixels_over_one += b.pixels_over_one;
    a.max_diff = a.max_diff.max(b.max_diff);
}

fn random_frame(rng: &mut Rng, w: u32, h: u32, pad: usize, white: f32, lo: f32, hi: f32) -> Frame {
    let px: Vec<[f32; 4]> = (0..w * h)
        .map(|_| [rng.range(lo, hi), rng.range(lo, hi), rng.range(lo, hi), rng.f32()])
        .collect();
    hdr_frame(w, h, pad, white, &px)
}

#[test]
fn sdr_ramp_is_byte_exact_at_every_white_level() {
    let Some(t) = tonemapper() else { return };
    for white in [80.0f32, 100.0, 150.0, 203.0, 250.0, 320.0, 480.0, 1000.0] {
        let k = white / 80.0;
        // 256 levels x 4 rows: grey, red, green, blue ramps.
        let mut px = Vec::new();
        for row in 0..4 {
            for c in 0..256u32 {
                let l = srgb_eotf(c as f32 / 255.0) * k;
                px.push(match row {
                    0 => [l, l, l, 1.0],
                    1 => [l, 0.0, 0.0, 1.0],
                    2 => [0.0, l, 0.0, 1.0],
                    _ => [0.0, 0.0, l, 1.0],
                });
            }
        }
        let f = hdr_frame(256, 4, 0, white, &px);
        for op in OPERATORS {
            for dither in [false, true] {
                let s = settings(op, dither, 1.0, 4.0, 1.0);
                let g = t.tonemap(&f, &s).unwrap();
                let c = to_sdr8(&f, &s).unwrap();
                assert_eq!(g.data(), c.data(), "white {white} {op:?} dither {dither}: GPU != CPU");
                // ... and both are the identity on the 8-bit code.
                for row in 0..4usize {
                    for x in 0..256usize {
                        let p = &g.row(row as u32)[x * 4..x * 4 + 4];
                        let want = [x as u8; 3];
                        let got = [p[0], p[1], p[2]];
                        let expect = match row {
                            0 => want,
                            1 => [want[0], 0, 0],
                            2 => [0, want[1], 0],
                            _ => [0, 0, want[2]],
                        };
                        assert_eq!(got, expect, "white {white} {op:?} dither {dither} level {x}");
                        assert_eq!(p[3], 255);
                    }
                }
            }
        }
    }
    eprintln!("SDR ramp: 8 white levels x 4 operators x dither on/off x 4 ramps: 0 differences");
}

#[test]
fn sdr_exact_pixels_never_differ_inside_hdr_content() {
    let Some(t) = tonemapper() else { return };
    let mut rng = Rng(0x5EED);
    let (w, h) = (211u32, 97u32);
    let white = 203.0f32;
    let k = white / 80.0;
    let mut codes = Vec::new();
    let px: Vec<[f32; 4]> = (0..w * h)
        .map(|i| {
            if i % 2 == 0 {
                // SDR-representable colour.
                let c = [rng.next_u32() as u8, rng.next_u32() as u8, rng.next_u32() as u8];
                codes.push(Some(c));
                let l = c.map(|v| srgb_eotf(f32::from(v) / 255.0) * k);
                [l[0], l[1], l[2], 1.0]
            } else {
                codes.push(None);
                [rng.range(-0.5, 12.0), rng.range(-0.5, 12.0), rng.range(-0.5, 12.0), 1.0]
            }
        })
        .collect();
    let f = hdr_frame(w, h, 24, white, &px);
    for op in OPERATORS {
        let s = settings(op, true, 1.0, 4.0, 1.0);
        let g = t.tonemap(&f, &s).unwrap();
        let c = to_sdr8(&f, &s).unwrap();
        let mut exact = 0;
        for (i, code) in codes.iter().enumerate() {
            if let Some(code) = code {
                let (x, y) = (i % w as usize, i / w as usize);
                let gp = &g.row(y as u32)[x * 4..x * 4 + 3];
                let cp = &c.row(y as u32)[x * 4..x * 4 + 3];
                assert_eq!(gp, cp, "{op:?} pixel {x},{y}");
                assert_eq!(gp, code, "{op:?} pixel {x},{y} is not the original colour");
                exact += 1;
            }
        }
        check(&format!("mixed content {op:?}"), &diff_rgba(&g, &c));
        eprintln!("  {exact} SDR-exact pixels: 0 differences");
    }
}

#[test]
fn hdr_gradients_up_to_ten_times_white() {
    let Some(t) = tonemapper() else { return };
    let mut total = Diff::default();
    for white in [80.0f32, 203.0, 300.0] {
        let (w, h) = (601u32, 8u32);
        // Rows: grey, warm, cool, saturated red/green/blue, yellow ramp to 10x SDR white.
        let k = white / 80.0;
        let mut px = Vec::new();
        for row in 0..h {
            for x in 0..w {
                let v = 10.0 * k * x as f32 / (w - 1) as f32;
                px.push(match row {
                    0 => [v, v, v, 1.0],
                    1 => [v, v * 0.7, v * 0.4, 1.0],
                    2 => [v * 0.4, v * 0.7, v, 1.0],
                    3 => [v, 0.0, 0.0, 1.0],
                    4 => [0.0, v, 0.0, 1.0],
                    5 => [0.0, 0.0, v, 1.0],
                    6 => [v, v, 0.0, 1.0],
                    _ => [v, 0.02 * k, 0.05 * k, 1.0],
                });
            }
        }
        let f = hdr_frame(w, h, 0, white, &px);
        for op in OPERATORS {
            for dither in [false, true] {
                for (knee, peak, exposure) in [
                    (1.0, 4.0, 1.0),
                    (0.8, 4.0, 1.0),
                    (0.5, 10.0, 1.0),
                    (0.75, 2.0, 0.5),
                    (0.9, 1.0, 2.0),
                ] {
                    let s = settings(op, dither, knee, peak, exposure);
                    let d = compare(&t, &f, &s);
                    assert!(d.max_diff <= 1, "{op:?} white {white} {s:?}: {d:?}");
                    merge(&mut total, &d);
                }
            }
        }
    }
    check(
        "HDR gradients (all operators, dither on/off, 5 knee/peak/exposure sets, 3 white levels)",
        &total,
    );
}

#[test]
fn random_colours_including_negative_channels() {
    let Some(t) = tonemapper() else { return };
    let mut rng = Rng(0x00C0_FFEE);
    let mut total = Diff::default();
    for (lo, hi) in [(0.0, 1.0), (-1.0, 3.0), (-3.0, 25.0), (-0.05, 0.05), (-100.0, 100.0)] {
        let f = random_frame(&mut rng, 256, 128, 8, 203.0, lo, hi);
        for op in OPERATORS {
            for dither in [false, true] {
                let d = compare(&t, &f, &settings(op, dither, 0.8, 4.0, 1.0));
                assert!(d.max_diff <= 1, "{op:?} range {lo}..{hi}: {d:?}");
                merge(&mut total, &d);
            }
        }
    }
    check("random colours incl. negatives", &total);
}

#[test]
fn non_finite_and_extreme_half_floats() {
    let Some(t) = tonemapper() else { return };
    // NaN, -NaN, +Inf, -Inf, max, -max, smallest subnormal, -0, one, 8x white.
    let specials: [u16; 10] = [
        0x7e00,
        0xfe00,
        0x7c00,
        0xfc00,
        0x7bff,
        0xfbff,
        0x0001,
        0x8000,
        f16::from_f32(1.0).to_bits(),
        f16::from_f32(8.0).to_bits(),
    ];
    let mut bits = Vec::new();
    for &r in &specials {
        for &g in &specials {
            for &b in &specials {
                bits.push([r, g, b, 0x7e00]);
            }
        }
    }
    let n = bits.len() as u32;
    let f = hdr_frame_bits(n, 1, 203.0, &bits);
    let mut total = Diff::default();
    for op in OPERATORS {
        for dither in [false, true] {
            for knee in [1.0, 0.8] {
                let d = compare(&t, &f, &settings(op, dither, knee, 4.0, 1.0));
                merge(&mut total, &d);
            }
        }
    }
    check("NaN/Inf/extreme half floats", &total);
    // Spot checks of the documented mapping (NaN -> 0, +Inf -> white, -Inf -> 0).
    let f = hdr_frame_bits(
        4,
        1,
        80.0,
        &[
            [0x7e00, 0x7e00, 0x7e00, 0],
            [0x7c00, 0x7c00, 0x7c00, 0],
            [0xfc00, 0xfc00, 0xfc00, 0],
            [0x7c00, 0xfc00, 0x7e00, 0],
        ],
    );
    let g =
        t.tonemap(&f, &settings(TonemapOperator::ReinhardExtended, false, 1.0, 4.0, 1.0)).unwrap();
    assert_eq!(g.data(), &[0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 0, 0, 255]);
}

#[test]
fn tiny_odd_and_padded_sizes() {
    let Some(t) = tonemapper() else { return };
    let mut rng = Rng(77);
    let mut total = Diff::default();
    let sizes = [
        (1, 1),
        (3, 2),
        (1, 17),
        (17, 1),
        (2, 2),
        (5, 5),
        (7, 9),
        (8, 8),
        (9, 8),
        (63, 65),
        (255, 3),
        (257, 257),
        (64, 64),
    ];
    for (w, h) in sizes {
        for pad in [0usize, 2, 8, 16, 250, 264] {
            let f = random_frame(&mut rng, w, h, pad, 203.0, -0.5, 6.0);
            for op in
                [TonemapOperator::Clip, TonemapOperator::ReinhardExtended, TonemapOperator::AcesFit]
            {
                let d = compare(&t, &f, &settings(op, true, 0.8, 4.0, 1.0));
                assert!(d.max_diff <= 1, "{w}x{h} pad {pad} {op:?}: {d:?}");
                merge(&mut total, &d);
            }
        }
    }
    check("tiny/odd/padded sizes", &total);
}

#[test]
fn padding_bytes_are_never_read() {
    let Some(t) = tonemapper() else { return };
    // Two frames with identical pixels but different padding contents must tonemap equally.
    let mut rng = Rng(5);
    let a = random_frame(&mut rng, 31, 13, 40, 203.0, 0.0, 5.0);
    let mut data = a.data().to_vec();
    let stride = a.stride();
    for y in 0..13usize {
        for b in data[y * stride + 31 * 8..(y + 1) * stride].iter_mut().take(40) {
            *b = 0xFF; // NaN bit patterns in the padding
        }
    }
    let mut b =
        Frame::from_raw(a.size(), stride, PixelFormat::Rgba16F, ColorSpace::ScRgbLinear, data)
            .unwrap();
    b.sdr_white_nits = a.sdr_white_nits;
    let s = TonemapSettings::default();
    assert_eq!(t.tonemap(&a, &s).unwrap().data(), t.tonemap(&b, &s).unwrap().data());
}

#[test]
fn deterministic_across_calls_and_threads() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let mut rng = Rng(9);
    let f = random_frame(&mut rng, 300, 200, 0, 203.0, -1.0, 8.0);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let first = t.tonemap(&f, &s).unwrap();
    let second = t.tonemap(&f, &s).unwrap();
    assert_eq!(first.data(), second.data(), "same input twice");
    thread::scope(|scope| {
        let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| t.tonemap(&f, &s).unwrap())).collect();
        for h in handles {
            assert_eq!(h.join().unwrap().data(), first.data(), "concurrent call differs");
        }
    });
    // A second tonemapper (fresh pipelines) agrees too.
    let t2 = GpuTonemapper::new(ctx).unwrap();
    assert_eq!(t2.tonemap(&f, &s).unwrap().data(), first.data());
    eprintln!(
        "determinism: repeated, 4 concurrent threads and a fresh tonemapper are byte-identical"
    );
}

#[test]
fn tiling_is_transparent() {
    let Some(ctx) = gpu() else { return };
    let mut rng = Rng(123);
    let (w, h) = (203u32, 131u32);
    let f = random_frame(&mut rng, w, h, 16, 203.0, -0.5, 6.0);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let reference = GpuTonemapper::new(ctx).unwrap().tonemap(&f, &s).unwrap();
    for limits in [
        TileLimits { max_dimension: 64, max_buffer_bytes: 128 << 20 },
        TileLimits { max_dimension: 37, max_buffer_bytes: 128 << 20 },
        TileLimits { max_dimension: 1000, max_buffer_bytes: 512 },
        TileLimits { max_dimension: 16, max_buffer_bytes: 1024 },
        TileLimits { max_dimension: 1, max_buffer_bytes: 256 },
    ] {
        let t = GpuTonemapper::new(ctx).unwrap();
        t.set_tile_limits(limits);
        let tiled = t.tonemap(&f, &s).unwrap();
        assert_eq!(
            tiled.data(),
            reference.data(),
            "tiled with {limits:?} differs (dither must be tile independent)"
        );
    }
    // A budget that cannot hold a single padded row is a clean error, not a panic.
    let t = GpuTonemapper::new(ctx).unwrap();
    t.set_tile_limits(TileLimits { max_dimension: 1000, max_buffer_bytes: 100 });
    assert!(matches!(t.tonemap(&f, &s), Err(GpuError::TooLarge { .. })));
    eprintln!("tiling: 5 limit overrides (down to 1x1 tiles) byte-identical to the untiled result");
}

#[test]
fn frame_wider_than_the_texture_limit_is_tiled() {
    let Some(ctx) = gpu() else { return };
    let max = ctx.limits().max_texture_dimension_2d;
    if max > 65_536 {
        eprintln!("SKIPPED: device texture limit {max} makes this test too large");
        return;
    }
    let (w, h) = (max + 37, 3u32);
    let mut rng = Rng(31);
    let f = random_frame(&mut rng, w, h, 0, 203.0, 0.0, 5.0);
    let t = GpuTonemapper::new(ctx).unwrap();
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    check(&format!("{w}x{h} (wider than max texture {max})"), &compare(&t, &f, &s));
}

#[test]
#[ignore = "large: 8K and 16K frames; run with --ignored"]
fn large_frames_8k_and_16k() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    for (w, h) in [(7680u32, 4320u32), (16384, 2160), (15360, 8640)] {
        let mut rng = Rng(u64::from(w));
        let f = random_frame(&mut rng, w, h, 0, 203.0, 0.0, 5.0);
        let started = std::time::Instant::now();
        let g = t.tonemap(&f, &s).unwrap();
        let gpu_t = started.elapsed();
        let c = to_sdr8(&f, &s).unwrap();
        check(&format!("{w}x{h} (gpu {gpu_t:.2?})"), &diff_rgba(&g, &c));
    }
}

#[test]
fn error_paths() {
    let Some(t) = tonemapper() else { return };
    let mut rng = Rng(1);
    let f = random_frame(&mut rng, 4, 4, 0, 203.0, 0.0, 2.0);
    let bad = TonemapSettings { peak: 0.5, knee: 0.9, ..TonemapSettings::default() };
    assert!(matches!(t.tonemap(&f, &bad), Err(GpuError::Settings(_))));
    for nan in [f32::NAN, 0.0, -5.0, f32::INFINITY] {
        let mut g = f.clone();
        g.sdr_white_nits = Some(nan);
        assert!(
            matches!(t.tonemap(&g, &TonemapSettings::default()), Err(GpuError::InvalidSdrWhite(_))),
            "{nan}"
        );
    }
    // Wrong format/colour-space combination.
    let weird =
        Frame::from_raw(Size::new(1, 1), 8, PixelFormat::Rgba16F, ColorSpace::Srgb, vec![0; 8])
            .unwrap();
    assert!(matches!(
        t.tonemap(&weird, &TonemapSettings::default()),
        Err(GpuError::UnsupportedFrame { .. })
    ));
    // Missing sdr_white_nits falls back to 80 nits like the CPU.
    let mut unset = f.clone();
    unset.sdr_white_nits = None;
    check("unset sdr white", &compare(&t, &unset, &TonemapSettings::default()));
    // Empty frame.
    let empty =
        Frame::from_raw(Size::new(0, 0), 0, PixelFormat::Rgba16F, ColorSpace::ScRgbLinear, vec![])
            .unwrap();
    let out = t.tonemap(&empty, &TonemapSettings::default()).unwrap();
    assert_eq!(out.size(), Size::new(0, 0));
}

#[test]
fn sdr_frames_pass_through_unchanged() {
    let Some(t) = tonemapper() else { return };
    let data: Vec<u8> = (0..5 * 3 * 4).map(|i| (i * 13 % 256) as u8).collect();
    let bgra =
        Frame::from_raw(Size::new(5, 3), 20, PixelFormat::Bgra8, ColorSpace::Srgb, data).unwrap();
    let s = TonemapSettings::default();
    assert_eq!(t.tonemap(&bgra, &s).unwrap().data(), to_sdr8(&bgra, &s).unwrap().data());
}

#[test]
fn metadata_is_preserved() {
    let Some(t) = tonemapper() else { return };
    let mut rng = Rng(2);
    let mut f = random_frame(&mut rng, 8, 8, 0, 203.0, 0.0, 2.0);
    f.origin = ssx_types::Point::new(-1920, 40);
    f.scale_factor = 1.5;
    f.timestamp = Some(std::time::Duration::from_millis(1234));
    let g = t.tonemap(&f, &TonemapSettings::default()).unwrap();
    assert_eq!((g.origin, g.scale_factor, g.timestamp), (f.origin, f.scale_factor, f.timestamp));
    assert_eq!(g.sdr_white_nits, None);
}

#[test]
fn no_adapter_is_a_typed_error() {
    let opts = GpuOptions {
        adapter: Some("definitely-not-a-gpu-\u{1F984}".into()),
        ..GpuOptions::default()
    };
    match GpuContext::new(&opts) {
        Err(GpuError::NoAdapter(msg)) => eprintln!("NoAdapter: {msg}"),
        Err(e) => panic!("expected NoAdapter, got {e}"),
        Ok(_) => panic!("a bogus adapter name must not match anything"),
    }
    // Requesting a backend that is not compiled for this platform also yields NoAdapter.
    #[cfg(target_os = "linux")]
    {
        let opts = GpuOptions { backend: ssx_gpu::BackendChoice::Metal, ..GpuOptions::default() };
        assert!(matches!(GpuContext::new(&opts), Err(GpuError::NoAdapter(_))));
    }
    // `#N` selects by index; an index past the end matches nothing.
    let opts = GpuOptions { adapter: Some("#9999".into()), ..GpuOptions::default() };
    assert!(matches!(GpuContext::new(&opts), Err(GpuError::NoAdapter(_))));
}

#[test]
fn device_loss_is_recovered() {
    // A private context: destroying the shared one would race with the other tests.
    let Ok(ctx) = GpuContext::new_default() else {
        eprintln!("SKIPPED: no usable GPU adapter");
        return;
    };
    let t = GpuTonemapper::new(&ctx).unwrap();
    let mut rng = Rng(3);
    let f = random_frame(&mut rng, 40, 30, 0, 203.0, 0.0, 4.0);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let before = t.tonemap(&f, &s).unwrap();
    let gen0 = ctx.generation();
    ctx.simulate_device_loss();
    assert!(ctx.is_lost());
    let after = t.tonemap(&f, &s).expect("call after device loss recreates the device");
    assert_eq!(after.data(), before.data());
    assert!(ctx.generation() > gen0 && !ctx.is_lost());
    // Explicit recreation also works and cached resources follow the new generation.
    ctx.recreate().unwrap();
    assert_eq!(t.tonemap(&f, &s).unwrap().data(), before.data());
    eprintln!(
        "device loss: recreated (generation {gen0} -> {}), results identical",
        ctx.generation()
    );
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn handles_are_send_and_sync() {
    assert_send_sync::<GpuContext>();
    assert_send_sync::<GpuTonemapper>();
    assert_send_sync::<ssx_gpu::YuvConverter>();
    assert_send_sync::<ssx_gpu::GpuFx>();
    assert_send_sync::<ssx_gpu::DeviceHandle>();
}

#[test]
fn global_context_is_shared() {
    let Ok(a) = GpuContext::global() else {
        eprintln!("SKIPPED: no usable GPU adapter");
        return;
    };
    let b = GpuContext::global().unwrap();
    assert!(std::ptr::eq(a, b));
    assert!(GpuContext::global().is_ok());
}

// --- GPU-resident path -------------------------------------------------------------

fn read_rgba8(ctx: &GpuContext, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<u8> {
    let handle = ctx.handle().unwrap();
    let dev = handle.device();
    let padded = (w as usize * 4).div_ceil(256) * 256;
    let buf = dev.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (padded * h as usize) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    handle.queue().submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
    dev.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let view = slice.get_mapped_range().unwrap();
    let mut out = Vec::new();
    for row in view.chunks_exact(padded).take(h as usize) {
        out.extend_from_slice(&row[..w as usize * 4]);
    }
    out
}

fn upload_hdr(ctx: &GpuContext, f: &Frame) -> wgpu::Texture {
    let handle = ctx.handle().unwrap();
    let tex = handle.device().create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: f.width(), height: f.height(), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
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
            bytes_per_row: Some(f.stride() as u32),
            rows_per_image: Some(f.height()),
        },
        wgpu::Extent3d { width: f.width(), height: f.height(), depth_or_array_layers: 1 },
    );
    tex
}

fn output_texture(ctx: &GpuContext, w: u32, h: u32) -> wgpu::Texture {
    ctx.handle().unwrap().device().create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

#[test]
fn tonemap_texture_stays_on_the_gpu() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let mut rng = Rng(44);
    let (w, h) = (97u32, 61u32);
    let f = random_frame(&mut rng, w, h, 0, 203.0, -0.5, 6.0);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let input = upload_hdr(ctx, &f);
    let output = output_texture(ctx, w, h);
    t.tonemap_texture(&input, &output, 203.0, &s).unwrap();
    let mut got = Frame::from_rgba8(w, h, read_rgba8(ctx, &output, w, h)).unwrap();
    got.origin = f.origin;
    check("tonemap_texture", &diff_rgba(&got, &to_sdr8(&f, &s).unwrap()));
    // Same result as the CPU-frame entry point on the same device.
    assert_eq!(got.data(), t.tonemap(&f, &s).unwrap().data());

    // Invalid settings are rejected before touching the GPU.
    let bad = TonemapSettings { exposure: -1.0, ..s };
    assert!(matches!(t.tonemap_texture(&input, &output, 203.0, &bad), Err(GpuError::Settings(_))));
    // Wrong texture formats surface as a typed validation error, not a panic.
    let wrong = ctx.handle().unwrap().device().create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    assert!(matches!(t.tonemap_texture(&wrong, &output, 203.0, &s), Err(GpuError::Validation(_))));
}

#[test]
fn tonemap_pass_is_reusable_across_frames_and_settings() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let (w, h) = (64u32, 40u32);
    let handle = ctx.handle().unwrap();
    let input = upload_hdr(ctx, &random_frame(&mut Rng(1), w, h, 0, 203.0, 0.0, 1.0));
    let output = output_texture(ctx, w, h);
    let pass = t
        .create_pass(
            &input.create_view(&wgpu::TextureViewDescriptor::default()),
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        )
        .unwrap();
    for (seed, op, dither) in [
        (11u64, TonemapOperator::ReinhardExtended, true),
        (12, TonemapOperator::Clip, false),
        (13, TonemapOperator::AcesFit, true),
    ] {
        let f = random_frame(&mut Rng(seed), w, h, 0, 203.0, -0.2, 7.0);
        // New frame data into the same texture, new settings into the same pass.
        handle.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            f.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(f.stride() as u32),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let s = settings(op, dither, 0.8, 4.0, 1.0);
        pass.set_params(&s, Some(203.0)).unwrap();
        pass.set_region([0, 0], Size::new(w, h));
        let mut enc =
            handle.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        pass.record(&mut enc, Size::new(w, h));
        handle.queue().submit([enc.finish()]);
        let got = Frame::from_rgba8(w, h, read_rgba8(ctx, &output, w, h)).unwrap();
        check(&format!("TonemapPass {op:?}"), &diff_rgba(&got, &to_sdr8(&f, &s).unwrap()));
    }
}

/// Timing for 4K (`cargo test --release -p ssx-gpu -- --ignored bench --nocapture`).
#[test]
#[ignore = "benchmark"]
fn bench_tonemap_4k() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let (w, h) = (3840u32, 2160u32);
    let f = random_frame(&mut Rng(8), w, h, 0, 203.0, 0.0, 5.0);
    let s = TonemapSettings { knee: 0.8, ..TonemapSettings::default() };
    let _ = t.tonemap(&f, &s).unwrap(); // warm-up: pipeline, allocation
    let n = 5;
    let started = std::time::Instant::now();
    for _ in 0..n {
        let _ = t.tonemap(&f, &s).unwrap();
    }
    let per = started.elapsed() / n;
    let cpu_started = std::time::Instant::now();
    let _ = to_sdr8(&f, &s).unwrap();
    let cpu = cpu_started.elapsed();
    let info = ctx.adapter_info();
    eprintln!(
        "BENCH tonemap 4K on {} ({:?}): upload+dispatch+readback {per:.2?}/frame ({:.1} fps); CPU reference {cpu:.2?}",
        info.name,
        info.device_type,
        1.0 / per.as_secs_f64()
    );
    // GPU-resident path (no upload/readback): dispatch only.
    let input = upload_hdr(ctx, &f);
    let output = output_texture(ctx, w, h);
    let pass = t
        .create_pass(
            &input.create_view(&wgpu::TextureViewDescriptor::default()),
            &output.create_view(&wgpu::TextureViewDescriptor::default()),
        )
        .unwrap();
    pass.set_params(&s, Some(203.0)).unwrap();
    pass.set_region([0, 0], Size::new(w, h));
    let handle = ctx.handle().unwrap();
    let started = std::time::Instant::now();
    for _ in 0..n {
        let mut enc =
            handle.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        pass.record(&mut enc, Size::new(w, h));
        handle.queue().submit([enc.finish()]);
    }
    handle.device().poll(wgpu::PollType::wait_indefinitely()).unwrap();
    eprintln!("BENCH tonemap 4K dispatch only (GPU resident): {:.2?}/frame", started.elapsed() / n);
}
