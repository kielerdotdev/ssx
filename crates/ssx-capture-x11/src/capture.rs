//! [`X11Capture`]: the [`CaptureBackend`] implementation.

use std::sync::{Arc, Mutex};

use ssx_capture::{Capabilities, CaptureBackend, CaptureError, CaptureOptions, Result};
use ssx_types::{Frame, Monitor, Point, Rect, WindowInfo};
use x11rb::protocol::{
    composite::ConnectionExt as _,
    xproto::{ConnectionExt as _, Window},
};

use crate::{
    config::{WindowCaptureMode, X11Config},
    cursor::blend_cursor,
    error::{X11Error, X11Result},
    grab::Area,
    monitors::X11Monitor,
    session::Session,
    windows::parse_window_id,
};

/// Which optional X features the connected server offers and this backend is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    /// Screenshots are read through MIT-SHM (false on remote displays, old servers, or
    /// when disabled in [`X11Config`]).
    pub shm: bool,
    /// XFixes is present, so the cursor can be captured.
    pub xfixes: bool,
    /// XComposite 0.2+ is present.
    pub composite: bool,
    /// A compositing manager owns `_NET_WM_CM_Sn` right now, so window capture can be
    /// occlusion-free.
    pub compositor_running: bool,
    /// RandR version `(major, minor)`, if present.
    pub randr: Option<(u32, u32)>,
}

/// Screen capture for X11 (and XWayland) through the X protocol.
///
/// # Coordinates
///
/// The virtual desktop is the root window: origin `(0, 0)`, size = the X screen. RandR
/// monitors are rectangles inside it; the gaps between them are part of the root and are
/// captured as whatever the server holds there (usually black). Only the screen named by
/// `DISPLAY` (`:0.1` selects screen 1) is captured.
///
/// # Pixels
///
/// Frames are [`ssx_types::PixelFormat::Bgra8`], always opaque, sRGB. Frames are
/// *physical* pixels; `scale_factor` is best-effort (see the crate docs).
///
/// # Windows
///
/// [`capture_window`](CaptureBackend::capture_window) has two modes (see
/// [`WindowCaptureMode`]). With a compositing manager running it reads the window's
/// off-screen `NameWindowPixmap`, which is unaffected by overlapping windows and includes
/// the WM frame. Without one X keeps no off-screen copy, so the window's rectangle is
/// cropped from the root window: whatever overlaps it on screen is captured too, and a
/// minimised or unmapped window cannot be captured at all.
///
/// # Robustness
///
/// The connection is re-established transparently if the X server restarted since the
/// last call. A capture that hits a dead connection returns an error instead of hanging.
#[derive(Debug)]
pub struct X11Capture {
    config: X11Config,
    session: Mutex<Option<Arc<Session>>>,
}

impl X11Capture {
    /// Connects using `$DISPLAY` and default options.
    pub fn connect() -> Result<Self> {
        Self::with_config(X11Config::default())
    }

    /// Connects with explicit options. Fails early (with
    /// [`CaptureError::NoBackend`]) when no server is reachable, so callers can fall
    /// back to another backend.
    pub fn with_config(config: X11Config) -> Result<Self> {
        let session = Session::connect(&config)?;
        Ok(Self { config, session: Mutex::new(Some(Arc::new(session))) })
    }

    /// Optional features of the current connection.
    pub fn features(&self) -> Result<Features> {
        self.with_session(|s| {
            Ok(Features {
                shm: s.shm_available(),
                xfixes: s.ext.xfixes,
                composite: s.ext.composite,
                compositor_running: s.compositor_running(),
                randr: s.ext.randr,
            })
        })
    }

    /// Monitors with rotation, outputs and physical size in addition to
    /// [`CaptureBackend::monitors`].
    pub fn monitor_details(&self) -> Result<Vec<X11Monitor>> {
        self.with_session(Session::monitor_details)
    }

    /// Runs `f` on a live session, reconnecting once if a *previously used* session turns
    /// out to be dead (server restarted).
    fn with_session<T>(&self, f: impl Fn(&Session) -> X11Result<T>) -> Result<T> {
        let (session, reused) = {
            let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
            match guard.as_ref() {
                Some(s) => (Arc::clone(s), true),
                None => {
                    let s = Arc::new(Session::connect(&self.config)?);
                    *guard = Some(Arc::clone(&s));
                    (s, false)
                }
            }
        };
        match f(&session) {
            Err(e) if e.is_fatal() => {
                self.forget(&session);
                tracing::warn!(error = %e, reused, "X connection failed");
                if !reused {
                    return Err(e.into());
                }
                let fresh = match Session::connect(&self.config) {
                    Ok(s) => Arc::new(s),
                    // Report the original failure: it explains what broke.
                    Err(_) => return Err(e.into()),
                };
                *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&fresh));
                match f(&fresh) {
                    Err(e2) => {
                        if e2.is_fatal() {
                            self.forget(&fresh);
                        }
                        Err(e2.into())
                    }
                    Ok(v) => Ok(v),
                }
            }
            other => other.map_err(Into::into),
        }
    }

    fn forget(&self, dead: &Arc<Session>) {
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, dead)) {
            *guard = None;
        }
    }
}

/// Grabs `area` of the root window and adds the cursor if requested.
fn grab_root(s: &Session, area: Area, opts: &CaptureOptions, scale: f64) -> X11Result<Frame> {
    let mut frame =
        s.grab(s.root, s.root_depth, s.root_visual, area, Point::new(area.x, area.y))?;
    frame.scale_factor = scale;
    if opts.include_cursor {
        add_cursor(s, &mut frame);
    }
    Ok(frame)
}

