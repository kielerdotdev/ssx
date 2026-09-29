//! Test harness: a private D-Bus session bus plus mock `xdg-desktop-portal`,
//! `org.kde.KWin.ScreenShot2` and `org.gnome.Mutter.DisplayConfig` services.
//!
//! Nothing here talks to the real desktop. The bus is a throw-away `dbus-daemon` with a
//! minimal config (no service activation), so tests are hermetic and deterministic. When
//! `dbus-daemon` is not installed, [`Bus::start`] returns `None` and callers skip.

use std::{
    collections::HashMap,
    fmt::Write as _,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ssx_capture_portal::PortalConfig;
use zbus::{
    Connection, fdo,
    message::Header,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

/// A private session bus. Killed on drop.
pub struct Bus {
    child: Child,
    pub address: String,
    dir: PathBuf,
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique scratch directory for one test.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ssx-portal-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

impl Bus {
    /// Starts a private bus, or prints why the test is skipped and returns `None`.
    pub fn start() -> Option<Bus> {
        let dir = scratch_dir("bus");
        let socket = dir.join("bus");
        let config = format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig><type>session</type><listen>unix:path={}</listen>\
             <auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
            socket.display()
        );
        let config_path = dir.join("bus.conf");
        std::fs::write(&config_path, config).expect("write bus config");
        let spawned = Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--config-file={}", config_path.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run dbus-daemon ({e}); install the `dbus` package");
                return None;
            }
        };
        let mut line = String::new();
        let out = child.stdout.take().expect("piped stdout");
        if BufReader::new(out).read_line(&mut line).is_err() || line.trim().is_empty() {
            eprintln!("SKIP: dbus-daemon did not print an address");
            let _ = child.kill();
            return None;
        }
        Some(Bus { child, address: line.trim().to_owned(), dir })
    }

    /// A config that talks to this bus and never touches the real Wayland session.
    pub fn config(&self) -> PortalConfig {
        PortalConfig {
            bus_address: Some(self.address.clone()),
            wayland_outputs: false,
            timeout: Duration::from_secs(10),
            interactive_timeout: Duration::from_secs(10),
            layout_timeout: Duration::from_secs(5),
            ..PortalConfig::default()
        }
    }

    /// Connects a service connection to the bus and claims `name`.
    pub fn service(&self, name: &str) -> zbus::connection::Builder<'static> {
        zbus::connection::Builder::address(self.address.as_str())
            .expect("bus address")
            .name(name.to_owned())
            .expect("bus name")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs a future to completion on the calling thread.
pub fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    async_io::block_on(f)
}

pub fn ov(v: impl Into<Value<'static>>) -> OwnedValue {
    v.into().try_to_owned().expect("no fds in test values")
}

// ---------------------------------------------------------------------------------
// Known pixel patterns
// ---------------------------------------------------------------------------------

/// Straight RGBA of pixel `(x, y)` in the reference desktop image. Every pixel is distinct
/// in the low bits, so any misplaced crop or row shift changes the result.
pub fn pattern(x: u32, y: u32) -> [u8; 4] {
    [(x % 251) as u8, (y % 241) as u8, ((x / 7 + y / 5) % 256) as u8, 255]
}

/// A `w` x `h` RGBA image of [`pattern`] with `origin` added to the coordinates (so crops
/// of a larger image can be compared with `pattern`).
pub fn pattern_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&pattern(x, y));
        }
    }
    v
}

pub fn pattern_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_raw(w, h, pattern_rgba(w, h)).expect("size");
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).expect("encode");
    out
}

/// Percent-encodes everything but unreserved characters and `/`.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            s.push(b as char);
        } else {
            write!(s, "%{b:02X}").expect("write to String");
        }
    }
    s
}

// ---------------------------------------------------------------------------------
// Mock xdg-desktop-portal
// ---------------------------------------------------------------------------------

