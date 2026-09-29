//! Top-level window discovery via EWMH/ICCCM properties.
//!
//! * Order and membership: `_NET_CLIENT_LIST_STACKING` on the root (bottom-to-top per the
//!   spec, reversed here to front-to-back). Window managers without EWMH support fall back
//!   to the root's viewable, non-override-redirect children.
//! * Title: `_NET_WM_NAME` (UTF-8), else `WM_NAME` (Latin-1 or UTF-8 depending on its type).
//! * App name: the *class* half of `WM_CLASS` (`instance\0Class\0`), else the instance.
//! * Minimised: `_NET_WM_STATE_HIDDEN`, or `WM_STATE` == IconicState.
//! * Geometry: the client window's origin translated to root coordinates, extended by
//!   `_NET_FRAME_EXTENTS` (left, right, top, bottom) so the rectangle includes the window
//!   manager's decorations, which is what users expect from "capture window". Client-side
//!   shadows (`_GTK_FRAME_EXTENTS`) are not part of it.
//! * Focus: `_NET_ACTIVE_WINDOW`.

use ssx_types::{Rect, WindowInfo};
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, MapState, Window, WindowClass};

use crate::{
    error::{X11Error, X11Result},
    session::{Property, Session},
};

/// Formats a window id the way `xwininfo`/`xdotool` print it.
pub(crate) fn format_window_id(w: Window) -> String {
    format!("{w:#x}")
}

/// Parses `0x1a00003` or `27262979`.
pub(crate) fn parse_window_id(s: &str) -> X11Result<Window> {
    let t = s.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => t.parse::<u32>(),
    };
    parsed.ok().filter(|w| *w != 0).ok_or_else(|| X11Error::BadWindowId(s.to_owned()))
}

/// Extends a client rectangle by `[left, right, top, bottom]` frame extents.
pub(crate) fn apply_frame_extents(client: Rect, extents: [u32; 4]) -> Rect {
    let clamp = |v: u32| i64::from(v.min(i32::MAX as u32));
    let [l, r, t, b] = extents;
    let x = (i64::from(client.x) - clamp(l)).clamp(i64::from(i32::MIN), i64::from(i32::MAX));
    let y = (i64::from(client.y) - clamp(t)).clamp(i64::from(i32::MIN), i64::from(i32::MAX));
    let w = (i64::from(client.width) + clamp(l) + clamp(r)).min(i64::from(u32::MAX));
    let h = (i64::from(client.height) + clamp(t) + clamp(b)).min(i64::from(u32::MAX));
    Rect::new(x as i32, y as i32, w as u32, h as u32)
}

