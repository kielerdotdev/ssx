//! Live X11 recording under Xvfb: an x11rb client animates a red box, the session records
//! the display for two seconds, and the decoded video must show the box moving at the
//! right speed. Skips (with a printed reason) when Xvfb is not installed.

#![cfg(all(feature = "ffmpeg", unix, not(target_vendor = "apple")))]

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ssx_record::{
    encode::{HwPolicy, VideoSettings, ffmpeg::FfmpegProber},
    session::{RecordConfig, RecordingSession},
    source::{CaptureTarget, FrameSource, SourceConfig, SourceEvent, x11::X11Source},
    time::{Clock, Fps},
    verify::{RgbImage, inspect},
};
use ssx_types::Rect;
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, CreateGCAux, CreateWindowAux, WindowClass},
    rust_connection::RustConnection,
};

struct Xvfb {
    child: Child,
    display: String,
}

impl Xvfb {
    fn start(screen: &str) -> Option<Xvfb> {
        let mut child = match Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                screen,
                "-noreset",
                "-ac",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run Xvfb ({e}); install the `xvfb` package");
                return None;
            }
        };
        let mut line = String::new();
        let out = child.stdout.take()?;
        if BufReader::new(out).read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!("SKIP: Xvfb refused to start");
            return None;
        }
        let display = format!(":{}", line.trim());
        for _ in 0..200 {
            if RustConnection::connect(Some(&display)).is_ok() {
                return Some(Xvfb { child, display });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("SKIP: Xvfb never accepted connections");
        None
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Animates a red box across a black window until dropped.
struct Animator {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

const BOX: u16 = 80;
const STEP_PER_SEC: f64 = 360.0;

impl Animator {
    fn start(display: &str, w: u16, h: u16) -> Animator {
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, display) = (Arc::clone(&stop), display.to_owned());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (conn, screen) = RustConnection::connect(Some(&display)).expect("connect");
            let root = conn.setup().roots[screen].root;
            let win = conn.generate_id().unwrap();
            conn.create_window(
                24,
                win,
                root,
                0,
                0,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().background_pixel(0x000000).override_redirect(1),
            )
            .unwrap();
            conn.map_window(win).unwrap();
            let red = conn.generate_id().unwrap();
            conn.create_gc(red, win, &CreateGCAux::new().foreground(0xff0000)).unwrap();
            conn.flush().unwrap();
            let _ = ready_tx.send(());
            let t0 = Instant::now();
            while !stop2.load(Ordering::Relaxed) {
                let x = ((t0.elapsed().as_secs_f64() * STEP_PER_SEC) as i32) % i32::from(w - BOX);
                conn.clear_area(false, win, 0, 0, 0, 0).unwrap();
                conn.poly_fill_rectangle(
                    win,
                    red,
                    &[x11rb::protocol::xproto::Rectangle {
                        x: x as i16,
                        y: (h / 2 - BOX / 2) as i16,
                        width: BOX,
                        height: BOX,
                    }],
                )
                .unwrap();
                conn.flush().unwrap();
                std::thread::sleep(Duration::from_millis(8));
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).expect("animator ready");
        Animator { stop, thread: Some(thread) }
    }
}

impl Drop for Animator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Centroid x of the strongly red pixels on the row through the box centre.
fn box_x(img: &RgbImage, row: u32) -> Option<f64> {
    let (mut sum, mut n) = (0u64, 0u64);
    for x in 0..img.width {
        let [r, g, b] = img.pixel(x, row);
        if r > 180 && g < 90 && b < 90 {
            sum += u64::from(x);
            n += 1;
        }
    }
    (n > 20).then(|| sum as f64 / n as f64)
}

fn sw_config(path: &std::path::Path) -> RecordConfig {
    let mut c = RecordConfig::new(path);
    c.video = VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() };
    c
}

/// Decodes every frame of `path` and returns the box centroid of the frames that show it,
/// probing image row `row`.
fn box_positions(path: &std::path::Path, row: u32) -> (Vec<f64>, ssx_record::verify::MediaReport) {
    // The video's only motion is the box; decode again with the row probe on every frame.
    let rep = inspect(path).unwrap();
    let v = rep.video.clone().unwrap();
    // `inspect` keeps the first and last picture only; re-decode all frames' box position
    // through ffmpeg rawvideo for the comparison.
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("ffmpeg binary for the frame dump");
    let (w, hh) = (v.width as usize, v.height as usize);
    let frame_len = w * hh * 3;
    let mut xs = Vec::new();
    for chunk in out.stdout.chunks_exact(frame_len) {
        let img = RgbImage { width: v.width, height: v.height, data: chunk.to_vec() };
        if let Some(x) = box_x(&img, row) {
            xs.push(x);
        }
    }
    (xs, rep)
}

#[test]
fn x11_recording_shows_the_moving_box() {
    let Some(xvfb) = Xvfb::start("800x600x24") else { return };
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("SKIP: the ffmpeg binary is needed to dump frames for the motion check");
        return;
    }
    let _anim = Animator::start(&xvfb.display, 800, 600);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x11.mp4");
    let src = Box::new(
        X11Source::new(SourceConfig { fps: Fps::FPS_30, ..SourceConfig::default() })
            .on_display(xvfb.display.clone()),
    );
    let s = RecordingSession::start(sw_config(&path), src, vec![], &FfmpegProber).expect("start");
    assert_eq!(s.info().source, "x11-shm");
    std::thread::sleep(Duration::from_millis(2000));
    let r = s.stop().unwrap();
    let secs = r.duration.as_secs_f64();
    assert!((secs - 2.0).abs() < 0.25, "{secs}");
    assert_eq!(r.stats.slots, r.stats.encoded + r.stats.dropped_backpressure);
    assert!(
        r.stats.dropped_backpressure <= 3,
        "X11 capture at 30 fps must not drop: {:?}",
        r.stats
    );

    let (xs, rep) = box_positions(&path, 300);
    let v = rep.video.unwrap();
    assert_eq!((v.width, v.height), (800, 600));
    assert!(xs.len() >= 50, "box visible in {} of {} frames", xs.len(), v.frame_count());
    // Frame-to-frame displacement (handling the wrap-around of the animation).
    let span = f64::from(800 - BOX);
    let mut deltas: Vec<f64> = xs
        .windows(2)
        .map(|w| {
            let d = w[1] - w[0];
            if d < -span / 2.0 { d + span } else { d }
        })
        .collect();
    deltas.sort_by(f64::total_cmp);
    let median = deltas[deltas.len() / 2];
    let expected = STEP_PER_SEC / 30.0; // 12 px per frame
    assert!(
        (median - expected).abs() < 3.5,
        "median displacement {median:.1} px/frame, expected {expected:.1} (deltas {:?})",
        &deltas[deltas.len() / 4..deltas.len() / 4 + 5]
    );
    let moving = xs.windows(2).filter(|w| (w[1] - w[0]).abs() > 2.0).count();
    assert!(
        moving as f64 > xs.len() as f64 * 0.8,
        "the box moved in only {moving} of {} steps",
        xs.len()
    );
}

