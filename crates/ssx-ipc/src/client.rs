//! The connecting side, used by secondary instances and by the file-manager shim.
//!
//! Retry policy: only *connecting* is retried (the server may not be listening yet, or may be
//! between `bind` and `accept`). Once a request line has been written it is never re-sent,
//! because the request may have had side effects (an upload) and the transport cannot know.

use std::io;
use std::time::{Duration, Instant};

use interprocess::ConnectWaitMode;
use interprocess::local_socket::ConnectOptions;
#[cfg(unix)]
use interprocess::local_socket::traits::StreamCommon as _;

use crate::endpoint::{Endpoint, Location};
use crate::error::{Error, Result, TimeoutKind};
use crate::framing::{Conn, DEFAULT_MAX_LINE, LineReader, write_line};

/// Client timeouts and limits.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// How long [`Client::request`] keeps trying to connect (default 5 s).
    pub connect_timeout: Duration,
    /// Budget for writing the request and receiving the response line (default 10 s).
    pub request_timeout: Duration,
    /// How long [`Client::send_or_spawn`] waits for a freshly launched app (default 20 s).
    pub startup_timeout: Duration,
    /// First retry delay; doubles up to `max_backoff` (default 5 ms).
    pub initial_backoff: Duration,
    /// Upper bound for the retry delay (default 200 ms).
    pub max_backoff: Duration,
    /// Maximum response line length (default 4 MiB).
    pub max_line: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            startup_timeout: Duration::from_secs(20),
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(200),
            max_line: DEFAULT_MAX_LINE,
        }
    }
}

/// Talks to the primary instance.
#[derive(Debug, Clone)]
pub struct Client {
    endpoint: Endpoint,
    config: ClientConfig,
}

/// Errors that mean "nothing is listening (yet)" rather than "something is wrong".
fn is_not_listening(e: &io::Error) -> bool {
    #[cfg(windows)]
    const ERROR_PIPE_BUSY: i32 = 231;
    #[cfg(windows)]
    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) {
        return true;
    }
    matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    )
}

/// One connection attempt. `Ok(None)` means nobody is listening (yet).
pub(crate) fn try_connect(endpoint: &Endpoint) -> Result<Option<Conn>> {
    #[cfg(unix)]
    if !crate::endpoint::check_socket_owner(&endpoint.socket_path)? {
        return Ok(None);
    }
    let name = endpoint.name().map_err(Error::io("building endpoint name"))?;
    let stream = match ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(Duration::from_millis(250)))
        .connect_sync()
    {
        Ok(s) => s,
        Err(e) if is_not_listening(&e) => return Ok(None),
        Err(e) => return Err(Error::io("connecting to ssx instance")(e)),
    };
    #[cfg(unix)]
    {
        // Do not hand file paths to an impostor server.
        let ours = rustix::process::geteuid().as_raw();
        match stream.peer_creds() {
            Ok(c) if c.euid() == Some(ours) => {}
            Ok(c) => {
                return Err(Error::PeerRejected(format!(
                    "server runs as uid {:?}, expected {ours}",
                    c.euid()
                )));
            }
            Err(e) => return Err(Error::PeerRejected(format!("no server credentials: {e}"))),
        }
    }
    Conn::new(stream).map(Some)
}

impl Client {
    /// Client for the default per-user location.
    pub fn for_app(app_id: &str) -> Result<Self> {
        Self::at(&Location::user_default()?, app_id)
    }

    /// Client for an explicit location.
    pub fn at(location: &Location, app_id: &str) -> Result<Self> {
        Ok(Self { endpoint: location.endpoint(app_id)?, config: ClientConfig::default() })
    }

    pub(crate) fn from_endpoint(endpoint: Endpoint, config: ClientConfig) -> Self {
        Self { endpoint, config }
    }

    /// Replaces the timeouts/limits.
    #[must_use]
    pub fn with_config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }

    /// The active configuration.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Whether a server currently accepts connections.
    pub fn is_listening(&self) -> Result<bool> {
        Ok(try_connect(&self.endpoint)?.is_some())
    }

    /// Connects with exponential backoff until `budget` is spent.
    fn connect(&self, budget: Duration) -> Result<Option<Conn>> {
        let start = Instant::now();
        let mut delay = self.config.initial_backoff;
        loop {
            if let Some(conn) = try_connect(&self.endpoint)? {
                return Ok(Some(conn));
            }
            let Some(left) = budget.checked_sub(start.elapsed()).filter(|d| !d.is_zero()) else {
                return Ok(None);
            };
            std::thread::sleep(delay.min(left));
            delay = (delay * 2).min(self.config.max_backoff);
        }
    }

    fn exchange(&self, mut conn: Conn, line: &str) -> Result<String> {
        let start = Instant::now();
        let budget = self.config.request_timeout;
        write_line(&mut conn, line, self.config.max_line, budget)?;
        let left = budget.checked_sub(start.elapsed()).unwrap_or(Duration::from_millis(1));
        let mut reader = LineReader::new(self.config.max_line);
        reader.read_line(&mut conn, left, left)?.ok_or(Error::ConnectionClosed)
    }

    /// Sends one line and returns the one-line response.
    ///
    /// Connecting is retried with backoff for `connect_timeout` ([`Error::NotRunning`] if the
    /// server never appears). The request itself is sent at most once.
    pub fn request(&self, line: &str) -> Result<String> {
        let conn = self.connect(self.config.connect_timeout)?.ok_or(Error::NotRunning)?;
        self.exchange(conn, line)
    }

    /// Like [`request`](Self::request), but launches the application when nothing is
    /// listening, then waits (up to `startup_timeout`) for it to come up.
    ///
    /// `spawn` should start the app detached and return immediately. It is called at most
    /// once. Two shims racing may both spawn; the app's own single-instance guard (see
    /// `Instance::acquire`) makes the extra process a harmless secondary.
    pub fn send_or_spawn(&self, line: &str, spawn: impl Fn() -> io::Result<()>) -> Result<String> {
        if let Some(conn) = try_connect(&self.endpoint)? {
            return self.exchange(conn, line);
        }
        tracing::debug!("no ssx instance listening, launching one");
        spawn().map_err(Error::Spawn)?;
        let after = self.config.startup_timeout;
        let conn =
            self.connect(after)?.ok_or(Error::Timeout { kind: TimeoutKind::Startup, after })?;
        self.exchange(conn, line)
    }
}
