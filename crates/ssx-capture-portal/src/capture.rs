//! [`PortalCapture`]: the capture backend for GNOME (Mutter) and KDE Plasma (KWin) on
//! Wayland, choosing between two strategies at runtime.
//!
//! # Strategies
//!
//! | | KWin `ScreenShot2` | xdg-desktop-portal `Screenshot` |
//! |---|---|---|
//! | Desktops | KDE Plasma 5.27+ / 6 | GNOME, KDE, any portal-capable desktop |
//! | Prompt | none, if authorised (see [`crate::kwin_desktop_entry`]) | GNOME may ask once |
//! | Cursor | `include_cursor` honoured | not controllable |
//! | Unit of capture | one monitor, the workspace, an area, a window | the whole desktop (or the user's selection when interactive) |
//!
//! The portal answers with a single image of the whole desktop and no monitor list, so
//! per-monitor capture there is "capture the desktop, crop to the monitor"; the monitor
//! rectangles come from sources that need no permission (Mutter's `DisplayConfig`, then
//! Wayland `xdg-output`). If neither is available, monitors cannot be enumerated and only
//! [`CaptureBackend::capture_desktop`] (or the synthetic monitor
//! [`DESKTOP_MONITOR_ID`], which is the whole image) works.
//!
//! # Windows
//!
//! Window enumeration is **not** offered: Wayland deliberately gives clients no list of
//! other applications' windows. The interactive portal (which shows the desktop's own
//! picker) is the supported way to capture a window on GNOME. This is not faked.

use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

use ssx_capture::{Capabilities, CaptureBackend, CaptureError, CaptureOptions, Result};
use ssx_types::{Frame, Monitor, Rect};
use zbus::Connection;

use crate::{
    bus,
    kwin::{self, InteractiveKind, Target},
    layout::Layout,
    mutter, portal, wl_output,
};

/// Id of the synthetic monitor that covers the whole desktop image. Accepted by
/// [`CaptureBackend::capture_monitor`] on every strategy; it is the only monitor id that
/// works when no monitor layout source is available.
pub const DESKTOP_MONITOR_ID: &str = "desktop";

/// How screenshots are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// KDE's `org.kde.KWin.ScreenShot2`: silent, per-monitor, cursor option.
    KWin,
    /// `org.freedesktop.portal.Screenshot`: the universal route (GNOME's only one).
    Portal,
}

impl Strategy {
    const fn as_u8(self) -> u8 {
        match self {
            Strategy::KWin => 0,
            Strategy::Portal => 1,
        }
    }

    const fn from_u8(v: u8) -> Self {
        if v == 0 { Strategy::KWin } else { Strategy::Portal }
    }
}

/// Tunables. `Default` is right for a normal desktop session.
#[derive(Debug, Clone)]
pub struct PortalConfig {
    /// Session bus address to use instead of `DBUS_SESSION_BUS_ADDRESS` (tests, unusual
    /// setups).
    pub bus_address: Option<String>,
    /// Force a strategy instead of picking KWin when present and the portal otherwise.
    /// Detection fails with [`CaptureError::NoBackend`] if the forced one is unavailable.
    pub strategy: Option<Strategy>,
    /// Deadline for non-interactive captures and probes. Default 15 s.
    pub timeout: Duration,
    /// Deadline for interactive captures, which wait for a human. Default 60 s.
    pub interactive_timeout: Duration,
    /// Largest decoded screenshot accepted, in bytes (RGBA for the portal, raw pixel bytes
    /// for KWin). Default 1 GiB, about 16k x 16k.
    pub max_image_bytes: u64,
    /// Delete the file the portal writes after reading it. Default `true`.
    pub delete_portal_file: bool,
    /// When KWin refuses ([`CaptureError::PermissionDenied`]) or disappears, switch to
    /// the portal instead of failing. Default `true`. Disable it to surface the error
    /// (and the instructions for authorising the app) to the user.
    pub kwin_fallback_to_portal: bool,
    /// Also query Wayland `wl_output`/`xdg-output` for the monitor layout (needs
    /// `WAYLAND_DISPLAY`). Default `true`.
    pub wayland_outputs: bool,
    /// Connect to this Wayland socket instead of the one `WAYLAND_DISPLAY` names.
    pub wayland_socket: Option<std::path::PathBuf>,
    /// Deadline for the monitor layout query. Default 3 s.
    pub layout_timeout: Duration,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            bus_address: None,
            strategy: None,
            timeout: Duration::from_secs(15),
            interactive_timeout: Duration::from_secs(60),
            max_image_bytes: 1 << 30,
            delete_portal_file: true,
            kwin_fallback_to_portal: true,
            wayland_outputs: true,
            wayland_socket: None,
            layout_timeout: Duration::from_secs(3),
        }
    }
}