#[test]
fn x11_region_recording_crops() {
    let Some(xvfb) = Xvfb::start("800x600x24") else { return };
    let _anim = Animator::start(&xvfb.display, 800, 600);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("region.webm");
    let src = Box::new(
        X11Source::new(SourceConfig {
            target: CaptureTarget::Region(Rect::new(100, 200, 400, 200)),
            fps: Fps::FPS_30,
            cursor: false,
        })
        .on_display(xvfb.display.clone()),
    );
    let mut cfg = sw_config(&path);
    cfg.container = ssx_record::encode::Container::WebM;
    let s = RecordingSession::start(cfg, src, vec![], &FfmpegProber).expect("start");
    std::thread::sleep(Duration::from_millis(2300));
    let r = s.stop().unwrap();
    assert_eq!((r.size.width, r.size.height), (400, 200));
    // The box (80 px tall, centred on screen row 300) crosses the region (rows 200-400,
    // columns 100-500) every 2 s: row 100 of the region is inside the box.
    let (xs, rep) = box_positions(&path, 100);
    let v = rep.video.unwrap();
    assert_eq!((v.width, v.height), (400, 200));
    assert!(xs.len() >= 5, "the box must cross the recorded region: seen in {} frames", xs.len());
    // Region coordinates: the screen column of the box minus the region's left edge.
    assert!(xs.iter().all(|x| (0.0..=400.0).contains(x)), "{xs:?}");
}

#[test]
fn x11_source_capture_rate_at_1080p() {
    let Some(xvfb) = Xvfb::start("1920x1080x24") else { return };
    let _anim = Animator::start(&xvfb.display, 1920, 1080);
    // Unpaced ceiling: ask for 1000 fps and count what the loop can deliver in 2 s.
    let mut src =
        X11Source::new(SourceConfig { fps: Fps::from_int(1000), ..SourceConfig::default() })
            .on_display(xvfb.display.clone());
    let clock = Clock::start();
    src.start(clock).expect("start");
    let t0 = Instant::now();
    let mut n = 0u32;
    while t0.elapsed() < Duration::from_secs(2) {
        if let SourceEvent::Frame(_) = src.next_frame(Duration::from_millis(100)).unwrap() {
            n += 1;
        }
    }
    let fps = f64::from(n) / t0.elapsed().as_secs_f64();
    eprintln!("X11 MIT-SHM GetImage loop, 1920x1080 Xvfb, unpaced: {fps:.0} frames/s");
    assert!(fps > 30.0, "the X11 loop must sustain at least 30 fps at 1080p, got {fps:.1}");
    src.stop();
}
