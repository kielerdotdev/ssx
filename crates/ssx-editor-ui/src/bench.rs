//! Scripted workloads for measuring frame times on a real window (`--bench pan-zoom|objects`,
//! a hidden developer flag). Run under Xvfb + lavapipe they give pessimistic software-rendering
//! numbers; on a GPU machine they show what users get.
//!
//! The numbers reported are *frame periods*: the wall time between consecutive frames while
//! the script forces continuous repainting, so they include tile rendering, egui tessellation,
//! the GPU upload and presentation.

use std::time::Instant;

use egui::{Context, vec2};
use ssx_editor::{PointF, Tool};

use crate::{app::EditorApp, request::BenchMode};

/// The running benchmark.
#[derive(Debug)]
pub struct Bench {
    mode: BenchMode,
    frame: u32,
    started: Instant,
    last: Option<Instant>,
    periods_ms: Vec<f32>,
    engine_ms: Vec<f32>,
    seed: u64,
}

const WARMUP: usize = 6;

impl Bench {
    /// A benchmark of `mode`.
    pub fn new(mode: BenchMode) -> Self {
        Self {
            mode,
            frame: 0,
            started: Instant::now(),
            last: None,
            periods_ms: Vec::new(),
            engine_ms: Vec::new(),
            seed: 0x2545_F491_4F6C_DD1D,
        }
    }

    fn rand(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        ((self.seed >> 11) as f64 / (1u64 << 53) as f64) as f32
    }

    /// Advances the script by one frame; returns `true` when it has finished.
    pub fn step(&mut self, app: &mut EditorApp, ctx: &Context) -> bool {
        let now = Instant::now();
        if let Some(l) = self.last {
            self.periods_ms.push((now - l).as_secs_f32() * 1000.0);
            self.engine_ms.push(app.canvas.stats.last_frame_render_ms);
        }
        self.last = Some(now);
        let f = self.frame;
        self.frame += 1;
        ctx.request_repaint();
        if f < 10 {
            return false; // let the first paint settle
        }
        let t = f - 10;
        match self.mode {
            BenchMode::PanZoom => {
                match t {
                    0..=39 => app.bench_zoom(1.045),
                    40..=79 => app.bench_pan(vec2(
                        if t % 2 == 0 { 34.0 } else { -6.0 },
                        19.0 * if t < 60 { 1.0 } else { -1.0 },
                    )),
                    80..=119 => app.bench_zoom(1.0 / 1.045),
                    _ => return true,
                }
                false
            }
            BenchMode::Objects => {
                if t < 100 {
                    let (w, h) = app.doc.doc().image_size();
                    let (w, h) = (w as f32, h as f32);
                    let a = PointF::new(self.rand() * w * 0.8, self.rand() * h * 0.8);
                    let b = PointF::new(
                        a.x + 40.0 + self.rand() * 300.0,
                        a.y + 30.0 + self.rand() * 200.0,
                    );
                    let tool = [
                        Tool::Rectangle,
                        Tool::Ellipse,
                        Tool::Arrow,
                        Tool::Line,
                        Tool::Step,
                        Tool::Blur,
                    ][(t % 6) as usize];
                    app.bench_draw(tool, a, b);
                    false
                } else if t < 140 {
                    // With 100 objects on the picture: pan and zoom.
                    if t % 2 == 0 {
                        app.bench_zoom(if t < 120 { 1.06 } else { 1.0 / 1.06 });
                    } else {
                        app.bench_pan(vec2(28.0, 12.0));
                    }
                    false
                } else {
                    true
                }
            }
        }
    }

    /// Prints the statistics as one JSON line on stderr.
    pub fn report(&self, app: &EditorApp) {
        let mut p: Vec<f32> = self.periods_ms.iter().skip(WARMUP).copied().collect();
        let mut e: Vec<f32> = self.engine_ms.iter().skip(WARMUP).copied().collect();
        p.sort_by(f32::total_cmp);
        e.sort_by(f32::total_cmp);
        let pct = |v: &[f32], q: f32| -> f32 {
            if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f32 * q).round() as usize] }
        };
        let mean = |v: &[f32]| -> f32 {
            if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
        };
        let (w, h) = app.doc.doc().image_size();
        let json = serde_json::json!({
            "mode": match self.mode { BenchMode::PanZoom => "pan-zoom", BenchMode::Objects => "objects" },
            "image": [w, h],
            "objects": app.doc.doc().objects().len(),
            "frames": p.len(),
            "frame_ms": { "mean": mean(&p), "p50": pct(&p, 0.5), "p95": pct(&p, 0.95), "max": pct(&p, 1.0) },
            "engine_render_ms": { "mean": mean(&e), "p95": pct(&e, 0.95), "max": pct(&e, 1.0) },
            "regions_rendered": app.canvas.stats.regions_rendered,
            "total_engine_ms": app.canvas.stats.render_ms,
            "wall_s": self.started.elapsed().as_secs_f32(),
        });
        eprintln!("{json}");
    }
}