/// Wayland capture through KWin's `ScreenShot2` or the xdg-desktop-portal.
///
/// Create with [`PortalCapture::detect`]. The type is `Send + Sync`; every capture uses
/// its own D-Bus connection, so calls may run concurrently (note that on GNOME two
/// simultaneous portal requests each open their own dialog).
#[derive(Debug)]
pub struct PortalCapture {
    config: PortalConfig,
    active: AtomicU8,
    /// `ScreenShot2` interface version, when the service is on the bus.
    kwin_version: Option<u32>,
    portal_available: bool,
    /// Whether a monitor layout source answered at detection time.
    layout_available: bool,
}

impl PortalCapture {
    /// Detects the best strategy in the current session with default settings.
    ///
    /// Fails with [`CaptureError::NoBackend`] if there is no session bus or neither KWin's
    /// `ScreenShot2` nor the screenshot portal is reachable.
    pub fn detect() -> Result<Self> {
        Self::with_config(PortalConfig::default())
    }

    /// As [`PortalCapture::detect`] with explicit settings.
    pub fn with_config(config: PortalConfig) -> Result<Self> {
        let (kwin_version, portal_available) = async_io::block_on(bus::with_timeout(
            "portal-detect",
            "the D-Bus session bus",
            config.timeout,
            async {
                let conn = bus::connect(config.bus_address.as_deref()).await?;
                let kwin_version = if config.strategy == Some(Strategy::Portal) {
                    None
                } else {
                    kwin::probe_version(&conn).await
                };
                let portal_available = bus::name_available(&conn, portal::SERVICE).await;
                Ok((kwin_version, portal_available))
            },
        ))?;

        let strategy = match (config.strategy, kwin_version, portal_available) {
            (Some(Strategy::KWin), None, _) => {
                return Err(CaptureError::NoBackend(
                    "KWin's org.kde.KWin.ScreenShot2 is not on the session bus (is this a KDE \
                     Plasma Wayland session, version 5.27 or newer?)"
                        .into(),
                ));
            }
            (Some(Strategy::Portal) | None, None, false) => {
                return Err(CaptureError::NoBackend(
                    "neither org.kde.KWin.ScreenShot2 nor the xdg-desktop-portal Screenshot \
                     portal is available on the session bus; install xdg-desktop-portal with \
                     xdg-desktop-portal-gnome or -kde"
                        .into(),
                ));
            }
            (Some(Strategy::Portal), _, false) => {
                return Err(CaptureError::NoBackend(
                    "org.freedesktop.portal.Desktop is not available on the session bus".into(),
                ));
            }
            (Some(Strategy::KWin) | None, Some(_), _) => Strategy::KWin,
            (Some(Strategy::Portal) | None, None, true)
            | (Some(Strategy::Portal), Some(_), true) => Strategy::Portal,
        };

        let mut me = Self {
            config,
            active: AtomicU8::new(strategy.as_u8()),
            kwin_version,
            portal_available,
            layout_available: false,
        };
        me.layout_available = me.query_layout().is_some();
        tracing::debug!(
            ?strategy,
            kwin_version,
            portal_available,
            layout = me.layout_available,
            "detected"
        );
        Ok(me)
    }

    /// The strategy currently in use. May change from [`Strategy::KWin`] to
    /// [`Strategy::Portal`] at runtime if KWin refuses and fallback is enabled.
    pub fn strategy(&self) -> Strategy {
        Strategy::from_u8(self.active.load(Ordering::Relaxed))
    }

