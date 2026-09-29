//! Crate-level tests: the whole `to_sdr8` pipeline against the per-pixel reference.

#![allow(clippy::float_cmp)] // tests assert bit-exact pass-through on purpose

use half::f16;
use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Size};

use super::*;

const OPERATORS: [TonemapOperator; 4] = [
    TonemapOperator::Clip,
    TonemapOperator::ReinhardExtended,
    TonemapOperator::Bt2390,
    TonemapOperator::AcesFit,
];
const ROLLOFF_OPS: [TonemapOperator; 3] =
    [TonemapOperator::ReinhardExtended, TonemapOperator::Bt2390, TonemapOperator::AcesFit];

/// Small deterministic PRNG (xorshift64*), so tests need no dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn u16(&mut self) -> u16 {
        (self.next_u64() >> 32) as u16
    }
    fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }
}

fn settings(op: TonemapOperator) -> TonemapSettings {
    TonemapSettings { operator: op, ..TonemapSettings::default() }
}

/// Builds an `Rgba16F` frame from raw half bits (`w*h*3` values, alpha = 1.0) with
/// `pad_bytes` of garbage at the end of each row.
fn frame_from_bits(w: u32, h: u32, bits: &[u16], pad_bytes: usize, white: Option<f32>) -> Frame {
    let row = w as usize * 8;
    let stride = row + pad_bytes;
    let mut data = vec![0xAB_u8; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 3;
            let o = y * stride + x * 8;
            for c in 0..3 {
                data[o + c * 2..o + c * 2 + 2].copy_from_slice(&bits[i + c].to_le_bytes());
            }
            data[o + 6..o + 8].copy_from_slice(&f16::from_f32(1.0).to_le_bytes());
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
    f.sdr_white_nits = white;
    f
}

fn frame_from_f32(w: u32, h: u32, px: &[[f32; 3]], white: f32) -> Frame {
    let bits: Vec<u16> =
        px.iter().flat_map(|p| p.iter().map(|&v| f16::from_f32(v).to_bits())).collect();
    frame_from_bits(w, h, &bits, 0, Some(white))
}

fn pixels(f: &Frame) -> Vec<[u8; 4]> {
    let mut v = Vec::new();
    for y in 0..f.height() {
        for px in f.row(y).chunks_exact(4) {
            v.push([px[0], px[1], px[2], px[3]]);
        }
    }
    v
}

/// scRGB (1.0 = 80 nits) value of an 8-bit sRGB code shown at `white` nits SDR white.
fn code_to_scrgb(code: u8, white: f32) -> f32 {
    srgb_eotf(f32::from(code) / 255.0) * white / 80.0
}

#[test]
fn golden_values() {
    let s = TonemapSettings { dither: false, ..TonemapSettings::default() };
    let f = frame_from_f32(4, 1, &[[0.0; 3], [1.0; 3], [0.5; 3], [0.215_860_5; 3]], 80.0);
    let out = pixels(&to_sdr8(&f, &s).unwrap());
    assert_eq!(out[0], [0, 0, 0, 255]);
    assert_eq!(out[1], [255, 255, 255, 255]);
    assert_eq!(out[2], [188, 188, 188, 255]);
    assert_eq!(out[3], [128, 128, 128, 255]);
}

#[test]
fn sdr_white_maps_to_255_at_any_level() {
    for white in [80.0f32, 100.0, 200.0, 203.0, 480.0, 1000.0] {
        let v = white / 80.0;
        let f = frame_from_f32(1, 1, &[[v; 3]], white);
        for op in OPERATORS {
            for dither in [false, true] {
                let s = TonemapSettings { dither, ..settings(op) };
                assert_eq!(pixels(&to_sdr8(&f, &s).unwrap())[0], [255, 255, 255, 255], "{white}");
            }
        }
    }
}

#[test]
fn every_srgb_code_round_trips_byte_exactly_for_every_operator() {
    for white in [80.0f32, 200.0, 480.0, 203.5, 300.7, 1000.0] {
        let px: Vec<[f32; 3]> = (0..=255u8).map(|c| [code_to_scrgb(c, white); 3]).collect();
        let f = frame_from_f32(256, 1, &px, white);
        for op in OPERATORS {
            for dither in [false, true] {
                for knee in [1.0f32, 3.0] {
                    let s = TonemapSettings { knee, dither, ..settings(op) };
                    let out = pixels(&to_sdr8(&f, &s).unwrap());
                    for (c, o) in out.iter().enumerate() {
                        let c = c as u8;
                        assert_eq!(
                            *o,
                            [c, c, c, 255],
                            "white={white} op={op:?} dither={dither} knee={knee}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn random_sdr_colours_are_byte_exact() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for white in [80.0f32, 200.0, 480.0] {
        let codes: Vec<[u8; 3]> =
            (0..5000).map(|_| [rng.u16() as u8, rng.u16() as u8, rng.u16() as u8]).collect();
        let px: Vec<[f32; 3]> = codes.iter().map(|c| c.map(|v| code_to_scrgb(v, white))).collect();
        let f = frame_from_f32(100, 50, &px, white);
        for op in OPERATORS {
            let out = pixels(&to_sdr8(&f, &settings(op)).unwrap());
            for (i, c) in codes.iter().enumerate() {
                assert_eq!(out[i], [c[0], c[1], c[2], 255], "white={white} op={op:?} i={i}");
            }
        }
    }
}

#[test]
fn solid_region_is_identical_regardless_of_position_with_dither() {
    // A flat colour must come out flat: same byte at every pixel, dither on or off.
    let c = 173u8;
    let v = code_to_scrgb(c, 200.0);
    let f = frame_from_f32(37, 19, &vec![[v; 3]; 37 * 19], 200.0);
    let out = pixels(&to_sdr8(&f, &TonemapSettings::default()).unwrap());
    assert!(out.iter().all(|p| *p == [c, c, c, 255]));
}

#[test]
fn fast_path_matches_reference_on_random_bit_patterns() {
    // Random raw half bits cover normals, subnormals, NaN, +-Inf, negatives and huge
    // values; every width class (lane remainders) and stride padding is exercised.
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for &(w, h) in
        &[(1u32, 1u32), (2, 3), (3, 2), (5, 5), (7, 4), (13, 3), (17, 2), (31, 2), (33, 3)]
    {
        for op in OPERATORS {
            let white = [80.0f32, 200.0, 480.0, 333.3][(rng.u16() % 4) as usize];
            let st = TonemapSettings {
                operator: op,
                peak: rng.range(1.0, 10.0),
                knee: [1.0, 0.8, 0.5, 1.7][(rng.u16() % 4) as usize],
                dither: rng.u16().is_multiple_of(2),
                exposure: rng.range(0.3, 3.0),
            };
            let st = TonemapSettings { peak: st.peak.max(st.knee), ..st };
            // Mix random bit patterns with in-range values so both paths get traffic.
            let bits: Vec<u16> = (0..w * h * 3)
                .map(|i| {
                    if i % 5 == 0 {
                        rng.u16()
                    } else {
                        f16::from_f32(rng.range(-0.2, 2.0) * white / 80.0).to_bits()
                    }
                })
                .collect();
            let pad = (rng.u16() % 3) as usize * 8;
            let f = frame_from_bits(w, h, &bits, pad, Some(white));
            let out = to_sdr8(&f, &st).unwrap();
            let p = PixelParams::new(&st, white);
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 3) as usize;
                    let rgb = [0, 1, 2].map(|c| f16::from_bits(bits[i + c]).to_f32());
                    let want = reference_pixel(rgb, &p, x, y);
                    let got = &out.row(y)[x as usize * 4..x as usize * 4 + 4];
                    assert_eq!(got, [want[0], want[1], want[2], 255], "{op:?} {w}x{h} at {x},{y}");
                }
            }
        }
    }
}

#[test]
fn stride_padding_is_ignored() {
    let mut rng = Rng(42);
    let (w, h) = (9u32, 6u32);
    let bits: Vec<u16> =
        (0..w * h * 3).map(|_| f16::from_f32(rng.range(0.0, 6.0)).to_bits()).collect();
    let tight = to_sdr8(&frame_from_bits(w, h, &bits, 0, Some(200.0)), &TonemapSettings::default())
        .unwrap();
    for pad in [1usize, 8, 24, 100] {
        let padded =
            to_sdr8(&frame_from_bits(w, h, &bits, pad, Some(200.0)), &TonemapSettings::default())
                .unwrap();
        assert_eq!(padded.data(), tight.data(), "pad={pad}");
        assert_eq!(padded.stride(), w as usize * 4);
    }
}

#[test]
fn last_row_without_padding_is_accepted() {
    // Frame allows the final row to omit padding; build one by truncating.
    let bits = vec![f16::from_f32(1.0).to_bits(); 3 * 2 * 2];
    let full = frame_from_bits(2, 2, &bits, 16, Some(80.0));
    let stride = full.stride();
    let mut data = full.data().to_vec();
    data.truncate(stride + 16);
    let f = Frame::from_raw(
        Size::new(2, 2),
        stride,
        PixelFormat::Rgba16F,
        ColorSpace::ScRgbLinear,
        data,
    )
    .unwrap();
    let mut f = f;
    f.sdr_white_nits = Some(80.0);
    let out = to_sdr8(&f, &TonemapSettings::default()).unwrap();
    assert!(pixels(&out).iter().all(|p| *p == [255, 255, 255, 255]));
}

#[test]
fn empty_frames() {
    for (w, h) in [(0u32, 0u32), (0, 7), (7, 0)] {
        let mut f = Frame::new(Size::new(w, h), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
        f.sdr_white_nits = Some(200.0);
        let out = to_sdr8(&f, &TonemapSettings::default()).unwrap();
        assert_eq!(out.size(), Size::new(w, h));
        assert!(out.data().is_empty());
        assert_eq!(out.format(), PixelFormat::Rgba8);
    }
}

#[test]
fn output_format_and_metadata() {
    let mut f = frame_from_f32(3, 2, &[[0.5; 3]; 6], 200.0);
    f.origin = Point::new(-1920, 30);
    f.scale_factor = 1.5;
    f.timestamp = Some(std::time::Duration::from_millis(1234));
    let out = to_sdr8(&f, &TonemapSettings::default()).unwrap();
    assert_eq!(out.format(), PixelFormat::Rgba8);
    assert_eq!(out.color_space(), ColorSpace::Srgb);
    assert_eq!(out.stride(), 12);
    assert_eq!(out.origin, Point::new(-1920, 30));
    assert_eq!(out.scale_factor, 1.5);
    assert_eq!(out.timestamp, Some(std::time::Duration::from_millis(1234)));
    assert_eq!(out.sdr_white_nits, None);
    assert!(out.is_sdr8());
    assert!(!frame_needs_tonemap(&out));
    assert!(frame_needs_tonemap(&f));
    // The result can be encoded straight away.
    assert!(out.encode(ssx_types::EncodeOptions::default()).is_ok());
}

#[test]
fn alpha_is_forced_opaque() {
    let mut data = vec![0u8; 8];
    data[0..2].copy_from_slice(&f16::from_f32(0.4).to_le_bytes());
    data[6..8].copy_from_slice(&f16::from_f32(0.0).to_le_bytes()); // alpha 0
    let mut f =
        Frame::from_raw(Size::new(1, 1), 8, PixelFormat::Rgba16F, ColorSpace::ScRgbLinear, data)
            .unwrap();
    f.sdr_white_nits = Some(80.0);
    assert_eq!(pixels(&to_sdr8(&f, &TonemapSettings::default()).unwrap())[0][3], 255);
}

#[test]
fn missing_sdr_white_falls_back_to_80_nits() {
    let bits = vec![f16::from_f32(1.0).to_bits(); 3];
    let f = frame_from_bits(1, 1, &bits, 0, None);
    assert_eq!(pixels(&to_sdr8(&f, &TonemapSettings::default()).unwrap())[0], [255, 255, 255, 255]);
    let half = vec![f16::from_f32(0.5).to_bits(); 3];
    let f = frame_from_bits(1, 1, &half, 0, None);
    let s = TonemapSettings { dither: false, ..TonemapSettings::default() };
    assert_eq!(pixels(&to_sdr8(&f, &s).unwrap())[0], [188, 188, 188, 255]);
}

#[test]
fn invalid_sdr_white_is_an_error() {
    let bits = vec![f16::from_f32(1.0).to_bits(); 3];
    for bad in [0.0f32, -80.0, f32::NAN, f32::INFINITY] {
        let f = frame_from_bits(1, 1, &bits, 0, Some(bad));
        assert!(matches!(
            to_sdr8(&f, &TonemapSettings::default()),
            Err(HdrError::InvalidSdrWhite(_))
        ));
    }
}

#[test]
fn invalid_settings_are_rejected() {
    let f = frame_from_f32(1, 1, &[[1.0; 3]], 80.0);
    let bad = TonemapSettings { exposure: 0.0, ..TonemapSettings::default() };
    assert!(matches!(to_sdr8(&f, &bad), Err(HdrError::InvalidSetting { field: "exposure", .. })));
    let sdr = Frame::from_rgba8(1, 1, vec![1, 2, 3, 4]).unwrap();
    assert!(to_sdr8(&sdr, &bad).is_err());
    assert!(into_sdr8(sdr, &bad).is_err());
}

#[test]
fn eight_bit_input_passes_through_unchanged() {
    let s = TonemapSettings::default();
    let rgba = Frame::from_rgba8(2, 1, vec![10, 20, 30, 0, 40, 50, 60, 128]).unwrap();
    let out = to_sdr8(&rgba, &s).unwrap();
    assert_eq!(out.data(), rgba.data());
    assert_eq!(out.format(), PixelFormat::Rgba8);

    let bgra = Frame::from_raw(
        Size::new(2, 1),
        12, // padded stride
        PixelFormat::Bgra8,
        ColorSpace::Srgb,
        vec![30, 20, 10, 255, 60, 50, 40, 255, 9, 9, 9, 9],
    )
    .unwrap();
    let out = to_sdr8(&bgra, &s).unwrap();
    assert_eq!(out.data(), &[10, 20, 30, 255, 40, 50, 60, 255]);
    assert_eq!(out.stride(), 8);
    assert_eq!(into_sdr8(bgra, &s).unwrap().data(), out.data());
    assert!(!frame_needs_tonemap(&rgba));
}

#[test]
fn unsupported_combinations_error() {
    let f = Frame::new(Size::new(1, 1), PixelFormat::Rgba16F, ColorSpace::Srgb);
    assert!(matches!(
        to_sdr8(&f, &TonemapSettings::default()),
        Err(HdrError::UnsupportedFrame(PixelFormat::Rgba16F, ColorSpace::Srgb))
    ));
    let f = Frame::new(Size::new(1, 1), PixelFormat::Rgba8, ColorSpace::ScRgbLinear);
    assert!(to_sdr8(&f, &TonemapSettings::default()).is_err());
}

#[test]
fn non_finite_inputs_are_safe() {
    let nan = f32::NAN;
    let inf = f32::INFINITY;
    let px = [
        [nan, nan, nan],
        [inf, inf, inf],
        [-inf, -inf, -inf],
        [nan, 0.5, -inf],
        [inf, 0.0, 0.0],
        [-inf, inf, nan],
    ];
    let f = frame_from_f32(6, 1, &px, 80.0);
    for op in OPERATORS {
        for dither in [false, true] {
            let s = TonemapSettings { dither, ..settings(op) };
            let out = pixels(&to_sdr8(&f, &s).unwrap());
            assert_eq!(out[0], [0, 0, 0, 255], "NaN -> 0 ({op:?})");
            assert_eq!(out[1], [255, 255, 255, 255], "+Inf -> white ({op:?})");
            assert_eq!(out[2], [0, 0, 0, 255], "-Inf -> 0 ({op:?})");
            assert_eq!(out[3][0], 0);
            assert_eq!(out[3][2], 0);
            assert_eq!(out[4][0], 255);
            assert_eq!(out[5][1], 255);
        }
    }
    // The pure function agrees and stays finite / in range.
    let p = PixelParams::new(&TonemapSettings::default(), 200.0);
    for rgb in px {
        for c in tonemap_rgb(rgb, &p) {
            assert!(c.is_finite() && (0.0..=1.0).contains(&c));
        }
    }
    assert_eq!(scale_channel(f32::NAN, &p), 0.0);
    assert_eq!(scale_channel(f32::INFINITY, &p), p.peak);
    assert_eq!(scale_channel(f32::NEG_INFINITY, &p), 0.0);
}

#[test]
fn huge_finite_values_do_not_overflow() {
    let p = PixelParams::new(&TonemapSettings::default(), 80.0);
    for v in [f32::MAX, f32::MAX / 2.0, 65504.0, -f32::MAX] {
        for rgb in [[v; 3], [v, 1.0, 0.0], [v, -v, 0.5]] {
            for c in tonemap_rgb(rgb, &p) {
                assert!(c.is_finite() && (0.0..=1.0).contains(&c), "{rgb:?} -> {c}");
            }
        }
    }
}

#[test]
fn clip_operator_clamps_channels_independently() {
    let s = TonemapSettings { dither: false, ..settings(TonemapOperator::Clip) };
    let f = frame_from_f32(2, 1, &[[3.0, 0.5, 0.1], [1.0, 1.0, 5.0]], 80.0);
    let out = pixels(&to_sdr8(&f, &s).unwrap());
    assert_eq!(out[0], [255, 188, 89, 255]);
    assert_eq!(out[1], [255, 255, 255, 255]);
}

#[test]
fn default_operator_above_white_preserves_hue() {
    // Knee 1.0 leaves no headroom: bright colours are scaled by 1/max (hue-preserving).
    let s = TonemapSettings { dither: false, ..TonemapSettings::default() };
    let f = frame_from_f32(1, 1, &[[4.0, 2.0, 1.0]], 80.0);
    let out = pixels(&to_sdr8(&f, &s).unwrap())[0];
    let want = [1.0f32, 0.5, 0.25].map(|v| encode_channel(v, None));
    assert_eq!([out[0], out[1], out[2]], want);
}

#[test]
fn exposure_scales_linear_light() {
    let s = TonemapSettings { exposure: 2.0, dither: false, ..TonemapSettings::default() };
    let f = frame_from_f32(1, 1, &[[0.25; 3]], 80.0);
    assert_eq!(pixels(&to_sdr8(&f, &s).unwrap())[0], [188, 188, 188, 255]);
    let s = TonemapSettings { exposure: 0.5, ..s };
    let f = frame_from_f32(1, 1, &[[1.0; 3]], 80.0);
    assert_eq!(pixels(&to_sdr8(&f, &s).unwrap())[0], [188, 188, 188, 255]);
}

#[test]
fn hue_is_preserved_above_the_knee() {
    let mut rng = Rng(7);
    for op in ROLLOFF_OPS {
        for knee in [1.0f32, 0.8, 0.5] {
            let st =
                TonemapSettings { operator: op, knee, peak: 6.0, dither: false, exposure: 1.0 };
            let p = PixelParams::new(&st, 80.0);
            for _ in 0..2000 {
                let base = [rng.range(0.05, 1.0), rng.range(0.05, 1.0), rng.range(0.05, 1.0)];
                let m0 = base[0].max(base[1]).max(base[2]);
                let lambda = rng.range(knee / m0 * 1.001, 5.5 / m0);
                let rgb = base.map(|v| v * lambda);
                let out = tonemap_rgb(rgb, &p);
                for a in 0..3 {
                    for b in 0..3 {
                        // out[a]/out[b] == rgb[a]/rgb[b]  <=>  cross products agree.
                        let lhs = out[a] * rgb[b];
                        let rhs = out[b] * rgb[a];
                        assert!(
                            (lhs - rhs).abs() <= 2e-6 * lhs.abs().max(rhs.abs()).max(1e-3),
                            "{op:?} knee={knee}: {rgb:?} -> {out:?}"
                        );
                    }
                }
                assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
            }
        }
    }
}

#[test]
fn grey_ramp_through_the_pipeline_is_monotonic_and_continuous_at_the_knee() {
    for op in ROLLOFF_OPS {
        for knee in [1.0f32, 0.85, 0.6] {
            let st =
                TonemapSettings { operator: op, knee, peak: 5.0, dither: false, exposure: 1.0 };
            let p = PixelParams::new(&st, 200.0);
            let n = 5000;
            let mut prev = -1.0f32;
            let mut prev_enc = 0u8;
            for i in 0..=n {
                let sdr = 6.0 * i as f32 / n as f32; // multiples of SDR white
                let g = tonemap_rgb([sdr * 2.5; 3], &p)[0];
                assert!(g >= prev, "{op:?} knee={knee} not monotonic at {sdr}");
                prev = g;
                let e = encode_channel(g, None);
                assert!(e >= prev_enc);
                // Adjacent samples never jump by more than a couple of codes near the knee.
                assert!(sdr < 0.05 || e - prev_enc <= 3, "{op:?} knee={knee}: jump at {sdr}");
                prev_enc = e;
            }
            assert_eq!(prev, 1.0);
            // Exactly at and just above the knee the output is continuous.
            let at = tonemap_rgb([knee * 2.5; 3], &p)[0];
            let above = tonemap_rgb([(knee + 1e-4) * 2.5; 3], &p)[0];
            assert!((above - at).abs() < 5e-4, "{op:?} knee={knee}");
        }
    }
}

#[test]
fn peak_and_above_are_white_and_below_peak_is_not() {
    for op in ROLLOFF_OPS {
        let st =
            TonemapSettings { operator: op, knee: 0.75, peak: 4.0, dither: false, exposure: 1.0 };
        let p = PixelParams::new(&st, 80.0);
        for v in [4.0f32, 4.5, 100.0, 60000.0] {
            assert_eq!(tonemap_rgb([v; 3], &p), [1.0; 3], "{op:?} {v}");
        }
        let below = tonemap_rgb([3.0; 3], &p)[0];
        assert!(below < 1.0 && below > 0.75, "{op:?} {below}");
        // Below the knee: exact pass-through.
        assert_eq!(tonemap_rgb([0.7; 3], &p), [0.7; 3]);
    }
    // Real highlight detail survives with a lowered knee: 2x and 3x white differ.
    let st = TonemapSettings { knee: 0.75, ..TonemapSettings::default() };
    let p = PixelParams::new(&st, 80.0);
    assert!(tonemap_rgb([3.0; 3], &p)[0] > tonemap_rgb([2.0; 3], &p)[0]);
}

#[test]
fn negative_channels_are_gamut_mapped_softly() {
    let mut rng = Rng(99);
    let st = TonemapSettings { dither: false, ..TonemapSettings::default() };
    let p = PixelParams::new(&st, 200.0);
    for _ in 0..20_000 {
        // Scaled-domain colours with some negative channels, luminance within range.
        let x = [rng.range(-0.4, 1.0), rng.range(-0.4, 1.0), rng.range(-0.4, 1.0)];
        let y = luminance(x);
        let g = gamut_map(x);
        assert!(g.iter().all(|v| v.is_finite() && *v >= 0.0), "{x:?} -> {g:?}");
        if y > 0.0 {
            assert!((luminance(g) - y).abs() <= 1e-5 * y.max(1e-2), "{x:?} -> {g:?}");
            // No hard clip: the result lies on the line between x and the grey (y,y,y).
            let min = x[0].min(x[1]).min(x[2]);
            if min < 0.0 {
                let t = y / (y - min);
                for c in 0..3 {
                    assert!((g[c] - (y + t * (x[c] - y))).abs() < 1e-5);
                }
                assert!(g.iter().copied().fold(f32::MAX, f32::min) < 1e-5);
            } else {
                assert_eq!(g, x);
            }
        } else {
            assert_eq!(g, [0.0; 3]);
        }
        // The full pipeline stays in range too.
        let out = tonemap_rgb(x.map(|v| v * 2.5), &p);
        assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
    }
}

#[test]
fn negative_channel_frame_example() {
    // Saturated wide-gamut green: (-0.2, 1.0, -0.1) at SDR white 80.
    let f = frame_from_f32(1, 1, &[[-0.2, 1.0, -0.1]], 80.0);
    let s = TonemapSettings { dither: false, ..TonemapSettings::default() };
    let out = pixels(&to_sdr8(&f, &s).unwrap())[0];
    // Independent clamping would give (0, 255, 0). Desaturating towards grey keeps the
    // luminance instead: the most negative channel lands on zero and green shrinks a
    // little.
    let y = luminance([-0.2, 1.0, -0.1]);
    let g = gamut_map([-0.2, 1.0, -0.1]);
    assert!((luminance(g) - y).abs() < 1e-6);
    assert_eq!(out[3], 255);
    assert!(out[1] < 255 && out[1] > 200, "{out:?}");
    assert_eq!(out[0], 0);
}

#[test]
fn dither_is_deterministic() {
    let mut rng = Rng(5);
    let px: Vec<[f32; 3]> = (0..64 * 64)
        .map(|_| {
            let v = rng.range(0.0, 3.0);
            [v, v * 0.8, v * 0.5]
        })
        .collect();
    let f = frame_from_f32(64, 64, &px, 200.0);
    let s = TonemapSettings::default();
    let a = to_sdr8(&f, &s).unwrap();
    let b = to_sdr8(&f, &s).unwrap();
    assert_eq!(a.data(), b.data());
}

#[test]
fn dither_is_mean_preserving_on_non_representable_values() {
    // Grey whose encoded value is 100.4 codes: plain rounding gives 100 everywhere,
    // dithering mixes 100 and 101 with mean ~100.4.
    let target = 100.4f32;
    let lin = srgb_eotf(target / 255.0);
    let (w, h) = (128u32, 128u32);
    let f = frame_from_f32(w, h, &vec![[lin; 3]; (w * h) as usize], 80.0);
    let plain =
        to_sdr8(&f, &TonemapSettings { dither: false, ..TonemapSettings::default() }).unwrap();
    assert!(pixels(&plain).iter().all(|p| p[0] == 100));
    let dith = to_sdr8(&f, &TonemapSettings::default()).unwrap();
    let px = pixels(&dith);
    assert!(px.iter().all(|p| p[0] == 100 || p[0] == 101));
    assert!(px.iter().all(|p| p[0] == p[1] && p[1] == p[2]), "dither must be neutral");
    let mean = px.iter().map(|p| f64::from(p[0])).sum::<f64>() / px.len() as f64;
    // f16 quantises the input a little, so allow a bit more than the noise error.
    assert!((mean - 100.4).abs() < 0.05, "mean {mean}");
}

#[test]
fn dither_breaks_up_banding_in_hdr_gradient() {
    // A very shallow gradient (much less than one code across the frame) must not
    // collapse into a single hard step: with dither the transition is spread out.
    let (w, h) = (256u32, 16u32);
    let px: Vec<[f32; 3]> = (0..w * h)
        .map(|i| {
            let x = (i % w) as f32 / w as f32;
            [srgb_eotf((100.2 + 0.6 * x) / 255.0); 3]
        })
        .collect();
    let f = frame_from_f32(w, h, &px, 80.0);
    let out = to_sdr8(&f, &TonemapSettings::default()).unwrap();
    let mut transitions = 0;
    for y in 0..h {
        let row = out.row(y);
        for x in 1..w as usize {
            if row[x * 4] != row[(x - 1) * 4] {
                transitions += 1;
            }
        }
    }
    assert!(transitions > h as usize * 8, "only {transitions} transitions; dither ineffective");
}

#[test]
fn results_do_not_depend_on_thread_count() {
    let mut rng = Rng(11);
    let bits: Vec<u16> = (0..200 * 30 * 3).map(|_| rng.u16()).collect();
    let f = frame_from_bits(200, 30, &bits, 8, Some(203.0));
    let s = TonemapSettings { operator: TonemapOperator::Bt2390, knee: 0.7, ..Default::default() };
    let reference = to_sdr8(&f, &s).unwrap();
    for threads in [1usize, 2, 3] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let out = pool.install(|| to_sdr8(&f, &s).unwrap());
        assert_eq!(out.data(), reference.data(), "threads={threads}");
    }
}

/// CPU time consumed so far by the calling thread (Linux only), immune to the machine being
/// busy with other work.
fn thread_cpu_ms() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let ns: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(ns / 1e6)
}

/// Timing of a 4K conversion. Run with
/// `cargo test -p ssx-hdr --release -- --ignored timing --nocapture`.
///
/// Prints wall time on all cores and on a single thread (the single-thread figure divided
/// by the core count is what an otherwise idle machine achieves).
#[test]
#[ignore = "timing measurement; run in release mode with --nocapture"]
fn timing_4k() {
    use std::time::Instant;
    let (w, h) = (3840u32, 2160u32);
    let mut rng = Rng(1);
    let sdr = |rng: &mut Rng| f16::from_f32(rng.range(0.0, 1.0) * 2.5).to_bits();
    let hdr = |rng: &mut Rng| f16::from_f32(rng.range(-0.1, 12.0)).to_bits();
    let push = |v: &mut Vec<u16>, is_hdr: bool, rng: &mut Rng| {
        for _ in 0..3 {
            v.push(if is_hdr { hdr(rng) } else { sdr(rng) });
        }
    };
    // Typical desktop: SDR everywhere with a 1280x720 HDR window.
    let mut desktop = Vec::new();
    // Pure SDR content.
    let mut sdr_only = Vec::new();
    // Pathological: every fifth pixel HDR, randomly interleaved (branch-predictor hostile).
    let mut mixed = Vec::new();
    // Worst case: everything needs the full pipeline.
    let mut all_hdr = Vec::new();
    for y in 0..h {
        for x in 0..w {
            push(&mut desktop, (400..1680).contains(&x) && (300..1020).contains(&y), &mut rng);
            push(&mut sdr_only, false, &mut rng);
            push(&mut mixed, (x + y * w) % 5 == 0, &mut rng);
            push(&mut all_hdr, true, &mut rng);
        }
    }
    let single = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    for (name, bits) in
        [("sdr-only", &sdr_only), ("desktop", &desktop), ("mixed", &mixed), ("all-hdr", &all_hdr)]
    {
        let f = frame_from_bits(w, h, bits, 0, Some(200.0));
        for (label, s) in [
            ("default+dither", TonemapSettings::default()),
            ("default no-dither", TonemapSettings { dither: false, ..Default::default() }),
            (
                "bt2390 knee0.75",
                TonemapSettings {
                    operator: TonemapOperator::Bt2390,
                    knee: 0.75,
                    ..Default::default()
                },
            ),
            (
                "aces knee0.75 no-dither",
                TonemapSettings {
                    operator: TonemapOperator::AcesFit,
                    knee: 0.75,
                    dither: false,
                    ..Default::default()
                },
            ),
        ] {
            let _ = to_sdr8(&f, &s).unwrap(); // warm-up (thread pool, page faults)
            // Best of five: other processes on the machine only ever make runs slower.
            let mut all = f64::MAX;
            let mut cpu = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let _ = to_sdr8(&f, &s).unwrap();
                all = all.min(t.elapsed().as_secs_f64() * 1e3);
                let c = single.install(|| {
                    let c0 = thread_cpu_ms()?;
                    let _ = to_sdr8(&f, &s).unwrap();
                    Some(thread_cpu_ms()? - c0)
                });
                cpu = cpu.min(c.unwrap_or(f64::NAN));
            }
            println!(
                "3840x2160 {name:<8} {label:<24} {all:7.1} ms wall on all cores, {cpu:7.1} ms cpu on 1 thread"
            );
        }
    }
}
