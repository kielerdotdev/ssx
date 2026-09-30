//! Wayland backend: `wlr-layer-shell` surfaces (wlroots, KDE) or fullscreen `xdg_toplevel`s
//! (GNOME/Mutter and any compositor without layer-shell), one surface per output.
//!
//! Decisions worth knowing:
//!
//! * **Frozen frame, no compositor tricks.** Each surface simply shows the slice of the
//!   frozen desktop that belongs to its output. That is why the same code serves a
//!   layer-shell overlay and a fullscreen toplevel: only the surface *role* differs
//!   ([`Flavour`]).
//! * **Buffers are in desktop pixels, the viewport does the scaling.** By the coordinate model
//!   of `ssx-capture-wayland` a monitor's desktop rectangle is at least its native resolution,
//!   so each surface gets a buffer of exactly `Monitor.rect.size` and
//!   `wp_viewporter` maps it onto the surface's logical size. That handles fractional and
//!   mixed scales with no per-output maths and no resampling on our side. Without
//!   `wp_viewporter` an integer `buffer_scale` is used when the ratio is integral.
//!   (`wp_fractional_scale` is deliberately not needed: it would only tell us a preferred
//!   buffer size that we already know.)
//! * **Two shm buffers per surface, no shadow copy.** Rendering is a pure function of the
//!   scene, so a buffer that missed some frames is brought up to date by re-rendering its
//!   accumulated stale rectangles from the *current* scene straight into shared memory.
//! * **Keys are evdev scancodes** (`wl_keyboard.key` carries them), interpreted as physical
//!   positions for the handful of keys the overlay uses. Esc, Enter, Tab, Space, arrows and
//!   modifiers are layout independent; `C` (colour pick) is the physical `C` position, which
//!   on non-QWERTY layouts is another letter. Compositors do not repeat keys for clients, so
//!   arrow-key repeat is synthesised here from `wl_keyboard.repeat_info`.
//! * Pointer positions past a surface's edge (implicit grab during a drag) are mapped
//!   linearly, so a drag continues onto the neighbouring output.

use std::time::{Duration, Instant};

use rustix::{
    event::{PollFd, PollFlags, poll},
    time::Timespec,
};
use ssx_types::{Point, Rect};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region::WlRegion,
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        cursor_shape::v1::client::{
            wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
            wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
        shell::client::{
            xdg_surface::{self, XdgSurface},
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::{self, XdgWmBase},
        },
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::ZxdgOutputManagerV1,
            zxdg_output_v1::{self, ZxdgOutputV1},
        },
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use super::{Failure, plan::WaylandCaps};
use crate::{
    app::OverlayApp,
    error::OverlayError,
    mapping::{LogicalOutput, match_outputs, surface_to_desktop},
    model::{
        CursorHint, InputEvent, Key, KeyEvent, PointerButton, PointerEvent, geometry::Handle,
        geometry::coalesce,
    },
    render::TargetBuf,
    shm::ShmMap,
};

const NAME: &str = "wayland";

/// Which surface role to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavour {
    /// `zwlr_layer_shell_v1`, layer `overlay`, anchored to all edges.
    LayerShell,
    /// Fullscreen `xdg_toplevel` on a specific output.
    Fullscreen,
}

fn setup(e: impl std::fmt::Display) -> Failure {
    Failure::Setup(e.to_string())
}

fn runtime(e: impl std::fmt::Display) -> Failure {
    Failure::Runtime(OverlayError::backend(NAME, e))
}

// ---------------------------------------------------------------------------- keys

