//! Deadline-aware I/O over a stream that cannot time out natively (Windows named pipes).
//!
//! Why threads and not non-blocking mode: the only way to make a Win32 named pipe return
//! instead of block is `PIPE_NOWAIT`, and in that mode an empty pipe fails with
//! `ERROR_NO_DATA`, which Rust reports as [`io::ErrorKind::BrokenPipe`], the same kind a real
//! disconnect produces. A polling reader therefore sees "no data yet" as end-of-file: the server
//! hung up on every freshly accepted connection (the client had not written yet) and the client
//! saw its reply "close". Microsoft documents `PIPE_NOWAIT` as a LAN Manager 2.0 compatibility
//! leftover that must not be used for asynchronous I/O. Cancelling a pending read needs FFI
//! (`CancelIoEx`), which this crate forbids.
//!
//! So the pipe stays in its normal blocking mode and each direction gets one helper thread that
//! performs a single blocking operation per request. The caller waits on a channel with a
//! deadline. This is built on plain `Read`/`Write` halves so the exact same code is exercised on
//! Linux by the tests, using a double that mimics pipe semantics.
//!
//! Properties worth knowing:
//! * Helper threads are started lazily and are *request driven*: they only block inside the
//!   stream while a caller is waiting, never reading ahead. Dropping the [`Threaded`] while no
//!   operation is pending therefore releases the stream at once, so the peer sees EOF right
//!   away and the throw-away wake-up/probe connections cost no threads.
//! * An operation that times out cannot be cancelled. Its thread keeps the stream open until
//!   the peer sends, closes, or reads. The caller treats a timeout as fatal for the connection
//!   (same-user peers only, so this is a bounded resource leak, not a vulnerability), and the
//!   late result is kept rather than lost if the caller does ask again.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

type BoxRead = Box<dyn Read + Send>;
type BoxWrite = Box<dyn Write + Send>;

/// One helper thread that answers requests in order.
struct Worker<Req, Resp> {
    requests: Sender<Req>,
    responses: Receiver<Resp>,
    /// A request was sent and its response has not been collected.
    inflight: bool,
}

impl<Req: Send + 'static, Resp: Send + 'static> Worker<Req, Resp> {
    fn spawn(name: &str, mut serve: impl FnMut(Req) -> Resp + Send + 'static) -> io::Result<Self> {
        let (requests, request_rx) = mpsc::channel::<Req>();
        let (response_tx, responses) = mpsc::channel::<Resp>();
        std::thread::Builder::new().name(name.into()).spawn(move || {
            // Ends when the owner drops `requests` (or stops listening for responses).
            for req in request_rx {
                if response_tx.send(serve(req)).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self { requests, responses, inflight: false })
    }

    /// Sends `req` unless an earlier one is still outstanding.
    fn submit(&mut self, req: Req) -> io::Result<()> {
        if !self.inflight {
            self.requests.send(req).map_err(|_| io::ErrorKind::BrokenPipe)?;
            self.inflight = true;
        }
        Ok(())
    }

    /// Waits for the outstanding response until `deadline`.
    fn wait(&mut self, deadline: Instant) -> io::Result<Resp> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.responses.recv_timeout(left) {
            Ok(resp) => {
                self.inflight = false;
                Ok(resp)
            }
            Err(RecvTimeoutError::Timeout) => Err(io::ErrorKind::TimedOut.into()),
            Err(RecvTimeoutError::Disconnected) => {
                self.inflight = false;
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
    }
}

enum Lane<T, W> {
    /// Not started; holds the stream half.
    Idle(T),
    Running(W),
    /// Spawning failed or the half was lost; every further use fails.
    Failed,
}

type ReadWorker = Worker<usize, io::Result<Vec<u8>>>;
type WriteWorker = Worker<Vec<u8>, io::Result<()>>;

/// Blocking stream halves with per-call deadlines.
pub(crate) struct Threaded {
    reader: Lane<BoxRead, ReadWorker>,
    writer: Lane<BoxWrite, WriteWorker>,
    /// Bytes a read returned beyond what the caller's buffer could take.
    stash: Vec<u8>,
}

impl std::fmt::Debug for Threaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Threaded").finish_non_exhaustive()
    }
}

fn lost() -> io::Error {
    io::Error::from(io::ErrorKind::BrokenPipe)
}

impl Threaded {
    pub(crate) fn new(
        read: impl Read + Send + 'static,
        write: impl Write + Send + 'static,
    ) -> Self {
        Self {
            reader: Lane::Idle(Box::new(read)),
            writer: Lane::Idle(Box::new(write)),
            stash: Vec::new(),
        }
    }

