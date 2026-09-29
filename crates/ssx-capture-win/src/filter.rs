//! Window-list filtering and small string helpers (pure logic).
//!
//! `EnumWindows` returns every top-level window, including hundreds of invisible helper
//! windows. A capture picker wants roughly what Alt-Tab shows, so [`is_alt_tab_candidate`]
//! encodes those rules over plain facts gathered by the (thin, Windows-only) glue in
//! `windows.rs`.
//!
//! One deliberate difference from Alt-Tab: *owned* windows (dialogs) are kept, because
//! "capture that dialog" is a legitimate request. Cloaked windows (UWP apps suspended in
//! the background, windows on other virtual desktops) are dropped: they report
//! `WS_VISIBLE` but have no pixels.

use ssx_types::Rect;

/// Extended window styles that matter for the filter.
pub(crate) mod ex_style {
    /// `WS_EX_TOOLWINDOW`: floating toolbars, not shown in Alt-Tab.
    pub(crate) const TOOLWINDOW: u32 = 0x0000_0080;
    /// `WS_EX_APPWINDOW`: forces a top-level window onto the taskbar/Alt-Tab.
    pub(crate) const APPWINDOW: u32 = 0x0004_0000;
    /// `WS_EX_NOACTIVATE`: never takes focus (tooltips, IME candidate lists, overlays).
    pub(crate) const NOACTIVATE: u32 = 0x0800_0000;
}

/// Window class names of the shell's own top-level windows. These are visible, titled in
/// some builds, and never something a user means to screenshot as "a window".
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Shell_InputSwitchTopLevelWindow",
    "Windows.UI.Core.CoreWindow",
    "NotifyIconOverflowWindow",
    "XamlExplorerHostIslandWindow",
];

/// `true` for shell / desktop window classes (case-insensitive).
pub(crate) fn is_shell_class(class_name: &str) -> bool {
    SHELL_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(class_name))
}

/// Everything the filter needs to know about one window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowFacts<'a> {
    /// `IsWindowVisible`.
    pub(crate) visible: bool,
    /// `DWMWA_CLOAKED != 0`.
    pub(crate) cloaked: bool,
    pub(crate) title: &'a str,
    pub(crate) ex_style: u32,
    pub(crate) class_name: &'a str,
    /// The handle equals `GetShellWindow()` or `GetDesktopWindow()`.
    pub(crate) is_shell_or_desktop: bool,
    pub(crate) minimized: bool,
    /// Extended frame bounds (or the restored position for minimised windows).
    pub(crate) rect: Rect,
}

/// Whether a window belongs in the capture picker.
pub(crate) fn is_alt_tab_candidate(f: &WindowFacts<'_>) -> bool {
    let app_window = f.ex_style & ex_style::APPWINDOW != 0;
    let auxiliary = f.ex_style & (ex_style::TOOLWINDOW | ex_style::NOACTIVATE) != 0;
    f.visible
        && !f.cloaked
        && !f.title.trim().is_empty()
        && !f.is_shell_or_desktop
        && !is_shell_class(f.class_name)
        // Tool windows and never-activated windows are not Alt-Tab entries unless they
        // explicitly ask to be (`WS_EX_APPWINDOW`).
        && (app_window || !auxiliary)
        // A minimised window legitimately has no on-screen area; anything else with an
        // empty rectangle has nothing to capture.
        && (f.minimized || !f.rect.is_empty())
}

/// Whether a window's `GetWindowDisplayAffinity` value stops capture APIs from seeing its
/// content: `WDA_MONITOR` (0x1) makes it black, `WDA_EXCLUDEFROMCAPTURE` (0x11) makes it
/// invisible. Only `WDA_NONE` (0) is capturable. Password managers, DRM players and some
/// banking apps set these.
pub(crate) fn affinity_blocks_capture(affinity: u32) -> bool {
    affinity != 0
}

/// Formats a window handle as the opaque [`ssx_types::WindowInfo::id`]. Decimal, so it is
/// trivially parseable by callers that want to pass a raw `HWND` around.
pub(crate) fn format_window_id(hwnd: isize) -> String {
    hwnd.to_string()
}

/// Parses an id produced by [`format_window_id`]. Rejects empty, non-numeric and zero
/// (the null handle) input.
pub(crate) fn parse_window_id(id: &str) -> Option<isize> {
    id.trim().parse::<isize>().ok().filter(|h| *h != 0)
}

