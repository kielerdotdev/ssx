//! CPU benchmark of the overlay pipeline (no window): start-up conversion, first frame and
//! interactive redraws at a given desktop size.
//!
//! ```text
//! cargo run --release -p ssx-overlay --example bench -- 3840x2160
//! ```
//!
//! "Frame time" is `begin_frame` (scene + damage) plus rendering every dirty rectangle into a
//! BGRA buffer; presenting (X11 `PutImage`, `wl_shm` commit) is measured separately by
//! `tests/perf.rs`.

#![allow(clippy::cast_possible_wrap)] // benchmark coordinates are screen-sized

use std::time::{Duration, Instant};

use ssx_overlay::{
    OverlayInput, OverlayOptions, SelectMode,
    app::OverlayApp,
    demo::demo_frame,
    model::{InputEvent, Key, KeyEvent, PointerButton, PointerEvent},
    render::TargetBuf,
};
use ssx_types::Point;

fn mv(app: &mut OverlayApp, x: i32, y: i32) {
    app.handle(InputEvent::Pointer(PointerEvent::Move { pos: Point::new(x, y) }));
}

struct Stats {
    frames: Vec<Duration>,
    pixels: u64,
}

impl Stats {
    fn report(&mut self, name: &str) {
        self.frames.sort();
        let n = self.frames.len().max(1);
        let total: Duration = self.frames.iter().sum();
        let avg = total / n as u32;
        let p95 = self.frames[(n * 95 / 100).min(n - 1)];
        let max = self.frames.last().copied().unwrap_or_default();
        println!(
            "{name:<34} avg {:>7.3} ms  p95 {:>7.3} ms  max {:>7.3} ms  => {:>7.0} fps  (avg dirty {:>9} px)",
            avg.as_secs_f64() * 1e3,
            p95.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3,
            1.0 / avg.as_secs_f64().max(1e-9),
            self.pixels / n as u64,
        );
    }
}

fn frame(app: &mut OverlayApp, buf: &mut [u8], size: ssx_types::Size, s: &mut Stats) {
    let t = Instant::now();
    let rects = app.begin_frame();
    let mut px = 0u64;
    for r in &rects {
        let mut target = TargetBuf { origin: Point::new(0, 0), size, data: buf };
        app.render(*r, &mut target);
        px += r.area();
    }
    s.frames.push(t.elapsed());
    s.pixels += px;
}