/// What the mock portal does with a `Screenshot` request.
#[derive(Clone, Debug)]
pub enum PortalBehavior {
    /// Write a PNG of `w` x `h` to `dir/file_name` and answer with its URI.
    Png { w: u32, h: u32, file_name: String, delay: Duration },
    /// Answer with a URI, whatever it points at.
    RawUri { uri: String },
    /// Write these bytes to `dir/file_name` and answer with its URI.
    Bytes { bytes: Vec<u8>, file_name: String },
    /// Response code 1.
    Cancelled,
    /// Response code 2.
    Denied,
    /// Never respond.
    Silent,
}

#[derive(Default, Debug)]
pub struct PortalLog {
    /// `interactive` option of every request, in order.
    pub interactive: Vec<Option<bool>>,
}

pub struct MockPortal {
    pub behavior: Arc<Mutex<PortalBehavior>>,
    pub log: Arc<Mutex<PortalLog>>,
    pub dir: PathBuf,
}

#[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
impl MockPortal {
    #[zbus(property, name = "version")]
    #[allow(clippy::unused_self)] // signature dictated by the zbus property macro
    fn version(&self) -> u32 {
        2
    }

    fn screenshot(
        &self,
        parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let _ = parent_window; // unused by the mock, but part of the D-Bus signature
        let interactive = options.get("interactive").and_then(|v| match &**v {
            Value::Bool(b) => Some(*b),
            _ => None,
        });
        let request_no = {
            let mut log = self.log.lock().expect("log");
            log.interactive.push(interactive);
            log.interactive.len()
        };
        let token = match options.get("handle_token").map(|v| &**v) {
            Some(Value::Str(s)) => s.to_string(),
            _ => return Err(fdo::Error::InvalidArgs("handle_token missing".into())),
        };
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::Failed("no sender".into()))?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

        let behavior = self.behavior.lock().expect("behavior").clone();
        let dir = self.dir.clone();
        let conn = conn.clone();
        let response_path = path.clone();
        std::thread::spawn(move || {
            let respond = |code: u32, uri: Option<String>, delay: Duration| {
                std::thread::sleep(delay);
                let mut results: HashMap<&str, Value<'_>> = HashMap::new();
                if let Some(u) = &uri {
                    results.insert("uri", Value::from(u.as_str()));
                }
                let r = block_on(conn.emit_signal(
                    None::<&str>,
                    response_path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(code, results),
                ));
                assert!(r.is_ok(), "emit Response: {r:?}");
            };
            match behavior {
                PortalBehavior::Png { w, h, file_name, delay } => {
                    let p = dir.join(file_name.replace("{n}", &request_no.to_string()));
                    std::fs::write(&p, pattern_png(w, h)).expect("write png");
                    respond(0, Some(file_uri(&p)), delay);
                }
                PortalBehavior::Bytes { bytes, file_name } => {
                    let p = dir.join(file_name);
                    std::fs::write(&p, bytes).expect("write bytes");
                    respond(0, Some(file_uri(&p)), Duration::ZERO);
                }
                PortalBehavior::RawUri { uri } => respond(0, Some(uri), Duration::ZERO),
                PortalBehavior::Cancelled => respond(1, None, Duration::ZERO),
                PortalBehavior::Denied => respond(2, None, Duration::ZERO),
                PortalBehavior::Silent => {}
            }
        });
        Ok(OwnedObjectPath::from(ObjectPath::try_from(path).map_err(zbus::Error::from)?))
    }
}

/// Keeps a mock portal alive and lets the test steer it.
pub struct PortalHandle {
    pub behavior: Arc<Mutex<PortalBehavior>>,
    pub log: Arc<Mutex<PortalLog>>,
    pub dir: PathBuf,
    _conn: Connection,
}

impl PortalHandle {
    pub fn set(&self, b: PortalBehavior) {
        *self.behavior.lock().expect("behavior") = b;
    }

    pub fn requests(&self) -> usize {
        self.log.lock().expect("log").interactive.len()
    }

    pub fn interactive_flags(&self) -> Vec<Option<bool>> {
        self.log.lock().expect("log").interactive.clone()
    }

    /// Files currently in the portal's output directory.
    pub fn files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.dir)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    }
}

