//! Best-effort UI scale factor for X11.
//!
//! X has no per-monitor scale. Desktops publish one global DPI instead, and toolkits derive
//! their scale from it. In order of preference:
//!
//! 1. `Xft/DPI` from the XSETTINGS manager (`_XSETTINGS_SETTINGS` on the owner of the
//!    `_XSETTINGS_Sn` selection), stored as DPI * 1024. This is the live value GTK/Qt follow.
//! 2. `Xft.dpi` in the `RESOURCE_MANAGER` property (what `~/.Xresources` sets).
//! 3. `Gdk/WindowScalingFactor` (integer) from XSETTINGS.
//! 4. `1.0`.
//!
//! `scale = dpi / 96`, clamped to 0.5..=8. The value is the same for every monitor. It is
//! *informational*: capture always returns physical pixels, X11 never scales them.

use crate::{error::X11Result, session::Session};

/// Reference DPI at which the scale factor is 1.0.
const BASE_DPI: f64 = 96.0;

/// Extracts `Xft.dpi` from an X resource database string.
pub(crate) fn parse_xft_dpi(resources: &str) -> Option<f64> {
    resources.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "Xft.dpi" {
            return None;
        }
        value.trim().parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0)
    })
}

/// A value from an XSETTINGS blob.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum XSetting {
    Int(i32),
    String(String),
    Color([u16; 4]),
}

/// Parses the `_XSETTINGS_SETTINGS` property (XSETTINGS specification 0.5). Truncated or
/// malformed input yields the settings decoded before the fault, never a panic.
pub(crate) fn parse_xsettings(data: &[u8]) -> Vec<(String, XSetting)> {
    let mut out = Vec::new();
    let Some(&order) = data.first() else { return out };
    let big = order == 1;
    let mut cur = Cursor { data, pos: 4, big };
    let _serial = cur.u32();
    let Some(count) = cur.u32() else { return out };
    for _ in 0..count {
        let Some(kind) = cur.u8() else { break };
        let _ = cur.u8();
        let Some(name_len) = cur.u16() else { break };
        let Some(name) = cur.bytes(name_len as usize) else { break };
        cur.align4();
        let name = String::from_utf8_lossy(name).into_owned();
        if cur.u32().is_none() {
            break; // last-change serial
        }
        let value = match kind {
            0 => cur.u32().map(|v| XSetting::Int(v.cast_signed())),
            1 => {
                let Some(len) = cur.u32() else { break };
                let v = cur
                    .bytes(len as usize)
                    .map(|b| XSetting::String(String::from_utf8_lossy(b).into_owned()));
                cur.align4();
                v
            }
            2 => (|| Some(XSetting::Color([cur.u16()?, cur.u16()?, cur.u16()?, cur.u16()?])))(),
            _ => None,
        };
        let Some(value) = value else { break };
        out.push((name, value));
    }
    out
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    big: bool,
}

impl<'a> Cursor<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b: [u8; 2] = self.bytes(2)?.try_into().ok()?;
        Some(if self.big { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    }
    fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.bytes(4)?.try_into().ok()?;
        Some(if self.big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }
    fn align4(&mut self) {
        self.pos = self.pos.saturating_add(3) & !3;
    }
}

/// Converts DPI to a clamped scale factor; values within 1% of 1.0 snap to exactly 1.0.
pub(crate) fn dpi_to_scale(dpi: f64) -> f64 {
    let s = (dpi / BASE_DPI).clamp(0.5, 8.0);
    if (s - 1.0).abs() < 0.01 { 1.0 } else { s }
}

impl Session {
    /// Detects the global scale factor (see the module docs); never fails, because scale
    /// is cosmetic: on any error it reports 1.0.
    pub(crate) fn scale_factor(&self) -> f64 {
        self.try_scale_factor().unwrap_or_else(|e| {
            tracing::debug!(error = %e, "scale detection failed; assuming 1.0");
            1.0
        })
    }

    fn try_scale_factor(&self) -> X11Result<f64> {
        let xsettings = self.read_xsettings()?;
        let lookup = |name: &str| xsettings.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        if let Some(XSetting::Int(dpi1024)) = lookup("Xft/DPI")
            && *dpi1024 > 0
        {
            return Ok(dpi_to_scale(f64::from(*dpi1024) / 1024.0));
        }
        if let Some(rm) = self.property(
            self.root,
            self.atoms.resource_manager,
            x11rb::protocol::xproto::AtomEnum::STRING,
            1 << 20,
        )? && let Some(dpi) = parse_xft_dpi(&String::from_utf8_lossy(&rm.value))
        {
            return Ok(dpi_to_scale(dpi));
        }
        if let Some(XSetting::Int(f)) = lookup("Gdk/WindowScalingFactor")
            && *f >= 1
        {
            return Ok(f64::from(*f).clamp(1.0, 8.0));
        }
        Ok(1.0)
    }

