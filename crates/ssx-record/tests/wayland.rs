//! Wayland recording tests.
//!
//! * **Headless sway** (wlroots, `wlr-screencopy`; sway 1.9 has no ext protocol): an
//!   animated layer-shell surface is recorded for two seconds, decoded, and the moving box is
//!   checked. Also measures the streaming loop against the per-call reconnecting still API.
//! * **Mock compositor** (from `ssx-capture-wayland`'s test harness, reused by path, not
//!   modified): `ext-image-copy-capture-v1` and `wlr-screencopy` frame streams, `wl_shm`
//!   formats, transforms, renegotiation, failures, multi-output stitching.
//!
//! Tests skip with a printed reason when `sway` is missing.
#![cfg(all(target_os = "linux", feature = "ffmpeg"))]
#![allow(clippy::too_many_lines, clippy::cast_possible_wrap)] // small sizes and timestamps

#[path = "../../ssx-capture-wayland/tests/common/mod.rs"]
mod common;

use std::{
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use common::{
    OutputCfg, Sway, first_pattern_mismatch,
    mock::{
        ARGB2101010, BGR888, Behavior, MockCfg, MockOutput, MockServer, XBGR8888, XRGB8888,
        output_image,
    },
    pattern,
};
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, ftruncate, memfd_create},
    time::Timespec,
};
use ssx_capture::{CaptureBackend, CaptureOptions};
use ssx_capture_wayland::{Transform, WaylandCapture};
use ssx_record::{
    encode::{HwPolicy, VideoSettings, ffmpeg::FfmpegProber},
    session::{RecordConfig, RecordingSession},
    source::{
        CaptureTarget, FrameSource, SourceConfig, SourceEvent,
        wlroots::{Protocol, WaylandTarget, WlrootsSource},
    },
    time::{Clock, Fps},
    verify::{RgbImage, inspect},
};
use ssx_types::{Frame, Rect, Size};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

// ---- an animated layer-shell client ---------------------------------------------------------

const BOX: u32 = 80;
const SPEED: f64 = 360.0; // px per second

struct Buf {
    buffer: wl_buffer::WlBuffer,
    file: File,
    busy: bool,
}