pub fn start_portal(bus: &Bus, behavior: PortalBehavior) -> PortalHandle {
    let dir = scratch_dir("portal-out");
    let behavior = Arc::new(Mutex::new(behavior));
    let log = Arc::new(Mutex::new(PortalLog::default()));
    let mock = MockPortal { behavior: behavior.clone(), log: log.clone(), dir: dir.clone() };
    let conn = block_on(async {
        bus.service("org.freedesktop.portal.Desktop")
            .serve_at("/org/freedesktop/portal/desktop", mock)
            .expect("serve portal")
            .build()
            .await
            .expect("portal connection")
    });
    PortalHandle { behavior, log, dir, _conn: conn }
}

// ---------------------------------------------------------------------------------
// Mock Mutter DisplayConfig
// ---------------------------------------------------------------------------------

type Spec = (String, String, String, String);
type Props = HashMap<String, OwnedValue>;
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Physical = (Spec, Vec<Mode>, Props);
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);

/// One monitor of the mock's layout: connector, physical size, logical position, scale.
#[derive(Clone, Copy, Debug)]
pub struct MockMonitor {
    pub connector: &'static str,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub primary: bool,
}

pub struct MockMutter {
    pub monitors: Vec<MockMonitor>,
}

fn spec(c: &str) -> Spec {
    (c.into(), "VEN".into(), format!("prod-{c}"), "0x0".into())
}

#[zbus::interface(name = "org.gnome.Mutter.DisplayConfig")]
impl MockMutter {
    fn get_current_state(&self) -> (u32, Vec<Physical>, Vec<Logical>, Props) {
        let physical = self
            .monitors
            .iter()
            .map(|m| {
                let mut mode_props = Props::new();
                mode_props.insert("is-current".into(), ov(true));
                let mut props = Props::new();
                props.insert("display-name".into(), ov(format!("Mock {}", m.connector)));
                (
                    spec(m.connector),
                    vec![(
                        format!("{}x{}@60.000", m.width, m.height),
                        m.width,
                        m.height,
                        60.0,
                        1.0,
                        vec![1.0, 2.0],
                        mode_props,
                    )],
                    props,
                )
            })
            .collect();
        let logical = self
            .monitors
            .iter()
            .map(|m| (m.x, m.y, m.scale, 0, m.primary, vec![spec(m.connector)], Props::new()))
            .collect();
        (7, physical, logical, Props::new())
    }
}

pub fn start_mutter(bus: &Bus, monitors: Vec<MockMonitor>) -> Connection {
    block_on(async {
        bus.service("org.gnome.Mutter.DisplayConfig")
            .serve_at("/org/gnome/Mutter/DisplayConfig", MockMutter { monitors })
            .expect("serve mutter")
            .build()
            .await
            .expect("mutter connection")
    })
}

// ---------------------------------------------------------------------------------
// Mock KWin ScreenShot2
// ---------------------------------------------------------------------------------

/// The image the mock KWin produces.
#[derive(Clone, Debug)]
pub struct KwinImage {
    pub width: u32,
    pub height: u32,
    /// `QImage::Format` value reported and used to encode the pixels.
    pub format: u32,
    /// Bytes per line; may exceed the row size (padding is filled with 0xEE).
    pub stride: u32,
    /// Report only this many bytes instead of the full image (simulates a broken writer).
    pub truncate_to: Option<usize>,
    pub scale: f64,
}