    /// The settings in effect.
    pub fn config(&self) -> &PortalConfig {
        &self.config
    }

    /// `org.kde.KWin.ScreenShot2` interface version, if that service was found.
    pub fn kwin_version(&self) -> Option<u32> {
        self.kwin_version
    }

    /// Whether the screenshot portal was found (needed for
    /// [`PortalCapture::capture_interactive`] and the fallback).
    pub fn portal_available(&self) -> bool {
        self.portal_available
    }

    /// Lets the desktop show its own selection UI and returns what the user picked.
    ///
    /// Uses the portal with `interactive = true` on every desktop. On GNOME this is the
    /// window/area/screen picker and needs no permission. The result is just the selection:
    /// its `origin` is `(0, 0)` and `scale_factor` `1.0` because the portal does not say
    /// where on the desktop it came from. Cancelling returns [`CaptureError::Cancelled`].
    /// Waits up to [`PortalConfig::interactive_timeout`].
    pub fn capture_interactive(&self) -> Result<Frame> {
        if !self.portal_available {
            return Err(CaptureError::NoBackend(
                "the xdg-desktop-portal Screenshot portal is not available".into(),
            ));
        }
        self.portal_shot(true)
    }

    /// KWin only: the monitor the user is working on (`CaptureActiveScreen`).
    pub fn kwin_capture_active_screen(&self, opts: &CaptureOptions) -> Result<Frame> {
        Ok(self.kwin_only(Target::ActiveScreen, *opts)?.frame)
    }

    /// KWin only: the focused window (`CaptureActiveWindow`).
    pub fn kwin_capture_active_window(&self, opts: &CaptureOptions) -> Result<Frame> {
        Ok(self.kwin_only(Target::ActiveWindow, *opts)?.frame)
    }

    /// KWin only: a rectangle in **logical** coordinates (`CaptureArea`). Prefer
    /// [`CaptureBackend::capture_region`], which takes physical pixels.
    pub fn kwin_capture_area(&self, logical: Rect, opts: &CaptureOptions) -> Result<Frame> {
        Ok(self.kwin_only(Target::Area(logical), *opts)?.frame)
    }

    /// KWin only: lets the user click a window or screen (`CaptureInteractive`); waits up
    /// to [`PortalConfig::interactive_timeout`].
    pub fn kwin_capture_interactive(
        &self,
        kind: InteractiveKind,
        opts: &CaptureOptions,
    ) -> Result<Frame> {
        Ok(self.kwin_call(Target::Interactive(kind), *opts, self.config.interactive_timeout)?.frame)
    }

    // ---- internals ----------------------------------------------------------------

    fn connect(&self) -> Result<Connection> {
        async_io::block_on(bus::with_timeout(
            "portal-connect",
            "the D-Bus session bus",
            self.config.timeout,
            bus::connect(self.config.bus_address.as_deref()),
        ))
    }

    /// The monitor layout from the first source that answers.
    fn query_layout(&self) -> Option<Layout> {
        let from_mutter = async_io::block_on(bus::with_timeout(
            "mutter",
            "GNOME display configuration",
            self.config.layout_timeout,
            async {
                let conn = bus::connect(self.config.bus_address.as_deref()).await?;
                if !bus::name_has_owner(&conn, mutter::SERVICE_NAME).await {
                    return Err(CaptureError::backend("mutter", "not running"));
                }
                mutter::query(&conn).await.map_err(|e| CaptureError::backend("mutter", e))
            },
        ));
        match from_mutter {
            Ok(infos) => {
                let layout = Layout::new(infos);
                if !layout.is_empty() {
                    return Some(layout);
                }
            }
            Err(e) => tracing::trace!(error = %e, "no Mutter layout"),
        }
        if self.config.wayland_outputs {
            match wl_output::query(self.config.layout_timeout, self.config.wayland_socket.clone()) {
                Ok(infos) => {
                    let layout = Layout::new(infos);
                    if !layout.is_empty() {
                        return Some(layout);
                    }
                }
                Err(e) => tracing::trace!(error = %e, "no Wayland output layout"),
            }
        }
        None
    }

