//! Hyprland window enumeration via its command socket (`j/clients`, `j/monitors`).
//!
//! **Not verified against a live Hyprland** (it cannot be installed in the development
//! environment). The parsers are written against the documented JSON of
//! `hyprctl -j clients` / `hyprctl -j monitors` (Hyprland 0.4x) and exercised with embedded
//! fixtures; unknown or missing fields are tolerated, which is why every field is optional.
//!
//! Coordinates: a client's `at`/`size` are in the global layout space, which is the same
//! *logical* space `wl_output`/xdg-output positions use (monitor `x`/`y` are logical,
//! monitor `width`/`height` are physical pixels). They are mapped to desktop pixels with
//! the same [`crate::coords::Layout`] as sway windows.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use ssx_capture::{CaptureError, Result};
use ssx_types::Rect;

#[derive(Debug, Default, Clone, Deserialize)]
struct WorkspaceRef {
    #[serde(default)]
    id: i64,
}

/// Fullscreen state was a bool in old releases and a number (0/1/2) in newer ones.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FullscreenField {
    Bool(bool),
    Num(i64),
}

impl FullscreenField {
    fn is_fullscreen(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Num(n) => *n != 0,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Client {
    #[serde(default)]
    address: String,
    #[serde(default)]
    mapped: Option<bool>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    at: Vec<i64>,
    #[serde(default)]
    size: Vec<i64>,
    #[serde(default)]
    workspace: WorkspaceRef,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    monitor: Option<i64>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "initialClass")]
    initial_class: Option<String>,
    #[serde(default, rename = "initialTitle")]
    initial_title: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    xwayland: bool,
    #[serde(default)]
    fullscreen: Option<FullscreenField>,
    #[serde(default, rename = "focusHistoryID")]
    focus_history_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct MonitorJson {
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: WorkspaceRef,
    #[serde(default, rename = "specialWorkspace")]
    special_workspace: WorkspaceRef,
    #[serde(default)]
    disabled: bool,
}

/// A window as Hyprland describes it, in logical global coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct HyprWindow {
    /// The window address, e.g. `0x5580d0c3b8f0`.
    pub address: String,
    pub title: String,
    pub class: Option<String>,
    pub pid: Option<i64>,
    pub rect: Rect,
    /// `focusHistoryID == 0`: the most recently focused window.
    pub focused: bool,
    pub floating: bool,
    pub fullscreen: bool,
    pub xwayland: bool,
    /// Mapped, not hidden, and its workspace is showing on some monitor.
    pub visible: bool,
    /// Focus recency rank, 0 = most recent (used for stacking order).
    pub focus_rank: i64,
}

/// Which workspaces are on screen, from `j/monitors`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisibleWorkspaces {
    ids: Vec<i64>,
}

/// Parses `j/monitors` into the set of workspace ids currently displayed.
pub fn parse_monitors(json: &[u8]) -> Result<VisibleWorkspaces> {
    let ms: Vec<MonitorJson> = serde_json::from_slice(json).map_err(|e| {
        CaptureError::backend("wayland", format!("Hyprland j/monitors: bad JSON: {e}"))
    })?;
    let mut ids = Vec::new();
    for m in ms.iter().filter(|m| !m.disabled) {
        ids.push(m.active_workspace.id);
        // Special workspaces (scratchpads) use negative ids; 0 means "none showing".
        if m.special_workspace.id != 0 {
            ids.push(m.special_workspace.id);
        }
    }
    Ok(VisibleWorkspaces { ids })
}

