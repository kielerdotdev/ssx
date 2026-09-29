//! D-Bus plumbing shared by the KWin, portal and Mutter clients: connecting, name-owner
//! probing and a runtime-agnostic timeout.
//!
//! A fresh connection is made per capture rather than cached. Screenshots are rare and
//! slow compared to a socket handshake, and a per-call connection has two properties
//! worth having: dropping it on timeout makes the portal close any dialog we opened
//! (xdg-desktop-portal tracks requests by bus name), and concurrent captures cannot
//! interfere through shared signal streams.

use std::{future::Future, time::Duration};

use ssx_capture::CaptureError;
use zbus::{Connection, connection::Builder, fdo::DBusProxy};

/// Connects to the session bus, or to `address` when given (tests, unusual setups).
pub(crate) async fn connect(address: Option<&str>) -> Result<Connection, CaptureError> {
    let builder = match address {
        Some(a) => Builder::address(a),
        None => Builder::session(),
    };
    let result = match builder {
        Ok(b) => b.build().await,
        Err(e) => Err(e),
    };
    result.map_err(|e| {
        CaptureError::NoBackend(format!(
            "cannot connect to the D-Bus session bus ({e}); is DBUS_SESSION_BUS_ADDRESS set and \
             a graphical session running?"
        ))
    })
}

/// Whether some connection currently owns the well-known name `name`.
pub(crate) async fn name_has_owner(conn: &Connection, name: &str) -> bool {
    let Ok(bus_name) = zbus::names::BusName::try_from(name) else { return false };
    match DBusProxy::new(conn).await {
        Ok(proxy) => proxy.name_has_owner(bus_name).await.unwrap_or(false),
        Err(_) => false,
    }
}

/// Whether `name` is either running or can be started by bus activation.
pub(crate) async fn name_available(conn: &Connection, name: &str) -> bool {
    if name_has_owner(conn, name).await {
        return true;
    }
    match DBusProxy::new(conn).await {
        Ok(proxy) => proxy
            .list_activatable_names()
            .await
            .is_ok_and(|names| names.iter().any(|n| n.as_str() == name)),
        Err(_) => false,
    }
}

/// Runs `fut`, failing with a [`CaptureError::Backend`] after `limit`.
pub(crate) async fn with_timeout<T>(
    backend: &'static str,
    what: &str,
    limit: Duration,
    fut: impl Future<Output = Result<T, CaptureError>>,
) -> Result<T, CaptureError> {
    futures_lite::future::or(fut, async {
        async_io::Timer::after(limit).await;
        Err(CaptureError::backend(
            backend,
            format!("timed out after {limit:.1?} waiting for {what}"),
        ))
    })
    .await
}

/// `true` when a D-Bus error name says that the service (or the whole bus) is absent.
pub(crate) fn is_service_missing(error_name: &str) -> bool {
    matches!(
        error_name,
        "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.NoServer"
            | "org.freedesktop.DBus.Error.Spawn.ServiceNotFound"
            | "org.freedesktop.DBus.Error.Spawn.ChildExited"
            | "org.freedesktop.DBus.Error.Spawn.ExecFailed"
            | "org.freedesktop.DBus.Error.Spawn.ChildSignaled"
            | "org.freedesktop.DBus.Error.Spawn.Failed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_fires_and_passes_through() {
        let slow =
            async_io::block_on(with_timeout("t", "nothing", Duration::from_millis(30), async {
                async_io::Timer::after(Duration::from_secs(30)).await;
                Ok(1)
            }));
        assert!(matches!(slow, Err(CaptureError::Backend { .. })));
        let fast =
            async_io::block_on(with_timeout("t", "x", Duration::from_secs(5), async { Ok(7) }));
        assert_eq!(fast.unwrap(), 7);
    }

    #[test]
    fn missing_service_names() {
        assert!(is_service_missing("org.freedesktop.DBus.Error.ServiceUnknown"));
        assert!(!is_service_missing("org.kde.KWin.ScreenShot2.Error.Cancelled"));
    }
}