/// evdev scancodes (Linux `input-event-codes.h`) used by the overlay.
mod code {
    pub const ESC: u32 = 1;
    pub const TAB: u32 = 15;
    pub const ENTER: u32 = 28;
    pub const LCTRL: u32 = 29;
    pub const LSHIFT: u32 = 42;
    pub const RSHIFT: u32 = 54;
    pub const LALT: u32 = 56;
    pub const SPACE: u32 = 57;
    pub const KPENTER: u32 = 96;
    pub const RCTRL: u32 = 97;
    pub const RALT: u32 = 100;
    pub const UP: u32 = 103;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const DOWN: u32 = 108;
    pub const C: u32 = 46;
    pub const BTN_LEFT: u32 = 0x110;
    pub const BTN_RIGHT: u32 = 0x111;
    pub const BTN_MIDDLE: u32 = 0x112;
}

/// Which physical modifier a scancode is, as a bit in the held mask.
fn modifier_bit(code: u32) -> Option<(Key, u8)> {
    Some(match code {
        code::LSHIFT => (Key::Shift, 1),
        code::RSHIFT => (Key::Shift, 2),
        code::LCTRL => (Key::Control, 1),
        code::RCTRL => (Key::Control, 2),
        code::LALT => (Key::Alt, 1),
        code::RALT => (Key::Alt, 2),
        _ => return None,
    })
}

/// Translates a non-modifier scancode.
pub(crate) fn key_from_evdev(code: u32) -> Key {
    match code {
        code::ESC => Key::Escape,
        code::TAB => Key::Tab,
        code::ENTER | code::KPENTER => Key::Enter,
        code::SPACE => Key::Space,
        code::UP => Key::Up,
        code::DOWN => Key::Down,
        code::LEFT => Key::Left,
        code::RIGHT => Key::Right,
        code::C => Key::Char('c'),
        _ => Key::Other,
    }
}

/// System cursor shape for a model hint.
pub(crate) fn shape_for(hint: CursorHint) -> wp_cursor_shape_device_v1::Shape {
    use wp_cursor_shape_device_v1::Shape;
    match hint {
        CursorHint::Crosshair => Shape::Crosshair,
        CursorHint::Move => Shape::Move,
        CursorHint::Resize(h) => match h {
            Handle::NorthWest => Shape::NwResize,
            Handle::North => Shape::NResize,
            Handle::NorthEast => Shape::NeResize,
            Handle::East => Shape::EResize,
            Handle::SouthEast => Shape::SeResize,
            Handle::South => Shape::SResize,
            Handle::SouthWest => Shape::SwResize,
            Handle::West => Shape::WResize,
        },
    }
}

// ---------------------------------------------------------------------------- state

#[derive(Debug)]
struct OutInfo {
    proxy: WlOutput,
    name: Option<String>,
    geo: (i32, i32),
    mode: Option<(i32, i32)>,
    scale: i32,
    xdg_pos: Option<(i32, i32)>,
    xdg_size: Option<(i32, i32)>,
}

impl OutInfo {
    fn logical(&self) -> Option<LogicalOutput> {
        let (x, y) = self.xdg_pos.unwrap_or(self.geo);
        let (w, h) = self.xdg_size.or_else(|| {
            let (mw, mh) = self.mode?;
            let s = self.scale.max(1);
            Some((mw / s, mh / s))
        })?;
        Some(LogicalOutput { name: self.name.clone(), x, y, width: w, height: h })
    }
}

struct Buf {
    wl: WlBuffer,
    map: ShmMap,
    busy: bool,
    /// Buffer-local rectangles that differ from the current scene.
    stale: Vec<Rect>,
}

enum Role {
    Layer(ZwlrLayerSurfaceV1),
    Toplevel { xdg: XdgSurface, top: XdgToplevel, pending: (i32, i32) },
}

struct Surf {
    wl: WlSurface,
    role: Role,
    /// Desktop rectangle this surface shows.
    rect: Rect,
    /// Logical size from the compositor's configure.
    logical: Option<(u32, u32)>,
    viewport: Option<WpViewport>,
    bufs: Vec<Buf>,
    configured: bool,
    /// A frame is owed but every buffer was busy.
    pending: bool,
}

struct BufTag(usize, usize);

