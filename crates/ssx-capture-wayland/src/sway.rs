//! sway (and other i3-ipc speaking wlroots compositors) window enumeration.
//!
//! `GET_TREE` returns the whole container tree. Windows are the leaves: containers of type
//! `con`/`floating_con` without children that carry a `pid` (native Wayland views also
//! have `app_id`, Xwayland views have `window` + `window_properties`).
//!
//! Verified against sway 1.9 (see `tests/sway_live.rs`): every `rect` in the tree is in
//! the **global logical** layout space (the same space as `wl_output.geometry` x/y and
//! xdg-output logical positions), *not* output-relative and *not* scaled by the output
//! scale. `rect` covers borders and content but **not** the title bar, which sits directly
//! above it (`deco_rect.height` tall; `deco_rect` itself is global for floating windows but
//! workspace-relative for tiled ones, so only its height is used); `window_rect` is the
//! client surface relative to `rect`. Hidden windows (inactive workspace, scratchpad) keep
//! meaningless coordinates, so they are flagged rather than trusted.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use serde::Deserialize;
use ssx_capture::{CaptureError, Result};
use ssx_types::Rect;

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_GET_TREE: u32 = 4;
/// Refuse absurd replies rather than allocating what a broken peer claims.
const MAX_REPLY: usize = 64 * 1024 * 1024;

#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct JRect {
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
}

impl JRect {
    fn to_rect(self) -> Rect {
        Rect::new(self.x, self.y, self.width.max(0) as u32, self.height.max(0) as u32)
    }
}

#[derive(Debug, Default, Deserialize)]
struct WindowProps {
    class: Option<String>,
    instance: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Node {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    rect: JRect,
    #[serde(default)]
    window_rect: JRect,
    #[serde(default)]
    deco_rect: JRect,
    #[serde(default)]
    border: Option<String>,
    #[serde(default)]
    layout: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    window: Option<i64>,
    #[serde(default)]
    window_properties: Option<WindowProps>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    visible: Option<bool>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    fullscreen_mode: Option<i32>,
    #[serde(default)]
    scratchpad_state: Option<String>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

/// A window as sway describes it, in logical global coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct SwayWindow {
    /// The container id (`swaymsg [con_id=N]`).
    pub con_id: i64,
    pub title: String,
    /// Wayland `app_id`, or the X11 class for Xwayland windows.
    pub app_id: Option<String>,
    pub pid: Option<i64>,
    /// The window as the user sees it (title bar and borders included) in logical global
    /// coordinates.
    pub rect: Rect,
    /// The client surface only, in logical global coordinates.
    pub content_rect: Rect,
    pub focused: bool,
    /// On screen right now (active workspace, not a hidden scratchpad window).
    pub visible: bool,
    pub floating: bool,
    pub fullscreen: bool,
    pub xwayland: bool,
    pub workspace: Option<String>,
}

struct Ctx<'a> {
    output: Option<&'a str>,
    workspace: Option<&'a str>,
    /// Inside the invisible `__i3` output that holds the scratchpad.
    scratchpad: bool,
    floating: bool,
    /// The parent container is tabbed or stacked (title bars belong to the tab strip).
    in_tab_strip: bool,
}

/// Parses a `GET_TREE` reply into windows, in tree order.
pub fn parse_tree(json: &[u8]) -> Result<Vec<SwayWindow>> {
    let root: Node = serde_json::from_slice(json)
        .map_err(|e| CaptureError::backend("wayland", format!("sway GET_TREE: bad JSON: {e}")))?;
    let mut out = Vec::new();
    walk(
        &root,
        &Ctx {
            output: None,
            workspace: None,
            scratchpad: false,
            floating: false,
            in_tab_strip: false,
        },
        true,
        &mut out,
    );
    Ok(out)
}

fn walk(node: &Node, ctx: &Ctx<'_>, parent_visible: bool, out: &mut Vec<SwayWindow>) {
    let mut next = Ctx { ..*ctx };
    next.in_tab_strip = matches!(node.layout.as_deref(), Some("tabbed" | "stacked"));
    match node.kind.as_str() {
        "output" => {
            next.output = node.name.as_deref();
            next.scratchpad = node.name.as_deref() == Some("__i3");
        }
        "workspace" => {
            next.workspace = node.name.as_deref();
        }
        _ => {}
    }
    // A workspace that is not shown has `visible: false`; its windows are hidden, too.
    let visible_here = match node.kind.as_str() {
        "workspace" => node.visible.unwrap_or(true),
        _ => parent_visible,
    };
    let is_leaf = node.nodes.is_empty() && node.floating_nodes.is_empty();
    if is_leaf
        && matches!(node.kind.as_str(), "con" | "floating_con")
        && (node.pid.is_some() || node.app_id.is_some() || node.window.is_some())
    {
        out.push(to_window(node, ctx, visible_here));
        return;
    }
    for c in &node.nodes {
        walk(c, &next, visible_here, out);
    }
    let mut floating = Ctx { ..next };
    floating.floating = true;
    floating.in_tab_strip = false; // floating containers always have their own title bar
    for c in &node.floating_nodes {
        walk(c, &floating, visible_here, out);
    }
}

