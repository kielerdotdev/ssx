//! [`WaylandCapture`]: the [`CaptureBackend`] implementation.
//!
//! Each trait method opens a fresh [`Session`] (see `wl.rs` for why), builds the
//! coordinate [`Layout`] from it, and captures from that one consistent snapshot.

use std::time::Duration;

use ssx_capture::{Capabilities, CaptureBackend, CaptureError, CaptureOptions, Result, blit};
use ssx_types::{ColorSpace, Frame, Monitor, PixelFormat, Point, Rect, Size, WindowInfo};

use crate::{
    capture::{RawShot, ext_capture, wlr_capture},
    coords::{Layout, MonitorGeom, Part},
    ipc::{self, Ipc, WindowSource},
    resample::resample,
    transform::Transform,
    wl::{Session, Target},
};

/// Which screen-copy protocol a [`WaylandCapture`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// `ext-image-copy-capture-v1`.
    ExtImageCopyCapture,
    /// `wlr-screencopy-unstable-v1`.
    WlrScreencopy,
}

impl Protocol {
    fn backend_name(self) -> &'static str {
        match self {
            Protocol::ExtImageCopyCapture => "wayland-ext-image-copy-capture",
            Protocol::WlrScreencopy => "wayland-wlr-screencopy",
        }
    }
}

/// Construction options for [`WaylandCapture`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Upper bound for every wait on the compositor (connect, each roundtrip, each frame
    /// copy). Default 5 s. A capture never blocks longer than this per step.
    pub timeout: Duration,
    /// Force a protocol instead of picking the best advertised one.
    pub protocol: Option<Protocol>,
    /// Which compositor socket to talk to (default: `$WAYLAND_DISPLAY`).
    pub target: Target,
    /// Window-enumeration IPC (default: detect sway / Hyprland from the environment).
    pub ipc: Ipc,
    /// Override the desktop scale `S` (default: the largest monitor scale). See
    /// `docs/wayland-coordinates.md`. `Some(1.0)` yields the raw logical layout.
    pub desktop_scale: Option<f64>,
    /// Capture windows through `ext-foreign-toplevel-image-capture-source` when the
    /// compositor offers it (no occlusion, no decorations). Falls back to cropping the
    /// screen on any failure.
    pub toplevel_capture: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            protocol: None,
            target: Target::Env,
            ipc: Ipc::Auto,
            desktop_scale: None,
            toplevel_capture: true,
        }
    }
}

/// Full description of an output, beyond what [`Monitor`] carries.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Connector name (`DP-1`, `HEADLESS-1`); also the [`Monitor::id`].
    pub id: String,
    /// The compositor's description string, if it sent one.
    pub description: Option<String>,
    pub make: String,
    pub model: String,
    /// Rectangle on the virtual desktop (desktop pixels, see the coordinate docs).
    pub rect: Rect,
    /// Position and size in the compositor's logical layout.
    pub logical: Rect,
    /// Current mode as scanned out (before `transform`).
    pub mode_size: Size,
    /// Native pixel size of the image as displayed (transform applied).
    pub native_size: Size,
    /// This output's own effective scale (1.0, 1.25, 1.5, 2.0, ...).
    pub scale_factor: f64,
    /// The integer `wl_output.scale`.
    pub integer_scale: i32,
    pub transform: Transform,
    pub refresh_hz: Option<f32>,
    pub physical_size_mm: (i32, i32),
}

/// Wayland screen capture via `ext-image-copy-capture-v1` or `wlr-screencopy-v1`.
#[derive(Debug, Clone)]
pub struct WaylandCapture {
    cfg: Config,
    protocol: Protocol,
    windows: Option<WindowSource>,
    ext_toplevel: bool,
}

fn no_backend_message() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let hint = if desktop.to_ascii_lowercase().contains("gnome")
        || desktop.to_ascii_lowercase().contains("kde")
    {
        format!(" ({desktop} does not let ordinary clients capture the screen)")
    } else {
        String::new()
    };
    format!(
        "the compositor advertises neither ext-image-copy-capture-v1 nor \
         wlr-screencopy-unstable-v1{hint}; use the xdg-desktop-portal backend \
         (ssx-capture-portal) on this desktop"
    )
}

