//! Monitor discovery through XRandR.
//!
//! Three tiers, because servers differ:
//!
//! 1. **RandR 1.5 `GetMonitors`** (Xorg, XWayland, Xvfb): named monitors with geometry and
//!    the primary flag. This also reports *user-defined* monitors (`xrandr --setmonitor`)
//!    and merged/tiled outputs as one monitor, which is what a screenshot tool wants.
//! 2. **RandR 1.2 CRTCs** (`GetScreenResourcesCurrent` + `GetCrtcInfo`): one monitor per
//!    active CRTC, named after its first output.
//! 3. **No RandR**: one monitor covering the root window.
//!
//! Refresh rate comes from the active CRTC's mode (`dot_clock / (htotal * vtotal)` with the
//! interlace/doublescan corrections `xrandr` applies). `GetScreenResourcesCurrent` is used
//! instead of `GetScreenResources` because the latter forces a hardware re-probe, which can
//! blank displays and take seconds.

use ssx_types::{Monitor, Rect};
use x11rb::protocol::{
    randr::{self, ConnectionExt as _, ModeFlag, ModeInfo, Rotation as RrRotation},
    xproto::ConnectionExt as _,
};

use crate::{
    error::{X11Error, X11Result},
    session::Session,
};

/// Rotation of a monitor's CRTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    /// Unrotated.
    #[default]
    Normal,
    /// Rotated 90 degrees counter-clockwise ("left").
    Left,
    /// Rotated 180 degrees ("inverted").
    Inverted,
    /// Rotated 270 degrees counter-clockwise ("right").
    Right,
}

/// A [`Monitor`] plus X11-specific detail.
#[derive(Debug, Clone, PartialEq)]
pub struct X11Monitor {
    /// The backend-agnostic description. `id` equals the RandR monitor name.
    pub monitor: Monitor,
    /// CRTC rotation. `monitor.rect` is already in rotated (screen) coordinates.
    pub rotation: Rotation,
    /// Reflection along X / Y, if any.
    pub reflect_x: bool,
    /// See [`X11Monitor::reflect_x`].
    pub reflect_y: bool,
    /// RandR output names driving this monitor (e.g. `["DP-1"]`).
    pub outputs: Vec<String>,
    /// Physical size in millimetres, when the server knows it.
    pub size_mm: Option<(u32, u32)>,
}

fn rotation_of(r: RrRotation) -> (Rotation, bool, bool) {
    let rot = if u16::from(r) & u16::from(RrRotation::ROTATE90) != 0 {
        Rotation::Left
    } else if u16::from(r) & u16::from(RrRotation::ROTATE180) != 0 {
        Rotation::Inverted
    } else if u16::from(r) & u16::from(RrRotation::ROTATE270) != 0 {
        Rotation::Right
    } else {
        Rotation::Normal
    };
    (
        rot,
        u16::from(r) & u16::from(RrRotation::REFLECT_X) != 0,
        u16::from(r) & u16::from(RrRotation::REFLECT_Y) != 0,
    )
}

/// Refresh rate in Hz for a mode, with the corrections `xrandr` applies.
pub(crate) fn mode_refresh_hz(m: &ModeInfo) -> Option<f32> {
    let (h, mut v) = (f64::from(m.htotal), f64::from(m.vtotal));
    if h == 0.0 || v == 0.0 || m.dot_clock == 0 {
        return None;
    }
    let flags = u32::from(m.mode_flags);
    if flags & u32::from(ModeFlag::DOUBLE_SCAN) != 0 {
        v *= 2.0;
    }
    if flags & u32::from(ModeFlag::INTERLACE) != 0 {
        v /= 2.0;
    }
    Some((f64::from(m.dot_clock) / (h * v)) as f32)
}

/// Makes monitor ids unique (`name`, `name#2`, ...): RandR names *should* be unique but
/// nothing stops a driver reporting two outputs with one name, and ids must be usable as
/// keys.
fn uniquify(monitors: &mut [X11Monitor]) {
    for i in 1..monitors.len() {
        let mut n = 1;
        let base = monitors[i].monitor.id.clone();
        while monitors[..i].iter().any(|m| m.monitor.id == monitors[i].monitor.id) {
            n += 1;
            monitors[i].monitor.id = format!("{base}#{n}");
        }
    }
}