fn to_window(node: &Node, ctx: &Ctx<'_>, parent_visible: bool) -> SwayWindow {
    let props = node.window_properties.as_ref();
    let xwayland = node.shell.as_deref() == Some("xwayland") || node.window.is_some();
    let title =
        node.name.clone().or_else(|| props.and_then(|p| p.title.clone())).unwrap_or_default();
    let app_id = node
        .app_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| props.and_then(|p| p.class.clone()).filter(|s| !s.is_empty()))
        .or_else(|| props.and_then(|p| p.instance.clone()).filter(|s| !s.is_empty()));
    let base = node.rect.to_rect();
    let mut rect = base;
    // sway's `rect` covers borders and content but *not* the title bar, which sits
    // directly above it (`deco_rect.height` tall). Include it so the rectangle is what the
    // user sees as "the window". Title bars inside a tabbed/stacked strip are shared
    // between siblings, so those windows keep the plain container rect.
    let deco_h = node.deco_rect.height.max(0);
    if node.border.as_deref() == Some("normal") && deco_h > 0 && !ctx.in_tab_strip {
        rect = Rect::new(
            rect.x,
            rect.y.saturating_sub(deco_h),
            rect.width,
            rect.height + deco_h as u32,
        );
    }
    let wr = node.window_rect;
    let content_rect = Rect::new(
        base.x.saturating_add(wr.x),
        base.y.saturating_add(wr.y),
        wr.width.max(0) as u32,
        wr.height.max(0) as u32,
    );
    let hidden_scratch = ctx.scratchpad
        || matches!(node.scratchpad_state.as_deref(), Some("fresh" | "changed"))
            && node.visible == Some(false);
    SwayWindow {
        con_id: node.id,
        title,
        app_id,
        pid: node.pid,
        rect,
        content_rect,
        focused: node.focused,
        visible: parent_visible && node.visible.unwrap_or(true) && !hidden_scratch,
        floating: ctx.floating || node.kind == "floating_con",
        fullscreen: node.fullscreen_mode.unwrap_or(0) != 0,
        xwayland,
        workspace: ctx.workspace.map(str::to_owned),
    }
}

/// Sends one i3-ipc request and returns the raw reply payload.
pub fn request(socket: &Path, msg_type: u32, payload: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let io = |what: &str, e: std::io::Error| {
        CaptureError::backend("wayland", format!("sway IPC {what} ({}): {e}", socket.display()))
    };
    let mut s = UnixStream::connect(socket).map_err(|e| io("connect", e))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| io("set timeout", e))?;
    s.set_write_timeout(Some(timeout)).map_err(|e| io("set timeout", e))?;
    let mut msg = Vec::with_capacity(14 + payload.len());
    msg.extend_from_slice(IPC_MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&msg_type.to_ne_bytes());
    msg.extend_from_slice(payload);
    s.write_all(&msg).map_err(|e| io("write", e))?;
    let mut header = [0u8; 14];
    s.read_exact(&mut header).map_err(|e| io("read header", e))?;
    if &header[..6] != IPC_MAGIC {
        return Err(CaptureError::backend("wayland", "sway IPC: reply lacks the i3-ipc magic"));
    }
    let len = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]) as usize;
    if len > MAX_REPLY {
        return Err(CaptureError::backend("wayland", format!("sway IPC: {len} byte reply")));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).map_err(|e| io("read body", e))?;
    Ok(body)
}