    fn reader(&mut self) -> io::Result<&mut ReadWorker> {
        if matches!(self.reader, Lane::Idle(_)) {
            let Lane::Idle(mut src) = std::mem::replace(&mut self.reader, Lane::Failed) else {
                return Err(lost());
            };
            let worker = Worker::spawn("ssx-ipc-read", move |want: usize| {
                let mut buf = vec![0_u8; want];
                loop {
                    match src.read(&mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            return Ok(buf);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            })?;
            self.reader = Lane::Running(worker);
        }
        match &mut self.reader {
            Lane::Running(w) => Ok(w),
            Lane::Idle(_) | Lane::Failed => Err(lost()),
        }
    }

    fn writer(&mut self) -> io::Result<&mut WriteWorker> {
        if matches!(self.writer, Lane::Idle(_)) {
            let Lane::Idle(mut dst) = std::mem::replace(&mut self.writer, Lane::Failed) else {
                return Err(lost());
            };
            let worker = Worker::spawn("ssx-ipc-write", move |data: Vec<u8>| {
                dst.write_all(&data)?;
                dst.flush()
            })?;
            self.writer = Lane::Running(worker);
        }
        match &mut self.writer {
            Lane::Running(w) => Ok(w),
            Lane::Idle(_) | Lane::Failed => Err(lost()),
        }
    }

    /// Reads at least one byte (or 0 on EOF) before `deadline`.
    pub(crate) fn read_some(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.stash.is_empty() {
            let worker = self.reader()?;
            // A request left over from a timed-out call is reused: its answer is what we want.
            worker.submit(buf.len())?;
            let chunk = worker.wait(deadline)??;
            self.stash = chunk;
            if self.stash.is_empty() {
                return Ok(0);
            }
        }
        let n = self.stash.len().min(buf.len());
        let (head, rest) = self.stash.split_at(n);
        buf.get_mut(..n).ok_or_else(lost)?.copy_from_slice(head);
        self.stash = rest.to_vec();
        Ok(n)
    }

    /// Writes all of `data` before `deadline`.
    pub(crate) fn write_all(&mut self, data: &[u8], deadline: Instant) -> io::Result<()> {
        let worker = self.writer()?;
        if worker.inflight {
            // An earlier, timed-out write is still going; keep byte order by finishing it first.
            worker.wait(deadline)??;
        }
        worker.submit(data.to_vec())?;
        worker.wait(deadline)?
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A duplex pipe double with Windows named-pipe semantics: reads block until data or until
    //! the peer is gone, there is no half-close and no timeouts, and EOF appears only when the
    //! other end is dropped. Writes block while `capacity` bytes are unread (a full pipe buffer).

    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};

    #[derive(Default)]
    struct State {
        data: VecDeque<u8>,
        writer_gone: bool,
        reader_gone: bool,
    }

    struct Shared {
        state: Mutex<State>,
        changed: Condvar,
        capacity: usize,
    }

    pub(crate) struct PipeRead(Arc<Shared>);
    pub(crate) struct PipeWrite(Arc<Shared>);

    /// One direction of the duplex pipe.
    pub(crate) fn pipe(capacity: usize) -> (PipeRead, PipeWrite) {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            capacity,
        });
        (PipeRead(Arc::clone(&shared)), PipeWrite(shared))
    }

    impl Read for PipeRead {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if !st.data.is_empty() {
                    let n = st.data.len().min(buf.len());
                    for slot in buf.iter_mut().take(n) {
                        *slot = st.data.pop_front().unwrap_or_default();
                    }
                    self.0.changed.notify_all();
                    return Ok(n);
                }
                if st.writer_gone {
                    return Ok(0);
                }
                st = self.0.changed.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    impl Write for PipeWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if st.reader_gone {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let room = self.0.capacity.saturating_sub(st.data.len());
                if room > 0 {
                    let n = room.min(buf.len());
                    st.data.extend(buf.iter().take(n));
                    self.0.changed.notify_all();
                    return Ok(n);
                }
                st = self.0.changed.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for PipeRead {
        fn drop(&mut self) {
            self.0.state.lock().unwrap_or_else(PoisonError::into_inner).reader_gone = true;
            self.0.changed.notify_all();
        }
    }

    impl Drop for PipeWrite {
        fn drop(&mut self) {
            self.0.state.lock().unwrap_or_else(PoisonError::into_inner).writer_gone = true;
            self.0.changed.notify_all();
        }
    }

    /// Both ends of a duplex connection: `(a_read, a_write)` talks to `(b_read, b_write)`.
    pub(crate) struct Duplex {
        pub(crate) read: PipeRead,
        pub(crate) write: PipeWrite,
    }

    pub(crate) fn duplex(capacity: usize) -> (Duplex, Duplex) {
        let (b_read, a_write) = pipe(capacity);
        let (a_read, b_write) = pipe(capacity);
        (Duplex { read: a_read, write: a_write }, Duplex { read: b_read, write: b_write })
    }
}