impl Session {
    /// Lists monitors with X11 detail. Never returns an empty list on a working server.
    pub(crate) fn monitor_details(&self) -> X11Result<Vec<X11Monitor>> {
        let scale = self.scale_factor();
        let mut monitors = match self.ext.randr {
            Some((maj, min)) if (maj, min) >= (1, 5) => self.monitors_v15(scale)?,
            Some((maj, min)) if (maj, min) >= (1, 3) => self.monitors_from_crtcs(scale)?,
            _ => Vec::new(),
        };
        if monitors.is_empty() {
            let (w, h) = self.root_size()?;
            monitors.push(X11Monitor {
                monitor: Monitor {
                    id: "root".into(),
                    name: format!("X screen {}", self.screen_num),
                    rect: Rect::new(0, 0, w, h),
                    scale_factor: scale,
                    primary: true,
                    refresh_hz: None,
                    hdr: None,
                },
                rotation: Rotation::Normal,
                reflect_x: false,
                reflect_y: false,
                outputs: Vec::new(),
                size_mm: None,
            });
        }
        if !monitors.iter().any(|m| m.monitor.primary) {
            monitors[0].monitor.primary = true;
        }
        uniquify(&mut monitors);
        Ok(monitors)
    }

    fn resources(&self) -> X11Result<randr::GetScreenResourcesCurrentReply> {
        self.conn
            .randr_get_screen_resources_current(self.root)?
            .reply()
            .map_err(|e| X11Error::from_reply("RRGetScreenResourcesCurrent", e))
    }

    fn atom_name(&self, atom: u32) -> X11Result<String> {
        let r = self
            .conn
            .get_atom_name(atom)?
            .reply()
            .map_err(|e| X11Error::from_reply("GetAtomName", e))?;
        Ok(String::from_utf8_lossy(&r.name).into_owned())
    }