fn session(w: u32, h: u32, mode: SelectMode) -> (OverlayApp, Vec<u8>) {
    let f = demo_frame(w, h, Point::new(0, 0));
    let input = OverlayInput {
        desktop: f,
        monitors: vec![],
        windows: vec![],
        options: OverlayOptions { mode, ..OverlayOptions::default() },
    };
    (OverlayApp::new(input).expect("app"), vec![0u8; w as usize * h as usize * 4])
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "3840x2160".into());
    let (w, h) = arg
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .expect("size as WxH");
    let size = ssx_types::Size::new(w, h);
    println!("desktop {w}x{h} ({:.1} MB)", f64::from(w) * f64::from(h) * 4.0 / 1e6);

    // Start-up: convert to BGRA + pre-dim (multi-threaded), then the first full frame.
    let f = demo_frame(w, h, Point::new(0, 0));
    let input = OverlayInput {
        desktop: f,
        monitors: vec![],
        windows: vec![],
        options: OverlayOptions::default(),
    };
    let t = Instant::now();
    let mut app = OverlayApp::new(input).expect("app");
    println!("ingest (convert + pre-dim)          {:>7.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let t = Instant::now();
    let rects = app.begin_frame();
    for r in &rects {
        let mut target = TargetBuf { origin: Point::new(0, 0), size, data: &mut buf };
        app.render(*r, &mut target);
    }
    println!("first full frame render             {:>7.2} ms", t.elapsed().as_secs_f64() * 1e3);
    println!();

    let (cx, cy) = (w as i32 / 2, h as i32 / 2);
    let n = 400;

    // 1. Idle pointer: crosshair + loupe + no selection.
    let (mut app, mut buf) = session(w, h, SelectMode::Rect);
    frame(&mut app, &mut buf, size, &mut Stats { frames: vec![], pixels: 0 });
    let mut s = Stats { frames: vec![], pixels: 0 };
    for i in 0..n {
        mv(&mut app, 100 + i * (w as i32 - 200) / n, 100 + i * (h as i32 - 200) / n);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("idle pointer (guides + loupe)");

    // 2. Drag a rectangle from the top-left to almost the bottom-right corner.
    let mut s = Stats { frames: vec![], pixels: 0 };
    mv(&mut app, 200, 200);
    app.handle(InputEvent::Pointer(PointerEvent::Down {
        pos: Point::new(200, 200),
        button: PointerButton::Left,
        time_ms: 0,
    }));
    for i in 1..=n {
        mv(&mut app, 200 + i * (w as i32 - 400) / n, 200 + i * (h as i32 - 400) / n);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("drag rect to ~full screen");

    app.handle(InputEvent::Pointer(PointerEvent::Up {
        pos: Point::new(w as i32 - 200, h as i32 - 200),
        button: PointerButton::Left,
    }));
    frame(&mut app, &mut buf, size, &mut Stats { frames: vec![], pixels: 0 });

    // 3. Move that big selection by its body.
    let mut s = Stats { frames: vec![], pixels: 0 };
    mv(&mut app, cx, cy);
    app.handle(InputEvent::Pointer(PointerEvent::Down {
        pos: Point::new(cx, cy),
        button: PointerButton::Left,
        time_ms: 10_000,
    }));
    for i in 1..=n {
        let d = (i % 200) - 100;
        mv(&mut app, cx + d, cy + d / 2);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("move big selection (body drag)");
    app.handle(InputEvent::Pointer(PointerEvent::Up {
        pos: Point::new(cx, cy),
        button: PointerButton::Left,
    }));

    // 4. Resize with a handle, Shift held (aspect-locked).
    let mut s = Stats { frames: vec![], pixels: 0 };
    app.handle(InputEvent::Key(KeyEvent { key: Key::Shift, pressed: true }));
    let corner = Point::new(w as i32 - 200 + 150, h as i32 - 200 + 100);
    mv(&mut app, corner.x, corner.y);
    for i in 0..n {
        mv(&mut app, corner.x - i / 2, corner.y - i / 3);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("pointer moves with selection present");

    // 5. Ellipse drag: the cut-out is not a rectangle, so damage is the bounding boxes.
    let (mut app, mut buf) = session(w, h, SelectMode::Ellipse);
    frame(&mut app, &mut buf, size, &mut Stats { frames: vec![], pixels: 0 });
    let mut s = Stats { frames: vec![], pixels: 0 };
    mv(&mut app, 200, 200);
    app.handle(InputEvent::Pointer(PointerEvent::Down {
        pos: Point::new(200, 200),
        button: PointerButton::Left,
        time_ms: 0,
    }));
    for i in 1..=n {
        mv(&mut app, 200 + i * (w as i32 - 400) / n, 200 + i * (h as i32 - 400) / n);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("drag ellipse to ~full screen");

    // 6. Freeform stroke (hand-drawn circle of 400 points).
    let (mut app, mut buf) = session(w, h, SelectMode::Freeform);
    frame(&mut app, &mut buf, size, &mut Stats { frames: vec![], pixels: 0 });
    let mut s = Stats { frames: vec![], pixels: 0 };
    let r = f64::from(h.min(w)) / 3.0;
    mv(&mut app, cx + r as i32, cy);
    app.handle(InputEvent::Pointer(PointerEvent::Down {
        pos: Point::new(cx + r as i32, cy),
        button: PointerButton::Left,
        time_ms: 0,
    }));
    for i in 1..=n {
        let a = f64::from(i) / f64::from(n) * std::f64::consts::TAU;
        mv(&mut app, cx + (r * a.cos()) as i32, cy + (r * a.sin()) as i32);
        frame(&mut app, &mut buf, size, &mut s);
    }
    s.report("freeform circle stroke");
}
