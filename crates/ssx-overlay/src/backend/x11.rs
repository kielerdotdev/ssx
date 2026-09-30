//! X11 backend: one override-redirect window covering the virtual desktop.
//!
//! Decisions:
//!
//! * **Override-redirect**, so no window manager decorates, moves, animates or re-stacks
//!   it and the first frame is on screen as soon as the server has processed our requests.
//!   The window is positioned at the desktop origin of the frozen frame, so X11 root
//!   coordinates and desktop pixels coincide up to that offset.
//! * The pointer and keyboard are **grabbed** so Escape and clicks reach us regardless of
//!   which client has focus, with a short retry because a menu that is closing may still hold
//!   a grab. The desktop is frozen, so nothing else should be interacting anyway.
//! * Presentation is `PutImage` of the dirty rectangles from a shadow buffer. Motion events
//!   are drained in batches and rendered once per batch, which is what keeps dragging
//!   smooth even when the server delivers hundreds of motion events per second.
//! * Keys are decoded from the keyboard mapping's keysyms, so `Escape`, `Enter` and the
//!   arrows work on any layout; `C` is matched as a letter keysym.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::time::Timespec;
use ssx_types::{Point, Rect, Size};
use x11rb::{
    COPY_FROM_PARENT, CURRENT_TIME,
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        xproto::{
            self, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
            GrabStatus, ImageFormat, KeyButMask, StackMode, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use super::Failure;
use crate::{
    app::OverlayApp,
    error::OverlayError,
    model::{
        CursorHint, InputEvent, Key, KeyEvent, Modifiers, PointerButton, PointerEvent,
        geometry::Handle,
    },
    render::TargetBuf,
};

const NAME: &str = "x11";

fn runtime(e: impl std::fmt::Display) -> Failure {
    Failure::Runtime(OverlayError::backend(NAME, e))
}

fn setup(e: impl std::fmt::Display) -> Failure {
    Failure::Setup(e.to_string())
}

// ---------------------------------------------------------------------------- keys

/// X11 keysyms the overlay reacts to.
mod ks {
    pub const SPACE: u32 = 0x20;
    pub const TAB: u32 = 0xff09;
    pub const ISO_LEFT_TAB: u32 = 0xfe20;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const KP_LEFT: u32 = 0xff96;
    pub const KP_UP: u32 = 0xff97;
    pub const KP_RIGHT: u32 = 0xff98;
    pub const KP_DOWN: u32 = 0xff99;
    pub const SHIFT_L: u32 = 0xffe1;
    pub const SHIFT_R: u32 = 0xffe2;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const CONTROL_R: u32 = 0xffe4;
    pub const META_L: u32 = 0xffe7;
    pub const META_R: u32 = 0xffe8;
    pub const ALT_L: u32 = 0xffe9;
    pub const ALT_R: u32 = 0xffea;
}

/// Translates a keysym to a logical key. `ISO_Left_Tab` (Shift+Tab) is reported as
/// `(Tab, true)` so the caller can add the Shift the layout swallowed.
pub(crate) fn key_from_keysym(sym: u32) -> (Key, bool) {
    let k = match sym {
        ks::SPACE => Key::Space,
        ks::TAB => Key::Tab,
        ks::ISO_LEFT_TAB => return (Key::Tab, true),
        ks::RETURN | ks::KP_ENTER => Key::Enter,
        ks::ESCAPE => Key::Escape,
        ks::LEFT | ks::KP_LEFT => Key::Left,
        ks::UP | ks::KP_UP => Key::Up,
        ks::RIGHT | ks::KP_RIGHT => Key::Right,
        ks::DOWN | ks::KP_DOWN => Key::Down,
        ks::SHIFT_L | ks::SHIFT_R => Key::Shift,
        ks::CONTROL_L | ks::CONTROL_R => Key::Control,
        ks::ALT_L | ks::ALT_R | ks::META_L | ks::META_R => Key::Alt,
        0x41..=0x5a => Key::Char((sym as u8 + 32) as char),
        0x61..=0x7a | 0x30..=0x39 => Key::Char(sym as u8 as char),
        _ => Key::Other,
    };
    (k, false)
}

/// Decodes core-protocol modifier bits.
pub(crate) fn modifiers_from_mask(state: u16) -> Modifiers {
    Modifiers {
        shift: state & u16::from(KeyButMask::SHIFT) != 0,
        ctrl: state & u16::from(KeyButMask::CONTROL) != 0,
        alt: state & u16::from(KeyButMask::MOD1) != 0,
    }
}

struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    fn sym(&self, code: u8, col: usize) -> u32 {
        let Some(i) = usize::from(code).checked_sub(usize::from(self.min)) else { return 0 };
        self.syms.get(i * self.per + col).copied().unwrap_or(0)
    }

    /// Unshifted keysym, falling back to the second column for keys defined only there.
    fn key(&self, code: u8) -> (Key, bool) {
        let s0 = self.sym(code, 0);
        let (k, extra) = key_from_keysym(s0);
        if k == Key::Other && extra == false {
            return key_from_keysym(self.sym(code, 1));
        }
        (k, extra)
    }
}

// ---------------------------------------------------------------------------- cursors

fn glyph_for(hint: CursorHint) -> u16 {
    // Glyph indices in the standard X11 "cursor" font.
    match hint {
        CursorHint::Crosshair => 34,
        CursorHint::Move => 52,
        CursorHint::Resize(h) => match h {
            Handle::NorthWest => 134,
            Handle::North => 138,
            Handle::NorthEast => 136,
            Handle::East => 96,
            Handle::SouthEast => 14,
            Handle::South => 16,
            Handle::SouthWest => 12,
            Handle::West => 70,
        },
    }
}

// ---------------------------------------------------------------------------- session

struct Session {
    conn: RustConnection,
    win: xproto::Window,
    gc: xproto::Gcontext,
    depth: u8,
    origin: Point,
    size: Size,
    buf: Vec<u8>,
    keymap: Keymap,
    cursors: HashMap<u16, xproto::Cursor>,
    cursor_font: xproto::Font,
    current_cursor: u16,
    t0: Instant,
    pointer_grabbed: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best effort: the process is about to exit or return to its caller anyway.
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
        let _ = self.conn.get_input_focus().map(|c| c.reply());
    }
}