    fn monitors_v15(&self, scale: f64) -> X11Result<Vec<X11Monitor>> {
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)?
            .reply()
            .map_err(|e| X11Error::from_reply("RRGetMonitors", e))?;
        // Resources are only needed for refresh/rotation; tolerate their absence.
        let resources = self.resources().ok();
        let mut out = Vec::new();
        for m in &reply.monitors {
            if m.width == 0 || m.height == 0 {
                continue;
            }
            let name = self.atom_name(m.name)?;
            let mut refresh_hz = None;
            let mut rotation = (Rotation::Normal, false, false);
            let mut outputs = Vec::new();
            for &output in &m.outputs {
                let Ok(info) = self
                    .conn
                    .randr_get_output_info(
                        output,
                        resources.as_ref().map_or(0, |r| r.config_timestamp),
                    )
                    .map_err(X11Error::from)
                    .and_then(|c| {
                        c.reply().map_err(|e| X11Error::from_reply("RRGetOutputInfo", e))
                    })
                else {
                    continue;
                };
                outputs.push(String::from_utf8_lossy(&info.name).into_owned());
                if refresh_hz.is_none() && info.crtc != 0 {
                    if let Some((hz, rot)) = self.crtc_details(info.crtc, resources.as_ref()) {
                        refresh_hz = hz;
                        rotation = rot;
                    }
                }
            }
            out.push(X11Monitor {
                monitor: Monitor {
                    id: name.clone(),
                    name,
                    rect: Rect::new(
                        i32::from(m.x),
                        i32::from(m.y),
                        u32::from(m.width),
                        u32::from(m.height),
                    ),
                    scale_factor: scale,
                    primary: m.primary,
                    refresh_hz,
                    hdr: None,
                },
                rotation: rotation.0,
                reflect_x: rotation.1,
                reflect_y: rotation.2,
                outputs,
                size_mm: (m.width_in_millimeters != 0 && m.height_in_millimeters != 0)
                    .then_some((m.width_in_millimeters, m.height_in_millimeters)),
            });
        }
        Ok(out)
    }

    /// Refresh rate and rotation of a CRTC.
    #[allow(clippy::type_complexity)] // a one-off tuple of two small Copy values
    fn crtc_details(
        &self,
        crtc: u32,
        resources: Option<&randr::GetScreenResourcesCurrentReply>,
    ) -> Option<(Option<f32>, (Rotation, bool, bool))> {
        let ts = resources.map_or(0, |r| r.config_timestamp);
        let info = self.conn.randr_get_crtc_info(crtc, ts).ok()?.reply().ok()?;
        let hz = resources
            .and_then(|r| r.modes.iter().find(|m| m.id == info.mode))
            .and_then(mode_refresh_hz);
        Some((hz, rotation_of(info.rotation)))
    }

    fn monitors_from_crtcs(&self, scale: f64) -> X11Result<Vec<X11Monitor>> {
        let resources = self.resources()?;
        let primary_output =
            self.conn.randr_get_output_primary(self.root)?.reply().map(|r| r.output).unwrap_or(0);
        let mut out = Vec::new();
        for &crtc in &resources.crtcs {
            let Some(info) = self
                .conn
                .randr_get_crtc_info(crtc, resources.config_timestamp)?
                .reply()
                .ok()
                .filter(|i| i.mode != 0 && i.width != 0 && i.height != 0)
            else {
                continue;
            };
            let mut names = Vec::new();
            for &o in &info.outputs {
                if let Ok(oi) =
                    self.conn.randr_get_output_info(o, resources.config_timestamp)?.reply()
                {
                    names.push(String::from_utf8_lossy(&oi.name).into_owned());
                }
            }
            let name = names.first().cloned().unwrap_or_else(|| format!("crtc-{crtc}"));
            let (rot, rx, ry) = rotation_of(info.rotation);
            out.push(X11Monitor {
                monitor: Monitor {
                    id: name.clone(),
                    name,
                    rect: Rect::new(
                        i32::from(info.x),
                        i32::from(info.y),
                        u32::from(info.width),
                        u32::from(info.height),
                    ),
                    scale_factor: scale,
                    primary: info.outputs.contains(&primary_output) && primary_output != 0,
                    refresh_hz: resources
                        .modes
                        .iter()
                        .find(|m| m.id == info.mode)
                        .and_then(mode_refresh_hz),
                    hdr: None,
                },
                rotation: rot,
                reflect_x: rx,
                reflect_y: ry,
                outputs: names,
                size_mm: None,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(dot_clock: u32, h: u16, v: u16, flags: ModeFlag) -> ModeInfo {
        ModeInfo {
            id: 1,
            width: 1920,
            height: 1080,
            dot_clock,
            hsync_start: 0,
            hsync_end: 0,
            htotal: h,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: v,
            name_len: 0,
            mode_flags: flags,
        }
    }

    #[test]
    fn refresh_from_modeline() {
        // Standard 1080p60 CVT-RB-ish timing: 148.5 MHz / (2200 * 1125) = 60.0 Hz.
        let hz = mode_refresh_hz(&mode(148_500_000, 2200, 1125, ModeFlag::from(0u32))).unwrap();
        assert!((hz - 60.0).abs() < 0.01, "{hz}");
    }

    #[test]
    fn refresh_applies_interlace_and_doublescan() {
        let base = mode(74_250_000, 2200, 1125, ModeFlag::from(0u32));
        let plain = mode_refresh_hz(&base).unwrap();
        let interlaced =
            mode_refresh_hz(&mode(74_250_000, 2200, 1125, ModeFlag::INTERLACE)).unwrap();
        let dbl = mode_refresh_hz(&mode(74_250_000, 2200, 1125, ModeFlag::DOUBLE_SCAN)).unwrap();
        assert!((interlaced - plain * 2.0).abs() < 0.01);
        assert!((dbl - plain / 2.0).abs() < 0.01);
    }

    #[test]
    fn zero_timings_have_no_refresh() {
        assert_eq!(mode_refresh_hz(&mode(0, 2200, 1125, ModeFlag::from(0u32))), None);
        assert_eq!(mode_refresh_hz(&mode(1, 0, 1125, ModeFlag::from(0u32))), None);
        assert_eq!(mode_refresh_hz(&mode(1, 10, 0, ModeFlag::from(0u32))), None);
    }

    #[test]
    fn rotation_flags() {
        assert_eq!(rotation_of(RrRotation::ROTATE0), (Rotation::Normal, false, false));
        assert_eq!(rotation_of(RrRotation::ROTATE90), (Rotation::Left, false, false));
        assert_eq!(rotation_of(RrRotation::ROTATE180), (Rotation::Inverted, false, false));
        assert_eq!(rotation_of(RrRotation::ROTATE270), (Rotation::Right, false, false));
        let both =
            RrRotation::from(u16::from(RrRotation::ROTATE0) | u16::from(RrRotation::REFLECT_X));
        assert_eq!(rotation_of(both), (Rotation::Normal, true, false));
    }

    #[test]
    fn duplicate_ids_are_made_unique() {
        let mk = |id: &str| X11Monitor {
            monitor: Monitor {
                id: id.into(),
                name: id.into(),
                rect: Rect::new(0, 0, 1, 1),
                scale_factor: 1.0,
                primary: false,
                refresh_hz: None,
                hdr: None,
            },
            rotation: Rotation::Normal,
            reflect_x: false,
            reflect_y: false,
            outputs: vec![],
            size_mm: None,
        };
        let mut v = vec![mk("A"), mk("A"), mk("A"), mk("B")];
        uniquify(&mut v);
        let ids: Vec<_> = v.iter().map(|m| m.monitor.id.as_str()).collect();
        assert_eq!(ids, ["A", "A#2", "A#3", "B"]);
    }
}