/// Parses `j/clients`. `visible` decides, per workspace id, whether it is on screen.
pub fn parse_clients(json: &[u8], visible: &VisibleWorkspaces) -> Result<Vec<HyprWindow>> {
    let cs: Vec<Client> = serde_json::from_slice(json).map_err(|e| {
        CaptureError::backend("wayland", format!("Hyprland j/clients: bad JSON: {e}"))
    })?;
    let mut out = Vec::new();
    for c in cs {
        // Unmapped clients are not windows yet (or any more).
        if c.mapped == Some(false) {
            continue;
        }
        let (x, y) = (c.at.first().copied().unwrap_or(0), c.at.get(1).copied().unwrap_or(0));
        let (w, h) = (c.size.first().copied().unwrap_or(0), c.size.get(1).copied().unwrap_or(0));
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let rect = Rect::new(clamp(x), clamp(y), clamp(w.max(0)) as u32, clamp(h.max(0)) as u32);
        let title = c.title.filter(|t| !t.is_empty()).or(c.initial_title).unwrap_or_default();
        let class = c.class.filter(|t| !t.is_empty()).or(c.initial_class);
        let on_screen = !c.hidden && c.monitor != Some(-1) && visible.ids.contains(&c.workspace.id);
        let rank = c.focus_history_id.unwrap_or(i64::MAX);
        out.push(HyprWindow {
            address: c.address,
            title,
            class,
            pid: c.pid,
            rect,
            focused: rank == 0,
            floating: c.floating,
            fullscreen: c.fullscreen.is_some_and(|f| f.is_fullscreen()),
            xwayland: c.xwayland,
            visible: on_screen,
            focus_rank: rank,
        });
    }
    Ok(out)
}

/// Candidate command-socket paths for an instance signature, newest layout first.
///
/// Hyprland moved its sockets from `/tmp/hypr/<sig>/` to `$XDG_RUNTIME_DIR/hypr/<sig>/`
/// (0.40); both are tried.
pub fn socket_candidates(runtime_dir: Option<&str>, signature: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(rt) = runtime_dir.filter(|s| !s.is_empty()) {
        v.push(Path::new(rt).join("hypr").join(signature).join(".socket.sock"));
    }
    v.push(Path::new("/tmp/hypr").join(signature).join(".socket.sock"));
    v
}

/// Sends one command (e.g. `j/clients`) and reads the reply until the peer closes.
pub fn request(socket: &Path, command: &str, timeout: Duration) -> Result<Vec<u8>> {
    let io = |what: &str, e: std::io::Error| {
        CaptureError::backend("wayland", format!("Hyprland IPC {what} ({}): {e}", socket.display()))
    };
    let mut s = UnixStream::connect(socket).map_err(|e| io("connect", e))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| io("set timeout", e))?;
    s.set_write_timeout(Some(timeout)).map_err(|e| io("set timeout", e))?;
    s.write_all(command.as_bytes()).map_err(|e| io("write", e))?;
    let mut buf = Vec::new();
    // Bounded read: a broken peer must not make us allocate without limit.
    s.take(64 * 1024 * 1024).read_to_end(&mut buf).map_err(|e| io("read", e))?;
    Ok(buf)
}

