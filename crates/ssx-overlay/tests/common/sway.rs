//! A private headless sway, an injector of virtual pointer/keyboard input, and `grim` capture.
//!
//! Adapted from the `ssx-capture-wayland` test harness (kept independent of the crate under
//! test on purpose). Input is injected through the `zwlr_virtual_pointer_v1` and
//! `zwp_virtual_keyboard_v1` protocols by a client thread that owns the virtual devices for
//! the whole test: a headless sway has no input devices, so the seat only grows pointer and
//! keyboard capabilities while such a client is alive.
#![cfg(target_os = "linux")]

use std::{
    fmt::Write as _,
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use ssx_types::Frame;
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// One headless output.
pub struct OutputCfg {
    pub name: &'static str,
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
    pub scale: &'static str,
}

impl OutputCfg {
    pub fn new(name: &'static str, w: u32, h: u32, x: i32, y: i32) -> Self {
        Self { name, w, h, x, y, scale: "1" }
    }
    pub fn scale(mut self, s: &'static str) -> Self {
        self.scale = s;
        self
    }
}

/// A headless sway instance in a private `XDG_RUNTIME_DIR`. Killed on drop.
pub struct Sway {
    child: Child,
    pub dir: tempfile::TempDir,
    pub wayland_socket: PathBuf,
    pub ipc_socket: PathBuf,
    input: Option<Injector>,
}

fn ipc_request(sock: &Path, ty: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut msg = b"i3-ipc".to_vec();
    msg.extend((payload.len() as u32).to_ne_bytes());
    msg.extend(ty.to_ne_bytes());
    msg.extend(payload);
    s.write_all(&msg).map_err(|e| e.to_string())?;
    let mut hdr = [0u8; 14];
    s.read_exact(&mut hdr).map_err(|e| e.to_string())?;
    let len = u32::from_ne_bytes(hdr[6..10].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

impl Sway {
    /// Starts sway with the given outputs. `None` (after printing why) if sway is missing or
    /// cannot start headless here; panics instead when `CI` is set.
    pub fn start(outputs: &[OutputCfg]) -> Option<Sway> {
        if !super::have("sway") {
            eprintln!(
                "SKIP: `sway` is not installed (CI installs it with `apt install sway grim`)"
            );
            return None;
        }
        match Self::try_start(outputs) {
            Ok(s) => Some(s),
            Err(e) if std::env::var_os("CI").is_some() => panic!("sway failed to start in CI: {e}"),
            Err(e) => {
                eprintln!("SKIP: sway is installed but could not start headless here: {e}");
                None
            }
        }
    }

    fn try_start(outputs: &[OutputCfg]) -> Result<Sway, String> {
        let dir =
            tempfile::Builder::new().prefix("ssx-ov-sway").tempdir().map_err(|e| e.to_string())?;
        let mut cfg = String::from(
            "xwayland disable\ndefault_border none\ndefault_floating_border none\nfocus_follows_mouse no\n",
        );
        for o in outputs {
            let _ = writeln!(
                cfg,
                "output {} resolution {}x{} position {} {} scale {}",
                o.name, o.w, o.h, o.x, o.y, o.scale
            );
        }
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
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("spawn sway: {e}"))?;
        let mut sway = Sway {
            child,
            wayland_socket: PathBuf::new(),
            ipc_socket: PathBuf::new(),
            dir,
            input: None,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = sway.child.try_wait() {
                return Err(format!("sway exited early ({status}): {}", sway.log_tail()));
            }
            if Instant::now() > deadline {
                return Err(format!("sway did not come up in time: {}", sway.log_tail()));
            }
            let (mut wl, mut ipc) = (None, None);
            if let Ok(rd) = std::fs::read_dir(sway.dir.path()) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let ext = e.path().extension().map(std::ffi::OsStr::to_ascii_lowercase);
                    if n.starts_with("wayland-") && ext.as_deref() != Some("lock".as_ref()) {
                        wl = Some(e.path());
                    } else if n.starts_with("sway-ipc.") && ext.as_deref() == Some("sock".as_ref())
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
        // Outputs at their configured geometry.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v: serde_json::Value =
                serde_json::from_slice(&ipc_request(&sway.ipc_socket, 3, b"")?)
                    .map_err(|e| e.to_string())?;
            let ok = outputs.iter().all(|w| {
                v.as_array().is_some_and(|a| {
                    a.iter().any(|o| {
                        o["name"] == w.name
                            && o["current_mode"]["width"] == w.w
                            && o["current_mode"]["height"] == w.h
                    })
                })
            });
            if ok {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!("outputs never reached the configured state: {v}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        sway.input = Some(Injector::start(&sway.wayland_socket)?);
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

    /// `WAYLAND_DISPLAY` value (socket file name) for clients.
    pub fn wayland_display(&self) -> String {
        self.wayland_socket.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// Wrapper script that points the helper at this sway (and away from X11).
    pub fn helper_wrapper(&self, dir: &Path) -> PathBuf {
        super::wrapper(
            dir,
            &[
                ("WAYLAND_DISPLAY", &self.wayland_display()),
                ("XDG_RUNTIME_DIR", self.dir.path().to_str().unwrap()),
            ],
            &["DISPLAY"],
            &super::helper_bin(),
        )
    }

    /// Input injection.
    pub fn input(&self) -> &Injector {
        self.input.as_ref().expect("injector started")
    }

    /// Runs `grim` on one output and returns the RGBA capture; `None` if grim is missing.
    pub fn grim(&self, output: &str) -> Option<Frame> {
        if !super::have("grim") {
            return None;
        }
        let out = Command::new("grim")
            .env("WAYLAND_DISPLAY", &self.wayland_socket)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .args(["-o", output, "-t", "png", "-"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Frame::decode(&out.stdout).ok()?.into_rgba8().ok()
    }

    /// Runs a sway command over IPC.
    pub fn command(&self, cmd: &str) -> String {
        String::from_utf8_lossy(
            &ipc_request(&self.ipc_socket, 0, cmd.as_bytes()).unwrap_or_default(),
        )
        .into_owned()
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- virtual input

enum Cmd {
    Motion { x: f64, y: f64, extent: (u32, u32) },
    Button { code: u32, pressed: bool },
    Axis { value: f64 },
    Key { code: u32, pressed: bool },
    Sync(mpsc::Sender<()>),
    Stop,
}

/// Owns a virtual pointer and keyboard on the sway seat, driven through a channel.
pub struct Injector {
    tx: mpsc::Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    extent: std::sync::Mutex<(u32, u32)>,
}

struct St;
impl Dispatch<WlRegistry, GlobalListContents> for St {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(St: ignore WlSeat);
delegate_noop!(St: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(St: ignore ZwlrVirtualPointerV1);
delegate_noop!(St: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(St: ignore ZwpVirtualKeyboardV1);

const KEYMAP: &str = "xkb_keymap {\n\txkb_keycodes { include \"evdev+aliases(qwerty)\" };\n\txkb_types { include \"complete\" };\n\txkb_compat { include \"complete\" };\n\txkb_symbols { include \"pc+us+inet(evdev)\" };\n};\n\0";

impl Injector {
    fn start(socket: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
        let conn = Connection::from_socket(stream).map_err(|e| format!("wayland: {e}"))?;
        let (globals, mut queue) = registry_queue_init::<St>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|e| format!("wl_seat: {e}"))?;
        let pm: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).map_err(|e| format!("virtual pointer manager: {e}"))?;
        let km: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|e| format!("virtual keyboard manager: {e}"))?;
        let pointer = pm.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = km.create_virtual_keyboard(&seat, &qh, ());
        // Keymap: any valid xkb keymap; the overlay reads raw evdev codes.
        let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| e.to_string())?;
        rustix::fs::ftruncate(&fd, KEYMAP.len() as u64).map_err(|e| e.to_string())?;
        let mut f = File::from(fd);
        f.write_all(KEYMAP.as_bytes()).map_err(|e| e.to_string())?;
        {
            use std::os::fd::AsFd;
            keyboard.keymap(1, f.as_fd(), KEYMAP.len() as u32);
        }
        let mut st = St;
        queue.roundtrip(&mut st).map_err(|e| e.to_string())?;

        let (tx, rx) = mpsc::channel::<Cmd>();
        let t0 = Instant::now();
        let thread = std::thread::spawn(move || {
            let mut st = St;
            let _keep = (&seat, &pm, &km, &f);
            while let Ok(cmd) = rx.recv() {
                let t = t0.elapsed().as_millis() as u32;
                match cmd {
                    Cmd::Motion { x, y, extent } => {
                        pointer.motion_absolute(t, x as u32, y as u32, extent.0, extent.1);
                        pointer.frame();
                    }
                    Cmd::Button { code, pressed } => {
                        pointer.button(
                            t,
                            code,
                            if pressed {
                                wayland_client::protocol::wl_pointer::ButtonState::Pressed
                            } else {
                                wayland_client::protocol::wl_pointer::ButtonState::Released
                            },
                        );
                        pointer.frame();
                    }
                    Cmd::Axis { value } => {
                        pointer.axis(
                            t,
                            wayland_client::protocol::wl_pointer::Axis::VerticalScroll,
                            value,
                        );
                        pointer.frame();
                    }
                    Cmd::Key { code, pressed } => {
                        keyboard.key(t, code, u32::from(pressed));
                    }
                    Cmd::Sync(done) => {
                        let _ = queue.roundtrip(&mut st);
                        let _ = done.send(());
                        continue;
                    }
                    Cmd::Stop => break,
                }
                let _ = conn.flush();
            }
            pointer.destroy();
            keyboard.destroy();
            let _ = queue.roundtrip(&mut st);
        });
        Ok(Self { tx, thread: Some(thread), extent: std::sync::Mutex::new((1, 1)) })
    }
}

impl Injector {
    /// Sets the layout size (logical px) that absolute pointer positions are relative to.
    /// The layout's top-left must be (0,0).
    pub fn set_layout(&self, w: u32, h: u32) {
        *self.extent.lock().unwrap() = (w, h);
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Sync(tx));
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }

    /// Moves the pointer to a global logical position.
    pub fn move_to(&self, x: f64, y: f64) {
        let extent = *self.extent.lock().unwrap();
        self.send(Cmd::Motion { x, y, extent });
        self.sync();
        std::thread::sleep(Duration::from_millis(60));
    }

    pub fn button(&self, code: u32, pressed: bool) {
        self.send(Cmd::Button { code, pressed });
        self.sync();
        std::thread::sleep(Duration::from_millis(60));
    }

    pub fn left_down(&self) {
        self.button(0x110, true);
    }
    pub fn left_up(&self) {
        self.button(0x110, false);
    }
    pub fn right_click(&self) {
        self.button(0x111, true);
        self.button(0x111, false);
    }

    pub fn wheel(&self, up: bool) {
        self.send(Cmd::Axis { value: if up { -15.0 } else { 15.0 } });
        self.sync();
        std::thread::sleep(Duration::from_millis(60));
    }

    /// Presses and releases an evdev key.
    pub fn tap(&self, code: u32) {
        self.send(Cmd::Key { code, pressed: true });
        self.sync();
        std::thread::sleep(Duration::from_millis(40));
        self.send(Cmd::Key { code, pressed: false });
        self.sync();
        std::thread::sleep(Duration::from_millis(60));
    }

    pub fn key(&self, code: u32, pressed: bool) {
        self.send(Cmd::Key { code, pressed });
        self.sync();
        std::thread::sleep(Duration::from_millis(40));
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Evdev codes used by tests.
pub mod evdev {
    pub const ESC: u32 = 1;
    pub const TAB: u32 = 15;
    pub const ENTER: u32 = 28;
    pub const LSHIFT: u32 = 42;
    pub const SPACE: u32 = 57;
    pub const C: u32 = 46;
    pub const UP: u32 = 103;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const DOWN: u32 = 108;
}
