//! Manual capture tool for Windows developers and CI runners.
//!
//! ```text
//! cargo run -p ssx-capture-win --example capture -- [OUT_DIR] [--cursor]
//! ```
//!
//! Lists the monitors and windows, then captures every monitor and the first few windows
//! into `OUT_DIR` (default `./ssx-capture-out`) as PNG files, printing what each frame is
//! (format, colour space, SDR white, timings). HDR (float scRGB) frames are written twice:
//! `*-hdr-preview.png` is a *naive* clip-tonemap (SDR white maps to 1.0, then the sRGB curve)
//! meant only for eyeballing; real tonemapping lives in `ssx-hdr`.
//!
//! Set `RUST_LOG`-style output by installing a `tracing` subscriber in your own binary; the
//! backend logs each fallback with its reason via `tracing`.

#[cfg(not(windows))]
fn main() {
    eprintln!("This example only runs on Windows.");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_main::run()
}

#[cfg(windows)]
mod windows_main {
    use std::{path::PathBuf, time::Instant};

    use ssx_capture::{CaptureBackend, CaptureOptions};
    use ssx_capture_win::WindowsCapture;
    use ssx_types::{ColorSpace, Frame, PixelFormat};

    /// Naive HDR to SDR conversion for eyeballing only: SDR white -> 1.0, hard clip,
    /// sRGB OETF.
    fn preview(frame: &Frame) -> Result<Frame, Box<dyn std::error::Error>> {
        let white_scrgb = frame.sdr_white_nits.unwrap_or(80.0) / 80.0;
        let mut out = Vec::with_capacity(frame.width() as usize * frame.height() as usize * 4);
        for y in 0..frame.height() {
            for px in frame.row(y).chunks_exact(8) {
                let ch = |i: usize| half::f16::from_le_bytes([px[i], px[i + 1]]).to_f32();
                for c in [ch(0), ch(2), ch(4)] {
                    let lin = (c / white_scrgb).clamp(0.0, 1.0);
                    let srgb = if lin <= 0.003_130_8 {
                        lin * 12.92
                    } else {
                        1.055 * lin.powf(1.0 / 2.4) - 0.055
                    };
                    out.push((srgb * 255.0 + 0.5) as u8);
                }
                out.push(255);
            }
        }
        Ok(Frame::from_rgba8(frame.width(), frame.height(), out)?)
    }

    fn save(
        frame: &Frame,
        dir: &std::path::Path,
        stem: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match (frame.format(), frame.color_space()) {
            (PixelFormat::Rgba16F, ColorSpace::ScRgbLinear) => {
                preview(frame)?.save(dir.join(format!("{stem}-hdr-preview.png")))?;
            }
            _ => frame.clone().into_rgba8()?.save(dir.join(format!("{stem}.png")))?,
        }
        Ok(())
    }

    fn sanitize(name: &str) -> String {
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
    }

    pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut out_dir = PathBuf::from("ssx-capture-out");
        let mut include_cursor = false;
        for arg in std::env::args().skip(1) {
            if arg == "--cursor" {
                include_cursor = true;
            } else {
                out_dir = PathBuf::from(arg);
            }
        }
        std::fs::create_dir_all(&out_dir)?;
        let opts = CaptureOptions { include_cursor };

        let backend = WindowsCapture::new()?;
        println!("backend: {} ({:?})", backend.name(), backend.capabilities());

        let monitors = backend.monitors()?;
        for m in &monitors {
            println!(
                "monitor {} {:?}: {:?} scale {} refresh {:?} hdr {:?}",
                m.id, m.name, m.rect, m.scale_factor, m.refresh_hz, m.hdr
            );
        }
        for (i, m) in monitors.iter().enumerate() {
            let t = Instant::now();
            let frame = backend.capture_monitor(&m.id, &opts)?;
            println!("captured monitor {} in {:?}: {frame:?}", m.id, t.elapsed());
            save(&frame, &out_dir, &format!("monitor{i}-{}", sanitize(&m.id)))?;
        }

        let windows = backend.windows()?;
        println!("{} windows", windows.len());
        for (i, w) in windows.iter().enumerate() {
            println!(
                "window [{}] {:?} ({:?}) {:?} minimised={} focused={}",
                w.id, w.title, w.app_name, w.rect, w.minimized, w.focused
            );
            if i >= 3 || w.minimized {
                continue;
            }
            let t = Instant::now();
            match backend.capture_window(&w.id, &opts) {
                Ok(frame) => {
                    println!("captured window {:?} in {:?}: {frame:?}", w.title, t.elapsed());
                    save(&frame, &out_dir, &format!("window{i}-{}", sanitize(&w.title)))?;
                }
                Err(e) => println!("could not capture window {:?}: {e}", w.title),
            }
        }
        println!("wrote PNGs to {}", out_dir.display());
        Ok(())
    }
}
