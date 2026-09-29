//! `org.freedesktop.portal.Screenshot` via `ashpd`.
//!
//! # Behaviour by desktop
//!
//! * **GNOME**: `org.gnome.Shell.Screenshot` is restricted by Shell to allow-listed
//!   applications, so the portal is the only supported route. The first non-interactive
//!   request may show a permission dialog; a refusal comes back as response code 2.
//! * **KDE**: works too (the KWin path is preferred because it is silent).
//! * The portal has **no cursor option**: whether the pointer is in the picture is up to
//!   the backend, and `include_cursor` is ignored.
//! * `interactive = false` returns the whole desktop as one PNG (`file://` URI);
//!   `interactive = true` lets the desktop show its own selection UI and returns just the
//!   selection.
//!
//! The URI points at a file the portal *created for us*; we read it and delete it so
//! screenshots do not pile up in the user's Pictures folder.

use std::{io::Cursor, path::Path, time::Duration};

use ashpd::{
    Error as PortalError, PortalError as PortalDbusError,
    desktop::{ResponseError, screenshot::Screenshot},
};
use ssx_capture::CaptureError;
use ssx_types::Frame;
use zbus::Connection;

use crate::{bus, uri};

/// Backend label used in errors.
pub(crate) const BACKEND: &str = "xdg-portal";
/// Well-known bus name of the portal frontend.
pub(crate) const SERVICE: &str = "org.freedesktop.portal.Desktop";

const NO_PORTAL: &str = "the xdg-desktop-portal Screenshot portal is unavailable; install \
    xdg-desktop-portal and the backend for your desktop (xdg-desktop-portal-gnome or \
    xdg-desktop-portal-kde) and make sure it is running in this session";

/// Per-call parameters.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Params {
    pub interactive: bool,
    pub timeout: Duration,
    /// Largest decoded image accepted, in bytes of RGBA.
    pub max_bytes: u64,
    /// Delete the file the portal produced after reading it.
    pub delete_file: bool,
}

/// Maps an `ashpd` error to a [`CaptureError`]. Pure, so it is unit-tested.
pub(crate) fn map_error(err: PortalError, interactive: bool) -> CaptureError {
    match err {
        PortalError::Response(ResponseError::Cancelled) => CaptureError::Cancelled,
        PortalError::Response(ResponseError::Other) => {
            if interactive {
                CaptureError::backend(
                    BACKEND,
                    "the portal ended the interactive screenshot with response code 2 (the \
                     desktop could not take the screenshot)",
                )
            } else {
                CaptureError::PermissionDenied(
                    "the screenshot portal answered with response code 2. On GNOME this usually \
                     means the permission prompt was denied (or the app was previously denied); \
                     grant screenshot access in the desktop's privacy settings, or use the \
                     interactive capture, which the desktop always allows"
                        .into(),
                )
            }
        }
        PortalError::Portal(e) => match e {
            PortalDbusError::Cancelled(_) => CaptureError::Cancelled,
            PortalDbusError::NotAllowed(m) => CaptureError::PermissionDenied(format!(
                "the screenshot portal refused the request: {m}"
            )),
            PortalDbusError::ZBus(z) => map_zbus(z),
            other => CaptureError::backend(BACKEND, other),
        },
        PortalError::Zbus(z) => map_zbus(z),
        PortalError::PortalNotFound(_) => CaptureError::NoBackend(NO_PORTAL.into()),
        PortalError::NoResponse => {
            CaptureError::backend(BACKEND, "the portal closed the request without answering")
        }
        other => CaptureError::backend(BACKEND, other),
    }
}

fn map_zbus(e: zbus::Error) -> CaptureError {
    if let zbus::Error::MethodError(name, message, _) = &e {
        let n = name.as_str();
        if bus::is_service_missing(n)
            || matches!(
                n,
                "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownObject"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
            )
        {
            return CaptureError::NoBackend(NO_PORTAL.into());
        }
        if n == "org.freedesktop.portal.Error.NotAllowed" || n.ends_with(".AccessDenied") {
            return CaptureError::PermissionDenied(format!(
                "the screenshot portal refused the request: {}",
                message.as_deref().unwrap_or(n)
            ));
        }
    }
    CaptureError::backend(BACKEND, e)
}

/// Takes a screenshot through the portal and decodes it.
pub(crate) async fn screenshot(conn: &Connection, params: Params) -> Result<Frame, CaptureError> {
    let what = if params.interactive { "the interactive portal screenshot" } else { "the portal" };
    bus::with_timeout(BACKEND, what, params.timeout, async {
        let request = Screenshot::request()
            .interactive(params.interactive)
            .connection(Some(conn.clone()))
            .send()
            .await
            .map_err(|e| map_error(e, params.interactive))?;
        let shot = request.response().map_err(|e| map_error(e, params.interactive))?;
        frame_from_uri(shot.uri().as_str(), params.max_bytes, params.delete_file)
    })
    .await
}

