//! Timing checks for 4K frames. `#[ignore]`d so CI stays fast and deterministic; run with
//! `cargo test -p ssx-imgfx --release --test perf -- --ignored --nocapture`.

use std::time::Instant;

use ssx_imgfx::{
    BlendMode, BlurMethod, Effect, Lens, LensShape, PointF, ResizeFilter, ShadowParams,
    composite_over, gaussian_blur, magnify, pixelate, resize, rotate, rotate_90,
};
use ssx_types::{Frame, Point, Rect};

fn frame_4k() -> Frame {
    let (w, h) = (3840u32, 2160u32);
    let mut data = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8, 255]);
        }
    }
    Frame::from_rgba8(w, h, data).expect("exact-size buffer")
}

fn time<T>(label: &str, f: impl FnOnce() -> T) {
    let t = Instant::now();
    let out = f();
    eprintln!("{label:<44} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    drop(out);
}

#[test]
#[ignore = "timing report; run with --release --ignored --nocapture"]
fn report_4k_timings() {
    let base = frame_4k();
    eprintln!("threads: {}", rayon::current_num_threads());
    for (sigma, method) in [
        (2.0, BlurMethod::Exact),
        (4.0, BlurMethod::Exact),
        (10.0, BlurMethod::Box3),
        (10.0, BlurMethod::Auto),
    ] {
        let mut f = base.clone();
        time(&format!("gaussian blur 4K sigma={sigma} {method:?}"), || {
            gaussian_blur(&mut f, None, sigma, method).unwrap();
        });
    }
    let mut f = base.clone();
    time("gaussian blur 600x400 region sigma=12", || {
        gaussian_blur(&mut f, Some(Rect::new(500, 500, 600, 400)), 12.0, BlurMethod::Auto).unwrap();
    });
    let mut f = base.clone();
    time("pixelate 4K block=16", || pixelate(&mut f, None, 16).unwrap());
    time("resize 4K -> 1080p lanczos3", || resize(&base, 1920, 1080, ResizeFilter::Lanczos3));
    time("resize 4K -> 1080p bilinear", || resize(&base, 1920, 1080, ResizeFilter::Bilinear));
    let small = resize(&base, 1920, 1080, ResizeFilter::Bilinear).unwrap();
    time("resize 1080p -> 4K lanczos3", || resize(&small, 3840, 2160, ResizeFilter::Lanczos3));
    time("rotate 90 4K", || rotate_90(&base));
    time("rotate 17.5 deg 4K bilinear", || rotate(&base, 17.5, [0; 4], true));
    time("effect: sepia 4K (copy + matrix)", || Effect::Sepia.apply(&base, None));
    let src = base.clone();
    let mut dst = base.clone();
    time("composite 4K over 4K (normal, 50%)", || {
        composite_over(&mut dst, &src, Point::new(0, 0), 0.5, BlendMode::Normal).unwrap();
    });
    time("composite 4K over 4K (multiply)", || {
        composite_over(&mut dst, &src, Point::new(0, 0), 1.0, BlendMode::Multiply).unwrap();
    });
    time("magnify 400x400 lens", || {
        let lens = Lens {
            dst: Rect::new(100, 100, 400, 400),
            shape: LensShape::Ellipse,
            source_center: PointF::new(1000.0, 1000.0),
            zoom: 3.0,
        };
        magnify(&src, &mut dst, &lens).unwrap();
    });
    time("drop shadow on 1080p", || {
        Effect::DropShadow { params: ShadowParams::default() }.apply(&small, None)
    });
}
