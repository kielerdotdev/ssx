//! The listening side: accepts connections from the current user only and serves lines.
//!
//! The blocking, thread-based API is deliberate: the transport must be usable from a GUI
//! main loop, a tokio runtime, or a plain CLI without pulling in an async runtime. A caller
//! that wants async can run [`Server::serve`] (own threads) or drive [`Server::incoming`]
//! from a dedicated thread and forward lines over a channel.

use std::fmt;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use interprocess::local_socket::traits::{Listener as _, StreamCommon as _};
use interprocess::local_socket::{Listener, ListenerOptions};

use crate::client;
use crate::endpoint::Endpoint;
use crate::error::{Error, Result};
use crate::framing::{Conn, DEFAULT_MAX_LINE, LineReader, write_line};

/// Facts about the process on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerInfo {
    /// Process id, when the OS reports it (Linux, Windows, FreeBSD, OpenBSD).
    pub pid: Option<u32>,
    /// Effective user id (Unix only).
    pub uid: Option<u32>,
}

/// Who may talk to the server.
#[derive(Clone, Default)]
pub enum PeerPolicy {
    /// Only connections from the same effective user (Unix: `SO_PEERCRED`/`LOCAL_PEERCRED`
    /// uid must match; Windows: enforced by the pipe ACL, pid is informational).
    #[default]
    SameUser,
    /// Custom check, evaluated after the same-user check has passed.
    Also(Arc<dyn Fn(&PeerInfo) -> bool + Send + Sync>),
}

impl fmt::Debug for PeerPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SameUser => f.write_str("SameUser"),
            Self::Also(_) => f.write_str("Also(..)"),
        }
    }
}

/// Server limits.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Maximum line length in bytes (default 4 MiB).
    pub max_line: usize,
    /// How long a connection may sit silent between requests (default 30 s).
    pub idle_timeout: Duration,
    /// Once a request has started, how long it may take to arrive completely, and how long a
    /// response write may take (default 10 s). Defeats slow-loris clients.
    pub request_timeout: Duration,
    /// Simultaneous connections served by [`Server::serve`]; extra ones are dropped (default 32).
    pub max_connections: usize,
    /// Peer admission policy.
    pub peer_policy: PeerPolicy,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_line: DEFAULT_MAX_LINE,
            idle_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(10),
            max_connections: 32,
            peer_policy: PeerPolicy::SameUser,
        }
    }
}

/// The primary instance's endpoint. Dropping it removes the socket and releases the
/// single-instance lock (in that order).
pub struct Server {
    // Field order matters: the listener (which unlinks the socket) drops before the lock.
    listener: Listener,
    _lock: File,
    endpoint: Endpoint,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("endpoint", &self.endpoint.describe())
            .finish_non_exhaustive()
    }
}