/// Fetches windows (monitors first, to know which workspaces are visible).
pub fn windows(socket: &Path, timeout: Duration) -> Result<Vec<HyprWindow>> {
    let visible = parse_monitors(&request(socket, "j/monitors", timeout)?)?;
    parse_clients(&request(socket, "j/clients", timeout)?, &visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of `hyprctl -j monitors` (Hyprland 0.4x), trimmed: two monitors, one with a
    /// special workspace showing.
    const MONITORS: &str = r#"[
      {"id":0,"name":"DP-1","description":"Dell Inc. DELL U2723QE","make":"Dell Inc.",
       "model":"DELL U2723QE","serial":"ABC","width":3840,"height":2160,"physicalWidth":597,
       "physicalHeight":336,"refreshRate":59.99700,"x":0,"y":0,
       "activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""},
       "reserved":[0,30,0,0],"scale":2.00,"transform":0,"focused":true,"dpmsStatus":true,
       "vrr":false,"solitary":"0","activelyTearing":false,"directScanoutTo":"0",
       "disabled":false,"currentFormat":"XRGB8888","mirrorOf":"none","availableModes":["3840x2160@59.997Hz"]},
      {"id":1,"name":"HDMI-A-1","description":"LG","make":"LG","model":"LG","serial":"",
       "width":1920,"height":1080,"physicalWidth":527,"physicalHeight":296,"refreshRate":60.0,
       "x":1920,"y":0,"activeWorkspace":{"id":4,"name":"4"},
       "specialWorkspace":{"id":-98,"name":"special:scratch"},"reserved":[0,0,0,0],
       "scale":1.00,"transform":0,"focused":false,"dpmsStatus":true,"disabled":false}
    ]"#;

    /// Shape of `hyprctl -j clients`: tiled, floating, xwayland, hidden, other workspace,
    /// unmapped, and one with the old boolean `fullscreen`.
    const CLIENTS: &str = r#"[
      {"address":"0x5580d0c3b8f0","mapped":true,"hidden":false,"at":[10,40],"size":[1900,1030],
       "workspace":{"id":1,"name":"1"},"floating":false,"pseudo":false,"monitor":0,
       "class":"kitty","title":"~/ssx","initialClass":"kitty","initialTitle":"kitty",
       "pid":1234,"xwayland":false,"pinned":false,"fullscreen":0,"fullscreenClient":0,
       "grouped":[],"tags":[],"swallowing":"0x0","focusHistoryID":0,"inhibitingIdle":false},
      {"address":"0x5580d0c3c000","mapped":true,"hidden":false,"at":[200,300],"size":[800,600],
       "workspace":{"id":1,"name":"1"},"floating":true,"monitor":0,
       "class":"firefox","title":"Mozilla Firefox","initialClass":"firefox",
       "initialTitle":"Mozilla Firefox","pid":2345,"xwayland":false,"fullscreen":0,
       "focusHistoryID":1},
      {"address":"0x5580d0c3d000","mapped":true,"hidden":false,"at":[1930,10],"size":[1000,700],
       "workspace":{"id":4,"name":"4"},"floating":false,"monitor":1,
       "class":"steam","title":"","initialClass":"steam","initialTitle":"Steam",
       "pid":3456,"xwayland":true,"fullscreen":2,"focusHistoryID":2},
      {"address":"0x5580d0c3e000","mapped":true,"hidden":true,"at":[0,0],"size":[100,100],
       "workspace":{"id":1,"name":"1"},"floating":false,"monitor":0,"class":"grouped-tab",
       "title":"hidden tab","pid":4567,"xwayland":false,"fullscreen":false,"focusHistoryID":3},
      {"address":"0x5580d0c3f000","mapped":true,"hidden":false,"at":[0,0],"size":[500,500],
       "workspace":{"id":7,"name":"7"},"floating":false,"monitor":0,"class":"mpv",
       "title":"video.mkv","pid":5678,"xwayland":false,"fullscreen":true,"focusHistoryID":4},
      {"address":"0x5580d0c40000","mapped":false,"hidden":false,"at":[0,0],"size":[1,1],
       "workspace":{"id":1,"name":"1"},"floating":false,"monitor":0,"class":"ghost",
       "title":"unmapped","pid":6789,"xwayland":false,"fullscreen":0,"focusHistoryID":5},
      {"address":"0x5580d0c41000","mapped":true,"hidden":false,"at":[5,5],"size":[50,50],
       "workspace":{"id":-98,"name":"special:scratch"},"floating":true,"monitor":1,
       "class":"scratch","title":"special","pid":7890,"xwayland":false,"fullscreen":0,
       "focusHistoryID":6}
    ]"#;

    fn parsed() -> Vec<HyprWindow> {
        let vis = parse_monitors(MONITORS.as_bytes()).unwrap();
        parse_clients(CLIENTS.as_bytes(), &vis).unwrap()
    }

    fn by_addr<'a>(ws: &'a [HyprWindow], a: &str) -> &'a HyprWindow {
        ws.iter().find(|w| w.address == a).unwrap_or_else(|| panic!("{a} missing"))
    }

    #[test]
    fn visible_workspaces_include_special_when_showing() {
        let v = parse_monitors(MONITORS.as_bytes()).unwrap();
        assert_eq!(v.ids, vec![1, 4, -98]);
    }

    #[test]
    fn unmapped_clients_are_dropped() {
        let ws = parsed();
        assert!(ws.iter().all(|w| w.address != "0x5580d0c40000"));
        assert_eq!(ws.len(), 6);
    }

    #[test]
    fn tiled_window_fields() {
        let ws = parsed();
        let k = by_addr(&ws, "0x5580d0c3b8f0");
        assert_eq!(k.title, "~/ssx");
        assert_eq!(k.class.as_deref(), Some("kitty"));
        assert_eq!(k.rect, Rect::new(10, 40, 1900, 1030));
        assert!(k.focused && k.visible && !k.floating && !k.xwayland && !k.fullscreen);
        assert_eq!(k.pid, Some(1234));
    }

    #[test]
    fn floating_xwayland_fullscreen_flags() {
        let ws = parsed();
        assert!(by_addr(&ws, "0x5580d0c3c000").floating);
        let s = by_addr(&ws, "0x5580d0c3d000");
        assert!(s.xwayland && s.fullscreen, "numeric fullscreen=2");
        assert_eq!(s.title, "Steam", "empty title falls back to initialTitle");
        assert!(by_addr(&ws, "0x5580d0c3f000").fullscreen, "old boolean fullscreen");
    }

    #[test]
    fn visibility_follows_hidden_flag_and_workspace() {
        let ws = parsed();
        assert!(!by_addr(&ws, "0x5580d0c3e000").visible, "hidden (grouped tab)");
        assert!(!by_addr(&ws, "0x5580d0c3f000").visible, "workspace 7 is not showing");
        assert!(by_addr(&ws, "0x5580d0c3d000").visible, "workspace 4 shows on HDMI-A-1");
        assert!(by_addr(&ws, "0x5580d0c41000").visible, "special workspace is showing");
    }

    #[test]
    fn focus_rank_orders_stacking() {
        let ws = parsed();
        assert_eq!(by_addr(&ws, "0x5580d0c3c000").focus_rank, 1);
        assert_eq!(by_addr(&ws, "0x5580d0c3b8f0").focus_rank, 0);
    }

    #[test]
    fn tolerates_missing_and_extra_fields() {
        let v = VisibleWorkspaces { ids: vec![1] };
        let ws = parse_clients(br#"[{"address":"0x1","brand_new_field":{"a":1}}]"#, &v).unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].rect, Rect::new(0, 0, 0, 0));
        assert!(!ws[0].visible, "workspace id 0 is not showing");
        assert_eq!(parse_clients(b"[]", &v).unwrap(), vec![]);
    }

    #[test]
    fn negative_geometry_is_kept_and_sizes_clamped() {
        let v = VisibleWorkspaces { ids: vec![1] };
        let json = br#"[{"address":"0x1","at":[-1920,-20],"size":[-5,100],"workspace":{"id":1}}]"#;
        let ws = parse_clients(json, &v).unwrap();
        assert_eq!(ws[0].rect, Rect::new(-1920, -20, 0, 100));
    }

    #[test]
    fn garbage_is_an_error() {
        let v = VisibleWorkspaces::default();
        assert!(parse_clients(b"{not json", &v).is_err());
        assert!(parse_clients(b"", &v).is_err());
        assert!(parse_monitors(b"null").is_err());
    }

    #[test]
    fn socket_paths_cover_old_and_new_layouts() {
        let c = socket_candidates(Some("/run/user/1000"), "abc_123_456");
        assert_eq!(c[0], Path::new("/run/user/1000/hypr/abc_123_456/.socket.sock"));
        assert_eq!(c[1], Path::new("/tmp/hypr/abc_123_456/.socket.sock"));
        assert_eq!(socket_candidates(None, "sig").len(), 1);
    }
}
