//! `ssx monitors` and `ssx windows`.

use ssx_types::{Monitor, Rect, WindowInfo};

use crate::{
    app::App,
    cli::ListArgs,
    error::CliResult,
    output::{Style, Table, out_text},
};

/// `x,y WxH`.
pub fn rect_text(r: Rect) -> String {
    format!("{},{} {}x{}", r.x, r.y, r.width, r.height)
}

/// HDR column text for a monitor.
pub fn hdr_text(m: &Monitor) -> String {
    match m.hdr {
        None => "unknown".to_owned(),
        Some(h) if h.active => format!("on ({} nits SDR white)", h.sdr_white_nits.round()),
        Some(_) => "off".to_owned(),
    }
}

/// The monitor table.
pub fn render_monitors(monitors: &[Monitor], style: Style) -> String {
    let mut t = Table::new(["ID", "NAME", "RECT", "SCALE", "PRIMARY", "HZ", "HDR"]);
    for m in monitors {
        t.row([
            m.id.clone(),
            m.name.clone(),
            rect_text(m.rect),
            format!("{:.2}", m.scale_factor),
            if m.primary { "yes" } else { "" }.to_owned(),
            m.refresh_hz.map_or_else(String::new, |h| format!("{h:.0}")),
            hdr_text(m),
        ]);
    }
    t.render(style)
}

/// Shortens `s` to at most `max` characters, marking the cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// The window table.
pub fn render_windows(windows: &[WindowInfo], style: Style) -> String {
    let mut t = Table::new(["ID", "TITLE", "APP", "RECT", "STATE"]);
    for w in windows {
        let state = match (w.focused, w.minimized) {
            (true, _) => "focused",
            (false, true) => "minimized",
            _ => "",
        };
        t.row([
            w.id.clone(),
            truncate(&w.title.replace('\n', " "), 48),
            w.app_name.clone().unwrap_or_default(),
            rect_text(w.rect),
            state.to_owned(),
        ]);
    }
    t.render(style)
}

/// `ssx monitors`.
pub fn monitors(app: &App, args: &ListArgs) -> CliResult<()> {
    let settings = app.load_settings()?;
    let services = app.services(&settings)?;
    let list = services.capturer.monitors()?;
    if args.json {
        out_text(&format!("{}\n", serde_json::to_string_pretty(&list)?));
    } else if list.is_empty() {
        crate::output::err_line("no monitors were reported");
    } else {
        out_text(&render_monitors(&list, app.out));
    }
    Ok(())
}

/// `ssx windows`.
pub fn windows(app: &App, args: &ListArgs) -> CliResult<()> {
    let settings = app.load_settings()?;
    let services = app.services(&settings)?;
    let list = services.capturer.windows()?;
    if args.json {
        out_text(&format!("{}\n", serde_json::to_string_pretty(&list)?));
    } else if list.is_empty() {
        crate::output::err_line("no windows were reported");
    } else {
        out_text(&render_windows(&list, app.out));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ssx_types::HdrInfo;

    use super::*;

    fn monitor(id: &str, x: i32, hdr: Option<HdrInfo>) -> Monitor {
        Monitor {
            id: id.into(),
            name: format!("Display {id}"),
            rect: Rect::new(x, 0, 1920, 1080),
            scale_factor: 1.5,
            primary: x == 0,
            refresh_hz: Some(59.94),
            hdr,
        }
    }

    #[test]
    fn monitor_table_shows_scale_and_hdr_state() {
        let out = render_monitors(
            &[
                monitor(
                    "A",
                    0,
                    Some(HdrInfo { active: true, sdr_white_nits: 203.0, max_luminance_nits: None }),
                ),
                monitor("B", -1920, Some(HdrInfo::SDR)),
                monitor("C", 1920, None),
            ],
            Style::plain(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("ID") && lines[0].contains("HDR"));
        assert!(
            lines[1].contains("1.50")
                && lines[1].contains("yes")
                && lines[1].contains("on (203 nits SDR white)"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("-1920,0 1920x1080") && lines[2].ends_with("off"),
            "{}",
            lines[2]
        );
        assert!(lines[3].ends_with("unknown"));
        assert!(lines[1].contains(" 60 "), "refresh is rounded: {}", lines[1]);
    }

    #[test]
    fn window_table_marks_state_and_shortens_titles() {
        let w = |id: &str, title: &str, focused, minimized| WindowInfo {
            id: id.into(),
            title: title.into(),
            app_name: Some("app".into()),
            rect: Rect::new(1, 2, 3, 4),
            minimized,
            focused,
        };
        let out = render_windows(
            &[
                w("1", "short", true, false),
                w("2", &"x".repeat(200), false, true),
                w("3", "two\nlines", false, false),
            ],
            Style::plain(),
        );
        assert!(out.contains("focused") && out.contains("minimized"));
        assert!(out.contains('\u{2026}'), "long titles are cut");
        assert!(out.contains("two lines"), "newlines never break the table");
        assert!(out.lines().all(|l| l.chars().count() < 120));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("héllo wörld", 5), "héll\u{2026}");
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("", 0), "");
    }
}