/// Loads (and by default deletes) the file a portal URI points at.
///
/// The file is removed even when decoding fails, since it is ours either way.
pub(crate) fn frame_from_uri(
    uri_text: &str,
    max_bytes: u64,
    delete: bool,
) -> Result<Frame, CaptureError> {
    let path = uri::file_uri_to_path(uri_text).map_err(|e| CaptureError::backend(BACKEND, e))?;
    let read = std::fs::read(&path);
    if delete {
        remove_quietly(&path);
    }
    let bytes = read.map_err(|e| {
        CaptureError::backend(
            BACKEND,
            format!("cannot read the screenshot file {} the portal reported: {e}", path.display()),
        )
    })?;
    decode(&bytes, max_bytes)
}

fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "removed portal screenshot file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not delete portal screenshot");
        }
    }
}

/// Decodes an image file, refusing anything whose RGBA size would exceed `max_bytes`
/// *before* allocating for it (the header is inspected first).
pub(crate) fn decode(bytes: &[u8], max_bytes: u64) -> Result<Frame, CaptureError> {
    let bad = |e: image::ImageError| {
        CaptureError::backend(BACKEND, format!("the portal returned an unreadable image: {e}"))
    };
    let guess = || {
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| CaptureError::backend(BACKEND, format!("unreadable image: {e}")))
    };
    let (w, h) = guess()?.into_dimensions().map_err(bad)?;
    if w == 0 || h == 0 {
        return Err(CaptureError::backend(BACKEND, "the portal returned an empty image"));
    }
    let need = u64::from(w) * u64::from(h) * 4;
    if need > max_bytes {
        return Err(CaptureError::backend(
            BACKEND,
            format!(
                "screenshot of {w}x{h} ({need} bytes decoded) exceeds the configured limit of \
                 {max_bytes} bytes"
            ),
        ));
    }
    let mut reader = guess()?;
    // Our own bound (checked above) replaces the decoder's conservative default.
    reader.no_limits();
    let img = reader.decode().map_err(bad)?;
    Ok(Frame::from_image(img.into_rgba8()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn response_codes() {
        assert!(matches!(
            map_error(PortalError::Response(ResponseError::Cancelled), false),
            CaptureError::Cancelled
        ));
        assert!(matches!(
            map_error(PortalError::Response(ResponseError::Cancelled), true),
            CaptureError::Cancelled
        ));
        assert!(matches!(
            map_error(PortalError::Response(ResponseError::Other), false),
            CaptureError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_error(PortalError::Response(ResponseError::Other), true),
            CaptureError::Backend { .. }
        ));
        assert!(matches!(map_error(PortalError::NoResponse, false), CaptureError::Backend { .. }));
    }

    #[test]
    fn portal_dbus_errors() {
        assert!(matches!(
            map_error(PortalError::Portal(PortalDbusError::Cancelled("x".into())), false),
            CaptureError::Cancelled
        ));
        assert!(matches!(
            map_error(PortalError::Portal(PortalDbusError::NotAllowed("x".into())), false),
            CaptureError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_error(PortalError::Portal(PortalDbusError::Failed("x".into())), false),
            CaptureError::Backend { .. }
        ));
    }

    #[test]
    fn decodes_and_bounds_images() {
        let f = decode(&png(5, 3), 1 << 20).unwrap();
        assert_eq!((f.width(), f.height()), (5, 3));
        assert_eq!(&f.row(0)[0..4], &[1, 2, 3, 255]);
        // exactly at the limit is fine, one byte less is not
        assert!(decode(&png(5, 3), 5 * 3 * 4).is_ok());
        assert!(matches!(decode(&png(5, 3), 5 * 3 * 4 - 1), Err(CaptureError::Backend { .. })));
        assert!(matches!(decode(b"not an image", 1 << 20), Err(CaptureError::Backend { .. })));
        assert!(matches!(decode(&[], 1 << 20), Err(CaptureError::Backend { .. })));
        let truncated = &png(50, 50)[..40];
        assert!(matches!(decode(truncated, 1 << 20), Err(CaptureError::Backend { .. })));
    }

    #[test]
    fn uri_to_frame_deletes_the_file() {
        let dir = std::env::temp_dir().join(format!("ssx-portal-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a b.png");
        std::fs::write(&p, png(2, 2)).unwrap();
        let uri = "file://".to_owned() + &dir.display().to_string() + "/a%20b.png";
        let f = frame_from_uri(&uri, 1 << 20, true).unwrap();
        assert_eq!(f.width(), 2);
        assert!(!p.exists());
        // now missing
        assert!(matches!(frame_from_uri(&uri, 1 << 20, true), Err(CaptureError::Backend { .. })));
        // undecodable file is still deleted
        std::fs::write(&p, b"junk").unwrap();
        assert!(frame_from_uri(&uri, 1 << 20, true).is_err());
        assert!(!p.exists());
        // delete = false keeps it
        std::fs::write(&p, png(1, 1)).unwrap();
        assert!(frame_from_uri(&uri, 1 << 20, false).is_ok());
        assert!(p.exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
