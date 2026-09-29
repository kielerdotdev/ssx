//! Client for KWin's `org.kde.KWin.ScreenShot2` (Plasma 5.27+ / 6).
//!
//! # Protocol
//!
//! Every `Capture*` method takes an `a{sv}` option dictionary and a **pipe file
//! descriptor** (`h`). KWin replies with an `a{sv}` describing the image
//! (`type`="raw", `width`, `height`, `stride`, `format` = `QImage::Format`, `scale`, …)
//! and *then*, from a worker thread, writes `stride * height` raw bytes to the pipe and
//! closes it. Two consequences shape this file:
//!
//! * The pipe buffer is 64 KiB, so a screenshot cannot be read *after* the reply: KWin
//!   would block on a full pipe. The read therefore runs concurrently with the call
//!   (`zip` below), and our copy of the write end is dropped as soon as the call has been
//!   sent, otherwise EOF would never arrive.
//! * The image geometry is only known from the reply, so the amount to read is capped by
//!   a configured maximum and validated against the reply afterwards.
//!
//! # Authorisation
//!
//! KWin only serves this interface to programs whose `.desktop` file declares
//! `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`. Others get a permission
//! error; see [`crate::kwin_desktop_entry`] and the mapping in [`map_dbus_error`].
//!
//! We always request `native-resolution` because the `ssx` contract is physical pixels;
//! without it KWin returns the *logical* size.

use std::{collections::HashMap, time::Duration};

use async_io::Async;
use futures_lite::{AsyncReadExt, future::zip};
use ssx_capture::CaptureError;
use ssx_types::{Frame, Rect};
use zbus::{
    Connection,
    zvariant::{Fd, OwnedValue, Value},
};

use crate::{bus, qimage};

/// Backend label used in errors.
pub(crate) const BACKEND: &str = "kwin-screenshot2";
/// Well-known bus name of the service.
pub(crate) const SERVICE: &str = "org.kde.KWin.ScreenShot2";
const PATH: &str = "/org/kde/KWin/ScreenShot2";

/// What `CaptureInteractive` should let the user pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveKind {
    /// Click a window.
    Window,
    /// Click a screen.
    Screen,
}

/// One of KWin's capture methods with its arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// `CaptureWorkspace`: all screens united (interface version 3+).
    Workspace,
    /// `CaptureActiveScreen` (version 2+).
    ActiveScreen,
    /// `CaptureScreen(name)`.
    Screen(String),
    /// `CaptureArea(x, y, w, h)` in logical coordinates.
    Area(Rect),
    /// `CaptureWindow(handle)`: KWin's window UUID.
    Window(String),
    /// `CaptureActiveWindow` (version 2+).
    ActiveWindow,
    /// `CaptureInteractive(kind)`.
    Interactive(InteractiveKind),
}

impl Target {
    /// Lowest interface `Version` that has this method.
    fn min_version(&self) -> u32 {
        match self {
            Target::Workspace => 3,
            Target::ActiveScreen | Target::ActiveWindow => 2,
            _ => 1,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Target::Workspace => "CaptureWorkspace",
            Target::ActiveScreen => "CaptureActiveScreen",
            Target::Screen(_) => "CaptureScreen",
            Target::Area(_) => "CaptureArea",
            Target::Window(_) => "CaptureWindow",
            Target::ActiveWindow => "CaptureActiveWindow",
            Target::Interactive(_) => "CaptureInteractive",
        }
    }
}

/// Per-call parameters.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Params {
    pub include_cursor: bool,
    pub timeout: Duration,
    /// Upper bound on the raw pixel bytes accepted.
    pub max_bytes: u64,
    /// Interface version reported by the service.
    pub version: u32,
}

/// A decoded KWin screenshot plus the metadata of its reply.
#[derive(Debug)]
pub(crate) struct Shot {
    pub frame: Frame,
    /// `QImage::devicePixelRatio()` reported by KWin (`None` before interface version 4).
    pub scale: Option<f64>,
}

/// Reads the interface `Version` property; `None` if the service is not on the bus.
pub(crate) async fn probe_version(conn: &Connection) -> Option<u32> {
    if !bus::name_has_owner(conn, SERVICE).await {
        return None;
    }
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
        .destination(SERVICE)
        .ok()?
        .path(PATH)
        .ok()?
        .interface(SERVICE)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    // A service that is present but does not answer `Version` is still usable as v1.
    Some(proxy.get_property::<u32>("Version").await.unwrap_or(1))
}

