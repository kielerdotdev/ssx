//! Monitor layout from GNOME's `org.gnome.Mutter.DisplayConfig.GetCurrentState`.
//!
//! This D-Bus call is readable by any client without a permission prompt (unlike
//! `org.gnome.Shell.Screenshot`, which Shell restricts to allow-listed apps), which makes
//! it the right way to learn where monitors sit inside the whole-desktop portal image.
//!
//! The reply signature is `(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})`:
//! serial, physical monitors (spec, modes, properties), logical monitors
//! (x, y, scale, transform, primary, monitor specs, properties) and global properties.
//! Logical monitors are what the compositor arranges; the physical monitors supply names
//! and the current mode (refresh rate).

use std::collections::HashMap;

use ssx_types::Rect;
use zbus::{Connection, zvariant::OwnedValue};

use crate::layout::MonitorInfo;

const SERVICE: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";

type Props = HashMap<String, OwnedValue>;
/// `(connector, vendor, product, serial)`
type Spec = (String, String, String, String);
/// `(id, width, height, refresh, preferred_scale, supported_scales, properties)`
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Physical = (Spec, Vec<Mode>, Props);
/// `(x, y, scale, transform, primary, monitors, properties)`
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
/// The whole `GetCurrentState` reply body.
pub(crate) type State = (u32, Vec<Physical>, Vec<Logical>, Props);

/// Bus name to probe for the presence of Mutter.
pub(crate) const SERVICE_NAME: &str = SERVICE;

/// Fetches and parses the current display state.
pub(crate) async fn query(conn: &Connection) -> Result<Vec<MonitorInfo>, zbus::Error> {
    let reply =
        conn.call_method(Some(SERVICE), PATH, Some(SERVICE), "GetCurrentState", &()).await?;
    let state: State = reply.body().deserialize()?;
    Ok(parse_state(&state))
}

fn prop_bool(props: &Props, key: &str) -> bool {
    props.get(key).is_some_and(|v| matches!(&**v, zbus::zvariant::Value::Bool(true)))
}

fn prop_str(props: &Props, key: &str) -> Option<String> {
    match props.get(key).map(|v| &**v) {
        Some(zbus::zvariant::Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

/// Converts a `GetCurrentState` reply to monitors. One entry per *logical* monitor
/// (mirrored displays share one), named after its first physical monitor.
pub(crate) fn parse_state(state: &State) -> Vec<MonitorInfo> {
    let (_, physical, logical, _) = state;
    let mut out = Vec::new();
    for (x, y, scale, transform, primary, specs, _) in logical {
        let Some(connector) = specs.first().map(|s| s.0.clone()) else { continue };
        let phys = physical.iter().find(|(spec, _, _)| spec.0 == connector);
        let current =
            phys.and_then(|(_, modes, _)| modes.iter().find(|m| prop_bool(&m.6, "is-current")));
        let Some(mode) = current else { continue };
        let (mut pw, mut ph) = (mode.1, mode.2);
        // Transforms 1, 3, 5, 7 (90 and 270 degrees, with or without flip) swap the axes.
        if transform % 2 == 1 {
            std::mem::swap(&mut pw, &mut ph);
        }
        if !(*scale > 0.0 && scale.is_finite()) || pw <= 0 || ph <= 0 {
            continue;
        }
        let lw = (f64::from(pw) / scale).round() as u32;
        let lh = (f64::from(ph) / scale).round() as u32;
        let name = phys
            .and_then(|(_, _, props)| prop_str(props, "display-name"))
            .unwrap_or_else(|| connector.clone());
        out.push(MonitorInfo {
            id: connector,
            name,
            logical: Rect::new(*x, *y, lw, lh),
            scale: *scale,
            primary: *primary,
            refresh_hz: Some(mode.3 as f32),
        });
    }
    out
}

#[cfg(test)]
pub(crate) mod sample {
    //! A `GetCurrentState` reply in the shape GNOME 46 produces for a laptop panel plus
    //! an external 4K monitor placed to its left (what `gdbus call` prints, transcribed).

    use zbus::zvariant::{OwnedValue, Value};

    use super::*;

    fn ov(v: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    fn spec(c: &str, product: &str) -> Spec {
        (c.into(), "VEN".into(), product.into(), "0x0".into())
    }

    fn mode(id: &str, w: i32, h: i32, hz: f64, current: bool) -> Mode {
        let mut props = Props::new();
        if current {
            props.insert("is-current".into(), ov(Value::Bool(true)));
        }
        (id.into(), w, h, hz, 1.0, vec![1.0, 2.0], props)
    }

    /// `eDP-1` 1920x1080 @ scale 1.25 (logical 1536x864) primary at (0,0);
    /// `DP-2` 3840x2160 @ scale 2 (logical 1920x1080) at (-1920, 0).
    pub(crate) fn dual() -> State {
        let mut p1 = Props::new();
        p1.insert("display-name".into(), ov(Value::from("Built-in display")));
        let mut p2 = Props::new();
        p2.insert("display-name".into(), ov(Value::from("Dell Inc. 27\"")));
        (
            3,
            vec![
                (
                    spec("eDP-1", "0x1234"),
                    vec![
                        mode("1920x1080@144.0", 1920, 1080, 144.0, false),
                        mode("1920x1080@60.0", 1920, 1080, 60.0, true),
                    ],
                    p1,
                ),
                (
                    spec("DP-2", "U2723QE"),
                    vec![mode("3840x2160@59.997", 3840, 2160, 59.997, true)],
                    p2,
                ),
            ],
            vec![
                (0, 0, 1.25, 0, true, vec![spec("eDP-1", "0x1234")], Props::new()),
                (-1920, 0, 2.0, 0, false, vec![spec("DP-2", "U2723QE")], Props::new()),
            ],
            Props::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dual_monitor_sample() {
        let m = parse_state(&sample::dual());
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "eDP-1");
        assert_eq!(m[0].name, "Built-in display");
        assert_eq!(m[0].logical, Rect::new(0, 0, 1536, 864));
        assert!((m[0].scale - 1.25).abs() < 1e-9);
        assert!(m[0].primary);
        assert_eq!(m[0].refresh_hz, Some(60.0), "the current mode, not the 144 Hz one");
        assert_eq!(m[1].id, "DP-2");
        assert_eq!(m[1].name, "Dell Inc. 27\"");
        assert_eq!(m[1].logical, Rect::new(-1920, 0, 1920, 1080));
        assert!(!m[1].primary);
        assert!((m[1].refresh_hz.unwrap() - 59.997).abs() < 1e-3);
    }

    #[test]
    fn rotated_monitor_swaps_axes() {
        let mut s = sample::dual();
        s.2[1].3 = 1; // 90 degrees
        let m = parse_state(&s);
        assert_eq!(m[1].logical, Rect::new(-1920, 0, 1080, 1920));
    }

    #[test]
    fn tolerates_missing_or_broken_entries() {
        let mut s = sample::dual();
        s.1[1].1.clear(); // no current mode
        s.2[0].2 = 0.0; // bogus scale
        assert!(parse_state(&s).is_empty());
        let mut s = sample::dual();
        s.2[0].5.clear(); // logical monitor without physical monitors
        assert_eq!(parse_state(&s).len(), 1);
        let empty: State = (0, vec![], vec![], Props::new());
        assert!(parse_state(&empty).is_empty());
    }

    #[test]
    fn name_falls_back_to_connector() {
        let mut s = sample::dual();
        s.1[0].2.clear();
        assert_eq!(parse_state(&s)[0].name, "eDP-1");
    }
}
