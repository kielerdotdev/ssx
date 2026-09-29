//! One live connection to the X server plus everything learned about it at connect time.
//!
//! A [`Session`] is immutable after construction apart from the shared-memory segment
//! (behind a mutex). [`crate::X11Capture`] throws the session away when the connection
//! breaks and builds a new one on the next call, so a restarted X server (or a
//! `startx` cycle) does not require restarting ssx.

use std::{collections::HashMap, sync::Mutex};

use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        composite, randr,
        xproto::{self, Atom, AtomEnum, ConnectionExt as _, Format, ImageOrder, Visualtype},
        xfixes,
    },
    rust_connection::RustConnection,
};

use crate::{
    config::X11Config,
    error::{X11Error, X11Result},
    shm::ShmState,
};

macro_rules! atoms {
    ($($field:ident => $name:literal),* $(,)?) => {
        /// Atoms interned once per session.
        #[derive(Debug, Clone, Copy)]
        pub(crate) struct Atoms { $(pub $field: Atom),* }

        impl Atoms {
            fn intern(conn: &RustConnection) -> X11Result<Self> {
                // Pipeline every request, then collect: one round trip in total.
                $(let $field = conn.intern_atom(false, $name.as_bytes())?;)*
                Ok(Self { $($field: $field.reply()?.atom),* })
            }
        }
    };
}

atoms! {
    net_client_list_stacking => "_NET_CLIENT_LIST_STACKING",
    net_active_window => "_NET_ACTIVE_WINDOW",
    net_wm_name => "_NET_WM_NAME",
    net_wm_state => "_NET_WM_STATE",
    net_wm_state_hidden => "_NET_WM_STATE_HIDDEN",
    net_frame_extents => "_NET_FRAME_EXTENTS",
    utf8_string => "UTF8_STRING",
    resource_manager => "RESOURCE_MANAGER",
    xsettings_settings => "_XSETTINGS_SETTINGS",
    wm_state => "WM_STATE",
}

/// Optional protocol extensions the server offers.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Extensions {
    /// MIT-SHM 1.2+ (file-descriptor passing) is present.
    pub shm_fd: bool,
    pub xfixes: bool,
    /// Composite 0.2+ (`NameWindowPixmap`).
    pub composite: bool,
    /// RandR version `(major, minor)` if present.
    pub randr: Option<(u32, u32)>,
}

pub(crate) struct Session {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub root: xproto::Window,
    pub root_visual: u32,
    pub root_depth: u8,
    pub msb_first: bool,
    pub formats: Vec<Format>,
    pub visuals: HashMap<u32, Visualtype>,
    pub atoms: Atoms,
    pub ext: Extensions,
    pub config: X11Config,
    pub shm: Mutex<ShmState>,
    pub xsettings_selection: Atom,
    pub cm_selection: Atom,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("screen", &self.screen_num)
            .field("root", &format_args!("{:#x}", self.root))
            .field("root_depth", &self.root_depth)
            .field("ext", &self.ext)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(crate) fn connect(config: &X11Config) -> X11Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(config.display.as_deref()).map_err(|source| {
                X11Error::Connect {
                    display: config
                        .display
                        .clone()
                        .or_else(|| std::env::var("DISPLAY").ok())
                        .unwrap_or_default(),
                    source,
                }
            })?;
        let setup = conn.setup();
        let screen = setup.roots.get(screen_num).ok_or(X11Error::Malformed("no such screen"))?;
        let mut visuals = HashMap::new();
        for depth in &screen.allowed_depths {
            for v in &depth.visuals {
                visuals.insert(v.visual_id, *v);
            }
        }
        let root = screen.root;
        let root_visual = screen.root_visual;
        let root_depth = screen.root_depth;
        let msb_first = setup.image_byte_order == ImageOrder::MSB_FIRST;
        let formats = setup.pixmap_formats.clone();

        let atoms = Atoms::intern(&conn)?;
        let xsettings_selection = conn
            .intern_atom(false, format!("_XSETTINGS_S{screen_num}").as_bytes())?
            .reply()?
            .atom;
        let cm_selection =
            conn.intern_atom(false, format!("_NET_WM_CM_S{screen_num}").as_bytes())?.reply()?.atom;

