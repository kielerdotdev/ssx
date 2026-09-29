//! Test harness: a private `Xvfb` per test plus a tiny x11rb client that paints windows
//! with known colours.
//!
//! `Xvfb -displayfd` lets the server choose a free display number and report it, so tests
//! never collide with each other or with a real X server. When `Xvfb` is missing (or
//! refuses a configuration such as depth 32) [`Xvfb::start`] prints why and returns `None`
//! and the test skips.

#![allow(dead_code)] // each test binary uses a subset of the harness

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

use ssx_types::{Frame, PixelFormat};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Window, WindowClass,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

/// A running Xvfb. Killed on drop.
pub struct Xvfb {
    child: Child,
    pub display: String,
}

impl Xvfb {
    /// Starts Xvfb with `screen` (e.g. `"800x600x24"`) and extra arguments.
    pub fn start(screen: &str, extra: &[&str]) -> Option<Xvfb> {
        Self::start_on(None, screen, extra)
    }

    /// Like [`Xvfb::start`] but on a specific display (`":42"`), used to restart a server
    /// on the display an `X11Capture` is already bound to.
    pub fn start_on(display: Option<&str>, screen: &str, extra: &[&str]) -> Option<Xvfb> {
        let mut cmd = Command::new("Xvfb");
        match display {
            Some(d) => cmd.arg(d),
            None => cmd.args(["-displayfd", "1"]),
        };
        cmd.args(["-screen", "0", screen, "-noreset", "-ac", "-nolisten", "tcp"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run Xvfb ({e}); install the `xvfb` package");
                return None;
            }
        };
        let display = if let Some(d) = display {
            d.to_owned()
        } else {
            let mut line = String::new();
            let out = child.stdout.take()?;
            if BufReader::new(out).read_line(&mut line).unwrap_or(0) == 0 {
                eprintln!("SKIP: Xvfb refused to start with screen {screen} {extra:?}");
                let _ = child.wait();
                return None;
            }
            format!(":{}", line.trim())
        };
        let xvfb = Xvfb { child, display };
        // Wait until it accepts connections (with an explicit display there is no
        // -displayfd handshake).
        for _ in 0..200 {
            if RustConnection::connect(Some(&xvfb.display)).is_ok() {
                return Some(xvfb);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        eprintln!("SKIP: Xvfb on {} never accepted connections", xvfb.display);
        None
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Starts the default 800x600x24 server, or skips.
pub fn xvfb() -> Option<Xvfb> {
    Xvfb::start("800x600x24", &[])
}

/// Background colour painted on the root by [`Client::new`], `0xRRGGBB`.
pub const ROOT_BG: u32 = 0x10_20_30;

/// A minimal X client for arranging test scenes.
pub struct Client {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub root_visual: u32,
}

impl Client {
    pub fn new(display: &str) -> Client {
        let (conn, screen) = RustConnection::connect(Some(display)).expect("connect test client");
        let s = &conn.setup().roots[screen];
        let (root, depth, root_visual) = (s.root, s.root_depth, s.root_visual);
        let c = Client { conn, root, depth, root_visual };
        c.conn
            .change_window_attributes(
                root,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .background_pixel(ROOT_BG),
            )
            .unwrap();
        c.conn.clear_area(false, root, 0, 0, 0, 0).unwrap();
        c.sync();
        c
    }

    pub fn sync(&self) {
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }

    /// Creates and maps an override-redirect-free window filled with `rgb` (`0xRRGGBB`).
    /// Only valid for `TrueColor` 24/32-bit roots with the standard masks.
    pub fn window(&self, x: i16, y: i16, w: u16, h: u16, rgb: u32) -> Window {
        self.window_pixel(x, y, w, h, rgb, false)
    }

    pub fn window_pixel(
        &self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        pixel: u32,
        unmapped: bool,
    ) -> Window {
        let id = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                self.root,
                x,
                y,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().background_pixel(pixel).event_mask(EventMask::EXPOSURE),
            )
            .unwrap();
        if !unmapped {
            self.conn.map_window(id).unwrap();
        }
        self.sync();
        id
    }

    pub fn set_utf8_title(&self, win: Window, title: &str) {
        let name = self.atom("_NET_WM_NAME");
        let utf8 = self.atom("UTF8_STRING");
        self.conn.change_property8(PropMode::REPLACE, win, name, utf8, title.as_bytes()).unwrap();
    }

    pub fn set_wm_name(&self, win: Window, title: &[u8]) {
        self.conn
            .change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title)
            .unwrap();
    }

    pub fn set_class(&self, win: Window, instance: &str, class: &str) {
        let mut v = instance.as_bytes().to_vec();
        v.push(0);
        v.extend_from_slice(class.as_bytes());
        v.push(0);
        self.conn
            .change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, &v)
            .unwrap();
    }

    pub fn set_cardinals(&self, win: Window, name: &str, values: &[u32]) {
        let a = self.atom(name);
        self.conn.change_property32(PropMode::REPLACE, win, a, AtomEnum::CARDINAL, values).unwrap();
    }

    pub fn set_windows_prop(&self, win: Window, name: &str, values: &[u32]) {
        let a = self.atom(name);
        self.conn.change_property32(PropMode::REPLACE, win, a, AtomEnum::WINDOW, values).unwrap();
    }

    pub fn set_atoms_prop(&self, win: Window, name: &str, values: &[&str]) {
        let a = self.atom(name);
        let vals: Vec<u32> = values.iter().map(|v| self.atom(v)).collect();
        self.conn.change_property32(PropMode::REPLACE, win, a, AtomEnum::ATOM, &vals).unwrap();
    }

    pub fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    pub fn move_window(&self, win: Window, x: i32, y: i32) {
        self.conn
            .configure_window(win, &x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y))
            .unwrap();
        self.sync();
    }
}