/// The dictionary part of a KWin reply that we understand.
#[derive(Debug, PartialEq)]
struct Meta {
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    scale: Option<f64>,
    screen: Option<String>,
}

fn get_u32(map: &HashMap<String, OwnedValue>, key: &str) -> Result<u32, CaptureError> {
    let v = map
        .get(key)
        .ok_or_else(|| CaptureError::backend(BACKEND, format!("reply lacks {key:?}")))?;
    match &**v {
        Value::U32(n) => Ok(*n),
        // Be lenient about integer width: KWin documents `u` but Qt may widen.
        Value::U64(n) => u32::try_from(*n)
            .map_err(|_| CaptureError::backend(BACKEND, format!("{key:?} out of range"))),
        Value::I32(n) => u32::try_from(*n)
            .map_err(|_| CaptureError::backend(BACKEND, format!("{key:?} out of range"))),
        other => Err(CaptureError::backend(
            BACKEND,
            format!("reply field {key:?} has unexpected type {}", other.value_signature()),
        )),
    }
}

fn parse_meta(map: &HashMap<String, OwnedValue>) -> Result<Meta, CaptureError> {
    match map.get("type").map(|v| &**v) {
        Some(Value::Str(s)) if s.as_str() == "raw" => {}
        Some(Value::Str(s)) => {
            return Err(CaptureError::backend(
                BACKEND,
                format!("unsupported image type {:?} (only \"raw\" is defined)", s.as_str()),
            ));
        }
        _ => return Err(CaptureError::backend(BACKEND, "reply lacks the image \"type\"")),
    }
    let scale = match map.get("scale").map(|v| &**v) {
        Some(Value::F64(s)) if s.is_finite() && *s >= 0.25 => Some(*s),
        _ => None,
    };
    let screen = match map.get("screen").map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    };
    Ok(Meta {
        width: get_u32(map, "width")?,
        height: get_u32(map, "height")?,
        stride: get_u32(map, "stride")?,
        format: get_u32(map, "format")?,
        scale,
        screen,
    })
}

/// Text appended to every KWin authorisation failure.
const AUTH_HINT: &str = "KWin only serves org.kde.KWin.ScreenShot2 to applications whose \
    .desktop file declares `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` and whose \
    `Exec=` is the absolute path of this executable. Install such a desktop entry (see \
    ssx_capture_portal::kwin_desktop_entry), then restart the application (a new KDE login may \
    be needed for the entry to be picked up); until then the xdg-desktop-portal fallback \
    can be used, which prompts on first use";

/// Maps a D-Bus error from KWin to a [`CaptureError`]. Pure, so it is unit-tested.
pub(crate) fn map_dbus_error(name: &str, message: &str, target: &Target) -> CaptureError {
    let last = name.rsplit('.').next().unwrap_or(name);
    match last {
        "Cancelled" => CaptureError::Cancelled,
        "NoAuthorized" | "NotAuthorized" | "AccessDenied" => CaptureError::PermissionDenied(
            format!("KWin refused {}: {AUTH_HINT}", target.describe()),
        ),
        "InvalidScreen" => match target {
            Target::Screen(n) => CaptureError::NotFound(n.clone()),
            _ => CaptureError::backend(BACKEND, "KWin: invalid screen"),
        },
        "InvalidWindow" => match target {
            Target::Window(h) => CaptureError::NotFound(h.clone()),
            _ => CaptureError::backend(BACKEND, "KWin: invalid window"),
        },
        "InvalidArea" => match target {
            Target::Area(r) => CaptureError::InvalidRegion(*r),
            _ => CaptureError::backend(BACKEND, "KWin: invalid area"),
        },
        "NoActiveWindow" => CaptureError::backend(BACKEND, "KWin: there is no active window"),
        "FileDescriptor" => CaptureError::backend(BACKEND, "KWin rejected the pipe descriptor"),
        "UnknownMethod" => CaptureError::unsupported(BACKEND, "this KWin version lacks the method"),
        _ if bus::is_service_missing(name) => {
            CaptureError::NoBackend("org.kde.KWin.ScreenShot2 is not on the session bus".into())
        }
        _ => CaptureError::backend(BACKEND, format!("{name}: {message}")),
    }
}