impl WaylandCapture {
    /// Connects to the compositor named by the environment and picks the best protocol.
    ///
    /// Fails with [`CaptureError::NoBackend`] when there is no compositor or it offers no
    /// screen-copy protocol (GNOME, KDE): use the portal backend there.
    pub fn detect() -> Result<Self> {
        Self::with_config(Config::default())
    }

    /// Like [`detect`](Self::detect) with explicit options.
    pub fn with_config(cfg: Config) -> Result<Self> {
        let session = Session::open(&cfg.target, cfg.timeout)?;
        let st = &session.state;
        let has_shm = st.shm.is_some();
        let ext_ok =
            has_shm && st.ext_copy_manager.is_some() && st.ext_output_source_manager.is_some();
        let wlr_ok = has_shm && st.wlr_manager.is_some();
        // Arm order encodes the priority: ext first, then wlr, when nothing is forced.
        let protocol = match (cfg.protocol, ext_ok, wlr_ok) {
            (Some(Protocol::ExtImageCopyCapture) | None, true, _) => Protocol::ExtImageCopyCapture,
            (Some(Protocol::WlrScreencopy) | None, _, true) => Protocol::WlrScreencopy,
            (Some(p), _, _) => {
                return Err(CaptureError::NoBackend(format!(
                    "{p:?} was requested but the compositor does not advertise it"
                )));
            }
            (None, false, false) => return Err(CaptureError::NoBackend(no_backend_message())),
        };
        let ext_toplevel = protocol == Protocol::ExtImageCopyCapture
            && st.ext_toplevel_source_manager.is_some()
            && st.has_global("ext_foreign_toplevel_list_v1");
        let windows = ipc::detect_from_env(&cfg.ipc);
        tracing::debug!(
            ?protocol,
            ipc = windows.as_ref().map(WindowSource::name),
            ext_toplevel,
            "wayland capture backend selected"
        );
        Ok(Self { cfg, protocol, windows, ext_toplevel })
    }

    /// The protocol in use.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Detailed output information (make/model, transform, integer + fractional scale...).
    pub fn outputs(&self) -> Result<Vec<OutputInfo>> {
        let s = self.open()?;
        let layout = self.layout(&s);
        Ok(layout
            .monitors
            .iter()
            .filter_map(|m| {
                let rec = s.state.outputs.iter().find(|o| o.id() == m.id)?;
                let (mw, mh, refresh) = rec.mode?;
                Some(OutputInfo {
                    id: m.id.clone(),
                    description: rec.description.clone(),
                    make: rec.make.clone(),
                    model: rec.model.clone(),
                    rect: m.rect,
                    logical: m.logical,
                    mode_size: Size::new(mw, mh),
                    native_size: m.native,
                    scale_factor: m.scale,
                    integer_scale: m.int_scale,
                    transform: m.transform,
                    refresh_hz: (refresh > 0).then(|| refresh as f32 / 1000.0),
                    physical_size_mm: rec.phys_mm,
                })
            })
            .collect())
    }

    fn open(&self) -> Result<Session> {
        Session::open(&self.cfg.target, self.cfg.timeout)
    }

    fn layout(&self, s: &Session) -> Layout {
        Layout::build(&s.geoms(), self.cfg.desktop_scale)
    }

    fn err(&self, msg: impl ToString) -> CaptureError {
        CaptureError::backend(self.name(), msg)
    }

    /// One whole output, upright, at native resolution.
    fn shoot_output(&self, s: &mut Session, m: &MonitorGeom, cursor: bool) -> Result<RawShot> {
        let (wl, transform) = s
            .state
            .outputs
            .iter()
            .find(|o| o.id() == m.id)
            .map(|o| (o.wl.clone(), o.transform))
            .ok_or_else(|| CaptureError::NotFound(m.id.clone()))?;
        match self.protocol {
            Protocol::WlrScreencopy => wlr_capture(s, &wl, transform, cursor, None),
            Protocol::ExtImageCopyCapture => {
                let mgr = s
                    .state
                    .ext_output_source_manager
                    .clone()
                    .ok_or_else(|| self.err("output image capture source manager vanished"))?;
                let source = mgr.create_source(&wl, &s.qh, ());
                let shot = ext_capture(s, &source, cursor, Transform::Normal);
                source.destroy();
                shot
            }
        }
    }