    /// Places a whole-desktop frame using `layout` when the image matches it.
    fn place_desktop(frame: &mut Frame, layout: Option<&Layout>) {
        if let Some(l) = layout {
            if let Some(scale) = l.image_scale(frame.size()) {
                frame.origin = l.desktop_origin(scale);
                frame.scale_factor = scale;
            } else {
                tracing::debug!(
                    size = ?frame.size(),
                    "screenshot size does not match the reported monitor layout; origin left at 0,0"
                );
            }
        }
    }

    fn kwin_params(&self, opts: CaptureOptions, timeout: Duration) -> Result<kwin::Params> {
        Ok(kwin::Params {
            include_cursor: opts.include_cursor,
            timeout,
            max_bytes: self.config.max_image_bytes,
            version: self.kwin_version.ok_or_else(|| {
                CaptureError::NoBackend("org.kde.KWin.ScreenShot2 was not detected".into())
            })?,
        })
    }

    fn kwin_call(
        &self,
        target: Target,
        opts: CaptureOptions,
        timeout: Duration,
    ) -> Result<kwin::Shot> {
        if self.strategy() != Strategy::KWin {
            return Err(CaptureError::unsupported(
                "xdg-portal",
                "KWin-specific captures (the KWin strategy is not active)",
            ));
        }
        let params = self.kwin_params(opts, timeout)?;
        let conn = self.connect()?;
        // The overall deadline is enforced inside `kwin::capture`.
        async_io::block_on(kwin::capture(&conn, &target, params))
    }

    fn kwin_only(&self, target: Target, opts: CaptureOptions) -> Result<kwin::Shot> {
        self.kwin_call(target, opts, self.config.timeout)
    }

    /// Decides what to do about a KWin failure: `Ok(())` means "carry on with the portal
    /// for this call", `Err` means "report this error".
    fn kwin_failed(&self, err: CaptureError) -> Result<()> {
        let recoverable = matches!(
            err,
            CaptureError::PermissionDenied(_)
                | CaptureError::NoBackend(_)
                | CaptureError::Unsupported { .. }
        );
        if !(recoverable && self.config.kwin_fallback_to_portal && self.portal_available) {
            return Err(err);
        }
        if !matches!(err, CaptureError::Unsupported { .. }) {
            tracing::warn!(error = %err, "KWin ScreenShot2 unavailable; switching to the xdg-desktop-portal");
            self.active.store(Strategy::Portal.as_u8(), Ordering::Relaxed);
        }
        Ok(())
    }

    fn portal_shot(&self, interactive: bool) -> Result<Frame> {
        let conn = self.connect()?;
        let params = portal::Params {
            interactive,
            timeout: if interactive {
                self.config.interactive_timeout
            } else {
                self.config.timeout
            },
            max_bytes: self.config.max_image_bytes,
            delete_file: self.config.delete_portal_file,
        };
        async_io::block_on(portal::screenshot(&conn, params))
    }

    fn portal_desktop(&self, opts: CaptureOptions) -> Result<Frame> {
        if !self.portal_available {
            return Err(CaptureError::NoBackend(
                "the xdg-desktop-portal Screenshot portal is not available".into(),
            ));
        }
        if opts.include_cursor {
            tracing::debug!("the screenshot portal cannot include the cursor; ignoring the option");
        }
        let mut frame = self.portal_shot(false)?;
        Self::place_desktop(&mut frame, self.query_layout().as_ref());
        Ok(frame)
    }

    fn desktop_from_kwin(&self, shot: kwin::Shot) -> Frame {
        let mut frame = shot.frame;
        if let Some(s) = shot.scale {
            frame.scale_factor = s;
        }
        Self::place_desktop(&mut frame, self.query_layout().as_ref());
        frame
    }