fn map_zbus_error(e: zbus::Error, target: &Target) -> CaptureError {
    match e {
        zbus::Error::MethodError(name, message, _) => {
            map_dbus_error(name.as_str(), message.as_deref().unwrap_or(""), target)
        }
        other => CaptureError::backend(BACKEND, other),
    }
}

/// Performs one capture and decodes the result.
pub(crate) async fn capture(
    conn: &Connection,
    target: &Target,
    params: Params,
) -> Result<Shot, CaptureError> {
    if params.version < target.min_version() {
        return Err(CaptureError::unsupported(
            BACKEND,
            match target {
                Target::Workspace => "CaptureWorkspace (needs KWin ScreenShot2 version 3)",
                _ => "this call (KWin ScreenShot2 is too old)",
            },
        ));
    }
    let what = target.describe();
    bus::with_timeout(BACKEND, what, params.timeout, capture_inner(conn, target, params)).await
}

async fn capture_inner(
    conn: &Connection,
    target: &Target,
    params: Params,
) -> Result<Shot, CaptureError> {
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("native-resolution", Value::Bool(true));
    if params.include_cursor {
        options.insert("include-cursor", Value::Bool(true));
    }

    let (reader, writer) = std::io::pipe()?;
    let reader = Async::new(reader)?;

    let call = async {
        let dest = Some(SERVICE);
        let path = PATH;
        let iface = Some(SERVICE);
        let fd = Fd::from(&writer);
        let reply = match target {
            Target::Workspace => {
                conn.call_method(dest, path, iface, "CaptureWorkspace", &(&options, fd)).await
            }
            Target::ActiveScreen => {
                conn.call_method(dest, path, iface, "CaptureActiveScreen", &(&options, fd)).await
            }
            Target::Screen(name) => {
                conn.call_method(dest, path, iface, "CaptureScreen", &(name.as_str(), &options, fd))
                    .await
            }
            Target::Area(r) => {
                conn.call_method(
                    dest,
                    path,
                    iface,
                    "CaptureArea",
                    &(r.x, r.y, r.width, r.height, &options, fd),
                )
                .await
            }
            Target::Window(handle) => {
                conn.call_method(
                    dest,
                    path,
                    iface,
                    "CaptureWindow",
                    &(handle.as_str(), &options, fd),
                )
                .await
            }
            Target::ActiveWindow => {
                conn.call_method(dest, path, iface, "CaptureActiveWindow", &(&options, fd)).await
            }
            Target::Interactive(kind) => {
                let kind: u32 = match kind {
                    InteractiveKind::Window => 0,
                    InteractiveKind::Screen => 1,
                };
                conn.call_method(dest, path, iface, "CaptureInteractive", &(kind, &options, fd))
                    .await
            }
        };
        // The message has been sent (or failed); KWin holds its own duplicate. Our copy of
        // the write end must go, or the reader below would never see EOF.
        drop(writer);
        reply
    };

    let read = async {
        let mut data = Vec::new();
        // `take(max + 1)` so that "exactly max" and "more than max" are distinguishable.
        (&reader).take(params.max_bytes.saturating_add(1)).read_to_end(&mut data).await?;
        Ok::<_, std::io::Error>(data)
    };

    let (reply, data) = zip(call, read).await;
    let reply = reply.map_err(|e| map_zbus_error(e, target))?;
    let map: HashMap<String, OwnedValue> = reply
        .body()
        .deserialize()
        .map_err(|e| CaptureError::backend(BACKEND, format!("malformed reply: {e}")))?;
    let meta = parse_meta(&map)?;
    let data = data?;

    let bpp = qimage::bytes_per_pixel(meta.format).ok_or_else(|| {
        CaptureError::backend(
            BACKEND,
            format!(
                "KWin returned QImage format {} which is not supported (HDR/float outputs need \
                 the xdg-desktop-portal path)",
                meta.format
            ),
        )
    })?;
    let expected = u64::from(meta.stride) * u64::from(meta.height.saturating_sub(1))
        + u64::from(meta.width) * bpp as u64;
    if expected > params.max_bytes || data.len() as u64 > params.max_bytes {
        return Err(CaptureError::backend(
            BACKEND,
            format!(
                "screenshot of {}x{} exceeds the configured limit of {} bytes",
                meta.width, meta.height, params.max_bytes
            ),
        ));
    }
    let frame =
        qimage::frame_from_raw(meta.width, meta.height, meta.stride as usize, meta.format, &data)
            .map_err(|e| CaptureError::backend(BACKEND, e))?;
    Ok(Shot { frame, scale: meta.scale })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: Vec<(&str, Value<'static>)>) -> HashMap<String, OwnedValue> {
        entries.into_iter().map(|(k, v)| (k.to_owned(), OwnedValue::try_from(v).unwrap())).collect()
    }

    fn good() -> Vec<(&'static str, Value<'static>)> {
        vec![
            ("type", Value::from("raw")),
            ("width", Value::U32(3)),
            ("height", Value::U32(2)),
            ("stride", Value::U32(16)),
            ("format", Value::U32(6)),
            ("scale", Value::F64(1.5)),
            ("screen", Value::from("DP-1")),
        ]
    }

    #[test]
    fn parses_a_full_reply() {
        let m = parse_meta(&dict(good())).unwrap();
        assert_eq!(
            m,
            Meta {
                width: 3,
                height: 2,
                stride: 16,
                format: 6,
                scale: Some(1.5),
                screen: Some("DP-1".into())
            }
        );
    }

    #[test]
    fn version_one_reply_has_no_scale() {
        let g: Vec<_> =
            good().into_iter().filter(|(k, _)| *k != "scale" && *k != "screen").collect();
        let m = parse_meta(&dict(g)).unwrap();
        assert_eq!(m.scale, None);
        assert_eq!(m.screen, None);
    }

    #[test]
    fn rejects_non_raw_and_incomplete_replies() {
        let mut g = good();
        g[0] = ("type", Value::from("png"));
        assert!(parse_meta(&dict(g)).is_err());
        let g: Vec<_> = good().into_iter().filter(|(k, _)| *k != "stride").collect();
        assert!(parse_meta(&dict(g)).is_err());
        let mut g = good();
        g[1] = ("width", Value::from("wide"));
        assert!(parse_meta(&dict(g)).is_err());
        assert!(parse_meta(&HashMap::new()).is_err());
    }

    #[test]
    fn absurd_scale_is_ignored() {
        let mut g = good();
        g[5] = ("scale", Value::F64(f64::NAN));
        assert_eq!(parse_meta(&dict(g)).unwrap().scale, None);
        let mut g = good();
        g[5] = ("scale", Value::F64(0.0));
        assert_eq!(parse_meta(&dict(g)).unwrap().scale, None);
    }

    #[test]
    fn error_mapping() {
        let screen = Target::Screen("DP-9".into());
        assert!(matches!(
            map_dbus_error("org.kde.KWin.ScreenShot2.Error.Cancelled", "", &screen),
            CaptureError::Cancelled
        ));
        let e =
            map_dbus_error("org.kde.KWin.ScreenShot2.Error.NoAuthorized", "", &Target::Workspace);
        match e {
            CaptureError::PermissionDenied(m) => {
                assert!(m.contains("X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2"));
            }
            other => panic!("wrong mapping: {other:?}"),
        }
        assert!(matches!(
            map_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "x", &Target::Workspace),
            CaptureError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_dbus_error("org.kde.KWin.ScreenShot2.Error.InvalidScreen", "", &screen),
            CaptureError::NotFound(n) if n == "DP-9"
        ));
        let area = Target::Area(Rect::new(1, 2, 3, 4));
        assert!(matches!(
            map_dbus_error("org.kde.KWin.ScreenShot2.Error.InvalidArea", "", &area),
            CaptureError::InvalidRegion(r) if r == Rect::new(1, 2, 3, 4)
        ));
        assert!(matches!(
            map_dbus_error("org.kde.KWin.ScreenShot2.Error.InvalidWindow", "", &Target::Window("w".into())),
            CaptureError::NotFound(w) if w == "w"
        ));
        assert!(matches!(
            map_dbus_error("org.freedesktop.DBus.Error.ServiceUnknown", "", &screen),
            CaptureError::NoBackend(_)
        ));
        assert!(matches!(
            map_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", "", &screen),
            CaptureError::Unsupported { .. }
        ));
        assert!(matches!(
            map_dbus_error("com.example.Weird", "boom", &screen),
            CaptureError::Backend { message, .. } if message.contains("boom")
        ));
    }

    #[test]
    fn version_gates() {
        assert_eq!(Target::Workspace.min_version(), 3);
        assert_eq!(Target::ActiveScreen.min_version(), 2);
        assert_eq!(Target::Screen("x".into()).min_version(), 1);
    }
}