struct AState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<ZwlrLayerShellV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    surface: Option<wl_surface::WlSurface>,
    size: Option<(u32, u32)>,
    bufs: Vec<Buf>,
    need_draw: bool,
    t0: Instant,
    frames: u64,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AState {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_output" => {
                    let o = reg.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(4),
                        qh,
                        st.outputs.len(),
                    );
                    st.outputs.push((o, None));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for AState {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = ev {
            st.outputs[*i].1 = Some(name);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AState {
    fn event(
        st: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = ev {
            ls.ack_configure(serial);
            st.size = Some((width, height));
            st.need_draw = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for AState {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        ev: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = ev {
            st.need_draw = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for AState {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = ev
            && let Some(b) = st.bufs.get_mut(*i)
        {
            b.busy = false;
        }
    }
}

delegate_noop!(AState: ignore wl_compositor::WlCompositor);
delegate_noop!(AState: ignore wl_shm::WlShm);
delegate_noop!(AState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(AState: ignore wl_surface::WlSurface);
delegate_noop!(AState: ignore ZwlrLayerShellV1);

fn draw(st: &mut AState, qh: &QueueHandle<AState>) {
    let (Some((w, h)), Some(surface), Some(shm)) = (st.size, st.surface.clone(), st.shm.clone())
    else {
        return;
    };
    if st.bufs.is_empty() {
        for i in 0..3 {
            let len = (w * h * 4) as usize;
            let fd = memfd_create("anim", MemfdFlags::CLOEXEC).expect("memfd");
            ftruncate(&fd, len as u64).expect("ftruncate");
            let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
            let buffer = pool.create_buffer(
                0,
                w as i32,
                h as i32,
                (w * 4) as i32,
                wl_shm::Format::Xrgb8888,
                qh,
                i,
            );
            pool.destroy();
            st.bufs.push(Buf { buffer, file: File::from(fd), busy: false });
        }
    }
    let Some(idx) = st.bufs.iter().position(|b| !b.busy) else { return };
    let x = ((st.t0.elapsed().as_secs_f64() * SPEED) as u32) % (w - BOX);
    let y0 = h / 2 - BOX / 2;
    let mut data = vec![0u8; (w * h * 4) as usize];
    for row in y0..y0 + BOX {
        let start = ((row * w + x) * 4) as usize;
        for px in data[start..start + (BOX * 4) as usize].chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]); // B G R X: red
        }
    }
    st.bufs[idx].file.write_all_at(&data, 0).expect("paint");
    st.bufs[idx].busy = true;
    surface.attach(Some(&st.bufs[idx].buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.frame(qh, ());
    surface.commit();
    st.need_draw = false;
    st.frames += 1;
}

struct Animator {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Animator {
    fn start(socket: &Path, output: &str) -> Animator {
        let stop = Arc::new(AtomicBool::new(false));
        let (socket, output, stop2) = (socket.to_path_buf(), output.to_owned(), Arc::clone(&stop));
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
            let conn = Connection::from_socket(stream).expect("conn");
            let mut queue: EventQueue<AState> = conn.new_event_queue();
            let qh = queue.handle();
            conn.display().get_registry(&qh, ());
            let mut st = AState {
                compositor: None,
                shm: None,
                shell: None,
                outputs: Vec::new(),
                surface: None,
                size: None,
                bufs: Vec::new(),
                need_draw: false,
                t0: Instant::now(),
                frames: 0,
            };
            queue.roundtrip(&mut st).unwrap();
            queue.roundtrip(&mut st).unwrap();
            let surface = st.compositor.clone().unwrap().create_surface(&qh, ());
            let out = st
                .outputs
                .iter()
                .find(|(_, n)| n.as_deref() == Some(output.as_str()))
                .map(|(o, _)| o.clone())
                .expect("output");
            let ls = st.shell.clone().unwrap().get_layer_surface(
                &surface,
                Some(&out),
                zwlr_layer_shell_v1::Layer::Background,
                "ssx-anim".into(),
                &qh,
                (),
            );
            ls.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            ls.set_exclusive_zone(-1);
            ls.set_size(0, 0);
            surface.commit();
            st.surface = Some(surface);
            let mut announced = false;
            while !stop2.load(Ordering::Relaxed) {
                queue.dispatch_pending(&mut st).unwrap();
                if st.need_draw {
                    draw(&mut st, &qh);
                }
                if !announced && st.frames > 0 {
                    announced = true;
                    let _ = tx.send(());
                }
                conn.flush().unwrap();
                let Some(guard) = queue.prepare_read() else { continue };
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let ts = Timespec { tv_sec: 0, tv_nsec: 10_000_000 };
                if poll(&mut fds, Some(&ts)).unwrap_or(0) > 0 {
                    let _ = guard.read();
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("animator painted its first frame");
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

fn sw_config(path: &Path) -> RecordConfig {
    let mut c = RecordConfig::new(path);
    c.video = VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() };
    c
}

fn cpu_seconds() -> f64 {
    // utime + stime of this process in clock ticks (100/s on Linux).
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}

// ---- headless sway ----------------------------------------------------------------------------

#[test]
fn sway_wlr_screencopy_records_the_animation() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 800, 600, 0, 0)], "") else {
        return;
    };
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("SKIP: the ffmpeg binary is needed to dump frames");
        return;
    }
    let _anim = Animator::start(&sway.wayland_socket, "HEADLESS-1");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sway.mp4");
    let src = WlrootsSource::new(SourceConfig { fps: Fps::FPS_30, ..SourceConfig::default() })
        .on_target(WaylandTarget::Path(sway.wayland_socket.clone()));
    let s = RecordingSession::start(sw_config(&path), Box::new(src), vec![], &FfmpegProber)
        .expect("start");
    assert_eq!(s.info().source, "wlroots");
    std::thread::sleep(Duration::from_millis(2300));
    let r = s.stop().unwrap();
    eprintln!("sway recording: {:?}", r.stats);
    assert!((r.duration.as_secs_f64() - 2.3).abs() < 0.3, "{:?}", r.duration);
    assert!(r.stats.dropped_backpressure <= 3, "{:?}", r.stats);

    let rep = inspect(&path).unwrap();
    let v = rep.video.clone().unwrap();
    assert_eq!((v.width, v.height), (800, 600));
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    let frame_len = 800 * 600 * 3;
    let xs: Vec<f64> = out
        .stdout
        .chunks_exact(frame_len)
        .filter_map(|c| box_x(&RgbImage { width: 800, height: 600, data: c.to_vec() }, 300))
        .collect();
    assert!(xs.len() >= 50, "box visible in {} of {} frames", xs.len(), v.frame_count());
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
    let expected = SPEED / 30.0;
    assert!(
        (median - expected).abs() < 4.0,
        "median displacement {median:.1} px/frame, expected {expected:.1}"
    );
    let moving = xs.windows(2).filter(|w| (w[1] - w[0]).abs() > 2.0).count();
    assert!(moving as f64 > xs.len() as f64 * 0.75, "moved in {moving} of {} steps", xs.len());
}

#[test]
fn sway_streaming_loop_versus_reconnecting_still_api() {
    let Some(sway) = Sway::start(&[OutputCfg::new("HEADLESS-1", 1280, 720, 0, 0)], "") else {
        return;
    };
    let _anim = Animator::start(&sway.wayland_socket, "HEADLESS-1");
    let secs = 3.0;

    // (a) the public still-image API in a loop: a new connection, registry snapshot, shm
    // pool and frame per call.
    let cap = WaylandCapture::with_config(sway.config()).unwrap();
    let (cpu0, t0) = (cpu_seconds(), Instant::now());
    let mut n_still = 0u32;
    while t0.elapsed().as_secs_f64() < secs {
        cap.capture_monitor("HEADLESS-1", &CaptureOptions { include_cursor: false }).unwrap();
        n_still += 1;
    }
    let (still_fps, still_cpu) =
        (f64::from(n_still) / t0.elapsed().as_secs_f64(), cpu_seconds() - cpu0);

    // (b) our streaming loop, unpaced (1000 fps grid), plain `copy` (no damage waiting).
    let mut src =
        WlrootsSource::new(SourceConfig { fps: Fps::from_int(1000), ..SourceConfig::default() })
            .on_target(WaylandTarget::Path(sway.wayland_socket.clone()))
            .without_damage();
    src.start(Clock::start()).expect("start");
    let (cpu0, t0) = (cpu_seconds(), Instant::now());
    let mut n_stream = 0u32;
    while t0.elapsed().as_secs_f64() < secs {
        if let SourceEvent::Frame(_) = src.next_frame(Duration::from_millis(200)).unwrap() {
            n_stream += 1;
        }
    }
    let (stream_fps, stream_cpu) =
        (f64::from(n_stream) / t0.elapsed().as_secs_f64(), cpu_seconds() - cpu0);
    src.stop();

    // (c) damage-driven streaming at 30 fps.
    let mut src = WlrootsSource::new(SourceConfig { fps: Fps::FPS_30, ..SourceConfig::default() })
        .on_target(WaylandTarget::Path(sway.wayland_socket.clone()));
    src.start(Clock::start()).expect("start");
    let (cpu0, t0) = (cpu_seconds(), Instant::now());
    let mut n_damage = 0u32;
    while t0.elapsed().as_secs_f64() < secs {
        if let SourceEvent::Frame(_) = src.next_frame(Duration::from_millis(50)).unwrap() {
            n_damage += 1;
        }
    }
    let (damage_fps, damage_cpu) =
        (f64::from(n_damage) / t0.elapsed().as_secs_f64(), cpu_seconds() - cpu0);
    src.stop();

    eprintln!("wlroots 1280x720 headless sway (compositor output refresh: 60 Hz):");
    eprintln!(
        "  still API in a loop (reconnect per frame): {still_fps:5.1} fps, CPU {:.0} ms/frame",
        still_cpu / f64::from(n_still) * 1000.0
    );
    eprintln!(
        "  streaming loop, plain copy, unpaced:       {stream_fps:5.1} fps, CPU {:.0} ms/frame",
        stream_cpu / f64::from(n_stream) * 1000.0
    );
    eprintln!(
        "  streaming loop, damage-driven, 30 fps:     {damage_fps:5.1} fps, CPU {:.0} ms/frame",
        damage_cpu / f64::from(n_damage.max(1)) * 1000.0
    );
    assert!(stream_fps > 30.0, "the streaming loop must sustain 30 fps, got {stream_fps:.1}");
    assert!(
        (damage_fps - 30.0).abs() < 6.0,
        "damage-driven 30 fps stream delivered {damage_fps:.1}"
    );
}

// ---- mock compositor: protocols, formats, failures ---------------------------------------------

fn mock_source(mock: &MockServer, fps: Fps) -> WlrootsSource {
    WlrootsSource::new(SourceConfig { fps, ..SourceConfig::default() })
        .on_target(WaylandTarget::Path(mock.socket.clone()))
}

fn pull(src: &mut WlrootsSource, n: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while frames.len() < n && Instant::now() < deadline {
        if let SourceEvent::Frame(f) = src.next_frame(Duration::from_millis(100)).expect("frame") {
            frames.push(f.frame);
        }
    }
    assert_eq!(frames.len(), n, "did not get {n} frames in time");
    frames
}

#[test]
fn ext_stream_is_pixel_exact_reuses_one_session_and_converts_formats() {
    for fmt in [XRGB8888, XBGR8888, ARGB2101010, BGR888] {
        let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 160, 120)]);
        cfg.formats = vec![fmt];
        let mock = MockServer::start(cfg);
        let mut src = mock_source(&mock, Fps::from_int(200));
        src.start(Clock::start()).unwrap();
        assert_eq!(src.protocol(), Some(Protocol::Ext));
        let frames = pull(&mut src, 8);
        for f in &frames {
            assert_eq!(f.size(), Size::new(160, 120));
            assert_eq!(first_pattern_mismatch(f, 1, (0, 0)), None, "format {fmt:#x}");
        }
        assert_eq!(mock.logged("create_session"), 1, "one session for the whole stream ({fmt:#x})");
        assert!(mock.logged("capture #") >= 8);
        src.stop();
    }
}

#[test]
fn ext_transform_and_cursor_option_are_honoured() {
    let mock = MockServer::start(MockCfg::new(vec![
        MockOutput::new("MOCK-1", 0, 0, 120, 80).transform(Transform::Rot90),
    ]));
    let mut src = WlrootsSource::new(SourceConfig {
        cursor: true,
        fps: Fps::from_int(200),
        ..SourceConfig::default()
    })
    .on_target(WaylandTarget::Path(mock.socket.clone()));
    src.start(Clock::start()).unwrap();
    let f = &pull(&mut src, 2)[1];
    assert_eq!(f.size(), Size::new(80, 120), "upright size after undoing the transform");
    assert_eq!(first_pattern_mismatch(f, 1, (0, 0)), None);
    assert_eq!(mock.logged("paint_cursors=true"), 1);
}

#[test]
fn ext_renegotiation_keeps_the_stream_alive() {
    for behavior in [Behavior::RenegotiateConstraintsFirst, Behavior::RenegotiateFailedFirst] {
        let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 120, 80)]);
        cfg.behavior = behavior;
        cfg.renegotiate = (XBGR8888, (0, 0));
        let mock = MockServer::start(cfg);
        let mut src = mock_source(&mock, Fps::from_int(200));
        src.start(Clock::start()).unwrap();
        for f in pull(&mut src, 5) {
            assert_eq!(first_pattern_mismatch(&f, 1, (0, 0)), None, "{behavior:?}");
        }
        src.stop();
    }
}

