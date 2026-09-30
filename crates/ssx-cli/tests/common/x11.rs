//! A private `Xvfb` per test plus a tiny x11rb client that paints known colours.
//!
//! `Xvfb -displayfd` lets the server pick a free display number, so parallel tests never
//! collide with each other or with a real X server.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use x11rb::{
    connection::Connection,
    protocol::xproto::{
        ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, Window,
        WindowClass,
    },
    rust_connection::RustConnection,
};

/// A running Xvfb, killed on drop.
pub struct Xvfb {
    child: Child,
    /// e.g. `:57`.
    pub display: String,
}

impl Xvfb {
    /// Starts a server with the given screen (`"800x600x24"`), or prints why and returns
    /// `None` so the test can skip.
    pub fn start(screen: &str) -> Option<Xvfb> {
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
            eprintln!("SKIP: Xvfb refused to start with screen {screen}");
            let _ = child.wait();
            return None;
        }
        let x = Xvfb { child, display: format!(":{}", line.trim()) };
        for _ in 0..200 {
            if RustConnection::connect(Some(&x.display)).is_ok() {
                return Some(x);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("SKIP: Xvfb on {} never accepted connections", x.display);
        None
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Background colour of the root window after [`Painter::new`].
pub const ROOT_BG: u32 = 0x10_20_30;

/// Paints windows with solid colours.
pub struct Painter {
    conn: RustConnection,
    root: Window,
}

impl Painter {
    /// Connects and paints the root window with [`ROOT_BG`].
    pub fn new(display: &str) -> Self {
        let (conn, screen) = RustConnection::connect(Some(display)).expect("connect painter");
        let root = conn.setup().roots[screen].root;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().background_pixel(ROOT_BG),
        )
        .expect("root background");
        conn.clear_area(false, root, 0, 0, 0, 0).expect("clear root");
        let p = Self { conn, root };
        p.sync();
        p
    }

    fn sync(&self) {
        self.conn.get_input_focus().expect("sync").reply().expect("sync reply");
    }

    /// Creates and maps a window filled with `rgb` (`0xRRGGBB`).
    pub fn window(&self, x: i16, y: i16, w: u16, h: u16, rgb: u32) {
        let id = self.conn.generate_id().expect("id");
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
                &CreateWindowAux::new().background_pixel(rgb).event_mask(EventMask::EXPOSURE),
            )
            .expect("create window");
        self.conn.map_window(id).expect("map");
        self.sync();
    }
}

/// The scene the tests paint: `(x, y, w, h, 0xRRGGBB)` in stacking order.
pub const SCENE: [(i32, i32, u32, u32, u32); 3] = [
    (10, 10, 100, 50, 0xff_00_00),
    (200, 100, 50, 50, 0x00_ff_00),
    (60, 30, 40, 40, 0x00_00_ff), // overlaps the first, on top
];

/// Paints [`SCENE`] on a fresh 800x600 server.
pub fn scene_server() -> Option<(Xvfb, Painter)> {
    let x = Xvfb::start("800x600x24")?;
    let p = Painter::new(&x.display);
    for (sx, sy, w, h, c) in SCENE {
        p.window(
            i16::try_from(sx).expect("x"),
            i16::try_from(sy).expect("y"),
            u16::try_from(w).expect("w"),
            u16::try_from(h).expect("h"),
            c,
        );
    }
    Some((x, p))
}

/// Expected RGB of every pixel of an `w`x`h` capture whose top-left is desktop `(ox, oy)`.
pub fn expected_scene(w: u32, h: u32, ox: i32, oy: i32) -> Vec<[u8; 3]> {
    let rgb = |c: u32| [(c >> 16) as u8, (c >> 8) as u8, c as u8];
    let mut out = vec![rgb(ROOT_BG); (w * h) as usize];
    for (sx, sy, sw, sh, c) in SCENE {
        for y in sy..sy + i32::try_from(sh).expect("h") {
            for x in sx..sx + i32::try_from(sw).expect("w") {
                let (lx, ly) = (x - ox, y - oy);
                if lx >= 0 && ly >= 0 && (lx as u32) < w && (ly as u32) < h {
                    out[(ly as u32 * w + lx as u32) as usize] = rgb(c);
                }
            }
        }
    }
    out
}