/// Builds the expected BGRA image of a scene: `bg` everywhere, then rectangles painted in
/// order (`x, y, w, h, 0xRRGGBB`). Rectangles may extend past the canvas.
pub fn expected_scene(
    width: u32,
    height: u32,
    origin: (i32, i32),
    bg: u32,
    rects: &[(i32, i32, u32, u32, u32)],
) -> Vec<u8> {
    let bgra =
        |rgb: u32| [(rgb & 0xff) as u8, ((rgb >> 8) & 0xff) as u8, ((rgb >> 16) & 0xff) as u8, 255];
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        out.extend_from_slice(&bgra(bg));
    }
    for &(rx, ry, rw, rh, rgb) in rects {
        for y in ry..ry + i32::try_from(rh).unwrap() {
            for x in rx..rx + i32::try_from(rw).unwrap() {
                let (lx, ly) = (x - origin.0, y - origin.1);
                if lx >= 0 && ly >= 0 && (lx as u32) < width && (ly as u32) < height {
                    let i = ((ly as u32 * width + lx as u32) * 4) as usize;
                    out[i..i + 4].copy_from_slice(&bgra(rgb));
                }
            }
        }
    }
    out
}

/// Tightly packed pixel data of a frame (drops stride padding).
pub fn pixels(frame: &Frame) -> Vec<u8> {
    assert_eq!(frame.format(), PixelFormat::Bgra8);
    let mut v = Vec::new();
    for y in 0..frame.height() {
        v.extend_from_slice(frame.row(y));
    }
    v
}

/// Asserts a frame equals the expected buffer, reporting the first differing pixel.
pub fn assert_pixels(frame: &Frame, expected: &[u8]) {
    let got = pixels(frame);
    assert_eq!(got.len(), expected.len(), "frame size {}x{}", frame.width(), frame.height());
    if let Some(i) = got.iter().zip(expected).position(|(a, b)| a != b) {
        let px = i / 4;
        let (x, y) = (px as u32 % frame.width(), px as u32 / frame.width());
        panic!(
            "first difference at ({x},{y}): got BGRA {:?}, expected {:?}",
            &got[px * 4..px * 4 + 4],
            &expected[px * 4..px * 4 + 4]
        );
    }
}

/// Pixel `(x, y)` of a frame as `[b, g, r, a]`.
pub fn px(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let r = frame.row(y);
    [r[x as usize * 4], r[x as usize * 4 + 1], r[x as usize * 4 + 2], r[x as usize * 4 + 3]]
}
