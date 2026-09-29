//! Monitor layout from the Wayland `wl_output` and `xdg-output` protocols.
//!
//! Any client may bind these globals: no permission, no prompt. `xdg-output` (implemented
//! by KWin, Mutter, wlroots compositors) supplies the **logical** position and size and
//! the connector name that `wl_output` lacks before version 4, so it is preferred whenever
//! present; plain `wl_output` is the fallback (integer scale only).
//!
//! The query runs on a helper thread with a deadline: a wedged compositor must not be
//! able to hang a capture call, and `wayland-client`'s roundtrip has no timeout of its own.

use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use ssx_types::Rect;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
    },
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use crate::layout::MonitorInfo;

/// Everything learned about one output, before interpretation.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RawOutput {
    pub global: u32,
    pub geometry_pos: Option<(i32, i32)>,
    pub rotated: bool,
    /// Current mode: width, height (unrotated), refresh in mHz.
    pub mode: Option<(i32, i32, i32)>,
    pub scale: Option<i32>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub make_model: Option<String>,
    pub xdg_pos: Option<(i32, i32)>,
    pub xdg_size: Option<(i32, i32)>,
    pub xdg_name: Option<String>,
    pub xdg_description: Option<String>,
}

/// Turns raw protocol data into monitors. Outputs without a current mode are skipped
/// (they are disabled or not yet described).
pub(crate) fn build_infos(raw: &[RawOutput]) -> Vec<MonitorInfo> {
    let mut out = Vec::new();
    for o in raw {
        let Some((mw, mh, mhz)) = o.mode else { continue };
        let (pw, ph) = if o.rotated { (mh, mw) } else { (mw, mh) };
        if pw <= 0 || ph <= 0 {
            continue;
        }
        let (logical, scale) = match (o.xdg_pos, o.xdg_size) {
            (Some((x, y)), Some((w, h))) if w > 0 && h > 0 => {
                (Rect::new(x, y, w as u32, h as u32), f64::from(pw) / f64::from(w))
            }
            _ => {
                let s = o.scale.filter(|s| *s > 0).unwrap_or(1);
                let (x, y) = o.geometry_pos.unwrap_or((0, 0));
                (Rect::new(x, y, (pw / s) as u32, (ph / s) as u32), f64::from(s))
            }
        };
        let id = o
            .xdg_name
            .clone()
            .or_else(|| o.name.clone())
            .unwrap_or_else(|| format!("wl-output-{}", o.global));
        let name = o
            .xdg_description
            .clone()
            .or_else(|| o.description.clone())
            .or_else(|| o.make_model.clone())
            .unwrap_or_else(|| id.clone());
        out.push(MonitorInfo {
            id,
            name,
            logical,
            scale,
            primary: false,
            refresh_hz: (mhz > 0).then(|| mhz as f32 / 1000.0),
        });
    }
    out
}

#[derive(Default)]
struct State {
    outputs: Vec<RawOutput>,
}

impl State {
    fn output(&mut self, global: u32) -> &mut RawOutput {
        if let Some(i) = self.outputs.iter().position(|o| o.global == global) {
            &mut self.outputs[i]
        } else {
            self.outputs.push(RawOutput { global, ..RawOutput::default() });
            self.outputs.last_mut().expect("just pushed")
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Hot-plug is irrelevant for a one-shot query.
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = state.output(*global);
        match event {
            wl_output::Event::Geometry { x, y, make, model, transform, .. } => {
                o.geometry_pos = Some((x, y));
                o.rotated = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
                o.make_model =
                    Some(format!("{make} {model}").trim().to_owned()).filter(|s| !s.is_empty());
            }
            wl_output::Event::Mode { flags, width, height, refresh } => {
                let current =
                    matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                if current {
                    o.mode = Some((width, height, refresh));
                }
            }
            wl_output::Event::Scale { factor } => o.scale = Some(factor),
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Description { description } => o.description = Some(description),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = state.output(*global);
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.xdg_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.xdg_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } => o.xdg_name = Some(name),
            zxdg_output_v1::Event::Description { description } => {
                o.xdg_description = Some(description);
            }
            _ => {}
        }
    }
}

fn connect(socket: Option<&Path>) -> Result<Connection, String> {
    match socket {
        Some(path) => {
            let stream = UnixStream::connect(path)
                .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
            Connection::from_socket(stream).map_err(|e| format!("Wayland handshake: {e}"))
        }
        None => Connection::connect_to_env().map_err(|e| format!("cannot connect to Wayland: {e}")),
    }
}