/// Fetches and parses the window list.
pub fn windows(socket: &Path, timeout: Duration) -> Result<Vec<SwayWindow>> {
    parse_tree(&request(socket, IPC_GET_TREE, b"", timeout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed but structurally faithful `swaymsg -t get_tree` from sway 1.9 with two
    /// outputs (left 1x 800x600, right 2x 640x480 = logical 320x240 at x=800), a tiled
    /// foot on workspace 1, a floating firefox on workspace 1, a native window on the
    /// right output, an Xwayland window, a window on a hidden workspace and a hidden
    /// scratchpad window.
    const TREE: &str = r#"{
      "id": 1, "type": "root", "name": "root", "focused": false,
      "rect": {"x":0,"y":0,"width":1120,"height":600},
      "nodes": [
        {"id": 2147483647, "type": "output", "name": "__i3", "focused": false,
         "rect": {"x":0,"y":0,"width":1120,"height":600},
         "nodes": [
           {"id": 2147483646, "type": "workspace", "name": "__i3_scratch", "visible": false,
            "rect": {"x":0,"y":0,"width":1120,"height":600}, "nodes": [],
            "floating_nodes": [
              {"id": 90, "type": "floating_con", "name": "hidden scratch", "app_id": "pavucontrol",
               "pid": 900, "visible": false, "focused": false, "scratchpad_state": "changed",
               "rect": {"x":100,"y":100,"width":400,"height":300},
               "window_rect": {"x":2,"y":2,"width":396,"height":296},
               "nodes": [], "floating_nodes": []}
            ]}
         ], "floating_nodes": []},
        {"id": 3, "type": "output", "name": "HEADLESS-1", "focused": false,
         "rect": {"x":0,"y":0,"width":800,"height":600},
         "nodes": [
           {"id": 4, "type": "workspace", "name": "1", "visible": true, "focused": false,
            "rect": {"x":0,"y":0,"width":800,"height":600},
            "nodes": [
              {"id": 10, "type": "con", "name": "~ : foot", "app_id": "foot", "pid": 1010,
               "visible": true, "focused": true, "shell": "xdg_shell", "fullscreen_mode": 0,
               "border": "normal",
               "rect": {"x":0,"y":18,"width":400,"height":582},
               "window_rect": {"x":2,"y":0,"width":396,"height":580},
               "deco_rect": {"x":0,"y":0,"width":400,"height":18},
               "nodes": [], "floating_nodes": []},
              {"id": 11, "type": "con", "name": "Legacy X app", "app_id": null, "pid": 1111,
               "window": 4194305, "visible": true, "focused": false, "shell": "xwayland",
               "window_properties": {"class": "XTerm", "instance": "xterm", "title": "Legacy X app"},
               "rect": {"x":400,"y":0,"width":400,"height":300},
               "window_rect": {"x":2,"y":20,"width":396,"height":278},
               "nodes": [], "floating_nodes": []},
              {"id": 12, "type": "con", "name": null, "app_id": null, "pid": 1212,
               "window": 4194306, "visible": true, "focused": false, "shell": "xwayland",
               "window_properties": {"class": "", "instance": "steam", "title": "Untitled"},
               "rect": {"x":400,"y":300,"width":400,"height":300},
               "window_rect": {"x":0,"y":0,"width":400,"height":300},
               "nodes": [], "floating_nodes": []}
            ],
            "floating_nodes": [
              {"id": 20, "type": "floating_con", "name": "Mozilla Firefox", "app_id": "firefox",
               "pid": 2020, "visible": true, "focused": false, "shell": "xdg_shell",
               "rect": {"x":50,"y":40,"width":500,"height":400},
               "window_rect": {"x":2,"y":20,"width":496,"height":378},
               "nodes": [], "floating_nodes": []}
            ]},
           {"id": 5, "type": "workspace", "name": "2", "visible": false,
            "rect": {"x":0,"y":0,"width":800,"height":600},
            "nodes": [
              {"id": 30, "type": "con", "name": "on ws2", "app_id": "mpv", "pid": 3030,
               "visible": false, "focused": false,
               "rect": {"x":0,"y":0,"width":800,"height":600},
               "window_rect": {"x":0,"y":0,"width":800,"height":600},
               "nodes": [], "floating_nodes": []}
            ], "floating_nodes": []}
         ], "floating_nodes": []},
        {"id": 6, "type": "output", "name": "HEADLESS-2", "focused": false,
         "rect": {"x":800,"y":0,"width":320,"height":240},
         "nodes": [
           {"id": 7, "type": "workspace", "name": "3", "visible": true,
            "rect": {"x":800,"y":0,"width":320,"height":240},
            "nodes": [
              {"id": 40, "type": "con", "name": "split parent", "layout": "splith",
               "rect": {"x":800,"y":0,"width":320,"height":240},
               "nodes": [
                 {"id": 41, "type": "con", "name": "nested", "app_id": "alacritty", "pid": 4141,
                  "visible": true, "focused": false,
                  "rect": {"x":800,"y":0,"width":160,"height":240},
                  "window_rect": {"x":1,"y":1,"width":158,"height":238},
                  "nodes": [], "floating_nodes": []}
               ], "floating_nodes": []}
            ], "floating_nodes": []}
         ], "floating_nodes": []}
      ], "floating_nodes": []
    }"#;

    fn win(ws: &[SwayWindow], id: i64) -> &SwayWindow {
        ws.iter().find(|w| w.con_id == id).unwrap_or_else(|| panic!("con {id} missing"))
    }

    #[test]
    fn finds_every_leaf_window_including_nested_floating_and_xwayland() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        let mut ids: Vec<i64> = ws.iter().map(|w| w.con_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![10, 11, 12, 20, 30, 41, 90]);
    }

    #[test]
    fn container_nodes_are_not_windows() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        assert!(ws.iter().all(|w| w.con_id != 40), "the split container has no pid");
    }

    #[test]
    fn native_window_fields() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        let w = win(&ws, 10);
        assert_eq!(w.title, "~ : foot");
        assert_eq!(w.app_id.as_deref(), Some("foot"));
        // sway reports (0,18,400,582) for the container: the 18px title bar sits above it
        // and is included in `rect`; `content_rect` is the client surface only.
        assert_eq!(w.rect, Rect::new(0, 0, 400, 600));
        assert_eq!(w.content_rect, Rect::new(2, 18, 396, 580));
        assert!(w.focused && w.visible && !w.floating && !w.xwayland && !w.fullscreen);
        assert_eq!(w.workspace.as_deref(), Some("1"));
        assert_eq!(w.pid, Some(1010));
    }

    #[test]
    fn xwayland_windows_use_window_properties() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        let x = win(&ws, 11);
        assert!(x.xwayland);
        assert_eq!(x.app_id.as_deref(), Some("XTerm"), "class stands in for app_id");
        let untitled = win(&ws, 12);
        assert_eq!(untitled.title, "Untitled", "null name falls back to the X11 title");
        assert_eq!(untitled.app_id.as_deref(), Some("steam"), "empty class falls back to instance");
    }

    #[test]
    fn floating_windows_are_flagged() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        let f = win(&ws, 20);
        assert!(f.floating && f.visible);
        assert_eq!(f.rect, Rect::new(50, 40, 500, 400));
    }

    #[test]
    fn nested_window_on_second_output_keeps_global_logical_coordinates() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        let n = win(&ws, 41);
        assert_eq!(n.rect.x, 800, "global logical space, not output relative");
        assert_eq!(n.content_rect, Rect::new(801, 1, 158, 238));
        assert_eq!(n.workspace.as_deref(), Some("3"));
    }

    #[test]
    fn hidden_workspace_and_scratchpad_windows_are_not_visible() {
        let ws = parse_tree(TREE.as_bytes()).unwrap();
        assert!(!win(&ws, 30).visible, "inactive workspace");
        assert!(!win(&ws, 90).visible, "hidden scratchpad");
    }

    #[test]
    fn title_bar_is_included_only_for_own_title_bars() {
        // Real sway 1.9 values: a floating window with `border normal` (deco_rect is in
        // global coordinates there), a window without decorations, and a child of a tabbed
        // container whose title bar belongs to the shared tab strip.
        let json = br#"{"type":"root","nodes":[
          {"type":"con","id":1,"pid":1,"app_id":"a","border":"normal","name":"floaty",
           "rect":{"x":38,"y":40,"width":304,"height":202},
           "deco_rect":{"x":38,"y":15,"width":304,"height":25},
           "window_rect":{"x":2,"y":0,"width":300,"height":200}},
          {"type":"con","id":2,"pid":2,"app_id":"b","border":"none",
           "rect":{"x":0,"y":0,"width":100,"height":100},
           "deco_rect":{"x":0,"y":0,"width":0,"height":0},
           "window_rect":{"x":0,"y":0,"width":100,"height":100}},
          {"type":"con","layout":"tabbed","nodes":[
            {"type":"con","id":3,"pid":3,"app_id":"c","border":"normal",
             "rect":{"x":0,"y":25,"width":100,"height":75},
             "deco_rect":{"x":0,"y":0,"width":50,"height":25},
             "window_rect":{"x":0,"y":0,"width":100,"height":75}}]}
        ]}"#;
        let ws = parse_tree(json).unwrap();
        let by = |id| ws.iter().find(|w| w.con_id == id).unwrap();
        assert_eq!(by(1).rect, Rect::new(38, 15, 304, 227));
        assert_eq!(by(1).content_rect, Rect::new(40, 40, 300, 200));
        assert_eq!(by(2).rect, Rect::new(0, 0, 100, 100));
        assert_eq!(by(3).rect, Rect::new(0, 25, 100, 75), "tab strip is shared");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_tree(b"not json").is_err());
        assert!(parse_tree(b"").is_err());
        assert_eq!(parse_tree(b"{}").unwrap(), vec![]);
        assert_eq!(parse_tree(br#"{"type":"root","nodes":[{"type":"output"}]}"#).unwrap(), vec![]);
    }

    #[test]
    fn negative_sizes_are_clamped() {
        let json =
            br#"{"type":"con","pid":1,"app_id":"x","rect":{"x":-5,"y":-5,"width":-1,"height":10}}"#;
        let ws = parse_tree(json).unwrap();
        assert_eq!(ws[0].rect, Rect::new(-5, -5, 0, 10));
    }
}