        let ext = query_extensions(&conn);
        tracing::debug!(?ext, screen_num, root_depth, "connected to X server");
        Ok(Self {
            conn,
            screen_num,
            root,
            root_visual,
            root_depth,
            msb_first,
            formats,
            visuals,
            atoms,
            ext,
            config: config.clone(),
            shm: Mutex::new(ShmState::default()),
            xsettings_selection,
            cm_selection,
        })
    }

    /// Current size of the root window (it changes when RandR resizes the screen).
    pub(crate) fn root_size(&self) -> X11Result<(u32, u32)> {
        let g = self
            .conn
            .get_geometry(self.root)?
            .reply()
            .map_err(|e| X11Error::from_reply("GetGeometry(root)", e))?;
        Ok((u32::from(g.width), u32::from(g.height)))
    }

    /// Reads a whole property, following `bytes_after` so large values (e.g.
    /// `RESOURCE_MANAGER`) are not truncated. `None` if the property is not set.
    /// Values are capped at `max_bytes`.
    pub(crate) fn property(
        &self,
        window: xproto::Window,
        property: Atom,
        type_: impl Into<Atom>,
        max_bytes: usize,
    ) -> X11Result<Option<Property>> {
        let type_ = type_.into();
        let mut value = Vec::new();
        let mut result_type = 0;
        let mut format = 0;
        loop {
            let offset = u32::try_from(value.len() / 4).unwrap_or(u32::MAX);
            let reply = self
                .conn
                .get_property(false, window, property, type_, offset, 16 * 1024)?
                .reply()
                .map_err(|e| match e {
                    x11rb::errors::ReplyError::X11Error(ref x)
                        if x.error_kind == x11rb::protocol::ErrorKind::Window =>
                    {
                        X11Error::NoSuchWindow(window)
                    }
                    other => X11Error::from_reply("GetProperty", other),
                })?;
            if reply.type_ == u32::from(AtomEnum::NONE) {
                return Ok(None);
            }
            result_type = reply.type_;
            format = reply.format;
            value.extend_from_slice(&reply.value);
            if reply.bytes_after == 0 || value.len() >= max_bytes {
                break;
            }
        }
        value.truncate(max_bytes);
        Ok(Some(Property { type_: result_type, format, value }))
    }

    /// The pixmap format the server uses for `depth`.
    pub(crate) fn format_for_depth(&self, depth: u8) -> X11Result<&Format> {
        self.formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or_else(|| X11Error::UnsupportedVisual(format!("no pixmap format for depth {depth}")))
    }

    /// Whether a compositing manager currently owns `_NET_WM_CM_Sn`.
    pub(crate) fn compositor_running(&self) -> bool {
        self.conn
            .get_selection_owner(self.cm_selection)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.owner != x11rb::NONE)
    }
}

/// A fetched window property.
#[derive(Debug, Clone)]
pub(crate) struct Property {
    pub type_: Atom,
    pub format: u8,
    pub value: Vec<u8>,
}

impl Property {
    /// Interprets a format-32 property as native-endian words.
    pub(crate) fn words(&self) -> Vec<u32> {
        if self.format != 32 {
            return Vec::new();
        }
        self.value.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect()
    }
}

fn query_extensions(conn: &RustConnection) -> Extensions {
    let mut ext = Extensions::default();
    let present = |name: &'static str| matches!(conn.extension_information(name), Ok(Some(_)));

    if present(x11rb::protocol::shm::X11_EXTENSION_NAME) {
        if let Some(v) = x11rb::protocol::shm::query_version(conn).ok().and_then(|c| c.reply().ok())
        {
            ext.shm_fd = (v.major_version, v.minor_version) >= (1, 2);
        }
    }
    if present(xfixes::X11_EXTENSION_NAME) {
        ext.xfixes = xfixes::query_version(conn, 2, 0).ok().and_then(|c| c.reply().ok()).is_some();
    }
    if present(composite::X11_EXTENSION_NAME) {
        ext.composite = composite::query_version(conn, 0, 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| (v.major_version, v.minor_version) >= (0, 2));
    }
    if present(randr::X11_EXTENSION_NAME) {
        ext.randr = randr::query_version(conn, 1, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|v| (v.major_version, v.minor_version));
    }
    ext
}