    /// Captures the `part.overlap` rectangle of one monitor at desktop resolution.
    fn capture_part(
        &self,
        s: &mut Session,
        layout: &Layout,
        part: &Part,
        cursor: bool,
    ) -> Result<Frame> {
        let m = &layout.monitors[part.monitor];
        let scale = layout.desktop_scale;

        // Cheap path: ask the compositor for just the region (wlr only; ext has no region).
        if let (Some(d), Protocol::WlrScreencopy) = (part.direct, self.protocol) {
            let (wl, transform) = s
                .state
                .outputs
                .iter()
                .find(|o| o.id() == m.id)
                .map(|o| (o.wl.clone(), o.transform))
                .ok_or_else(|| CaptureError::NotFound(m.id.clone()))?;
            let shot = wlr_capture(s, &wl, transform, cursor, Some(d.logical))?;
            let k = m.scale.round() as u32;
            if shot.width == d.logical.width * k && shot.height == d.logical.height * k {
                let f = Self::frame_from(shot, part.overlap.origin(), scale)?;
                return f
                    .crop(d.crop)
                    .map(|mut c| {
                        c.origin = part.overlap.origin();
                        c
                    })
                    .map_err(Into::into);
            }
            tracing::debug!(
                got = ?(shot.width, shot.height),
                "region capture returned an unexpected size, capturing the whole output"
            );
        }

        let shot = self.shoot_output(s, m, cursor)?;
        let target = (m.rect.width, m.rect.height);
        let shot = if (shot.width, shot.height) == target {
            shot
        } else {
            RawShot {
                bgra: resample(shot.bgra, (shot.width, shot.height), target),
                width: target.0,
                height: target.1,
            }
        };
        let full = Self::frame_from(shot, m.rect.origin(), scale)?;
        if part.overlap == m.rect { Ok(full) } else { Ok(full.crop_desktop(part.overlap)?) }
    }

    fn frame_from(shot: RawShot, origin: Point, scale: f64) -> Result<Frame> {
        let mut f = Frame::from_raw(
            Size::new(shot.width, shot.height),
            shot.width as usize * 4,
            PixelFormat::Bgra8,
            ColorSpace::Srgb,
            shot.bgra,
        )?;
        f.origin = origin;
        f.scale_factor = scale;
        Ok(f)
    }

    /// Stitches every monitor part of `region` into one frame.
    fn capture_region_in(
        &self,
        s: &mut Session,
        layout: &Layout,
        region: Rect,
        cursor: bool,
    ) -> Result<Frame> {
        if region.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        let parts = layout.plan(region);
        if parts.is_empty() {
            return Err(CaptureError::InvalidRegion(region));
        }
        if let [only] = parts.as_slice()
            && only.overlap == region
        {
            return self.capture_part(s, layout, only, cursor);
        }
        // Gaps between monitors and parts outside them stay transparent black.
        let mut canvas = Frame::new(region.size(), PixelFormat::Bgra8, ColorSpace::Srgb);
        canvas.origin = region.origin();
        canvas.scale_factor = layout.desktop_scale;
        for part in &parts {
            let f = self.capture_part(s, layout, part, cursor)?;
            blit(&mut canvas, &f)?;
        }
        Ok(canvas)
    }

    fn list_windows(&self, layout: &Layout) -> Result<Vec<WindowInfo>> {
        let src = self.windows.as_ref().ok_or_else(|| {
            CaptureError::unsupported(self.name(), "window enumeration (no sway or Hyprland IPC)")
        })?;
        ipc::list(src, layout, self.cfg.timeout)
    }

