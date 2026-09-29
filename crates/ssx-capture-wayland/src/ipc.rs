//! Compositor IPC detection and conversion of IPC windows to [`WindowInfo`].
//!
//! Wayland itself has no "list windows with geometry" protocol, so window enumeration is
//! compositor specific. Only sway (i3-ipc, `$SWAYSOCK`) and Hyprland
//! (`$HYPRLAND_INSTANCE_SIGNATURE`) are supported; on any other compositor
//! `capabilities().enumerate_windows` is `false` rather than pretending.

use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use ssx_capture::Result;
use ssx_types::WindowInfo;

use crate::{coords::Layout, hyprland, sway};

/// Which window-enumeration IPC to use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Ipc {
    /// Detect from the environment (`SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`).
    #[default]
    Auto,
    /// Use this sway i3-ipc socket.
    Sway(PathBuf),
    /// Use this Hyprland command socket (`.socket.sock`).
    Hyprland(PathBuf),
    /// No window enumeration.
    Disabled,
}

/// A detected, reachable IPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WindowSource {
    Sway(PathBuf),
    Hyprland(PathBuf),
}

impl WindowSource {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Sway(_) => "sway",
            Self::Hyprland(_) => "hyprland",
        }
    }
}

fn reachable(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Detects the IPC. `env` is injected so the logic is testable; `probe` decides whether a
/// socket counts as usable (a stale `$SWAYSOCK` left in the environment must not make us
/// claim window support).
pub(crate) fn detect(
    choice: &Ipc,
    env: &dyn Fn(&str) -> Option<String>,
    probe: &dyn Fn(&Path) -> bool,
) -> Option<WindowSource> {
    match choice {
        Ipc::Disabled => None,
        Ipc::Sway(p) => probe(p).then(|| WindowSource::Sway(p.clone())),
        Ipc::Hyprland(p) => probe(p).then(|| WindowSource::Hyprland(p.clone())),
        Ipc::Auto => {
            if let Some(sock) = env("SWAYSOCK").filter(|s| !s.is_empty()) {
                let p = PathBuf::from(sock);
                if probe(&p) {
                    return Some(WindowSource::Sway(p));
                }
            }
            let sig = env("HYPRLAND_INSTANCE_SIGNATURE").filter(|s| !s.is_empty())?;
            let rt = env("XDG_RUNTIME_DIR");
            hyprland::socket_candidates(rt.as_deref(), &sig)
                .into_iter()
                .find(|p| probe(p))
                .map(WindowSource::Hyprland)
        }
    }
}

/// [`detect`] against the real environment and filesystem.
pub(crate) fn detect_from_env(choice: &Ipc) -> Option<WindowSource> {
    detect(choice, &|k| std::env::var(k).ok(), &reachable)
}

/// Lists windows, front to back, in desktop coordinates.
pub(crate) fn list(
    source: &WindowSource,
    layout: &Layout,
    timeout: Duration,
) -> Result<Vec<WindowInfo>> {
    match source {
        WindowSource::Sway(sock) => Ok(sway_infos(&sway::windows(sock, timeout)?, layout)),
        WindowSource::Hyprland(sock) => Ok(hypr_infos(&hyprland::windows(sock, timeout)?, layout)),
    }
}

/// Stacking heuristic shared by both compositors: fullscreen first, then floating above
/// tiled, then (per compositor) recency. Windows that are not on screen go last.
fn stacking_key(visible: bool, fullscreen: bool, floating: bool) -> (bool, bool, bool) {
    (!visible, !fullscreen, !floating)
}

pub(crate) fn sway_infos(ws: &[sway::SwayWindow], layout: &Layout) -> Vec<WindowInfo> {
    // Within a group sway lists floating windows bottom to top, so reverse for
    // front-to-back. `sort_by_key` is stable, which keeps tree order for tiled windows.
    let mut idx: Vec<usize> = (0..ws.len()).collect();
    idx.sort_by_key(|&i| {
        let w = &ws[i];
        let order = if w.floating { ws.len() - i } else { i };
        (stacking_key(w.visible, w.fullscreen, w.floating), order)
    });
    idx.into_iter()
        .map(|i| {
            let w = &ws[i];
            WindowInfo {
                id: format!("sway:{}", w.con_id),
                title: w.title.clone(),
                app_name: w.app_id.clone(),
                rect: layout.logical_to_desktop(w.rect),
                minimized: !w.visible,
                focused: w.focused,
            }
        })
        .collect()
}

pub(crate) fn hypr_infos(ws: &[hyprland::HyprWindow], layout: &Layout) -> Vec<WindowInfo> {
    let mut order: Vec<&hyprland::HyprWindow> = ws.iter().collect();
    order.sort_by_key(|w| (stacking_key(w.visible, w.fullscreen, w.floating), w.focus_rank));
    order
        .into_iter()
        .map(|w| WindowInfo {
            id: format!("hyprland:{}", w.address),
            title: w.title.clone(),
            app_name: w.class.clone(),
            rect: layout.logical_to_desktop(w.rect),
            minimized: !w.visible,
            focused: w.focused,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ssx_types::{Rect, Size};

    use super::*;
    use crate::{coords::OutputGeom, transform::Transform};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn detects_sway_from_env_only_if_reachable() {
        let env = env_of(&[("SWAYSOCK", "/run/sway.sock")]);
        let yes = |_: &Path| true;
        let no = |_: &Path| false;
        assert_eq!(
            detect(&Ipc::Auto, &env, &yes),
            Some(WindowSource::Sway("/run/sway.sock".into()))
        );
        assert_eq!(detect(&Ipc::Auto, &env, &no), None, "stale SWAYSOCK");
    }

    #[test]
    fn detects_hyprland_and_tries_both_socket_layouts() {
        let env = env_of(&[
            ("HYPRLAND_INSTANCE_SIGNATURE", "sig"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        // Only the legacy /tmp location exists.
        let legacy = |p: &Path| p.starts_with("/tmp/hypr");
        assert_eq!(
            detect(&Ipc::Auto, &env, &legacy),
            Some(WindowSource::Hyprland("/tmp/hypr/sig/.socket.sock".into()))
        );
        let modern = |p: &Path| p.starts_with("/run/user/1000");
        assert_eq!(
            detect(&Ipc::Auto, &env, &modern),
            Some(WindowSource::Hyprland("/run/user/1000/hypr/sig/.socket.sock".into()))
        );
    }

    #[test]
    fn sway_wins_when_both_are_set_and_nothing_else_means_none() {
        let env = env_of(&[("SWAYSOCK", "/s"), ("HYPRLAND_INSTANCE_SIGNATURE", "h")]);
        assert_eq!(detect(&Ipc::Auto, &env, &|_| true), Some(WindowSource::Sway("/s".into())));
        assert_eq!(detect(&Ipc::Auto, &env_of(&[]), &|_| true), None);
        assert_eq!(detect(&Ipc::Disabled, &env, &|_| true), None);
        assert_eq!(
            detect(&Ipc::Sway("/x".into()), &env_of(&[]), &|_| true),
            Some(WindowSource::Sway("/x".into()))
        );
    }

    fn layout_2x() -> Layout {
        Layout::build(
            &[OutputGeom {
                id: "A".into(),
                logical: Rect::new(0, 0, 400, 300),
                mode: Size::new(800, 600),
                transform: Transform::Normal,
                int_scale: 2,
            }],
            None,
        )
    }

    fn sw(id: i64, floating: bool, visible: bool, fullscreen: bool) -> sway::SwayWindow {
        sway::SwayWindow {
            con_id: id,
            title: format!("w{id}"),
            app_id: Some("app".into()),
            pid: Some(id),
            rect: Rect::new(10, 20, 100, 50),
            content_rect: Rect::new(11, 21, 98, 48),
            focused: id == 1,
            visible,
            floating,
            fullscreen,
            xwayland: false,
            workspace: Some("1".into()),
        }
    }

    #[test]
    fn sway_rects_are_scaled_to_desktop_pixels() {
        let infos = sway_infos(&[sw(1, false, true, false)], &layout_2x());
        assert_eq!(infos[0].rect, Rect::new(20, 40, 200, 100));
        assert_eq!(infos[0].id, "sway:1");
        assert!(infos[0].focused && !infos[0].minimized);
    }

    #[test]
    fn sway_stacking_is_fullscreen_then_floating_then_tiled_then_hidden() {
        let ws = [
            sw(1, false, true, false),  // tiled a
            sw(2, false, true, false),  // tiled b
            sw(3, true, true, false),   // floating bottom
            sw(4, true, true, false),   // floating top
            sw(5, false, true, true),   // fullscreen
            sw(6, false, false, false), // hidden
        ];
        let ids: Vec<String> = sway_infos(&ws, &layout_2x()).into_iter().map(|w| w.id).collect();
        assert_eq!(ids, ["sway:5", "sway:4", "sway:3", "sway:1", "sway:2", "sway:6"]);
        let hidden = sway_infos(&ws, &layout_2x());
        assert!(hidden.last().is_some_and(|w| w.minimized));
    }

    #[test]
    fn hypr_rects_and_order() {
        let vis = hyprland::parse_monitors(
            br#"[{"id":0,"activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0}}]"#,
        )
        .unwrap();
        let ws = hyprland::parse_clients(
            br#"[
              {"address":"0xa","at":[10,20],"size":[100,50],"workspace":{"id":1},"floating":false,"focusHistoryID":1},
              {"address":"0xb","at":[0,0],"size":[10,10],"workspace":{"id":1},"floating":true,"focusHistoryID":0},
              {"address":"0xc","at":[0,0],"size":[10,10],"workspace":{"id":2},"floating":false,"focusHistoryID":2}
            ]"#,
            &vis,
        )
        .unwrap();
        let infos = hypr_infos(&ws, &layout_2x());
        let ids: Vec<&str> = infos.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["hyprland:0xb", "hyprland:0xa", "hyprland:0xc"]);
        assert_eq!(infos[1].rect, Rect::new(20, 40, 200, 100));
        assert!(infos[2].minimized);
    }
}