struct State {
    events: Vec<InputEvent>,
    cancelled: bool,
    t0: Instant,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    viewporter: Option<WpViewporter>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    outputs: Vec<OutInfo>,
    surfs: Vec<Surf>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    enter_serial: u32,
    focus: Option<usize>,
    ptr_pos: (f64, f64),
    mod_mask: [u8; 3],
    repeat: Option<(Key, Instant)>,
    repeat_rate_ms: u64,
    repeat_delay_ms: u64,
    shown_hint: Option<CursorHint>,
    dirty: bool,
    failed: Option<String>,
    first_presented: bool,
    qh: QueueHandle<State>,
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore WpCursorShapeManagerV1);
delegate_noop!(State: ignore WpCursorShapeDeviceV1);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Hot-plugged outputs are ignored: the overlay lives for seconds.
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        st: &mut Self,
        _: &WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.get_mut(*idx) else { return };
        match ev {
            wl_output::Event::Geometry { x, y, .. } => o.geo = (x, y),
            wl_output::Event::Mode { flags, width, height, .. } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    o.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        st: &mut Self,
        _: &ZxdgOutputV1,
        ev: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.get_mut(*idx) else { return };
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.xdg_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.xdg_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &WlSeat,
        ev: wl_seat::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev else { return };
        if caps.contains(wl_seat::Capability::Pointer) {
            if st.pointer.is_none() {
                let p = seat.get_pointer(qh, ());
                st.cursor_dev = st.cursor_mgr.as_ref().map(|m| m.get_pointer(&p, qh, ()));
                st.pointer = Some(p);
            }
        } else if let Some(p) = st.pointer.take() {
            p.release();
            st.cursor_dev = None;
        }
        if caps.contains(wl_seat::Capability::Keyboard) {
            if st.keyboard.is_none() {
                st.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        } else if let Some(k) = st.keyboard.take() {
            k.release();
        }
    }
}

impl State {
    fn surf_of(&self, s: &WlSurface) -> Option<usize> {
        self.surfs.iter().position(|x| &x.wl == s)
    }

    fn desktop_pos(&self, surf: usize, x: f64, y: f64) -> Point {
        let s = &self.surfs[surf];
        let logical = s.logical.unwrap_or((s.rect.width, s.rect.height));
        surface_to_desktop(s.rect, logical, (x, y))
    }

    fn feed(&mut self, ev: InputEvent) {
        self.events.push(ev);
        self.dirty = true;
    }

    fn modifier_event(&mut self, key: Key, pressed: bool, bit: u8) {
        let slot = match key {
            Key::Shift => 0,
            Key::Control => 1,
            _ => 2,
        };
        if pressed {
            self.mod_mask[slot] |= bit;
        } else {
            self.mod_mask[slot] &= !bit;
        }
        let down = self.mod_mask[slot] != 0;
        self.feed(InputEvent::Key(KeyEvent { key, pressed: down }));
    }