/// The cursor is decoration: failing to fetch it must not fail the screenshot.
fn add_cursor(s: &Session, frame: &mut Frame) {
    match s.cursor_image() {
        Ok(Some(cursor)) => blend_cursor(frame, &cursor),
        Ok(None) => tracing::debug!("XFixes unavailable; capturing without cursor"),
        Err(e) => tracing::debug!(error = %e, "could not fetch cursor image"),
    }
}

impl CaptureBackend for X11Capture {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            enumerate_monitors: true,
            enumerate_windows: true,
            capture_windows: true,
            cursor: true,
            hdr_float: false,
            native_desktop: true,
            needs_user_interaction: false,
        }
    }

    fn monitors(&self) -> Result<Vec<Monitor>> {
        Ok(self.monitor_details()?.into_iter().map(|m| m.monitor).collect())
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.with_session(Session::window_list)
    }

    fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        let monitor = self
            .monitor_details()?
            .into_iter()
            .map(|m| m.monitor)
            .find(|m| m.id == monitor_id)
            .ok_or_else(|| CaptureError::NotFound(monitor_id.to_owned()))?;
        self.with_session(|s| {
            let (w, h) = s.root_size()?;
            let Some(r) = monitor.rect.intersect(Rect::new(0, 0, w, h)) else {
                return Err(X11Error::Malformed("monitor lies outside the screen"));
            };
            let area = Area { x: r.x, y: r.y, width: r.width, height: r.height };
            grab_root(s, area, opts, monitor.scale_factor)
        })
    }

    fn capture_desktop(&self, opts: &CaptureOptions) -> Result<Frame> {
        self.with_session(|s| {
            let (w, h) = s.root_size()?;
            let area = Area { x: 0, y: 0, width: w, height: h };
            grab_root(s, area, opts, s.scale_factor())
        })
    }

    /// Captures `region`, **clipped to the screen**: the returned frame's `rect()` is the
    /// intersection of `region` and the root window (X rejects reads outside the drawable,
    /// so a selection dragged past the edge shrinks instead of failing).
    fn capture_region(&self, region: Rect, opts: &CaptureOptions) -> Result<Frame> {
        if region.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        let frame = self.with_session(|s| {
            let (w, h) = s.root_size()?;
            let Some(r) = region.intersect(Rect::new(0, 0, w, h)) else {
                return Ok(None);
            };
            let area = Area { x: r.x, y: r.y, width: r.width, height: r.height };
            grab_root(s, area, opts, s.scale_factor()).map(Some)
        })?;
        frame.ok_or(CaptureError::InvalidRegion(region))
    }

    fn capture_window(&self, window_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        let win: Window = parse_window_id(window_id)?;
        self.with_session(|s| capture_window(s, win, opts))
    }
}

fn capture_window(s: &Session, win: Window, opts: &CaptureOptions) -> X11Result<Frame> {
    let info = s.window_info(win, None)?.ok_or(X11Error::NoSuchWindow(win))?;
    if info.minimized || !s.is_viewable(win)? {
        return Err(X11Error::NotViewable(win));
    }
    let scale = s.scale_factor();
    let use_composite = match s.config.window_capture {
        WindowCaptureMode::Root => false,
        WindowCaptureMode::Auto => s.ext.composite && s.compositor_running(),
        WindowCaptureMode::Composite => true,
    };
    if use_composite {
        match capture_window_pixmap(s, win) {
            Ok(mut frame) => {
                frame.scale_factor = scale;
                if opts.include_cursor {
                    add_cursor(s, &mut frame);
                }
                return Ok(frame);
            }
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) if s.config.window_capture == WindowCaptureMode::Composite => return Err(e),
            Err(e) if e.is_gone() => return Err(X11Error::NoSuchWindow(win)),
            Err(e) => {
                tracing::debug!(error = %e, "XComposite window capture failed; cropping from the root window");
            }
        }
    }
    // Root crop, clipped: a window dragged partly off-screen would otherwise BadMatch.
    let (w, h) = s.root_size()?;
    let Some(r) = info.rect.intersect(Rect::new(0, 0, w, h)) else {
        return Err(X11Error::NotViewable(win));
    };
    let area = Area { x: r.x, y: r.y, width: r.width, height: r.height };
    grab_root(s, area, opts, scale)
}

/// Reads the window's redirected off-screen pixmap: the WM frame plus client, at its
/// full size, even when off-screen or covered.
fn capture_window_pixmap(s: &Session, win: Window) -> X11Result<Frame> {
    if !s.ext.composite {
        return Err(X11Error::Connection("the X server has no XComposite extension".into()));
    }
    let top = s.top_level_of(win)?;
    let attrs = s
        .conn
        .get_window_attributes(top)?
        .reply()
        .map_err(|e| X11Error::from_reply("GetWindowAttributes", e))?;
    let geo =
        s.conn.get_geometry(top)?.reply().map_err(|e| X11Error::from_reply("GetGeometry", e))?;

    let pixmap = x11rb::connection::Connection::generate_id(&s.conn)?;
    s.conn
        .composite_name_window_pixmap(top, pixmap)?
        .check()
        .map_err(|e| X11Error::from_reply("NameWindowPixmap", e))?;
    let bw = u32::from(geo.border_width) * 2;
    let area =
        Area { x: 0, y: 0, width: u32::from(geo.width) + bw, height: u32::from(geo.height) + bw };
    let result = s.grab(
        pixmap,
        geo.depth,
        attrs.visual,
        area,
        Point::new(i32::from(geo.x), i32::from(geo.y)),
    );
    // Always release the pixmap, even when the read failed.
    let _ = s.conn.free_pixmap(pixmap);
    result
}