/// Splits `WM_CLASS` into an application name.
pub(crate) fn app_name_from_wm_class(raw: &[u8]) -> Option<String> {
    let mut parts = raw.split(|&b| b == 0).filter(|p| !p.is_empty());
    let instance = parts.next()?;
    let class = parts.next().unwrap_or(instance);
    Some(String::from_utf8_lossy(class).into_owned())
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

impl Session {
    /// Top-level client windows, front-to-back.
    fn client_list(&self) -> X11Result<Vec<Window>> {
        if let Some(p) = self.property(
            self.root,
            self.atoms.net_client_list_stacking,
            AtomEnum::WINDOW,
            1 << 20,
        )? {
            let mut list = p.words();
            list.reverse();
            return Ok(list);
        }
        // No EWMH window manager: use the root's mapped, managed-looking children.
        let tree = self
            .conn
            .query_tree(self.root)?
            .reply()
            .map_err(|e| X11Error::from_reply("QueryTree", e))?;
        let mut list = Vec::new();
        for child in tree.children {
            let Ok(Ok(attrs)) = self.conn.get_window_attributes(child).map(|c| c.reply()) else {
                continue;
            };
            if attrs.map_state == MapState::VIEWABLE
                && !attrs.override_redirect
                && attrs.class == WindowClass::INPUT_OUTPUT
            {
                list.push(child);
            }
        }
        list.reverse();
        Ok(list)
    }

    fn active_window(&self) -> Option<Window> {
        self.property(self.root, self.atoms.net_active_window, AtomEnum::WINDOW, 4)
            .ok()
            .flatten()
            .and_then(|p| p.words().first().copied())
            .filter(|w| *w != 0)
    }

    /// Lists windows, skipping any that vanish mid-enumeration.
    pub(crate) fn window_list(&self) -> X11Result<Vec<WindowInfo>> {
        let active = self.active_window();
        let mut out = Vec::new();
        for win in self.client_list()? {
            match self.window_info(win, active) {
                Ok(Some(info)) => out.push(info),
                Ok(None) => {}
                Err(e) if e.is_gone() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Describes one window, or `None` if it does not exist.
    pub(crate) fn window_info(
        &self,
        win: Window,
        active: Option<Window>,
    ) -> X11Result<Option<WindowInfo>> {
        let gone = |e: x11rb::errors::ReplyError| match e {
            x11rb::errors::ReplyError::X11Error(ref x)
                if matches!(
                    x.error_kind,
                    x11rb::protocol::ErrorKind::Window | x11rb::protocol::ErrorKind::Drawable
                ) =>
            {
                X11Error::NoSuchWindow(win)
            }
            other => X11Error::from_reply("window query", other),
        };
        // Pipeline the three fixed-size requests.
        let geometry = self.conn.get_geometry(win)?;
        let origin = self.conn.translate_coordinates(win, self.root, 0, 0)?;
        let (geometry, origin) =
            match (geometry.reply().map_err(gone), origin.reply().map_err(gone)) {
                (Ok(g), Ok(o)) => (g, o),
                (Err(e), _) | (_, Err(e)) if e.is_gone() => return Ok(None),
                (Err(e), _) | (_, Err(e)) => return Err(e),
            };
        let client = Rect::new(
            i32::from(origin.dst_x),
            i32::from(origin.dst_y),
            u32::from(geometry.width),
            u32::from(geometry.height),
        );
        let extents = self
            .property(win, self.atoms.net_frame_extents, AtomEnum::CARDINAL, 16)?
            .map(|p| p.words())
            .and_then(|w| <[u32; 4]>::try_from(w).ok());
        let rect = extents.map_or(client, |e| apply_frame_extents(client, e));

        let title = self.window_title(win)?;
        let app_name = self
            .property(win, u32::from(AtomEnum::WM_CLASS), AtomEnum::STRING, 4096)?
            .and_then(|p| app_name_from_wm_class(&p.value));
        let minimized = self.is_minimized(win)?;
        Ok(Some(WindowInfo {
            id: format_window_id(win),
            title,
            app_name,
            rect,
            minimized,
            focused: active == Some(win),
        }))
    }

    fn window_title(&self, win: Window) -> X11Result<String> {
        let decode = |p: Property| -> String {
            let bytes = p.value.strip_suffix(&[0]).unwrap_or(&p.value);
            if p.type_ == u32::from(AtomEnum::STRING) {
                latin1(bytes)
            } else {
                String::from_utf8_lossy(bytes).into_owned()
            }
        };
        if let Some(p) =
            self.property(win, self.atoms.net_wm_name, self.atoms.utf8_string, 1 << 16)?
        {
            return Ok(decode(p));
        }
        let any = u32::from(AtomEnum::ANY);
        Ok(self
            .property(win, u32::from(AtomEnum::WM_NAME), any, 1 << 16)?
            .map(decode)
            .unwrap_or_default())
    }

    fn is_minimized(&self, win: Window) -> X11Result<bool> {
        if let Some(p) = self.property(win, self.atoms.net_wm_state, AtomEnum::ATOM, 4096)? {
            if p.words().contains(&self.atoms.net_wm_state_hidden) {
                return Ok(true);
            }
        }
        const ICONIC_STATE: u32 = 3;
        Ok(self
            .property(win, self.atoms.wm_state, self.atoms.wm_state, 8)?
            .and_then(|p| p.words().first().copied())
            == Some(ICONIC_STATE))
    }

    /// The ancestor of `win` that is a direct child of the root (the window manager's frame
    /// when the WM reparents, else `win` itself).
    pub(crate) fn top_level_of(&self, win: Window) -> X11Result<Window> {
        let mut cur = win;
        // Bounded: X trees are shallow; the cap only guards against a misbehaving server.
        for _ in 0..64 {
            let tree = self.conn.query_tree(cur)?.reply().map_err(|e| match e {
                x11rb::errors::ReplyError::X11Error(ref x)
                    if x.error_kind == x11rb::protocol::ErrorKind::Window =>
                {
                    X11Error::NoSuchWindow(cur)
                }
                other => X11Error::from_reply("QueryTree", other),
            })?;
            if tree.parent == self.root || tree.parent == x11rb::NONE {
                return Ok(cur);
            }
            cur = tree.parent;
        }
        Err(X11Error::Malformed("window ancestry is implausibly deep"))
    }

    /// Whether the window is mapped and viewable (so has pixels).
    pub(crate) fn is_viewable(&self, win: Window) -> X11Result<bool> {
        let attrs = self.conn.get_window_attributes(win)?.reply().map_err(|e| match e {
            x11rb::errors::ReplyError::X11Error(ref x)
                if x.error_kind == x11rb::protocol::ErrorKind::Window =>
            {
                X11Error::NoSuchWindow(win)
            }
            other => X11Error::from_reply("GetWindowAttributes", other),
        })?;
        Ok(attrs.map_state == xproto::MapState::VIEWABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ids_parse_hex_and_decimal() {
        assert_eq!(parse_window_id("0x1a00003").unwrap(), 0x01a0_0003);
        assert_eq!(parse_window_id("0X1A00003").unwrap(), 0x01a0_0003);
        assert_eq!(parse_window_id(" 27262979 ").unwrap(), 27_262_979);
        for bad in ["", "0x", "0", "zz", "-4", "0xfffffffff", "12 34"] {
            assert!(parse_window_id(bad).is_err(), "{bad:?}");
        }
        assert_eq!(format_window_id(0x1a0_0003), "0x1a00003");
    }

    #[test]
    fn frame_extents_grow_rect_outward() {
        let r = apply_frame_extents(Rect::new(100, 50, 400, 300), [1, 2, 30, 4]);
        assert_eq!(r, Rect::new(99, 20, 403, 334));
        // Negative client origin and absurd extents cannot overflow.
        let r = apply_frame_extents(Rect::new(-5, -5, 10, 10), [u32::MAX; 4]);
        assert_eq!((r.x, r.y, r.width, r.height), (i32::MIN, i32::MIN, u32::MAX, u32::MAX));
    }

    #[test]
    fn wm_class_yields_class_then_instance() {
        assert_eq!(app_name_from_wm_class(b"xterm\0XTerm\0").as_deref(), Some("XTerm"));
        assert_eq!(app_name_from_wm_class(b"solo\0").as_deref(), Some("solo"));
        assert_eq!(app_name_from_wm_class(b"\0Class\0").as_deref(), Some("Class"));
        assert_eq!(app_name_from_wm_class(b""), None);
        assert_eq!(app_name_from_wm_class(b"\0\0"), None);
    }

    #[test]
    fn latin1_decodes_high_bytes() {
        assert_eq!(latin1(&[0x63, 0x61, 0x66, 0xe9]), "café");
    }
}