    fn read_xsettings(&self) -> X11Result<Vec<(String, XSetting)>> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let owner = self.conn.get_selection_owner(self.xsettings_selection)?.reply()?.owner;
        if owner == x11rb::NONE {
            return Ok(Vec::new());
        }
        let prop = self.property(
            owner,
            self.atoms.xsettings_settings,
            self.atoms.xsettings_settings,
            1 << 20,
        );
        match prop {
            Ok(Some(p)) => Ok(parse_xsettings(&p.value)),
            Ok(None) => Ok(Vec::new()),
            // The manager may exit between the two requests.
            Err(e) if e.is_gone() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xsettings_blob(big: bool, entries: &[(&str, XSetting)]) -> Vec<u8> {
        let w32 = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let w16 = |v: u16| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut b = vec![u8::from(big), 0, 0, 0];
        b.extend(w32(7));
        b.extend(w32(entries.len() as u32));
        for (name, v) in entries {
            b.push(match v {
                XSetting::Int(_) => 0,
                XSetting::String(_) => 1,
                XSetting::Color(_) => 2,
            });
            b.push(0);
            b.extend(w16(name.len() as u16));
            b.extend(name.as_bytes());
            while !b.len().is_multiple_of(4) {
                b.push(0);
            }
            b.extend(w32(1));
            match v {
                XSetting::Int(i) => b.extend(w32(*i as u32)),
                XSetting::String(s) => {
                    b.extend(w32(s.len() as u32));
                    b.extend(s.as_bytes());
                    while !b.len().is_multiple_of(4) {
                        b.push(0);
                    }
                }
                XSetting::Color(c) => {
                    for x in c {
                        b.extend(w16(*x));
                    }
                }
            }
        }
        b
    }

    #[test]
    fn xft_dpi_is_found_among_other_resources() {
        let rm = "Xcursor.size:\t24\nXft.antialias:\t1\nXft.dpi:\t144\nXft.hinting:\t1\n";
        assert_eq!(parse_xft_dpi(rm), Some(144.0));
        assert_eq!(parse_xft_dpi("Xft.dpi: 96.5"), Some(96.5));
        assert_eq!(parse_xft_dpi("Xft.dpi:\tbanana"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: -3"), None);
        assert_eq!(parse_xft_dpi("Xft.dpix: 200\nfoo"), None);
        assert_eq!(parse_xft_dpi(""), None);
    }

    #[test]
    fn xsettings_roundtrip_both_byte_orders() {
        let entries = [
            ("Net/ThemeName", XSetting::String("Adwaita".into())),
            ("Xft/DPI", XSetting::Int(192 * 1024)),
            ("Gdk/WindowScalingFactor", XSetting::Int(2)),
            ("Some/Color", XSetting::Color([1, 2, 3, 65535])),
        ];
        for big in [false, true] {
            let parsed = parse_xsettings(&xsettings_blob(big, &entries));
            assert_eq!(parsed.len(), 4, "big={big}");
            assert_eq!(parsed[1], ("Xft/DPI".to_owned(), XSetting::Int(192 * 1024)));
            assert_eq!(parsed[0].1, XSetting::String("Adwaita".into()));
            assert_eq!(parsed[3].1, XSetting::Color([1, 2, 3, 65535]));
        }
    }

    #[test]
    fn truncated_xsettings_never_panic() {
        let blob = xsettings_blob(false, &[("Xft/DPI", XSetting::Int(98304))]);
        for cut in 0..blob.len() {
            let _ = parse_xsettings(&blob[..cut]);
        }
        assert!(parse_xsettings(&blob[..blob.len() - 1]).is_empty());
        // A claimed count far larger than the data must not loop or allocate wildly.
        let mut evil = vec![0, 0, 0, 0, 0, 0, 0, 0];
        evil.extend(u32::MAX.to_le_bytes());
        assert!(parse_xsettings(&evil).is_empty());
    }

    #[test]
    fn dpi_maps_to_scale() {
        assert!((dpi_to_scale(96.0) - 1.0).abs() < 1e-9);
        assert!((dpi_to_scale(96.5) - 1.0).abs() < 1e-9, "snaps near 1.0");
        assert!((dpi_to_scale(144.0) - 1.5).abs() < 1e-9);
        assert!((dpi_to_scale(192.0) - 2.0).abs() < 1e-9);
        assert!((dpi_to_scale(10.0) - 0.5).abs() < 1e-9, "clamped low");
        assert!((dpi_to_scale(10_000.0) - 8.0).abs() < 1e-9, "clamped high");
    }
}