fn query_blocking(socket: Option<&Path>) -> Result<Vec<RawOutput>, String> {
    let conn = connect(socket)?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let mut state = State::default();

    let manager: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    let advertised: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter().filter(|g| g.interface == "wl_output").map(|g| (g.name, g.version)).collect()
    });
    // Proxies must stay alive until the roundtrips have delivered their events.
    let mut outputs: Vec<WlOutput> = Vec::new();
    let mut xdg_outputs: Vec<ZxdgOutputV1> = Vec::new();
    for (name, version) in advertised {
        let output: WlOutput = globals.registry().bind(name, version.min(4), &qh, name);
        state.output(name);
        if let Some(m) = &manager {
            xdg_outputs.push(m.get_xdg_output(&output, &qh, name));
        }
        outputs.push(output);
    }
    // First roundtrip delivers wl_output state, second the xdg_output events they trigger.
    for _ in 0..2 {
        queue.roundtrip(&mut state).map_err(|e| format!("roundtrip: {e}"))?;
    }
    drop((outputs, xdg_outputs));
    Ok(state.outputs)
}

/// Queries the compositor at `socket`, or the one named by `WAYLAND_DISPLAY`. Returns an
/// error string when there is no compositor, it does not answer within `timeout`, or it
/// exposes no usable output.
pub(crate) fn query(
    timeout: Duration,
    socket: Option<PathBuf>,
) -> Result<Vec<MonitorInfo>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("ssx-wl-output".into())
        .spawn(move || {
            // The receiver may have given up already; nothing to do then.
            let _ = tx.send(query_blocking(socket.as_deref()));
        })
        .map_err(|e| format!("cannot spawn thread: {e}"))?;
    let raw = rx
        .recv_timeout(timeout)
        .map_err(|_| format!("Wayland compositor did not answer within {timeout:?}"))??;
    let infos = build_infos(&raw);
    if infos.is_empty() { Err("compositor reported no outputs".into()) } else { Ok(infos) }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable
mod tests {
    use super::*;

    fn raw(global: u32) -> RawOutput {
        RawOutput { global, mode: Some((1920, 1080, 60_000)), ..RawOutput::default() }
    }

    #[test]
    fn xdg_output_gives_fractional_scale() {
        let o = RawOutput {
            xdg_pos: Some((1536, 0)),
            xdg_size: Some((1536, 864)),
            xdg_name: Some("eDP-1".into()),
            xdg_description: Some("Built-in".into()),
            scale: Some(2),
            ..raw(1)
        };
        let m = build_infos(&[o]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "eDP-1");
        assert_eq!(m[0].name, "Built-in");
        assert_eq!(m[0].logical, Rect::new(1536, 0, 1536, 864));
        assert!((m[0].scale - 1.25).abs() < 1e-9);
        assert_eq!(m[0].refresh_hz, Some(60.0));
    }

    #[test]
    fn plain_wl_output_uses_integer_scale_and_mode() {
        let o = RawOutput {
            geometry_pos: Some((10, 20)),
            scale: Some(2),
            name: Some("DP-3".into()),
            make_model: Some("Acme Panel".into()),
            ..raw(7)
        };
        let m = build_infos(&[o]);
        assert_eq!(m[0].logical, Rect::new(10, 20, 960, 540));
        assert_eq!(m[0].scale, 2.0);
        assert_eq!(m[0].id, "DP-3");
        assert_eq!(m[0].name, "Acme Panel");
    }

    #[test]
    fn rotation_swaps_physical_axes() {
        let o = RawOutput { rotated: true, geometry_pos: Some((0, 0)), ..raw(2) };
        let m = build_infos(&[o]);
        assert_eq!(m[0].logical, Rect::new(0, 0, 1080, 1920));
        assert_eq!(m[0].id, "wl-output-2");
    }

    #[test]
    fn outputs_without_a_mode_or_size_are_skipped() {
        let no_mode = RawOutput { global: 3, ..RawOutput::default() };
        let zero = RawOutput { mode: Some((0, 0, 0)), ..raw(4) };
        assert!(build_infos(&[no_mode, zero]).is_empty());
        // xdg size of zero falls back to wl_output data
        let o = RawOutput { xdg_pos: Some((0, 0)), xdg_size: Some((0, 0)), ..raw(5) };
        assert_eq!(build_infos(&[o])[0].logical, Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn scale_zero_is_treated_as_one() {
        let o = RawOutput { scale: Some(0), ..raw(6) };
        assert_eq!(build_infos(&[o])[0].scale, 1.0);
    }
}