/// Decodes a UTF-16 buffer up to the first NUL (or its end), replacing invalid units.
pub(crate) fn utf16_until_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|u| *u == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// The file-name component of a Windows (or POSIX) path, e.g. `C:\a\b.exe` -> `b.exe`.
pub(crate) fn exe_name_from_path(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WindowFacts<'static> {
        WindowFacts {
            visible: true,
            cloaked: false,
            title: "Untitled - Notepad",
            ex_style: 0,
            class_name: "Notepad",
            is_shell_or_desktop: false,
            minimized: false,
            rect: Rect::new(10, 10, 800, 600),
        }
    }

    #[test]
    fn ordinary_window_is_listed() {
        assert!(is_alt_tab_candidate(&base()));
    }

    #[test]
    fn truth_table_single_disqualifiers() {
        let cases: Vec<(&str, WindowFacts<'static>, bool)> = vec![
            ("invisible", WindowFacts { visible: false, ..base() }, false),
            ("cloaked", WindowFacts { cloaked: true, ..base() }, false),
            ("empty title", WindowFacts { title: "", ..base() }, false),
            ("blank title", WindowFacts { title: "  \t", ..base() }, false),
            ("shell hwnd", WindowFacts { is_shell_or_desktop: true, ..base() }, false),
            ("shell class", WindowFacts { class_name: "Shell_TrayWnd", ..base() }, false),
            ("toolwindow", WindowFacts { ex_style: ex_style::TOOLWINDOW, ..base() }, false),
            (
                "toolwindow with appwindow",
                WindowFacts { ex_style: ex_style::TOOLWINDOW | ex_style::APPWINDOW, ..base() },
                true,
            ),
            ("noactivate", WindowFacts { ex_style: ex_style::NOACTIVATE, ..base() }, false),
            (
                "noactivate with appwindow",
                WindowFacts { ex_style: ex_style::NOACTIVATE | ex_style::APPWINDOW, ..base() },
                true,
            ),
            ("appwindow alone", WindowFacts { ex_style: ex_style::APPWINDOW, ..base() }, true),
            ("empty rect", WindowFacts { rect: Rect::new(0, 0, 0, 0), ..base() }, false),
            ("zero height", WindowFacts { rect: Rect::new(0, 0, 100, 0), ..base() }, false),
            (
                "minimised keeps listing with empty rect",
                WindowFacts { minimized: true, rect: Rect::new(0, 0, 0, 0), ..base() },
                true,
            ),
            ("minimised normal", WindowFacts { minimized: true, ..base() }, true),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(is_alt_tab_candidate(&facts), expected, "{name}");
        }
    }

    #[test]
    fn cloaked_wins_over_appwindow() {
        let f = WindowFacts { cloaked: true, ex_style: ex_style::APPWINDOW, ..base() };
        assert!(!is_alt_tab_candidate(&f));
    }

    #[test]
    fn owned_style_bits_unrelated_to_filter_are_ignored() {
        // WS_EX_TOPMOST (0x8) | WS_EX_WINDOWEDGE (0x100) etc. must not matter.
        let f = WindowFacts { ex_style: 0x0000_0108, ..base() };
        assert!(is_alt_tab_candidate(&f));
    }

    #[test]
    fn shell_classes_match_case_insensitively() {
        assert!(is_shell_class("Progman"));
        assert!(is_shell_class("workerw"));
        assert!(is_shell_class("Windows.UI.Core.CoreWindow"));
        assert!(!is_shell_class("Chrome_WidgetWin_1"));
        assert!(!is_shell_class(""));
    }

    #[test]
    fn display_affinity_gate() {
        assert!(!affinity_blocks_capture(0x0), "WDA_NONE");
        assert!(affinity_blocks_capture(0x1), "WDA_MONITOR");
        assert!(affinity_blocks_capture(0x11), "WDA_EXCLUDEFROMCAPTURE");
    }

    #[test]
    fn window_ids_round_trip() {
        for h in [1isize, 0x0001_0A2C, isize::MAX, -5] {
            assert_eq!(parse_window_id(&format_window_id(h)), Some(h));
        }
        assert_eq!(parse_window_id(" 42 "), Some(42));
    }

    #[test]
    fn bad_window_ids_are_rejected() {
        for bad in ["", "0", "abc", "0x10", "1.5", "99999999999999999999999"] {
            assert_eq!(parse_window_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn utf16_decoding_stops_at_nul() {
        let mut buf = [0u16; 8];
        for (i, u) in "Héllo".encode_utf16().enumerate() {
            buf[i] = u;
        }
        assert_eq!(utf16_until_nul(&buf), "Héllo");
        assert_eq!(utf16_until_nul(&[]), "");
        assert_eq!(utf16_until_nul(&[0, 65]), "");
        let full: Vec<u16> = "abc".encode_utf16().collect();
        assert_eq!(utf16_until_nul(&full), "abc", "no terminator");
        assert_eq!(utf16_until_nul(&[0xD800, 65, 0]), "\u{FFFD}A", "lone surrogate is replaced");
    }

    #[test]
    fn exe_names() {
        assert_eq!(
            exe_name_from_path(r"C:\Windows\System32\notepad.exe").as_deref(),
            Some("notepad.exe")
        );
        assert_eq!(exe_name_from_path("/usr/bin/x").as_deref(), Some("x"));
        assert_eq!(exe_name_from_path("plain.exe").as_deref(), Some("plain.exe"));
        assert_eq!(exe_name_from_path(""), None);
        assert_eq!(exe_name_from_path(r"C:\dir\"), None);
    }
}