impl Server {
    pub(crate) fn bind(endpoint: Endpoint, lock: File, config: ServerConfig) -> Result<Self> {
        let name = endpoint.name().map_err(Error::io("building endpoint name"))?;
        let opts = ListenerOptions::new().name(name).reclaim_name(true);
        #[cfg(unix)]
        let opts = {
            use interprocess::os::unix::local_socket::ListenerOptionsExt;
            opts.mode(0o600)
        };
        #[cfg(windows)]
        let opts = {
            use interprocess::os::windows::local_socket::ListenerOptionsExt;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;
            // Protected DACL granting access only to the pipe's owner (the current user);
            // nobody else, including "Everyone" and anonymous, gets an ACE.
            let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)")
                .map_err(|e| Error::NoRuntimeDir(e.to_string()))?;
            let sd = SecurityDescriptor::deserialize(&sddl)
                .map_err(Error::io("building pipe security descriptor"))?;
            opts.security_descriptor(sd)
        };
        let listener = opts.create_sync().map_err(Error::io("binding local socket"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Belt and braces: not every Unix applies the pre-bind fchmod to the socket inode.
            std::fs::set_permissions(&endpoint.socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(Error::io("restricting socket permissions"))?;
        }
        tracing::debug!(endpoint = %endpoint.describe(), "ssx-ipc primary listening");
        Ok(Self {
            listener,
            _lock: lock,
            endpoint,
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Path of the socket file (Unix only).
    #[cfg(unix)]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.endpoint.socket_path
    }

    /// Human-readable endpoint (socket path or pipe name).
    pub fn describe_endpoint(&self) -> String {
        self.endpoint.describe()
    }

    /// The configured limits.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// A clonable handle that stops [`accept`](Self::accept)/[`incoming`](Self::incoming).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle { flag: Arc::clone(&self.shutdown), endpoint: self.endpoint.clone() }
    }

    /// Blocks until an authorised peer connects.
    ///
    /// Unauthorised peers are dropped silently (logged) and do not surface as errors, so a
    /// hostile local process cannot terminate an `incoming()` loop.
    pub fn accept(&self) -> Result<Connection> {
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(Error::Shutdown);
            }
            let stream = self.listener.accept().map_err(Error::io("accepting connection"))?;
            if self.shutdown.load(Ordering::Acquire) {
                return Err(Error::Shutdown);
            }
            let peer = match self.admit(&stream) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "ssx-ipc dropped a connection");
                    continue;
                }
            };
            match Conn::new(stream) {
                Ok(conn) => {
                    return Ok(Connection {
                        conn,
                        reader: LineReader::new(self.config.max_line),
                        peer,
                        max_line: self.config.max_line,
                        idle_timeout: self.config.idle_timeout,
                        request_timeout: self.config.request_timeout,
                    });
                }
                Err(e) => tracing::warn!(error = %e, "ssx-ipc could not set up a connection"),
            }
        }
    }

    fn admit(&self, stream: &interprocess::local_socket::Stream) -> Result<PeerInfo> {
        let creds = stream.peer_creds();
        #[cfg(unix)]
        let peer = {
            let c = creds.map_err(|e| Error::PeerRejected(format!("no peer credentials: {e}")))?;
            PeerInfo { pid: c.pid().and_then(|p| u32::try_from(p).ok()), uid: c.euid() }
        };
        #[cfg(windows)]
        let peer = PeerInfo { pid: creds.ok().and_then(|c| c.pid()), uid: None };
        #[cfg(unix)]
        {
            let ours = rustix::process::geteuid().as_raw();
            if peer.uid != Some(ours) {
                return Err(Error::PeerRejected(format!(
                    "peer uid {:?} is not the current user ({ours})",
                    peer.uid
                )));
            }
        }
        if let PeerPolicy::Also(check) = &self.config.peer_policy
            && !check(&peer)
        {
            return Err(Error::PeerRejected("custom peer policy denied".into()));
        }
        Ok(peer)
    }

    /// Iterator over authorised connections; ends after [`ShutdownHandle::shutdown`].
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { server: self }
    }

    /// Serves request/response lines on background threads until shut down.
    ///
    /// Each connection may carry several request lines; every line is passed to `handler`
    /// and its return value is written back as one line.
    pub fn serve<H>(self, handler: H) -> Result<ServeHandle>
    where
        H: Fn(String) -> String + Send + Sync + 'static,
    {
        let shutdown = self.shutdown_handle();
        let handler = Arc::new(handler);
        let active = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::Builder::new()
            .name("ssx-ipc-accept".into())
            .spawn(move || self.accept_loop(&handler, &active))
            .map_err(Error::io("spawning accept thread"))?;
        Ok(ServeHandle { shutdown, thread: Some(thread) })
    }

    fn accept_loop<H>(&self, handler: &Arc<H>, active: &Arc<AtomicUsize>)
    where
        H: Fn(String) -> String + Send + Sync + 'static,
    {
        loop {
            let conn = match self.accept() {
                Ok(c) => c,
                Err(Error::Shutdown) => break,
                Err(e) => {
                    tracing::error!(error = %e, "ssx-ipc accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            if active.fetch_add(1, Ordering::AcqRel) >= self.config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                tracing::warn!("ssx-ipc connection limit reached, dropping connection");
                continue;
            }
            let handler = Arc::clone(handler);
            let guard = ActiveGuard(Arc::clone(active));
            let spawned =
                std::thread::Builder::new().name("ssx-ipc-conn".into()).spawn(move || {
                    let _guard = guard;
                    serve_connection(conn, &*handler);
                });
            if let Err(e) = spawned {
                tracing::error!(error = %e, "ssx-ipc could not spawn a connection thread");
            }
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_connection<H: Fn(String) -> String>(mut conn: Connection, handler: &H) {
    loop {
        match conn.read_line() {
            Ok(Some(line)) => {
                let reply = handler(line);
                if let Err(e) = conn.write_line(&reply) {
                    tracing::debug!(error = %e, "ssx-ipc reply failed");
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(error = %e, "ssx-ipc connection ended");
                return;
            }
        }
    }
}

/// Iterator returned by [`Server::incoming`].
#[derive(Debug)]
pub struct Incoming<'a> {
    server: &'a Server,
}

impl Iterator for Incoming<'_> {
    type Item = Result<Connection>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.server.accept() {
            Err(Error::Shutdown) => None,
            other => Some(other),
        }
    }
}

/// Stops a running [`Server`]. Cheap to clone; safe to call repeatedly.
#[derive(Debug, Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    endpoint: Endpoint,
}

impl ShutdownHandle {
    /// Requests shutdown and wakes the blocked `accept` with a throw-away connection.
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Release);
        // The wake-up connection is refused/ignored if the server is already gone.
        drop(client::try_connect(&self.endpoint));
    }
}

/// Running background server from [`Server::serve`].
#[derive(Debug)]
pub struct ServeHandle {
    shutdown: ShutdownHandle,
    thread: Option<JoinHandle<()>>,
}

impl ServeHandle {
    /// A handle to trigger shutdown from elsewhere.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Stops accepting, waits for the accept thread and thereby removes the socket.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shutdown.shutdown();
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::error!("ssx-ipc accept thread panicked");
        }
    }
}

impl Drop for ServeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One accepted connection.
#[derive(Debug)]
pub struct Connection {
    conn: Conn,
    reader: LineReader,
    peer: PeerInfo,
    max_line: usize,
    idle_timeout: Duration,
    request_timeout: Duration,
}

impl Connection {
    /// Who is on the other end.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// Reads the next line (`Ok(None)` at EOF). Errors on oversize lines, non-UTF-8 input,
    /// idle timeout and slow delivery.
    pub fn read_line(&mut self) -> Result<Option<String>> {
        self.reader.read_line(&mut self.conn, self.idle_timeout, self.request_timeout)
    }

    /// Writes one line (a newline terminator is added; embedded newlines are rejected).
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        write_line(&mut self.conn, line, self.max_line, self.request_timeout)
    }
}
