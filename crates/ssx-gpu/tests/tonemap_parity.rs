//! GPU tonemap vs the `ssx-hdr` CPU reference.
//!
//! Parity contract (see README): pixels that are SDR-representable are *byte-identical*;
//! every other pixel is within one code value; and at most a tiny fraction of pixels
//! differ at all (last-bit differences in `pow`/fused multiply-add flipping a dither
//! decision).

mod common;

use common::{Diff, Rng, diff_rgba, gpu, hdr_frame, hdr_frame_bits};
use ssx_gpu::GpuTonemapper;
use ssx_hdr::{TonemapOperator, TonemapSettings, to_sdr8};
use ssx_types::Frame;

const OPERATORS: [TonemapOperator; 4] = [
    TonemapOperator::Clip,
    TonemapOperator::ReinhardExtended,
    TonemapOperator::Bt2390,
    TonemapOperator::AcesFit,
];

fn settings(op: TonemapOperator, dither: bool) -> TonemapSettings {
    // A knee below 1 so the shoulder is actually exercised.
    TonemapSettings { operator: op, peak: 4.0, knee: 0.8, dither, exposure: 1.0 }
}

fn run(t: &GpuTonemapper, f: &Frame, s: &TonemapSettings) -> (Frame, Diff) {
    let gpu = t.tonemap(f, s).expect("gpu tonemap");
    let cpu = to_sdr8(f, s).expect("cpu tonemap");
    let d = diff_rgba(&gpu, &cpu);
    (gpu, d)
}

#[test]
fn smoke_gradient_matches_cpu() {
    let Some(ctx) = gpu() else { return };
    let t = GpuTonemapper::new(ctx).unwrap();
    let (w, h) = (300u32, 20u32);
    let px: Vec<[f32; 4]> = (0..w * h)
        .map(|i| {
            let v = (i % w) as f32 / w as f32 * 8.0;
            [v, v * 0.8, v * 0.5, 1.0]
        })
        .collect();
    let f = hdr_frame(w, h, 0, 80.0, &px);
    for op in OPERATORS {
        let (_, d) = run(&t, &f, &settings(op, true));
        eprintln!("{op:?}: {d:?}");
        assert!(d.max_diff <= 1, "{op:?}: {d:?}");
    }
    let _ = (Rng(1), hdr_frame_bits);
}