    /// Captures one window through the compositor's toplevel capture source, or `None`
    /// if that is unavailable or the window cannot be matched to a toplevel unambiguously.
    fn capture_toplevel(
        &self,
        s: &mut Session,
        win: &WindowInfo,
        layout: &Layout,
        cursor: bool,
    ) -> Option<Frame> {
        if !(self.cfg.toplevel_capture && self.ext_toplevel) {
            return None;
        }
        s.load_toplevels().ok()?;
        let mut matches = s.state.toplevels.iter().filter(|t| {
            t.done
                && !t.closed
                && t.title == win.title
                && win.app_name.as_deref().is_none_or(|a| a == t.app_id)
        });
        let handle = matches.next()?.handle.clone();
        if matches.next().is_some() {
            tracing::debug!(title = %win.title, "several toplevels match; cropping the screen instead");
            return None;
        }
        let mgr = s.state.ext_toplevel_source_manager.clone()?;
        let source = mgr.create_source(&handle, &s.qh, ());
        let shot = ext_capture(s, &source, cursor, Transform::Normal);
        source.destroy();
        match shot.and_then(|sh| Self::frame_from(sh, win.rect.origin(), layout.desktop_scale)) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::debug!(error = %e, "toplevel capture failed; cropping the screen instead");
                None
            }
        }
    }
}

impl CaptureBackend for WaylandCapture {
    fn name(&self) -> &'static str {
        self.protocol.backend_name()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            enumerate_monitors: true,
            enumerate_windows: self.windows.is_some(),
            capture_windows: self.windows.is_some(),
            cursor: true,
            hdr_float: false,
            native_desktop: false,
            needs_user_interaction: false,
        }
    }

    fn monitors(&self) -> Result<Vec<Monitor>> {
        let s = self.open()?;
        let layout = self.layout(&s);
        let primary = layout.primary();
        Ok(layout
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let refresh = s
                    .state
                    .outputs
                    .iter()
                    .find(|o| o.id() == m.id)
                    .and_then(|o| o.mode)
                    .map(|(_, _, r)| r)
                    .filter(|r| *r > 0)
                    .map(|r| r as f32 / 1000.0);
                Monitor {
                    id: m.id.clone(),
                    name: m.id.clone(),
                    rect: m.rect,
                    scale_factor: m.scale,
                    primary: Some(i) == primary,
                    refresh_hz: refresh,
                    hdr: None,
                }
            })
            .collect())
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        let s = self.open()?;
        self.list_windows(&self.layout(&s))
    }

    fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        let mut s = self.open()?;
        let layout = self.layout(&s);
        let idx = layout
            .monitors
            .iter()
            .position(|m| m.id == monitor_id)
            .ok_or_else(|| CaptureError::NotFound(monitor_id.to_owned()))?;
        let part = Part { monitor: idx, overlap: layout.monitors[idx].rect, direct: None };
        self.capture_part(&mut s, &layout, &part, opts.include_cursor)
    }

    /// Stitches all monitors (the protocols have no whole-desktop capture).
    fn capture_desktop(&self, opts: &CaptureOptions) -> Result<Frame> {
        let mut s = self.open()?;
        let layout = self.layout(&s);
        let bounds =
            layout.bounds().ok_or_else(|| self.err("the compositor has no enabled outputs"))?;
        self.capture_region_in(&mut s, &layout, bounds, opts.include_cursor)
    }

    fn capture_window(&self, window_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        let mut s = self.open()?;
        let layout = self.layout(&s);
        let win = self
            .list_windows(&layout)?
            .into_iter()
            .find(|w| w.id == window_id)
            .ok_or_else(|| CaptureError::NotFound(window_id.to_owned()))?;
        if win.minimized {
            return Err(self.err(format!(
                "window {window_id:?} is not on screen (inactive workspace or hidden scratchpad)"
            )));
        }
        if let Some(f) = self.capture_toplevel(&mut s, &win, &layout, opts.include_cursor) {
            return Ok(f);
        }
        // Screen crop: whatever is on top of the window appears in the shot (documented).
        let bounds = layout.bounds().ok_or_else(|| self.err("no enabled outputs"))?;
        let region = win.rect.intersect(bounds).ok_or(CaptureError::InvalidRegion(win.rect))?;
        self.capture_region_in(&mut s, &layout, region, opts.include_cursor)
    }

    fn capture_region(&self, region: Rect, opts: &CaptureOptions) -> Result<Frame> {
        let mut s = self.open()?;
        let layout = self.layout(&s);
        self.capture_region_in(&mut s, &layout, region, opts.include_cursor)
    }
}