#[derive(Clone, Debug)]
pub enum KwinBehavior {
    Image(KwinImage),
    /// Reply with this D-Bus error name.
    Error(&'static str),
    /// Never reply.
    Hang,
}

#[derive(Default, Debug)]
pub struct KwinLog {
    /// `(method, first string/int args rendered, options rendered as "k=v")`
    pub calls: Vec<KwinCall>,
}

#[derive(Clone, Debug)]
pub struct KwinCall {
    pub method: &'static str,
    /// Positional arguments other than options/pipe, rendered as text.
    pub args: Vec<String>,
    pub native_resolution: Option<bool>,
    pub include_cursor: Option<bool>,
}

/// Encodes `pattern`-derived pixels of size `w` x `h` as `QImage` `format` into a buffer
/// with the given stride. Alpha is `alpha_of(x, y)`; colour channels are the *straight*
/// pattern, premultiplied when the format calls for it.
pub fn encode_qimage(
    format: u32,
    w: u32,
    h: u32,
    stride: u32,
    alpha_of: impl Fn(u32, u32) -> u8,
) -> Vec<u8> {
    let bpp = match format {
        4 | 5 | 6 | 16 | 17 | 18 => 4,
        13 | 29 => 3,
        other => panic!("test encoder lacks format {other}"),
    };
    let mut out = Vec::new();
    for y in 0..h {
        let row_start = out.len();
        for x in 0..w {
            let [r, g, b, _] = pattern(x, y);
            let a = alpha_of(x, y);
            let pre = |c: u8| ((u32::from(c) * u32::from(a) + 127) / 255) as u8;
            match format {
                4 => out.extend_from_slice(&argb(0xFF, r, g, b)),
                5 => out.extend_from_slice(&argb(a, r, g, b)),
                6 => out.extend_from_slice(&argb(a, pre(r), pre(g), pre(b))),
                16 => out.extend_from_slice(&[r, g, b, 0]),
                17 => out.extend_from_slice(&[r, g, b, a]),
                18 => out.extend_from_slice(&[pre(r), pre(g), pre(b), a]),
                13 => out.extend_from_slice(&[r, g, b]),
                29 => out.extend_from_slice(&[b, g, r]),
                _ => unreachable!(),
            }
        }
        let row = (w * bpp) as usize;
        assert!(stride as usize >= row);
        // no padding after the very last row, like Qt's tightly sized buffers
        let pad = if y + 1 == h { 0 } else { stride as usize - row };
        out.resize(row_start + row + pad, 0xEE);
    }
    out
}

/// Alpha the mock KWin uses per pixel: opaque for formats without alpha, varying otherwise
/// so that premultiplied round trips are meaningful.
pub fn kwin_alpha(format: u32, x: u32, y: u32) -> u8 {
    if matches!(format, 4 | 13 | 16 | 29) { 255 } else { (128 + (x + y) % 128) as u8 }
}

fn argb(a: u8, r: u8, g: u8, b: u8) -> [u8; 4] {
    (u32::from(a) << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)).to_ne_bytes()
}

#[derive(zbus::DBusError, Debug)]
#[zbus(prefix = "org.kde.KWin.ScreenShot2.Error")]
pub enum KwinError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Cancelled(String),
    NoAuthorized(String),
    InvalidScreen(String),
    InvalidArea(String),
    InvalidWindow(String),
    NoActiveWindow(String),
    FileDescriptor(String),
}

pub struct MockKwin {
    pub version: u32,
    pub behavior: Arc<Mutex<KwinBehavior>>,
    pub log: Arc<Mutex<KwinLog>>,
    /// Screen names `CaptureScreen` accepts; others yield `InvalidScreen`.
    pub screens: Vec<String>,
}