#[test]
fn ext_stopped_session_and_dead_compositor_are_errors_not_hangs() {
    let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 120, 80)]);
    cfg.behavior = Behavior::Stop;
    let mock = MockServer::start(cfg);
    let mut src = mock_source(&mock, Fps::from_int(100));
    let err = src.start(Clock::start()).unwrap_err().to_string();
    assert!(err.contains("stopped") || err.contains("failed"), "{err}");

    let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 120, 80)]);
    cfg.behavior = Behavior::Disconnect;
    let mock = MockServer::start(cfg);
    let mut src = mock_source(&mock, Fps::from_int(100));
    let t = Instant::now();
    assert!(src.start(Clock::start()).is_err());
    assert!(t.elapsed() < Duration::from_secs(15));
}

#[test]
fn wlr_stream_with_y_invert_stride_padding_and_a_retry() {
    for (behavior, y_invert, pad) in [
        (Behavior::Normal, false, 0),
        (Behavior::Normal, true, 24),
        (Behavior::WlrFailOnce, false, 8),
    ] {
        let mut cfg = MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 130, 90)]);
        cfg.ext = false;
        cfg.wlr = true;
        cfg.y_invert = y_invert;
        cfg.stride_pad = pad;
        cfg.behavior = behavior;
        let mock = MockServer::start(cfg);
        let mut src = mock_source(&mock, Fps::from_int(200));
        src.start(Clock::start()).unwrap();
        assert_eq!(src.protocol(), Some(Protocol::Wlr));
        for f in pull(&mut src, 6) {
            assert_eq!(
                first_pattern_mismatch(&f, 1, (0, 0)),
                None,
                "{behavior:?} y_invert={y_invert} pad={pad}"
            );
        }
        // No hidden extra captures: at most the failed one plus what we got.
        assert!(mock.logged("wlr capture") <= 9, "{}", mock.logged("wlr capture"));
        src.stop();
    }
}

