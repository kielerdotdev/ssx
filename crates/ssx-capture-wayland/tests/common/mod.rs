//! Test support: a private headless sway, an i3-ipc client, and a painter client that puts
//! known pixels on outputs (layer-shell background surfaces) or in windows (xdg-toplevel).
//!
//! Everything here is independent of the crate under test on purpose, so a bug in the
//! crate cannot hide behind a matching bug in the harness.
#![allow(dead_code)] // each integration-test binary uses a different subset of the helpers
#![allow(clippy::cast_possible_wrap)] // fixtures use small, known buffer sizes

pub mod mock;

use std::{
    fmt::Write as _,
    fs::File,
    io::{Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, ftruncate, memfd_create},
    time::Timespec,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

/// `true` if `prog` is an executable in `PATH`.
pub fn have(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// Returns `None` (after printing why) when the tests cannot run here.
pub fn skip_unless_sway() -> bool {
    if !have("sway") {
        eprintln!("SKIP: `sway` is not installed (CI installs it with `apt install sway grim`)");
        return true;
    }
    false
}

/// A headless sway instance in a private `XDG_RUNTIME_DIR`. Killed on drop.
pub struct Sway {
    child: Child,
    pub dir: tempfile::TempDir,
    pub wayland_socket: PathBuf,
    pub ipc_socket: PathBuf,
}

/// Expected state of one output in the sway config.
pub struct OutputCfg {
    pub name: &'static str,
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
    pub scale: &'static str,
    pub transform: &'static str,
}

impl OutputCfg {
    pub fn new(name: &'static str, w: u32, h: u32, x: i32, y: i32) -> Self {
        Self { name, w, h, x, y, scale: "1", transform: "normal" }
    }
    pub fn scale(mut self, s: &'static str) -> Self {
        self.scale = s;
        self
    }
    pub fn transform(mut self, t: &'static str) -> Self {
        self.transform = t;
        self
    }
}

impl Sway {
    /// Starts sway with `outputs.len()` headless outputs configured as given. `extra` is
    /// appended verbatim to the config file. Returns `None` if sway is missing or (outside
    /// CI) cannot start, after printing the reason.
    pub fn start(outputs: &[OutputCfg], extra: &str) -> Option<Sway> {
        if skip_unless_sway() {
            return None;
        }
        match Self::try_start(outputs, extra) {
            Ok(s) => Some(s),
            Err(e) if std::env::var_os("CI").is_some() => panic!("sway failed to start in CI: {e}"),
            Err(e) => {
                eprintln!("SKIP: sway is installed but could not start headless here: {e}");
                None
            }
        }
    }

    fn try_start(outputs: &[OutputCfg], extra: &str) -> Result<Sway, String> {
        let dir =
            tempfile::Builder::new().prefix("ssx-sway").tempdir().map_err(|e| e.to_string())?;
        let mut cfg = String::from(
            "xwayland disable\ndefault_border none\ndefault_floating_border none\nfocus_follows_mouse no\n",
        );
        for o in outputs {
            let _ = writeln!(
                cfg,
                "output {} resolution {}x{} position {} {} scale {} transform {}",
                o.name, o.w, o.h, o.x, o.y, o.scale, o.transform
            );
        }
        cfg += extra;
        let cfg_path = dir.path().join("sway.conf");
        std::fs::write(&cfg_path, cfg).map_err(|e| e.to_string())?;
        let log = File::create(dir.path().join("sway.log")).map_err(|e| e.to_string())?;
        let child = Command::new("sway")
            .arg("-c")
            .arg(&cfg_path)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", outputs.len().to_string())
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env("XDG_RUNTIME_DIR", dir.path())
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("SWAYSOCK")
            .env_remove("I3SOCK")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("spawn sway: {e}"))?;
        let mut sway =
            Sway { child, wayland_socket: PathBuf::new(), ipc_socket: PathBuf::new(), dir };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = sway.child.try_wait() {
                return Err(format!("sway exited early ({status}): {}", sway.log_tail()));
            }
            if Instant::now() > deadline {
                return Err(format!("sway did not come up in time: {}", sway.log_tail()));
            }
            let mut wl = None;
            let mut ipc = None;
            if let Ok(rd) = std::fs::read_dir(sway.dir.path()) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("wayland-") && !n.to_ascii_lowercase().ends_with(".lock") {
                        wl = Some(e.path());
                    } else if n.starts_with("sway-ipc.")
                        && n.to_ascii_lowercase().ends_with(".sock")
                    {
                        ipc = Some(e.path());
                    }
                }
            }
            if let (Some(wl), Some(ipc)) = (wl, ipc) {
                sway.wayland_socket = wl;
                sway.ipc_socket = ipc;
                if ipc_request(&sway.ipc_socket, 3, b"").is_ok() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        sway.wait_outputs(outputs)?;
        Ok(sway)
    }

    fn log_tail(&self) -> String {
        let s = std::fs::read_to_string(self.dir.path().join("sway.log")).unwrap_or_default();
        s.lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Waits until `GET_OUTPUTS` reports the configured geometry.
    fn wait_outputs(&self, want: &[OutputCfg]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v: serde_json::Value =
                serde_json::from_slice(&ipc_request(&self.ipc_socket, 3, b"")?)
                    .map_err(|e| e.to_string())?;
            let ok = want.iter().all(|w| {
                v.as_array().is_some_and(|a| {
                    a.iter().any(|o| {
                        o["name"] == w.name
                            && o["current_mode"]["width"] == w.w
                            && o["current_mode"]["height"] == w.h
                            && o["rect"]["x"] == w.x
                            && o["rect"]["y"] == w.y
                    })
                })
            });
            if ok {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!("outputs never reached the configured state: {v}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Runs a sway command (`RUN_COMMAND`), returning its JSON result.
    pub fn command(&self, cmd: &str) -> serde_json::Value {
        let r = ipc_request(&self.ipc_socket, 0, cmd.as_bytes()).expect("sway command");
        serde_json::from_slice(&r).expect("sway command reply")
    }

    pub fn tree(&self) -> serde_json::Value {
        serde_json::from_slice(&ipc_request(&self.ipc_socket, 4, b"").expect("get_tree"))
            .expect("tree json")
    }

    pub fn outputs_json(&self) -> serde_json::Value {
        serde_json::from_slice(&ipc_request(&self.ipc_socket, 3, b"").expect("get_outputs"))
            .expect("outputs json")
    }

    /// Crate config pointing at this instance (no reliance on the process environment).
    pub fn config(&self) -> ssx_capture_wayland::Config {
        ssx_capture_wayland::Config {
            target: ssx_capture_wayland::Target::Path(self.wayland_socket.clone()),
            ipc: ssx_capture_wayland::Ipc::Sway(self.ipc_socket.clone()),
            ..ssx_capture_wayland::Config::default()
        }
    }

    /// Runs `grim` against this instance and returns the decoded PNG (independent
    /// reference implementation).
    pub fn grim(&self, args: &[&str]) -> Option<ssx_types::Frame> {
        if !have("grim") {
            eprintln!("note: `grim` not installed, skipping cross-check");
            return None;
        }
        let out = self
            .dir
            .path()
            .join(format!("grim-{}.png", args.join("_").replace(['/', ' ', ','], "-")));
        let st = Command::new("grim")
            .args(args)
            .arg(&out)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("WAYLAND_DISPLAY", &self.wayland_socket)
            .status()
            .expect("run grim");
        assert!(st.success(), "grim {args:?} failed");
        let bytes = std::fs::read(&out).expect("grim output");
        Some(ssx_types::Frame::decode(&bytes).expect("decode grim png"))
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One i3-ipc request/reply.
pub fn ipc_request(sock: &Path, ty: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let mut msg = b"i3-ipc".to_vec();
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&ty.to_ne_bytes());
    msg.extend_from_slice(payload);
    s.write_all(&msg).map_err(|e| e.to_string())?;
    let mut h = [0u8; 14];
    s.read_exact(&mut h).map_err(|e| e.to_string())?;
    let len = u32::from_ne_bytes([h[6], h[7], h[8], h[9]]) as usize;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

/// The colour the painter puts at buffer pixel `(x, y)`: unique per pixel for buffers up
/// to 1024x1024 so any shift, flip or rotation shows up in a pixel-exact comparison.
pub fn pattern(seed: u8, x: u32, y: u32) -> [u8; 3] {
    [
        (x & 0xff) as u8,
        (y & 0xff) as u8,
        seed.wrapping_mul(16).wrapping_add(((x >> 8) | ((y >> 8) << 2)) as u8),
    ]
}

/// Compares a BGRA frame against the painter pattern; returns the first mismatch.
pub fn first_pattern_mismatch(
    frame: &ssx_types::Frame,
    seed: u8,
    // Offset of the frame's top-left inside the painted buffer.
    offset: (u32, u32),
) -> Option<String> {
    assert_eq!(frame.format(), ssx_types::PixelFormat::Bgra8);
    for y in 0..frame.height() {
        let row = frame.row(y);
        for x in 0..frame.width() {
            let [r, g, b] = pattern(seed, x + offset.0, y + offset.1);
            let px = &row[x as usize * 4..x as usize * 4 + 4];
            if px != [b, g, r, 255] {
                return Some(format!(
                    "pixel ({x},{y}) is {px:?}, expected {:?} (buffer px {},{})",
                    [b, g, r, 255],
                    x + offset.0,
                    y + offset.1
                ));
            }
        }
    }
    None
}

/// Compares two frames pixel by pixel after normalising both to RGBA8 (grim output is
/// RGB PNG, ours BGRA). Returns the first mismatch.
pub fn first_frame_mismatch(a: &ssx_types::Frame, b: &ssx_types::Frame) -> Option<String> {
    if a.size() != b.size() {
        return Some(format!("size {:?} vs {:?}", a.size(), b.size()));
    }
    let a = a.clone().into_rgba8().expect("a is sdr8");
    let b = b.clone().into_rgba8().expect("b is sdr8");
    for y in 0..a.height() {
        let (ra, rb) = (a.row(y), b.row(y));
        for x in 0..a.width() as usize {
            // Alpha is ignored: grim's PNG is opaque RGB.
            if ra[x * 4..x * 4 + 3] != rb[x * 4..x * 4 + 3] {
                return Some(format!(
                    "pixel ({x},{y}): {:?} vs {:?}",
                    &ra[x * 4..x * 4 + 4],
                    &rb[x * 4..x * 4 + 4]
                ));
            }
        }
    }
    None
}

// --------------------------------------------------------------------------------------
// Painter client
// --------------------------------------------------------------------------------------

/// What to paint.
#[derive(Clone, Debug)]
pub enum Job {
    /// A background layer-shell surface covering `output`, painted with [`pattern`] at
    /// `scale`x buffer scale (so it is 1:1 on an output configured with that scale).
    Background { output: &'static str, scale: i32, seed: u8 },
    /// An xdg-toplevel window, painted with [`pattern`].
    Window { title: &'static str, app_id: &'static str, w: u32, h: u32, seed: u8 },
}

enum Role {
    Layer(ZwlrLayerSurfaceV1),
    Toplevel(xdg_toplevel::XdgToplevel, xdg_surface::XdgSurface),
}

struct Surf {
    job: Job,
    surface: wl_surface::WlSurface,
    role: Option<Role>,
    presented: bool,
    buffers: Vec<wl_buffer::WlBuffer>,
    size: (u32, u32),
}

struct PState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    surfs: Vec<Surf>,
}

/// A running painter thread. Dropping it closes the surfaces.
pub struct Painter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Painter {
    /// Starts painting and blocks until every surface has been presented by the compositor.
    pub fn spawn(socket: &Path, jobs: Vec<Job>) -> Painter {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let socket = socket.to_path_buf();
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = paint_main(&socket, jobs, &stop2, &tx) {
                let _ = tx.send(Err(e));
            }
        });
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("painter failed: {e}"),
            Err(e) => panic!("painter never presented its surfaces: {e}"),
        }
        Painter { stop, thread: Some(thread) }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn paint_main(
    socket: &Path,
    jobs: Vec<Job>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let mut queue: EventQueue<PState> = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = PState {
        compositor: None,
        shm: None,
        layer_shell: None,
        wm_base: None,
        outputs: Vec::new(),
        surfs: Vec::new(),
    };
    queue.roundtrip(&mut st).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut st).map_err(|e| e.to_string())?; // output names

    let compositor = st.compositor.clone().ok_or("no wl_compositor")?;
    for (i, job) in jobs.into_iter().enumerate() {
        let surface = compositor.create_surface(&qh, ());
        let role = match &job {
            Job::Background { output, .. } => {
                let shell = st.layer_shell.clone().ok_or("no wlr-layer-shell")?;
                let out = st
                    .outputs
                    .iter()
                    .find(|(_, n)| n.as_deref() == Some(*output))
                    .map(|(o, _)| o.clone())
                    .ok_or_else(|| format!("no output {output}"))?;
                let ls = shell.get_layer_surface(
                    &surface,
                    Some(&out),
                    zwlr_layer_shell_v1::Layer::Background,
                    "ssx-test".into(),
                    &qh,
                    i,
                );
                ls.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                ls.set_exclusive_zone(-1);
                ls.set_size(0, 0);
                Role::Layer(ls)
            }
            Job::Window { title, app_id, .. } => {
                let wm = st.wm_base.clone().ok_or("no xdg_wm_base")?;
                let xs = wm.get_xdg_surface(&surface, &qh, i);
                let tl = xs.get_toplevel(&qh, i);
                tl.set_title((*title).into());
                tl.set_app_id((*app_id).into());
                Role::Toplevel(tl, xs)
            }
        };
        surface.commit();
        st.surfs.push(Surf {
            job,
            surface,
            role: Some(role),
            presented: false,
            buffers: Vec::new(),
            size: (0, 0),
        });
    }

    let mut announced = false;
    while !stop.load(Ordering::SeqCst) {
        queue.dispatch_pending(&mut st).map_err(|e| e.to_string())?;
        if !announced && st.surfs.iter().all(|s| s.presented) {
            announced = true;
            let _ = ready.send(Ok(()));
        }
        conn.flush().map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else { continue };
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let ts = Timespec { tv_sec: 0, tv_nsec: 50_000_000 };
        let n = poll(&mut fds, Some(&ts)).unwrap_or(0);
        if n > 0 {
            let _ = guard.read();
        }
    }
    Ok(())
}

fn draw(
    st: &mut PState,
    qh: &QueueHandle<PState>,
    idx: usize,
    w: u32,
    h: u32,
    scale: i32,
    seed: u8,
) {
    let Some(shm) = st.shm.clone() else { return };
    let (bw, bh) = (w * scale as u32, h * scale as u32);
    let mut data = Vec::with_capacity((bw * bh * 4) as usize);
    for y in 0..bh {
        for x in 0..bw {
            let [r, g, b] = pattern(seed, x, y);
            data.extend_from_slice(&[b, g, r, 0xff]);
        }
    }
    let fd = memfd_create("ssx-test-paint", MemfdFlags::CLOEXEC).expect("memfd");
    ftruncate(&fd, data.len() as u64).expect("ftruncate");
    let file = File::from(fd);
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(&data, 0).expect("write pixels");
    }
    let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
    let buf = pool.create_buffer(
        0,
        bw as i32,
        bh as i32,
        (bw * 4) as i32,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    pool.destroy();
    let s = &mut st.surfs[idx];
    s.surface.set_buffer_scale(scale);
    s.surface.attach(Some(&buf), 0, 0);
    s.surface.damage_buffer(0, 0, bw as i32, bh as i32);
    s.surface.frame(qh, idx);
    s.surface.commit();
    s.buffers.push(buf);
    s.size = (w, h);
    s.presented = false;
}

impl Dispatch<wl_registry::WlRegistry, ()> for PState {
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
                "zwlr_layer_shell_v1" => {
                    st.layer_shell = Some(reg.bind(name, version.min(4), qh, ()));
                }
                "xdg_wm_base" => st.wm_base = Some(reg.bind(name, version.min(5), qh, ())),
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

impl Dispatch<wl_output::WlOutput, usize> for PState {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = ev {
            st.outputs[*idx].1 = Some(name);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for PState {
    fn event(
        st: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = ev {
            ls.ack_configure(serial);
            if let Job::Background { scale, seed, .. } = st.surfs[*idx].job.clone()
                && st.surfs[*idx].size != (width, height)
            {
                draw(st, qh, *idx, width, height, scale, seed);
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for PState {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        ev: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for PState {
    fn event(
        st: &mut Self,
        xs: &xdg_surface::XdgSurface,
        ev: xdg_surface::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            if let Job::Window { w, h, seed, .. } = st.surfs[*idx].job.clone() {
                // The toplevel's last configure size is stored in `size` by the toplevel
                // handler; fall back to the requested size.
                let (cw, ch) = st.surfs[*idx].size;
                let (cw, ch) = if cw == 0 || ch == 0 { (w, h) } else { (cw, ch) };
                draw(st, qh, *idx, cw, ch, 1, seed);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for PState {
    fn event(
        st: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        ev: xdg_toplevel::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = ev
            && width > 0
            && height > 0
        {
            st.surfs[*idx].size = (width as u32, height as u32);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, usize> for PState {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        ev: wl_callback::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = ev {
            st.surfs[*idx].presented = true;
        }
    }
}

delegate_noop!(PState: ignore wl_compositor::WlCompositor);
delegate_noop!(PState: ignore wl_shm::WlShm);
delegate_noop!(PState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(PState: ignore wl_buffer::WlBuffer);
delegate_noop!(PState: ignore wl_surface::WlSurface);
delegate_noop!(PState: ignore ZwlrLayerShellV1);
