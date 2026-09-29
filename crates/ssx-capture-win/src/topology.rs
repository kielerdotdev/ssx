//! Joining GDI monitors with `DisplayConfig` targets (pure logic).
//!
//! Windows exposes displays through two unrelated APIs. GDI (`EnumDisplayMonitors`) knows
//! rectangles and DPI, keyed by a device name such as `\\.\DISPLAY2`. `DisplayConfig` knows
//! the friendly name, refresh rate, and HDR/advanced-colour state, keyed by
//! adapter LUID + target id. The bridge is `DISPLAYCONFIG_SOURCE_DEVICE_NAME`, which
//! returns the GDI device name of a path's *source*. The glue in `display.rs` resolves
//! each active path to a [`DisplayPath`]; this module does the matching.

use crate::hdr::AdvancedColor;

/// One active `DisplayConfig` path, already resolved to plain data.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DisplayPath {
    /// GDI device name of the path's source, e.g. `\\.\DISPLAY1`.
    pub(crate) gdi_device_name: String,
    /// `monitorFriendlyDeviceName` of the target; empty if the monitor has no EDID name.
    pub(crate) friendly_name: String,
    pub(crate) refresh_hz: Option<f32>,
    /// `None` if the advanced-colour query failed (e.g. Windows older than 1709).
    pub(crate) advanced_color: Option<AdvancedColor>,
    /// Raw `SDRWhiteLevel`, `None` if the query failed.
    pub(crate) raw_sdr_white: Option<u32>,
}

/// Finds the path whose source is the GDI device `gdi_device_name` (case-insensitive).
///
/// In clone mode several paths share one source; the first one wins, which is also the
/// one Windows lists first (the primary target of the clone group).
pub(crate) fn find_path<'a>(
    paths: &'a [DisplayPath],
    gdi_device_name: &str,
) -> Option<&'a DisplayPath> {
    paths.iter().find(|p| p.gdi_device_name.eq_ignore_ascii_case(gdi_device_name))
}

/// Human-readable monitor name: the friendly name if there is one, else the GDI device
/// name without its `\\.\` prefix (`DISPLAY1`).
pub(crate) fn display_name(friendly_name: &str, gdi_device_name: &str) -> String {
    let friendly = friendly_name.trim();
    if !friendly.is_empty() {
        return friendly.to_owned();
    }
    gdi_device_name.trim_start_matches(r"\\.\").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(gdi: &str, friendly: &str) -> DisplayPath {
        DisplayPath {
            gdi_device_name: gdi.into(),
            friendly_name: friendly.into(),
            refresh_hz: Some(60.0),
            advanced_color: None,
            raw_sdr_white: None,
        }
    }

    #[test]
    fn matches_by_gdi_name_case_insensitively() {
        let paths = [path(r"\\.\DISPLAY1", "A"), path(r"\\.\DISPLAY2", "B")];
        assert_eq!(find_path(&paths, r"\\.\DISPLAY2").map(|p| p.friendly_name.as_str()), Some("B"));
        assert_eq!(find_path(&paths, r"\\.\display1").map(|p| p.friendly_name.as_str()), Some("A"));
        assert!(find_path(&paths, r"\\.\DISPLAY3").is_none());
        assert!(find_path(&[], r"\\.\DISPLAY1").is_none());
    }

    #[test]
    fn clone_mode_takes_the_first_path() {
        let paths = [path(r"\\.\DISPLAY1", "First"), path(r"\\.\DISPLAY1", "Second")];
        assert_eq!(
            find_path(&paths, r"\\.\DISPLAY1").map(|p| p.friendly_name.as_str()),
            Some("First")
        );
    }

    #[test]
    fn names_fall_back_to_the_device_name() {
        assert_eq!(display_name("DELL U2723QE", r"\\.\DISPLAY1"), "DELL U2723QE");
        assert_eq!(display_name("  ", r"\\.\DISPLAY2"), "DISPLAY2");
        assert_eq!(display_name("", r"\\.\DISPLAY3"), "DISPLAY3");
        assert_eq!(display_name("", "weird"), "weird");
    }
}
