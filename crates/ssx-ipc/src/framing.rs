//! Line-delimited framing with hard limits and deadlines.
//!
//! Why a hand-rolled reader: `BufRead::read_line` would happily buffer an unbounded line from a
//! hostile or buggy peer, and blocks forever on a silent peer. Local sockets on Windows (named
//! pipes) have no native I/O timeout in `interprocess`, so [`Conn`] uses native socket timeouts
//! where the OS supports them (Unix) and otherwise runs blocking I/O on helper threads
//! (see [`crate::threaded`]). It must *not* fall back to non-blocking polling: a non-blocking
//! pipe reports "no data yet" as `BrokenPipe`, which is indistinguishable from a hang-up.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::Stream as _;

use crate::error::{Error, Result, TimeoutKind};
use crate::threaded::Threaded;

/// Default maximum length of one line (excluding the terminator): 4 MiB.
pub const DEFAULT_MAX_LINE: usize = 4 * 1024 * 1024;

const MIN_TIMEOUT: Duration = Duration::from_millis(1);

#[derive(Debug)]
enum Io {
    /// `SO_RCVTIMEO`/`SO_SNDTIMEO` style timeouts on the stream itself.
    Native(Stream),
    /// Plain blocking halves driven from helper threads.
    Threaded(Threaded),
}

/// A connected stream with deadline-aware reads and writes.
#[derive(Debug)]
pub(crate) struct Conn {
    io: Io,
}

impl Conn {
    pub(crate) fn new(stream: Stream) -> Result<Self> {
        match stream.set_recv_timeout(Some(Duration::from_secs(3600))) {
            Ok(()) => Ok(Self { io: Io::Native(stream) }),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(Self::new_threaded(stream)),
            Err(e) => Err(Error::io("configuring stream")(e)),
        }
    }

    /// Uses helper threads even where native timeouts exist (tests exercise the Windows path
    /// on Unix this way).
    pub(crate) fn new_threaded(stream: Stream) -> Self {
        let (recv, send) = stream.split();
        Self { io: Io::Threaded(Threaded::new(recv, send)) }
    }

    /// A connection over arbitrary blocking halves (pipe-semantics test doubles).
    #[cfg(test)]
    pub(crate) fn from_halves(
        read: impl Read + Send + 'static,
        write: impl Write + Send + 'static,
    ) -> Self {
        Self { io: Io::Threaded(Threaded::new(read, write)) }
    }

    fn remaining(deadline: Instant) -> Option<Duration> {
        deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
    }