    fn monitor_from_portal(&self, monitor_id: &str, opts: CaptureOptions) -> Result<Frame> {
        if monitor_id == DESKTOP_MONITOR_ID {
            return self.portal_desktop(opts);
        }
        let layout = self.query_layout().ok_or_else(|| {
            CaptureError::unsupported(
                "xdg-portal",
                "per-monitor capture (no monitor layout source; use capture_desktop)",
            )
        })?;
        let monitor = layout
            .find(monitor_id)
            .cloned()
            .ok_or_else(|| CaptureError::NotFound(monitor_id.to_owned()))?;
        let desktop = self.portal_desktop(opts)?;
        // `portal_desktop` re-queried the layout; use the same snapshot for the crop math.
        let scale = layout.image_scale(desktop.size()).ok_or_else(|| {
            CaptureError::backend(
                "xdg-portal",
                format!(
                    "the {}x{} screenshot does not fit the reported monitor layout, so monitor \
                     {monitor_id:?} cannot be cropped out of it",
                    desktop.width(),
                    desktop.height()
                ),
            )
        })?;
        let rect = layout
            .crop_rect(&monitor, scale, desktop.size())
            .ok_or_else(|| CaptureError::NotFound(monitor_id.to_owned()))?;
        let mut frame = desktop.crop(rect).map_err(CaptureError::from)?;
        frame.origin = layout.public_rect(&monitor).origin();
        frame.scale_factor = monitor.scale;
        Ok(frame)
    }

    fn monitor_from_kwin(&self, monitor_id: &str, opts: CaptureOptions) -> Result<Frame> {
        let shot = self.kwin_only(Target::Screen(monitor_id.to_owned()), opts)?;
        let mut frame = shot.frame;
        let layout = self.query_layout();
        let info = layout.as_ref().and_then(|l| l.find(monitor_id).map(|m| (l, m)));
        if let Some((l, m)) = info {
            frame.origin = l.public_rect(m).origin();
            frame.scale_factor = m.scale;
        } else if let Some(s) = shot.scale {
            frame.scale_factor = s;
        }
        Ok(frame)
    }
}

impl CaptureBackend for PortalCapture {
    fn name(&self) -> &'static str {
        match self.strategy() {
            Strategy::KWin => kwin::BACKEND,
            Strategy::Portal => portal::BACKEND,
        }
    }

    fn capabilities(&self) -> Capabilities {
        let kwin = self.strategy() == Strategy::KWin;
        Capabilities {
            enumerate_monitors: self.layout_available,
            // Wayland gives clients no window list; not faked.
            enumerate_windows: false,
            // KWin can capture a window by UUID but this backend cannot enumerate any, so
            // advertising it would only produce a UI with nothing to pick.
            capture_windows: false,
            cursor: kwin,
            hdr_float: false,
            native_desktop: true,
            needs_user_interaction: !kwin,
        }
    }

    fn monitors(&self) -> Result<Vec<Monitor>> {
        self.query_layout().map(|l| l.to_monitors()).ok_or_else(|| {
            CaptureError::unsupported(
                self.name(),
                "monitor enumeration (no Mutter DisplayConfig or Wayland xdg-output available)",
            )
        })
    }

    fn capture_monitor(&self, monitor_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        if monitor_id == DESKTOP_MONITOR_ID {
            return self.capture_desktop(opts);
        }
        if self.strategy() == Strategy::KWin {
            match self.monitor_from_kwin(monitor_id, *opts) {
                Ok(f) => return Ok(f),
                Err(e) => self.kwin_failed(e)?,
            }
        }
        self.monitor_from_portal(monitor_id, *opts)
    }

    fn capture_desktop(&self, opts: &CaptureOptions) -> Result<Frame> {
        if self.strategy() == Strategy::KWin {
            match self.kwin_only(Target::Workspace, *opts) {
                Ok(shot) => return Ok(self.desktop_from_kwin(shot)),
                Err(e) => self.kwin_failed(e)?,
            }
        }
        self.portal_desktop(*opts)
    }

    fn capture_window(&self, window_id: &str, opts: &CaptureOptions) -> Result<Frame> {
        if self.strategy() == Strategy::KWin {
            // `window_id` is KWin's window UUID (as printed by KWin scripting / `kdotool`).
            return Ok(self.kwin_only(Target::Window(window_id.to_owned()), *opts)?.frame);
        }
        Err(CaptureError::unsupported(
            self.name(),
            "window capture (use capture_interactive, whose picker can select a window)",
        ))
    }
}
