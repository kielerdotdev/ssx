//! Throughput and CPU cost of the recording pipeline on the synthetic source.
//!
//! ```text
//! cargo run -p ssx-record --release --example perf -- --encoder libx264 --speed fastest
//! cargo run -p ssx-record --release --example perf -- --encoder mpeg4 --mode max
//! ```
//!
//! Two modes:
//!
//! * `realtime` (default): frames arrive live at `--fps`; reports whether the pipeline kept
//!   up (frames encoded vs. slots, drops) and the CPU use of the whole process;
//! * `max`: virtual time, the source waits for the pipeline (backpressure): reports how many
//!   frames per second the convert + encode stages sustain at most.
//!
//! CPU time and peak memory come from `/proc/self` (Linux only; `n/a` elsewhere).

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use ssx_record::{
    encode::{HwPolicy, SpeedPreset, VideoSettings, ffmpeg::FfmpegProber},
    session::{RecordConfig, RecordingSession},
    source::synthetic::{SyntheticConfig, SyntheticSource, TimeMode},
    time::Fps,
};

fn proc_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised command name; utime and stime are the 12th and 13th.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0) // CLK_TCK is 100 on Linux
}

fn peak_rss_mib() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024.0)
}

fn main() -> ExitCode {
    let mut encoder = "libx264".to_owned();
    let mut speed = SpeedPreset::Fastest;
    let (mut w, mut h) = (1920u32, 1080u32);
    let mut fps = 30.0f64;
    let mut seconds = 10.0f64;
    let mut max_mode = false;
    let mut out = std::env::temp_dir().join("ssx-perf.mp4");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut v = || it.next().unwrap_or_default();
        match a.as_str() {
            "--encoder" => encoder = v(),
            "--speed" => {
                speed = match v().as_str() {
                    "fastest" | "ultrafast" => SpeedPreset::Fastest,
                    "fast" | "veryfast" => SpeedPreset::Fast,
                    "balanced" | "medium" => SpeedPreset::Balanced,
                    "small" | "slow" => SpeedPreset::Small,
                    other => {
                        eprintln!("unknown speed {other}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--size" => {
                let s = v();
                let Some((a, b)) = s.split_once('x') else {
                    eprintln!("--size WxH");
                    return ExitCode::from(2);
                };
                w = a.parse().unwrap_or(w);
                h = b.parse().unwrap_or(h);
            }
            "--fps" => fps = v().parse().unwrap_or(fps),
            "--seconds" => seconds = v().parse().unwrap_or(seconds),
            "--mode" => max_mode = v() == "max",
            "-o" => out = PathBuf::from(v()),
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(fps_r) = Fps::from_f64(fps) else {
        eprintln!("bad fps");
        return ExitCode::from(2);
    };
    let frames = (fps * seconds).round() as u64;
    let mut synth = SyntheticConfig::new(w, h, fps_r).max_frames(frames);
    if max_mode {
        synth = synth.time(TimeMode::Virtual);
    }
    let mut cfg = RecordConfig::new(&out);
    cfg.fps = fps_r;
    cfg.video = VideoSettings {
        hw: HwPolicy::SoftwareOnly,
        speed,
        encoder_override: Some(encoder.clone()),
        ..VideoSettings::default()
    };

    let cpu0 = proc_cpu_seconds();
    let t0 = Instant::now();
    let session = match RecordingSession::start(
        cfg,
        Box::new(SyntheticSource::new(synth)),
        Vec::new(),
        &FfmpegProber,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let info = session.info().clone();
    let _ = session.wait_finished(Duration::from_secs_f64(seconds * 10.0 + 30.0));
    let rec = match session.stop() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("stop failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wall = t0.elapsed().as_secs_f64();
    let cpu = match (cpu0, proc_cpu_seconds()) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    };
    let s = &rec.stats;
    println!("encoder      : {}", info.encoder);
    println!(
        "input        : {w}x{h} @ {fps} fps, {seconds} s, mode {}",
        if max_mode { "max (virtual time)" } else { "realtime" }
    );
    println!("wall         : {wall:.2} s");
    println!(
        "encoded      : {} of {} slots ({} dropped under load, {} repeated)",
        s.encoded,
        s.slots,
        s.dropped_real(),
        s.duplicated
    );
    println!("throughput   : {:.1} frames/s", s.encoded as f64 / wall);
    if let Some(cpu) = cpu {
        println!(
            "cpu          : {cpu:.2} s = {:.0}% of one core ({:.0}% of {} cores)",
            cpu / wall * 100.0,
            cpu / wall * 100.0 / num_cpus() as f64,
            num_cpus()
        );
    } else {
        println!("cpu          : n/a");
    }
    if let Some(rss) = peak_rss_mib() {
        println!("peak rss     : {rss:.0} MiB");
    }
    println!(
        "file         : {} bytes ({:.1} Mbit/s)",
        rec.file_bytes,
        rec.file_bytes as f64 * 8.0 / rec.duration.as_secs_f64().max(0.001) / 1e6
    );
    println!("queue        : peak {} of {} frames", rec.peak_queue_frames, rec.queue_capacity);
    let _ = std::fs::remove_file(&out);
    ExitCode::SUCCESS
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}