impl MockKwin {
    async fn respond(
        &self,
        call: KwinCall,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        self.log.lock().expect("log").calls.push(call);
        let behavior = self.behavior.lock().expect("behavior").clone();
        match behavior {
            KwinBehavior::Hang => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            KwinBehavior::Error(name) => Err(match name {
                "Cancelled" => KwinError::Cancelled("cancelled".into()),
                "NoAuthorized" => KwinError::NoAuthorized("not authorised".into()),
                "InvalidScreen" => KwinError::InvalidScreen("invalid screen".into()),
                "InvalidArea" => KwinError::InvalidArea("invalid area".into()),
                "InvalidWindow" => KwinError::InvalidWindow("invalid window".into()),
                "NoActiveWindow" => KwinError::NoActiveWindow("none".into()),
                _ => KwinError::FileDescriptor("bad fd".into()),
            }),
            KwinBehavior::Image(img) => {
                let data = if matches!(img.format, 4 | 5 | 6 | 13 | 16 | 17 | 18 | 29) {
                    encode_qimage(img.format, img.width, img.height, img.stride, |x, y| {
                        kwin_alpha(img.format, x, y)
                    })
                } else {
                    // formats the test encoder does not model: any bytes will do
                    vec![0; img.stride as usize * img.height as usize]
                };
                let data = match img.truncate_to {
                    Some(n) => data[..n.min(data.len())].to_vec(),
                    None => data,
                };
                let mut reply: HashMap<String, OwnedValue> = HashMap::new();
                reply.insert("type".into(), ov("raw"));
                reply.insert("width".into(), ov(img.width));
                reply.insert("height".into(), ov(img.height));
                reply.insert("stride".into(), ov(img.stride));
                reply.insert("format".into(), ov(img.format));
                reply.insert("scale".into(), ov(img.scale));
                // Like KWin: reply first, write the pixels afterwards from another thread.
                let fd: std::os::fd::OwnedFd = pipe.into();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    let mut file = std::fs::File::from(fd);
                    // A broken reader just ends the write early; nothing to report.
                    let _ = file.write_all(&data);
                });
                Ok(reply)
            }
        }
    }
}

fn opts_of(options: &HashMap<String, OwnedValue>) -> (Option<bool>, Option<bool>) {
    let get = |k: &str| {
        options.get(k).and_then(|v| match &**v {
            Value::Bool(b) => Some(*b),
            _ => None,
        })
    };
    (get("native-resolution"), get("include-cursor"))
}

#[zbus::interface(name = "org.kde.KWin.ScreenShot2")]
impl MockKwin {
    #[zbus(property, name = "Version")]
    fn version(&self) -> u32 {
        self.version
    }

    async fn capture_workspace(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureWorkspace",
            args: vec![],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_active_screen(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureActiveScreen",
            args: vec![],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_active_window(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureActiveWindow",
            args: vec![],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_screen(
        &self,
        name: String,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        if !self.screens.contains(&name) {
            return Err(KwinError::InvalidScreen(format!("no screen {name}")));
        }
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureScreen",
            args: vec![name],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_area(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureArea",
            args: vec![x.to_string(), y.to_string(), width.to_string(), height.to_string()],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_window(
        &self,
        handle: String,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureWindow",
            args: vec![handle],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }

    async fn capture_interactive(
        &self,
        kind: u32,
        options: HashMap<String, OwnedValue>,
        pipe: zbus::zvariant::OwnedFd,
    ) -> Result<HashMap<String, OwnedValue>, KwinError> {
        let (n, c) = opts_of(&options);
        let call = KwinCall {
            method: "CaptureInteractive",
            args: vec![kind.to_string()],
            native_resolution: n,
            include_cursor: c,
        };
        self.respond(call, pipe).await
    }
}

pub struct KwinHandle {
    pub behavior: Arc<Mutex<KwinBehavior>>,
    pub log: Arc<Mutex<KwinLog>>,
    _conn: Connection,
}

impl KwinHandle {
    pub fn set(&self, b: KwinBehavior) {
        *self.behavior.lock().expect("behavior") = b;
    }

    pub fn calls(&self) -> Vec<KwinCall> {
        self.log.lock().expect("log").calls.clone()
    }
}

pub fn start_kwin(bus: &Bus, version: u32, screens: &[&str], behavior: KwinBehavior) -> KwinHandle {
    let behavior = Arc::new(Mutex::new(behavior));
    let log = Arc::new(Mutex::new(KwinLog::default()));
    let mock = MockKwin {
        version,
        behavior: behavior.clone(),
        log: log.clone(),
        screens: screens.iter().map(|s| (*s).to_owned()).collect(),
    };
    let conn = block_on(async {
        bus.service("org.kde.KWin.ScreenShot2")
            .serve_at("/org/kde/KWin/ScreenShot2", mock)
            .expect("serve kwin")
            .build()
            .await
            .expect("kwin connection")
    });
    KwinHandle { behavior, log, _conn: conn }
}