    /// Reads at least one byte (or 0 on EOF) before `deadline`.
    fn read_some(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        loop {
            let Some(left) = Self::remaining(deadline) else {
                return Err(io::ErrorKind::TimedOut.into());
            };
            let stream = match &mut self.io {
                Io::Threaded(t) => return t.read_some(buf, deadline),
                Io::Native(s) => s,
            };
            stream.set_recv_timeout(Some(left.max(MIN_TIMEOUT)))?;
            match stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    // The native timeout was set to the remaining time, so it is spent.
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Writes all of `data` before `deadline`.
    pub(crate) fn write_all(&mut self, mut data: &[u8], deadline: Instant) -> io::Result<()> {
        while !data.is_empty() {
            let Some(left) = Self::remaining(deadline) else {
                return Err(io::ErrorKind::TimedOut.into());
            };
            let stream = match &mut self.io {
                Io::Threaded(t) => return t.write_all(data, deadline),
                Io::Native(s) => s,
            };
            stream.set_send_timeout(Some(left.max(MIN_TIMEOUT)))?;
            match stream.write(data) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => data = data.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Incremental reader of `\n`-terminated UTF-8 lines.
#[derive(Debug)]
pub(crate) struct LineReader {
    buf: Vec<u8>,
    max_line: usize,
}

impl LineReader {
    pub(crate) fn new(max_line: usize) -> Self {
        Self { buf: Vec::new(), max_line }
    }

    /// Extracts one complete line from the buffer if present.
    fn take_line(&mut self) -> Result<Option<String>> {
        let Some(pos) = self.buf.iter().position(|&b| b == b'\n') else {
            if self.buf.len() > self.max_line {
                return Err(Error::LineTooLong { max: self.max_line });
            }
            return Ok(None);
        };
        if pos > self.max_line {
            return Err(Error::LineTooLong { max: self.max_line });
        }
        let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
        line.pop(); // '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line).map(Some).map_err(|_| Error::InvalidLine("line is not valid UTF-8"))
    }

    /// Reads one line.
    ///
    /// Waits up to `idle` for the first byte of a line, then `complete` (measured from that
    /// first byte) for the rest, so a peer dribbling bytes cannot hold a slot forever. Returns
    /// `Ok(None)` on a clean EOF between lines.
    pub(crate) fn read_line(
        &mut self,
        conn: &mut Conn,
        idle: Duration,
        complete: Duration,
    ) -> Result<Option<String>> {
        let mut deadline = Instant::now() + idle;
        let mut kind_after = idle;
        if !self.buf.is_empty() {
            deadline = Instant::now() + complete;
            kind_after = complete;
        }
        let mut chunk = [0_u8; 8192];
        loop {
            if let Some(line) = self.take_line()? {
                return Ok(Some(line));
            }
            let n = match conn.read_some(&mut chunk, deadline) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    return Err(Error::Timeout { kind: TimeoutKind::Read, after: kind_after });
                }
                Err(e) if is_disconnect(&e) => 0,
                Err(e) => return Err(Error::io("reading from peer")(e)),
            };
            if n == 0 {
                return if self.buf.is_empty() { Ok(None) } else { Err(Error::ConnectionClosed) };
            }
            if self.buf.is_empty() {
                deadline = Instant::now() + complete;
                kind_after = complete;
            }
            self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        }
    }
}

fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
    )
}

/// Validates and writes `line` plus a terminator.
pub(crate) fn write_line(
    conn: &mut Conn,
    line: &str,
    max_line: usize,
    timeout: Duration,
) -> Result<()> {
    if line.contains('\n') {
        return Err(Error::InvalidLine("line contains a newline"));
    }
    if line.len() > max_line {
        return Err(Error::LineTooLong { max: max_line });
    }
    let mut data = Vec::with_capacity(line.len() + 1);
    data.extend_from_slice(line.as_bytes());
    data.push(b'\n');
    match conn.write_all(&data, Instant::now() + timeout) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            Err(Error::Timeout { kind: TimeoutKind::Write, after: timeout })
        }
        Err(e) if is_disconnect(&e) => Err(Error::ConnectionClosed),
        Err(e) => Err(Error::io("writing to peer")(e)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use interprocess::os::unix::uds_local_socket::Stream as UdsStream;
    use std::os::unix::net::UnixStream;

    fn pair(threaded: bool) -> (Conn, UnixStream) {
        let (a, b) = UnixStream::pair().expect("pair");
        let s = Stream::UdSocket(UdsStream::from(a));
        let conn = if threaded { Ok(Conn::new_threaded(s)) } else { Conn::new(s) };
        (conn.expect("conn"), b)
    }

    /// Runs `f` with native timeouts (`false`) and with the helper-thread transport (`true`).
    fn both(f: impl Fn(bool)) {
        f(false);
        f(true);
    }

    #[test]
    fn reads_lines_split_across_writes_and_crlf() {
        both(|threaded| {
            let (mut conn, mut peer) = pair(threaded);
            peer.write_all(b"one\r\ntw").expect("write");
            let t = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                peer.write_all(b"o\nthree\n").expect("write");
                peer
            });
            let mut r = LineReader::new(100);
            let d = Duration::from_secs(2);
            assert_eq!(r.read_line(&mut conn, d, d).expect("l1").as_deref(), Some("one"));
            assert_eq!(r.read_line(&mut conn, d, d).expect("l2").as_deref(), Some("two"));
            assert_eq!(r.read_line(&mut conn, d, d).expect("l3").as_deref(), Some("three"));
            drop(t.join().expect("join"));
            assert_eq!(r.read_line(&mut conn, d, d).expect("eof"), None);
        });
    }

    #[test]
    fn rejects_oversize_with_and_without_newline() {
        both(|threaded| {
            let (mut conn, mut peer) = pair(threaded);
            peer.write_all(&[b'a'; 200]).expect("write");
            let mut r = LineReader::new(100);
            let d = Duration::from_secs(2);
            let err = r.read_line(&mut conn, d, d).expect_err("oversize");
            assert!(matches!(err, Error::LineTooLong { max: 100 }), "{err}");

            let (mut conn, mut peer) = pair(threaded);
            let mut data = vec![b'a'; 150];
            data.push(b'\n');
            peer.write_all(&data).expect("write");
            let mut r = LineReader::new(100);
            let err = r.read_line(&mut conn, d, d).expect_err("oversize");
            assert!(matches!(err, Error::LineTooLong { .. }), "{err}");
        });
    }

    #[test]
    fn exact_max_length_is_accepted() {
        let (mut conn, mut peer) = pair(false);
        let mut data = vec![b'a'; 100];
        data.push(b'\n');
        peer.write_all(&data).expect("write");
        let mut r = LineReader::new(100);
        let d = Duration::from_secs(2);
        assert_eq!(r.read_line(&mut conn, d, d).expect("ok").map(|l| l.len()), Some(100));
    }

    #[test]
    fn idle_and_slow_delivery_time_out() {
        both(|threaded| {
            let (mut conn, mut peer) = pair(threaded);
            let mut r = LineReader::new(100);
            let err = r
                .read_line(&mut conn, Duration::from_millis(80), Duration::from_secs(5))
                .expect_err("idle");
            assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Read, .. }), "{err}");

            // Partial line, then silence: the (shorter) completion deadline applies.
            peer.write_all(b"partial").expect("write");
            let start = Instant::now();
            let err = r
                .read_line(&mut conn, Duration::from_secs(5), Duration::from_millis(80))
                .expect_err("slow");
            assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Read, .. }), "{err}");
            assert!(start.elapsed() < Duration::from_secs(2));
        });
    }

    #[test]
    fn invalid_utf8_and_mid_line_eof() {
        let (mut conn, mut peer) = pair(false);
        peer.write_all(b"\xff\xfe\n").expect("write");
        let mut r = LineReader::new(100);
        let d = Duration::from_secs(2);
        assert!(matches!(r.read_line(&mut conn, d, d), Err(Error::InvalidLine(_))));

        let (mut conn, mut peer) = pair(false);
        peer.write_all(b"abc").expect("write");
        drop(peer);
        let mut r = LineReader::new(100);
        assert!(matches!(r.read_line(&mut conn, d, d), Err(Error::ConnectionClosed)));
    }

    #[test]
    fn write_line_validates() {
        let (mut conn, mut peer) = pair(false);
        let d = Duration::from_secs(1);
        assert!(matches!(write_line(&mut conn, "a\nb", 100, d), Err(Error::InvalidLine(_))));
        assert!(matches!(
            write_line(&mut conn, &"x".repeat(101), 100, d),
            Err(Error::LineTooLong { .. })
        ));
        write_line(&mut conn, "ok", 100, d).expect("write");
        let mut buf = [0_u8; 3];
        std::io::Read::read_exact(&mut peer, &mut buf).expect("read");
        assert_eq!(&buf, b"ok\n");
    }
}
