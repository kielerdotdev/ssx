//! Single-instance guard.
//!
//! See `endpoint.rs` for why the decision is an advisory file lock rather than socket
//! existence: it is atomic (`flock`/`LockFileEx`), released by the kernel on crash, and
//! therefore makes stale-socket recovery trivial and race-free.

use std::fs::{File, OpenOptions};

use fs4::TryLockError;

use crate::client::{Client, ClientConfig};
use crate::endpoint::{Endpoint, Location};
use crate::error::{Error, Result};
use crate::server::{Server, ServerConfig};

/// Outcome of [`Instance::acquire`].
#[derive(Debug)]
pub enum Acquired {
    /// We are the only instance; serve requests on this server.
    Primary(Server),
    /// Another instance is running (or starting); talk to it with this client.
    Secondary(Client),
}

/// Timeouts/limits for both roles.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Used if we become primary.
    pub server: ServerConfig,
    /// Used if we become secondary.
    pub client: ClientConfig,
}

/// Namespace for [`Instance::acquire`].
#[derive(Debug, Clone, Copy)]
pub struct Instance;

impl Instance {
    /// Becomes the primary instance for `app_id` or returns a client for the running one.
    pub fn acquire(app_id: &str) -> Result<Acquired> {
        Self::acquire_with(&Location::user_default()?, app_id, Options::default())
    }

    /// [`acquire`](Self::acquire) in an explicit location.
    pub fn acquire_in(location: &Location, app_id: &str) -> Result<Acquired> {
        Self::acquire_with(location, app_id, Options::default())
    }

    /// [`acquire`](Self::acquire) with explicit limits.
    pub fn acquire_with(location: &Location, app_id: &str, options: Options) -> Result<Acquired> {
        let endpoint = location.endpoint(app_id)?;
        let lock = open_lock(&endpoint)?;
        match fs4::FileExt::try_lock(&lock) {
            Ok(()) => {
                remove_stale_endpoint(&endpoint)?;
                Server::bind(endpoint, lock, options.server).map(Acquired::Primary)
            }
            Err(TryLockError::WouldBlock) => {
                // Refuse to talk to a foreign socket even before the first request.
                #[cfg(unix)]
                crate::endpoint::check_socket_owner(&endpoint.socket_path)?;
                Ok(Acquired::Secondary(Client::from_endpoint(endpoint, options.client)))
            }
            Err(TryLockError::Error(e)) => Err(Error::io("locking instance file")(e)),
        }
    }
}

fn open_lock(endpoint: &Endpoint) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&endpoint.lock_path).map_err(Error::io("opening instance lock file"))
}

/// We hold the lock, so any socket file still lying around belongs to a dead instance.
#[cfg_attr(windows, allow(clippy::unnecessary_wraps))] // Unix path can fail
fn remove_stale_endpoint(endpoint: &Endpoint) -> Result<()> {
    #[cfg(unix)]
    {
        if crate::endpoint::check_socket_owner(&endpoint.socket_path)? {
            tracing::info!(path = %endpoint.socket_path.display(), "removing stale socket");
            match std::fs::remove_file(&endpoint.socket_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io("removing stale socket")(e)),
            }
        }
    }
    #[cfg(windows)]
    let _ = endpoint; // Named pipes vanish with their last handle; nothing to clean.
    Ok(())
}