#[test]
fn two_outputs_are_stitched_and_regions_are_cropped() {
    let mock = MockServer::start(MockCfg::new(vec![
        MockOutput::new("MOCK-1", 0, 0, 160, 120),
        MockOutput::new("MOCK-2", 200, 20, 100, 100),
    ]));
    // Whole desktop: 300 x 120 canvas, a transparent gap between the outputs.
    let mut src = mock_source(&mock, Fps::from_int(200));
    src.start(Clock::start()).unwrap();
    let f = &pull(&mut src, 2)[1];
    assert_eq!(f.size(), Size::new(300, 120));
    let px = |x: u32, y: u32| {
        let p = &f.row(y)[x as usize * 4..x as usize * 4 + 4];
        [p[0], p[1], p[2]]
    };
    let bgr = |seed, x, y| {
        let [r, g, b] = pattern(seed, x, y);
        [b, g, r]
    };
    assert_eq!(px(5, 7), bgr(1, 5, 7));
    assert_eq!(px(200 + 3, 20 + 4), bgr(2, 3, 4));
    assert_eq!(px(180, 50), [0, 0, 0], "the gap stays black");
    src.stop();

    // A region spanning both outputs.
    let mut src = WlrootsSource::new(SourceConfig {
        target: CaptureTarget::Region(Rect::new(150, 30, 100, 50)),
        fps: Fps::from_int(200),
        cursor: false,
    })
    .on_target(WaylandTarget::Path(mock.socket.clone()));
    src.start(Clock::start()).unwrap();
    let f = &pull(&mut src, 2)[1];
    assert_eq!(f.size(), Size::new(100, 50));
    let p = |x: u32, y: u32| {
        let q = &f.row(y)[x as usize * 4..x as usize * 4 + 3];
        [q[0], q[1], q[2]]
    };
    assert_eq!(p(0, 0), bgr(1, 150, 30));
    assert_eq!(p(60, 10), bgr(2, 10, 20));
}

#[test]
fn unknown_monitor_and_picker_are_clear_errors() {
    let mock = MockServer::start(MockCfg::new(vec![MockOutput::new("MOCK-1", 0, 0, 120, 80)]));
    let mut src = WlrootsSource::new(SourceConfig {
        target: CaptureTarget::Monitor("nope".into()),
        ..SourceConfig::default()
    })
    .on_target(WaylandTarget::Path(mock.socket.clone()));
    assert!(matches!(
        src.start(Clock::start()),
        Err(ssx_record::error::SourceError::TargetNotFound(_))
    ));
    let mut src =
        WlrootsSource::new(SourceConfig { target: CaptureTarget::Pick, ..SourceConfig::default() })
            .on_target(WaylandTarget::Path(mock.socket.clone()));
    assert!(matches!(
        src.start(Clock::start()),
        Err(ssx_record::error::SourceError::Unsupported(_))
    ));
    // The mock's output image, for reference in failure messages.
    let _ = output_image(&MockOutput::new("MOCK-1", 0, 0, 2, 2), 0);
}