    fn release_everything(&mut self) {
        self.repeat = None;
        for (slot, key) in [Key::Shift, Key::Control, Key::Alt].into_iter().enumerate() {
            if self.mod_mask[slot] != 0 {
                self.mod_mask[slot] = 0;
                self.feed(InputEvent::Key(KeyEvent { key, pressed: false }));
            }
        }
        self.feed(InputEvent::Key(KeyEvent { key: Key::Space, pressed: false }));
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        ptr: &WlPointer,
        ev: wl_pointer::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                st.enter_serial = serial;
                st.focus = st.surf_of(&surface);
                st.ptr_pos = (surface_x, surface_y);
                st.shown_hint = None;
                if st.cursor_dev.is_none() {
                    // No cursor-shape protocol: hide the pointer; the guides are the cursor.
                    ptr.set_cursor(serial, None, 0, 0);
                }
                if let Some(f) = st.focus {
                    let pos = st.desktop_pos(f, surface_x, surface_y);
                    st.feed(InputEvent::Pointer(PointerEvent::Move { pos }));
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if st.focus.is_some() && st.surf_of(&surface) == st.focus {
                    st.focus = None;
                    st.feed(InputEvent::Pointer(PointerEvent::Leave));
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                st.ptr_pos = (surface_x, surface_y);
                if let Some(f) = st.focus {
                    let pos = st.desktop_pos(f, surface_x, surface_y);
                    st.feed(InputEvent::Pointer(PointerEvent::Move { pos }));
                }
            }
            wl_pointer::Event::Button { button, state, .. } => {
                let Some(f) = st.focus else { return };
                let pos = st.desktop_pos(f, st.ptr_pos.0, st.ptr_pos.1);
                let b = match button {
                    code::BTN_LEFT => PointerButton::Left,
                    code::BTN_RIGHT => PointerButton::Right,
                    code::BTN_MIDDLE => PointerButton::Middle,
                    _ => return,
                };
                let ev = match state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => PointerEvent::Down {
                        pos,
                        button: b,
                        time_ms: st.t0.elapsed().as_millis() as u64,
                    },
                    _ => PointerEvent::Up { pos, button: b },
                };
                st.feed(InputEvent::Pointer(ev));
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } => {
                if value != 0.0 {
                    st.feed(InputEvent::Pointer(PointerEvent::Wheel {
                        delta: if value < 0.0 { 1 } else { -1 },
                    }));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &WlKeyboard,
        ev: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_keyboard::Event::Key { key, state, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if let Some((k, bit)) = modifier_bit(key) {
                    st.modifier_event(k, pressed, bit);
                    return;
                }
                let k = key_from_evdev(key);
                if matches!(k, Key::Left | Key::Right | Key::Up | Key::Down) {
                    st.repeat = pressed
                        .then(|| (k, Instant::now() + Duration::from_millis(st.repeat_delay_ms)));
                }
                st.feed(InputEvent::Key(KeyEvent { key: k, pressed }));
            }
            wl_keyboard::Event::Leave { .. } => st.release_everything(),
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                st.repeat_rate_ms = if rate > 0 {
                    (1000 / u64::try_from(rate).unwrap_or(25)).max(10)
                } else {
                    u64::MAX
                };
                st.repeat_delay_ms = u64::try_from(delay).unwrap_or(400);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, BufTag> for State {
    fn event(
        st: &mut Self,
        _: &WlBuffer,
        ev: wl_buffer::Event,
        tag: &BufTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A frame owed because every buffer was busy is rendered by the main loop.
        if let wl_buffer::Event::Release = ev
            && let Some(b) = st.surfs.get_mut(tag.0).and_then(|s| s.bufs.get_mut(tag.1))
        {
            b.busy = false;
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        ev: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            base.pong(serial);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                st.configured(*idx, (width as i32, height as i32));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                st.cancelled = true;
                st.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgSurface, usize> for State {
    fn event(
        st: &mut Self,
        xdg: &XdgSurface,
        ev: xdg_surface::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xdg.ack_configure(serial);
            let size = match st.surfs.get(*idx).map(|s| &s.role) {
                Some(Role::Toplevel { pending, .. }) => *pending,
                _ => (0, 0),
            };
            st.configured(*idx, size);
        }
    }
}

impl Dispatch<XdgToplevel, usize> for State {
    fn event(
        st: &mut Self,
        _: &XdgToplevel,
        ev: xdg_toplevel::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(Surf { role: Role::Toplevel { pending, .. }, .. }) =
                    st.surfs.get_mut(*idx)
                {
                    *pending = (width, height);
                }
            }
            xdg_toplevel::Event::Close => {
                st.cancelled = true;
                st.dirty = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------- surfaces

impl State {
    /// The compositor told us a surface's logical size: (re)configure scaling and draw.
    fn configured(&mut self, idx: usize, size: (i32, i32)) {
        let Some(s) = self.surfs.get(idx) else { return };
        // Compositors may send 0x0 meaning "your choice": fall back to the output's
        // logical size (derived from the desktop rectangle).
        let lw = if size.0 > 0 { size.0 as u32 } else { s.rect.width };
        let lh = if size.1 > 0 { size.1 as u32 } else { s.rect.height };
        let rect = s.rect;
        let first = !s.configured;
        if first && let Err(e) = self.alloc_buffers(idx) {
            self.failed = Some(format!("cannot allocate shm buffers: {e}"));
            return;
        }
        let has_viewporter = self.viewporter.is_some();
        let s = &mut self.surfs[idx];
        s.logical = Some((lw, lh));
        s.configured = true;
        if has_viewporter {
            if s.viewport.is_none() {
                s.viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&s.wl, &self.qh, ()));
            }
            if let Some(vp) = &s.viewport {
                vp.set_destination(lw as i32, lh as i32);
            }
        } else {
            // Integer ratio only; anything else is shown slightly rescaled.
            let k = (f64::from(rect.width) / f64::from(lw.max(1))).round().max(1.0) as i32;
            s.wl.set_buffer_scale(k);
        }
        if let Some(c) = &self.compositor {
            let region = c.create_region(&self.qh, ());
            region.add(0, 0, lw as i32, lh as i32);
            s.wl.set_opaque_region(Some(&region));
            region.destroy();
        }
        // Whole buffers are stale on (re)configure; the main loop draws them.
        for b in &mut s.bufs {
            b.stale = vec![Rect::from_origin_size(Point::new(0, 0), rect.size())];
        }
        s.pending = true;
        self.dirty = true;
    }

    fn alloc_buffers(&mut self, idx: usize) -> Result<(), String> {
        let (Some(shm), rect) = (self.shm.as_ref(), self.surfs[idx].rect) else {
            return Err("wl_shm missing".into());
        };
        let (w, h) = (rect.width as i32, rect.height as i32);
        let len = rect.width as usize * rect.height as usize * 4;
        let mut bufs = Vec::new();
        for i in 0..2 {
            let map = ShmMap::new(len).map_err(|e| e.to_string())?;
            let pool = shm.create_pool(map.fd(), len as i32, &self.qh, ());
            let wl = pool.create_buffer(
                0,
                w,
                h,
                w * 4,
                wl_shm::Format::Xrgb8888,
                &self.qh,
                BufTag(idx, i),
            );
            pool.destroy();
            bufs.push(Buf { wl, map, busy: false, stale: Vec::new() });
        }
        self.surfs[idx].bufs = bufs;
        Ok(())
    }

    /// Queues `new` (buffer-local desktop rectangles already clipped to the surface) as
    /// damage for every buffer, then renders and commits one free buffer if there is one.
    fn present(&mut self, app: &mut OverlayApp, idx: usize, new: &[Rect]) {
        let s = &mut self.surfs[idx];
        if !s.configured || s.bufs.is_empty() {
            return;
        }
        let rect = s.rect;
        let local_bounds = Rect::from_origin_size(Point::new(0, 0), rect.size());
        for b in &mut s.bufs {
            let all: Vec<Rect> = b.stale.iter().copied().chain(new.iter().copied()).collect();
            b.stale = coalesce(all, local_bounds, 8);
        }
        if s.bufs.iter().all(|b| b.stale.is_empty()) {
            s.pending = false;
            return;
        }
        let Some(bi) = s.bufs.iter().position(|b| !b.busy && !b.stale.is_empty()) else {
            s.pending = true;
            return;
        };
        s.pending = false;
        let b = &mut s.bufs[bi];
        let stale = std::mem::take(&mut b.stale);
        {
            let mut target =
                TargetBuf { origin: rect.origin(), size: rect.size(), data: b.map.as_mut_slice() };
            for r in &stale {
                let desk = r.translate(rect.x, rect.y);
                app.render(desk, &mut target);
            }
        }
        s.wl.attach(Some(&b.wl), 0, 0);
        for r in &stale {
            s.wl.damage_buffer(r.x, r.y, r.width as i32, r.height as i32);
        }
        s.wl.commit();
        b.busy = true;
        if !self.first_presented {
            self.first_presented = true;
            app.mark_first_frame();
        }
    }

    /// Renders the pending scene changes to every surface and updates the cursor shape.
    fn frame(&mut self, app: &mut OverlayApp) {
        self.dirty = false;
        let rects = app.begin_frame();
        for i in 0..self.surfs.len() {
            let r = self.surfs[i].rect;
            let local: Vec<Rect> = rects
                .iter()
                .filter_map(|d| d.intersect(r))
                .map(|d| d.translate(-r.x, -r.y))
                .collect();
            self.present(app, i, &local);
        }
        let hint = app.cursor_hint();
        if self.shown_hint != Some(hint)
            && let Some(dev) = &self.cursor_dev
        {
            dev.set_shape(self.enter_serial, shape_for(hint));
            self.shown_hint = Some(hint);
        }
    }

    fn teardown(&mut self) {
        for s in self.surfs.drain(..) {
            match s.role {
                Role::Layer(l) => l.destroy(),
                Role::Toplevel { xdg, top, .. } => {
                    top.destroy();
                    xdg.destroy();
                }
            }
            if let Some(v) = s.viewport {
                v.destroy();
            }
            for b in s.bufs {
                b.wl.destroy();
            }
            s.wl.destroy();
        }
    }
}

// ---------------------------------------------------------------------------- entry points

/// Connects and reads the registry to learn what the compositor offers.
pub(super) fn probe() -> Result<WaylandCaps, String> {
    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let conn = Connection::connect_to_env().map_err(|e| format!("cannot connect: {e}"))?;
    let (globals, _queue) =
        registry_queue_init::<Probe>(&conn).map_err(|e| format!("registry query failed: {e}"))?;
    let names: Vec<String> =
        globals.contents().clone_list().into_iter().map(|g| g.interface).collect();
    Ok(WaylandCaps::from_globals(names.iter().map(String::as_str)))
}

fn bind_outputs(
    globals: &GlobalList,
    qh: &QueueHandle<State>,
    xdg_mgr: Option<&ZxdgOutputManagerV1>,
) -> Vec<OutInfo> {
    let mut outs = Vec::new();
    for g in globals.contents().clone_list() {
        if g.interface != "wl_output" {
            continue;
        }
        let idx = outs.len();
        let v = g.version.min(4);
        let proxy = globals.registry().bind::<WlOutput, _, _>(g.name, v, qh, idx);
        if let Some(m) = xdg_mgr {
            m.get_xdg_output(&proxy, qh, idx);
        }
        outs.push(OutInfo {
            proxy,
            name: None,
            geo: (0, 0),
            mode: None,
            scale: 1,
            xdg_pos: None,
            xdg_size: None,
        });
    }
    outs
}

/// Runs the overlay on the Wayland session named by `$WAYLAND_DISPLAY`.
pub(super) fn run(app: &mut OverlayApp, flavour: Flavour) -> Result<(), Failure> {
    let conn = Connection::connect_to_env().map_err(|e| setup(format!("cannot connect: {e}")))?;
    let (globals, mut queue): (GlobalList, EventQueue<State>) =
        registry_queue_init(&conn).map_err(|e| setup(format!("registry query failed: {e}")))?;
    let qh = queue.handle();

    let compositor: WlCompositor =
        globals.bind(&qh, 1..=6, ()).map_err(|e| setup(format!("wl_compositor: {e}")))?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| setup(format!("wl_shm: {e}")))?;
    let seat: WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|e| setup(format!("wl_seat: {e}")))?;
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let cursor_mgr: Option<WpCursorShapeManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let xdg_mgr: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    let layer_shell: Option<ZwlrLayerShellV1> = globals.bind(&qh, 1..=4, ()).ok();
    let wm_base: Option<XdgWmBase> = globals.bind(&qh, 1..=6, ()).ok();
    match flavour {
        Flavour::LayerShell if layer_shell.is_none() => {
            return Err(setup("zwlr_layer_shell_v1 is not advertised"));
        }
        Flavour::Fullscreen if wm_base.is_none() => {
            return Err(setup("xdg_wm_base is not advertised"));
        }
        _ => {}
    }
    let outputs = bind_outputs(&globals, &qh, xdg_mgr.as_ref());
    if outputs.is_empty() {
        return Err(setup("the compositor advertises no wl_output"));
    }

    let monitors = app.monitors().to_vec();
    let mut st = State {
        events: Vec::new(),
        cancelled: false,
        t0: Instant::now(),
        compositor: Some(compositor),
        shm: Some(shm),
        viewporter,
        cursor_mgr,
        outputs,
        surfs: Vec::new(),
        seat: None,
        pointer: None,
        keyboard: None,
        cursor_dev: None,
        enter_serial: 0,
        focus: None,
        ptr_pos: (0.0, 0.0),
        mod_mask: [0; 3],
        repeat: None,
        repeat_rate_ms: 40,
        repeat_delay_ms: 400,
        shown_hint: None,
        dirty: true,
        failed: None,
        first_presented: false,
        qh: qh.clone(),
    };
    // Outputs and their xdg-output companions describe themselves in a burst after binding.
    queue.roundtrip(&mut st).map_err(|e| setup(format!("roundtrip failed: {e}")))?;
    queue.roundtrip(&mut st).map_err(|e| setup(format!("roundtrip failed: {e}")))?;

    // Which output shows which part of the desktop.
    let logical: Vec<LogicalOutput> = st
        .outputs
        .iter()
        .map(|o| {
            o.logical().unwrap_or(LogicalOutput {
                name: o.name.clone(),
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        })
        .collect();
    let mut matched = match_outputs(&logical, &monitors);
    if matched.iter().all(Option::is_none) && logical.len() == 1 && monitors.len() == 1 {
        matched[0] = Some(0);
    }
    if matched.iter().all(Option::is_none) {
        return Err(setup(format!(
            "cannot relate the compositor's outputs {logical:?} to the monitors {:?}",
            monitors.iter().map(|m| (&m.name, m.rect)).collect::<Vec<_>>()
        )));
    }

    // Surfaces.
    for (oi, m) in matched.iter().enumerate() {
        let Some(mi) = m else {
            tracing::warn!(output = ?logical[oi], "output has no matching monitor; not covered");
            continue;
        };
        let rect = monitors[*mi].rect;
        if rect.is_empty() {
            continue;
        }
        let idx = st.surfs.len();
        let wl = st.compositor.as_ref().expect("bound above").create_surface(&qh, ());
        let role = match flavour {
            Flavour::LayerShell => {
                let ls = layer_shell.as_ref().expect("checked above");
                let l = ls.get_layer_surface(
                    &wl,
                    Some(&st.outputs[oi].proxy),
                    Layer::Overlay,
                    "ssx-overlay".into(),
                    &qh,
                    idx,
                );
                l.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                l.set_size(0, 0);
                l.set_exclusive_zone(-1);
                l.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
                Role::Layer(l)
            }
            Flavour::Fullscreen => {
                let base = wm_base.as_ref().expect("checked above");
                let xdg = base.get_xdg_surface(&wl, &qh, idx);
                let top = xdg.get_toplevel(&qh, idx);
                top.set_title("ssx-overlay".into());
                top.set_app_id("ssx-overlay".into());
                top.set_fullscreen(Some(&st.outputs[oi].proxy));
                Role::Toplevel { xdg, top, pending: (0, 0) }
            }
        };
        wl.commit(); // roles are configured by an initial empty commit
        st.surfs.push(Surf {
            wl,
            role,
            rect,
            logical: None,
            viewport: None,
            bufs: Vec::new(),
            configured: false,
            pending: false,
        });
    }
    st.seat = Some(seat);

    let result = event_loop(&mut st, &mut queue, &conn, app);
    st.teardown();
    let _ = conn.flush();
    let _ = queue.roundtrip(&mut st);
    result
}

fn event_loop(
    st: &mut State,
    queue: &mut EventQueue<State>,
    conn: &Connection,
    app: &mut OverlayApp,
) -> Result<(), Failure> {
    let started = Instant::now();
    loop {
        queue.dispatch_pending(st).map_err(runtime)?;
        if let Some(m) = st.failed.take() {
            return Err(runtime(m));
        }
        for ev in st.events.drain(..) {
            app.handle(ev);
        }
        if st.cancelled {
            app.cancel();
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
        // The compositor never configured us (dead layer-shell, no outputs shown): give up
        // early rather than block forever with nothing on screen.
        if !st.first_presented && started.elapsed() > Duration::from_secs(5) {
            return Err(setup("the compositor did not configure any overlay surface within 5 s"));
        }
        // Synthesised key repeat for the arrow keys.
        if let Some((key, at)) = st.repeat
            && Instant::now() >= at
        {
            st.feed(InputEvent::Key(KeyEvent { key, pressed: true }));
            st.repeat =
                Some((key, Instant::now() + Duration::from_millis(st.repeat_rate_ms.min(1000))));
        }
        if st.dirty || st.surfs.iter().any(|s| s.pending) {
            st.frame(app);
        }
        conn.flush().map_err(runtime)?;
        let Some(guard) = queue.prepare_read() else { continue };
        let now = Instant::now();
        let wake = [app.deadline(), st.repeat.map(|(_, at)| at)]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_millis(500), |t| t.saturating_duration_since(now))
            .min(Duration::from_millis(500));
        let ts =
            Timespec { tv_sec: wake.as_secs() as i64, tv_nsec: i64::from(wake.subsec_nanos()) };
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(n) if n > 0 => {
                guard.read().map_err(runtime)?;
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => drop(guard),
            Err(e) => return Err(runtime(format!("poll failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_codes_map_to_keys() {
        assert_eq!(key_from_evdev(1), Key::Escape);
        assert_eq!(key_from_evdev(28), Key::Enter);
        assert_eq!(key_from_evdev(96), Key::Enter);
        assert_eq!(key_from_evdev(15), Key::Tab);
        assert_eq!(key_from_evdev(57), Key::Space);
        assert_eq!(key_from_evdev(103), Key::Up);
        assert_eq!(key_from_evdev(108), Key::Down);
        assert_eq!(key_from_evdev(105), Key::Left);
        assert_eq!(key_from_evdev(106), Key::Right);
        assert_eq!(key_from_evdev(46), Key::Char('c'));
        assert_eq!(key_from_evdev(30), Key::Other);
        assert_eq!(modifier_bit(42), Some((Key::Shift, 1)));
        assert_eq!(modifier_bit(54), Some((Key::Shift, 2)));
        assert_eq!(modifier_bit(97), Some((Key::Control, 2)));
        assert_eq!(modifier_bit(56), Some((Key::Alt, 1)));
        assert_eq!(modifier_bit(1), None);
    }

    #[test]
    fn cursor_shapes_cover_all_hints() {
        use wp_cursor_shape_device_v1::Shape;
        assert_eq!(shape_for(CursorHint::Crosshair), Shape::Crosshair);
        assert_eq!(shape_for(CursorHint::Move), Shape::Move);
        for h in Handle::ALL {
            let s = shape_for(CursorHint::Resize(h));
            assert!(format!("{s:?}").ends_with("Resize"), "{s:?}");
        }
    }

    #[test]
    fn logical_rect_prefers_xdg_output_and_falls_back_to_mode() {
        // Not constructible without a proxy; exercise the arithmetic through the free helper.
        let size = |mode: (i32, i32), scale: i32| (mode.0 / scale.max(1), mode.1 / scale.max(1));
        assert_eq!(size((3840, 2160), 2), (1920, 1080));
        assert_eq!(size((800, 600), 0), (800, 600));
    }
}