fn grab_mask() -> EventMask {
    EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW
}

impl Session {
    fn connect(app: &OverlayApp) -> Result<Self, Failure> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| setup(format!("cannot connect to the X server: {e}")))?;
        let setup_info = conn.setup();
        let screen = setup_info.roots.get(screen_num).ok_or_else(|| setup("no such X screen"))?;
        let (root, depth, visual) = (screen.root, screen.root_depth, screen.root_visual);

        let fmt = setup_info
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or_else(|| setup(format!("no pixmap format for depth {depth}")))?;
        if fmt.bits_per_pixel != 32 || setup_info.image_byte_order != xproto::ImageOrder::LSB_FIRST {
            return Err(setup(format!(
                "unsupported pixel layout (depth {depth}, {} bpp, {:?}); need 32 bpp little-endian",
                fmt.bits_per_pixel, setup_info.image_byte_order
            )));
        }

        let bounds = app.bounds();
        let (x, y) = (i16::try_from(bounds.x), i16::try_from(bounds.y));
        let (w, h) = (u16::try_from(bounds.width), u16::try_from(bounds.height));
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (x, y, w, h) else {
            return Err(setup(format!("desktop {bounds:?} does not fit the X11 coordinate range")));
        };

        let cursor_font = conn.generate_id().map_err(setup)?;
        conn.open_font(cursor_font, b"cursor").map_err(setup)?;
        let win = conn.generate_id().map_err(setup)?;
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(screen.black_pixel)
            .border_pixel(0)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::FOCUS_CHANGE,
            );
        conn.create_window(depth, win, root, x, y, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)
            .map_err(setup)?;
        let _ = COPY_FROM_PARENT;
        conn.change_property8(xproto::PropMode::REPLACE, win, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, b"ssx-overlay").map_err(setup)?;
        conn.change_property8(xproto::PropMode::REPLACE, win, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, b"ssx-overlay\0ssx-overlay\0").map_err(setup)?;
        let gc = conn.generate_id().map_err(setup)?;
        conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0)).map_err(setup)?;

        // Detectable auto-repeat: without it every repeat is a release+press pair, which
        // would make Space (move-while-dragging) flicker off and on.
        if x11rb::protocol::xkb::use_extension(&conn, 1, 0).is_ok() {
            let _ = x11rb::protocol::xkb::per_client_flags(
                &conn,
                x11rb::protocol::xkb::ID::USE_CORE_KBD.into(),
                x11rb::protocol::xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                x11rb::protocol::xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                x11rb::protocol::xkb::BoolCtrl::default(),
                x11rb::protocol::xkb::BoolCtrl::default(),
                x11rb::protocol::xkb::BoolCtrl::default(),
            );
        }

        let min = setup_info.min_keycode;
        let count = setup_info.max_keycode - min + 1;
        let km = conn.get_keyboard_mapping(min, count).map_err(setup)?.reply().map_err(setup)?;
        let keymap = Keymap { min, per: usize::from(km.keysyms_per_keycode), syms: km.keysyms };

        let size = Size::new(bounds.width, bounds.height);
        let buf = vec![0u8; size.width as usize * size.height as usize * 4];
        Ok(Self {
            conn,
            win,
            gc,
            depth,
            origin: bounds.origin(),
            size,
            buf,
            keymap,
            cursors: HashMap::new(),
            cursor_font,
            current_cursor: 0,
            t0: Instant::now(),
            pointer_grabbed: false,
        })
    }

    fn cursor(&mut self, glyph: u16) -> Result<xproto::Cursor, Failure> {
        if let Some(c) = self.cursors.get(&glyph) {
            return Ok(*c);
        }
        let id = self.conn.generate_id().map_err(runtime)?;
        self.conn
            .create_glyph_cursor(id, self.cursor_font, self.cursor_font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)
            .map_err(runtime)?;
        self.cursors.insert(glyph, id);
        Ok(id)
    }

    fn map_and_grab(&mut self) -> Result<(), Failure> {
        let cross = self.cursor(glyph_for(CursorHint::Crosshair))?;
        self.current_cursor = glyph_for(CursorHint::Crosshair);
        self.conn.map_window(self.win).map_err(runtime)?;
        self.conn
            .configure_window(self.win, &xproto::ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(runtime)?;
        self.conn.change_window_attributes(self.win, &xproto::ChangeWindowAttributesAux::new().cursor(cross)).map_err(runtime)?;

        // A closing menu can still hold a grab for a moment; retry briefly.
        let deadline = Instant::now() + Duration::from_millis(1000);
        loop {
            let st = self
                .conn
                .grab_pointer(false, self.win, grab_mask(), GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, cross, CURRENT_TIME)
                .map_err(runtime)?
                .reply()
                .map_err(runtime)?
                .status;
            if st == GrabStatus::SUCCESS {
                self.pointer_grabbed = true;
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!(?st, "could not grab the pointer; continuing without a grab");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        loop {
            let st = self
                .conn
                .grab_keyboard(false, self.win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                .map_err(runtime)?
                .reply()
                .map_err(runtime)?
                .status;
            if st == GrabStatus::SUCCESS {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!(?st, "could not grab the keyboard; falling back to input focus");
                let _ = self.conn.set_input_focus(xproto::InputFocus::POINTER_ROOT, self.win, CURRENT_TIME);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.conn.flush().map_err(runtime)?;
        Ok(())
    }

    fn set_cursor(&mut self, hint: CursorHint) -> Result<(), Failure> {
        let glyph = glyph_for(hint);
        if glyph == self.current_cursor {
            return Ok(());
        }
        let c = self.cursor(glyph)?;
        self.current_cursor = glyph;
        if self.pointer_grabbed {
            self.conn.change_active_pointer_grab(c, CURRENT_TIME, grab_mask()).map_err(runtime)?;
        } else {
            self.conn
                .change_window_attributes(self.win, &xproto::ChangeWindowAttributesAux::new().cursor(c))
                .map_err(runtime)?;
        }
        Ok(())
    }

    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn desktop(&self, x: i16, y: i16) -> Point {
        Point::new(self.origin.x + i32::from(x), self.origin.y + i32::from(y))
    }

    /// Native event to model events.
    fn translate(&self, ev: &Event, out: &mut Vec<InputEvent>) {
        match ev {
            Event::MotionNotify(e) => {
                out.push(InputEvent::Modifiers(modifiers_from_mask(e.state.into())));
                out.push(InputEvent::Pointer(PointerEvent::Move { pos: self.desktop(e.event_x, e.event_y) }));
            }
            Event::EnterNotify(e) => {
                out.push(InputEvent::Modifiers(modifiers_from_mask(e.state.into())));
                out.push(InputEvent::Pointer(PointerEvent::Move { pos: self.desktop(e.event_x, e.event_y) }));
            }
            Event::LeaveNotify(_) => out.push(InputEvent::Pointer(PointerEvent::Leave)),
            Event::ButtonPress(e) => {
                let pos = self.desktop(e.event_x, e.event_y);
                out.push(InputEvent::Modifiers(modifiers_from_mask(e.state.into())));
                match e.detail {
                    1 => out.push(InputEvent::Pointer(PointerEvent::Down { pos, button: PointerButton::Left, time_ms: self.now_ms() })),
                    2 => out.push(InputEvent::Pointer(PointerEvent::Down { pos, button: PointerButton::Middle, time_ms: self.now_ms() })),
                    3 => out.push(InputEvent::Pointer(PointerEvent::Down { pos, button: PointerButton::Right, time_ms: self.now_ms() })),
                    4 => out.push(InputEvent::Pointer(PointerEvent::Wheel { delta: 1 })),
                    5 => out.push(InputEvent::Pointer(PointerEvent::Wheel { delta: -1 })),
                    _ => {}
                }
            }
            Event::ButtonRelease(e) => {
                let pos = self.desktop(e.event_x, e.event_y);
                let button = match e.detail {
                    1 => PointerButton::Left,
                    2 => PointerButton::Middle,
                    3 => PointerButton::Right,
                    _ => return,
                };
                out.push(InputEvent::Pointer(PointerEvent::Up { pos, button }));
            }
            Event::KeyPress(e) | Event::KeyRelease(e) => {
                let pressed = matches!(ev, Event::KeyPress(_));
                let (key, force_shift) = self.keymap.key(e.detail);
                if force_shift && pressed {
                    out.push(InputEvent::Key(KeyEvent { key: Key::Shift, pressed: true }));
                }
                out.push(InputEvent::Key(KeyEvent { key, pressed }));
                if force_shift && pressed {
                    out.push(InputEvent::Key(KeyEvent { key: Key::Shift, pressed: false }));
                }
            }
            _ => {}
        }
    }

    /// Sends `rect` (desktop pixels) of the shadow buffer to the window.
    fn put(&self, rect: Rect) -> Result<(), Failure> {
        let bounds = Rect::from_origin_size(self.origin, self.size);
        let Some(r) = rect.intersect(bounds) else { return Ok(()) };
        let stride = self.size.width as usize * 4;
        let row_bytes = r.width as usize * 4;
        let max = self.conn.maximum_request_bytes().saturating_sub(64).max(row_bytes);
        let rows_per = (max / row_bytes).clamp(1, usize::from(u16::MAX));
        let (lx, ly) = ((r.x - bounds.x) as usize, (r.y - bounds.y) as usize);
        let mut tmp = Vec::new();
        let mut row = 0usize;
        while row < r.height as usize {
            let n = rows_per.min(r.height as usize - row);
            let data: &[u8] = if r.width == self.size.width {
                &self.buf[(ly + row) * stride..(ly + row + n) * stride]
            } else {
                tmp.clear();
                for k in 0..n {
                    let o = (ly + row + k) * stride + lx * 4;
                    tmp.extend_from_slice(&self.buf[o..o + row_bytes]);
                }
                &tmp
            };
            self.conn
                .put_image(ImageFormat::Z_PIXMAP, self.win, self.gc, r.width as u16, n as u16, (lx as i32) as i16, (ly + row) as i16, 0, self.depth, data)
                .map_err(runtime)?;
            row += n;
        }
        Ok(())
    }

    fn draw(&mut self, app: &mut OverlayApp, rects: &[Rect]) -> Result<(), Failure> {
        for r in rects {
            {
                let mut t = TargetBuf { origin: self.origin, size: self.size, data: &mut self.buf };
                app.render(*r, &mut t);
            }
            self.put(*r)?;
        }
        Ok(())
    }
}

/// Runs the overlay on the X server named by `$DISPLAY`.
pub(super) fn run(app: &mut OverlayApp) -> Result<(), Failure> {
    let mut s = Session::connect(app)?;
    s.map_and_grab()?;

    // Where is the pointer right now? Seed the model so guides appear before the first move.
    if let Ok(reply) = s.conn.query_pointer(s.win).map_err(runtime)?.reply() {
        let p = s.desktop(reply.win_x, reply.win_y);
        app.handle(InputEvent::Modifiers(modifiers_from_mask(reply.mask.into())));
        app.handle(InputEvent::Pointer(PointerEvent::Move { pos: p }));
    }

    let mut first = true;
    let mut pending: Vec<InputEvent> = Vec::new();
    let mut need_frame = true;
    loop {
        if need_frame {
            let rects = app.begin_frame();
            s.draw(app, &rects)?;
            s.set_cursor(app.cursor_hint())?;
            s.conn.flush().map_err(runtime)?;
            if first {
                app.mark_first_frame();
                first = false;
            }
            need_frame = false;
        }
        if app.outcome().is_some() {
            return Ok(());
        }
        if let Some(d) = app.deadline()
            && Instant::now() >= d
        {
            app.cancel();
            return Ok(());
        }

        // Drain everything the server already sent; render once for the whole batch.
        let mut got = false;
        while let Some(ev) = s.conn.poll_for_event().map_err(runtime)? {
            got = true;
            match &ev {
                Event::Expose(e) => {
                    let r = Rect::new(
                        s.origin.x + i32::from(e.x),
                        s.origin.y + i32::from(e.y),
                        u32::from(e.width),
                        u32::from(e.height),
                    );
                    s.put(r)?;
                    need_frame = true;
                    s.conn.flush().map_err(runtime)?;
                }
                Event::DestroyNotify(_) | Event::UnmapNotify(_) => {
                    app.cancel();
                    return Ok(());
                }
                _ => {
                    pending.clear();
                    s.translate(&ev, &mut pending);
                    for p in pending.drain(..) {
                        app.handle(p);
                    }
                    need_frame = true;
                }
            }
            if app.outcome().is_some() {
                break;
            }
        }
        if got {
            continue;
        }

        // Block until the server says something or the deadline passes.
        let timeout = app
            .deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .map(|d| Timespec { tv_sec: d.as_secs() as i64, tv_nsec: i64::from(d.subsec_nanos()) });
        let mut fds = [PollFd::new(s.conn.stream(), PollFlags::IN)];
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(runtime(format!("poll failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms_map_to_keys() {
        assert_eq!(key_from_keysym(0xff1b), (Key::Escape, false));
        assert_eq!(key_from_keysym(0xff0d), (Key::Enter, false));
        assert_eq!(key_from_keysym(0xff8d), (Key::Enter, false));
        assert_eq!(key_from_keysym(0xfe20), (Key::Tab, true));
        assert_eq!(key_from_keysym(0x20), (Key::Space, false));
        assert_eq!(key_from_keysym(0xff51), (Key::Left, false));
        assert_eq!(key_from_keysym(0xffe2), (Key::Shift, false));
        assert_eq!(key_from_keysym(0xffe4), (Key::Control, false));
        assert_eq!(key_from_keysym(0xffe9), (Key::Alt, false));
        assert_eq!(key_from_keysym(0x43), (Key::Char('c'), false), "capital C");
        assert_eq!(key_from_keysym(0x63), (Key::Char('c'), false));
        assert_eq!(key_from_keysym(0xffbe), (Key::Other, false), "F1");
    }

    #[test]
    fn modifier_masks_decode() {
        let m = modifiers_from_mask(u16::from(KeyButMask::SHIFT | KeyButMask::MOD1));
        assert!(m.shift && m.alt && !m.ctrl);
        assert_eq!(modifiers_from_mask(0), Modifiers::default());
    }

    #[test]
    fn keymap_lookup_is_bounds_safe() {
        let km = Keymap { min: 8, per: 2, syms: vec![0xff1b, 0, 0x63, 0x43] };
        assert_eq!(km.key(8), (Key::Escape, false));
        assert_eq!(km.key(9), (Key::Char('c'), false));
        assert_eq!(km.key(3), (Key::Other, false), "below min");
        assert_eq!(km.key(200), (Key::Other, false), "above max");
    }
}
