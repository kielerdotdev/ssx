//! Merging `PostFiles` requests that arrive together into one batch.
//!
//! Windows Explorer starts one `ssx post-file --coalesce -- "%1"` process *per selected
//! file*. Each forwards its single path here and exits at once. Uploading each on its own
//! would give N runs, N notifications and N clipboard writes (the last one winning); merging
//! requests that arrive within ~400 ms gives one upload run and one combined URL list.
//!
//! * The window is **sliding**: every arrival pushes the deadline out to `now + window`, so a
//!   slow launch of the N-th process still joins. It is **capped** (`max_wait` after the first
//!   arrival), so a stream of requests cannot delay the upload forever.
//! * Requests only merge when their [`PostAction`] is equal (a plain upload and an
//!   "edit first" upload of the same file are different intents).
//! * Paths are **deduplicated, order preserved** (Explorer may deliver the same file twice; the
//!   first occurrence keeps its position).
//! * A batch also closes when it reaches `max_paths`, bounding memory and line lengths.
//! * The batch id is drawn from the shared [`RunIds`] when the batch *opens*, so the id given
//!   to the first caller in `Accepted` is the id the run will have.
//!
//! [`CoalesceState`] is the pure state machine (time is an argument). [`Coalescer`] adds a
//! thread that sleeps until the next deadline, driven by an injectable [`Clock`]: tests advance a
//! fake clock and [`poke`](Coalescer::poke) instead of sleeping. Nothing polls: with no open
//! batch the thread waits on a condition variable indefinitely.

use std::{
    collections::HashSet,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use ssx_core::ipc::PostAction;

use crate::{clock::Clock, ids::RunIds};

/// Timing and size limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceConfig {
    /// A batch closes this long after its last arrival.
    pub window: Duration,
    /// ...but never later than this after its first arrival.
    pub max_wait: Duration,
    /// A batch closes at once when it holds this many distinct paths.
    pub max_paths: usize,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_millis(400),
            max_wait: Duration::from_secs(2),
            max_paths: 10_000,
        }
    }
}

/// A closed batch: what to run. `W` is whatever the caller wants to hear about the result
/// (a channel sender for `wait` requests).
#[derive(Debug)]
pub struct Batch<W> {
    /// Run id (see the module docs).
    pub id: u64,
    /// The shared action.
    pub action: PostAction,
    /// Distinct paths in first-arrival order.
    pub paths: Vec<PathBuf>,
    /// One entry per request that asked to be told the result.
    pub waiters: Vec<W>,
    /// How many requests were merged.
    pub requests: usize,
}

#[derive(Debug)]
struct Open<W> {
    id: u64,
    action: PostAction,
    paths: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    waiters: Vec<W>,
    requests: usize,
    first: Duration,
    deadline: Duration,
}

impl<W> Open<W> {
    fn close(self) -> Batch<W> {
        Batch {
            id: self.id,
            action: self.action,
            paths: self.paths,
            waiters: self.waiters,
            requests: self.requests,
        }
    }
}

/// What [`CoalesceState::add`] did.
#[derive(Debug)]
pub struct Added<W> {
    /// The id of the batch the request joined.
    pub batch_id: u64,
    /// Set when this request filled the batch to `max_paths`: it is closed already.
    pub closed: Option<Batch<W>>,
}

/// The pure part. See the module docs.
#[derive(Debug)]
pub struct CoalesceState<W> {
    cfg: CoalesceConfig,
    ids: RunIds,
    open: Vec<Open<W>>,
}

impl<W> CoalesceState<W> {
    /// An empty state.
    pub fn new(cfg: CoalesceConfig, ids: RunIds) -> Self {
        Self { cfg, ids, open: Vec::new() }
    }

    /// Adds one request at time `now`.
    pub fn add(
        &mut self,
        now: Duration,
        action: &PostAction,
        paths: Vec<PathBuf>,
        waiter: Option<W>,
    ) -> Added<W> {
        let idx = if let Some(i) = self.open.iter().position(|o| &o.action == action) {
            i
        } else {
            self.open.push(Open {
                id: self.ids.next_id(),
                action: action.clone(),
                paths: Vec::new(),
                seen: HashSet::new(),
                waiters: Vec::new(),
                requests: 0,
                first: now,
                deadline: now,
            });
            self.open.len() - 1
        };
        let b = &mut self.open[idx];
        for p in paths {
            if b.seen.insert(p.clone()) {
                b.paths.push(p);
            }
        }
        b.waiters.extend(waiter);
        b.requests += 1;
        b.deadline = (now + self.cfg.window).min(b.first + self.cfg.max_wait);
        let batch_id = b.id;
        let closed = (b.paths.len() >= self.cfg.max_paths).then(|| self.open.remove(idx).close());
        Added { batch_id, closed }
    }

    /// Removes and returns the batches whose deadline has passed, oldest first.
    pub fn take_due(&mut self, now: Duration) -> Vec<Batch<W>> {
        let mut due = Vec::new();
        let mut i = 0;
        while i < self.open.len() {
            if self.open[i].deadline <= now {
                due.push(self.open.remove(i).close());
            } else {
                i += 1;
            }
        }
        due
    }

    /// The earliest deadline among open batches.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.open.iter().map(|o| o.deadline).min()
    }

    /// Closes everything now (shutdown: files handed over must not be lost).
    pub fn drain(&mut self) -> Vec<Batch<W>> {
        self.open.drain(..).map(Open::close).collect()
    }

    /// Number of open batches.
    pub fn open_batches(&self) -> usize {
        self.open.len()
    }
}

type Sink<W> = Box<dyn Fn(Batch<W>) + Send + Sync>;

struct Shared<W> {
    state: Mutex<CoalesceState<W>>,
    cv: Condvar,
    clock: Arc<dyn Clock>,
    sink: Sink<W>,
    stop: AtomicBool,
}

impl<W> Shared<W> {
    fn lock(&self) -> MutexGuard<'_, CoalesceState<W>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deliver(&self, batches: Vec<Batch<W>>) {
        for b in batches {
            let id = b.id;
            // A misbehaving sink must not kill the thread that every later batch needs.
            if std::panic::catch_unwind(AssertUnwindSafe(|| (self.sink)(b))).is_err() {
                tracing::error!(batch = id, "the batch handler panicked");
            }
        }
    }
}

/// The threaded coalescer. See the module docs.
pub struct Coalescer<W: Send + 'static> {
    shared: Arc<Shared<W>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl<W: Send + 'static> std::fmt::Debug for Coalescer<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coalescer").finish_non_exhaustive()
    }
}

impl<W: Send + 'static> Coalescer<W> {
    /// Starts the timer thread. `sink` receives every closed batch on that thread (or on the
    /// caller's thread for a batch closed by `max_paths` / shutdown) and should return
    /// promptly: hand the batch to the supervisor and return.
    pub fn new(
        cfg: CoalesceConfig,
        ids: RunIds,
        clock: Arc<dyn Clock>,
        sink: impl Fn(Batch<W>) + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(CoalesceState::new(cfg, ids)),
            cv: Condvar::new(),
            clock,
            sink: Box::new(sink),
            stop: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("ssx-coalesce".into())
            .spawn(move || run(&worker))
            .map_err(|e| tracing::error!("cannot start the coalescer thread: {e}"))
            .ok();
        Self { shared, thread: Mutex::new(thread) }
    }

    /// Adds a request; returns the id of the batch (= run) it belongs to.
    pub fn add(&self, action: &PostAction, paths: Vec<PathBuf>, waiter: Option<W>) -> u64 {
        let added = {
            let mut st = self.shared.lock();
            st.add(self.shared.clock.now(), action, paths, waiter)
        };
        self.shared.cv.notify_all();
        if let Some(b) = added.closed {
            self.shared.deliver(vec![b]);
        }
        added.batch_id
    }

    /// Makes the timer thread re-read the clock (tests advance a [`FakeClock`]
    /// (`crate::clock::FakeClock`) and call this; real time needs no poke).
    pub fn poke(&self) {
        // Taking the lock orders the poke after any state change the caller just made.
        drop(self.shared.lock());
        self.shared.cv.notify_all();
    }

    /// Delivers every open batch now and stops the thread.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let rest = self.shared.lock().drain();
        self.shared.cv.notify_all();
        self.shared.deliver(rest);
        let handle = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl<W: Send + 'static> Drop for Coalescer<W> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run<W>(shared: &Shared<W>) {
    let mut guard = shared.lock();
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let now = shared.clock.now();
        let due = guard.take_due(now);
        if !due.is_empty() {
            drop(guard);
            shared.deliver(due);
            guard = shared.lock();
            continue;
        }
        guard = match guard.next_deadline() {
            None => shared.cv.wait(guard).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let wait = deadline.saturating_sub(now).max(Duration::from_millis(1));
                shared.cv.wait_timeout(guard, wait).unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::{self, Receiver, Sender},
        time::Instant,
    };

    use super::*;
    use crate::clock::FakeClock;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn state() -> CoalesceState<u32> {
        CoalesceState::new(CoalesceConfig::default(), RunIds::new())
    }

    // ---- the pure state machine ---------------------------------------------------------

    #[test]
    fn requests_within_the_window_form_one_batch_in_order() {
        let mut s = state();
        let a = s.add(ms(0), &PostAction::Upload, paths(&["/a"]), Some(1));
        let b = s.add(ms(100), &PostAction::Upload, paths(&["/b"]), None);
        let c = s.add(ms(250), &PostAction::Upload, paths(&["/c"]), Some(3));
        assert_eq!((a.batch_id, b.batch_id, c.batch_id), (1, 1, 1), "one batch, one id");
        assert!(s.take_due(ms(649)).is_empty(), "the window slides: 250 + 400 = 650");
        let due = s.take_due(ms(650));
        assert_eq!(due.len(), 1);
        let batch = &due[0];
        assert_eq!(batch.paths, paths(&["/a", "/b", "/c"]));
        assert_eq!(batch.waiters, [1, 3]);
        assert_eq!(batch.requests, 3);
        assert_eq!(s.open_batches(), 0);
    }

    #[test]
    fn a_gap_longer_than_the_window_starts_a_new_batch() {
        let mut s = state();
        s.add(ms(0), &PostAction::Upload, paths(&["/a"]), None);
        let first = s.take_due(ms(400));
        assert_eq!(first[0].paths, paths(&["/a"]));
        let b = s.add(ms(401), &PostAction::Upload, paths(&["/b"]), None);
        assert_eq!(b.batch_id, 2, "a fresh batch gets a fresh id");
        assert_eq!(s.take_due(ms(801))[0].paths, paths(&["/b"]));
    }

    #[test]
    fn paths_are_deduplicated_keeping_the_first_position() {
        let mut s = state();
        s.add(ms(0), &PostAction::Upload, paths(&["/a", "/b", "/a"]), None);
        s.add(ms(10), &PostAction::Upload, paths(&["/c", "/b", "/a"]), None);
        let due = s.take_due(ms(10_000));
        assert_eq!(due[0].paths, paths(&["/a", "/b", "/c"]));
    }

    #[test]
    fn the_wait_is_capped_so_a_stream_of_requests_cannot_starve_the_upload() {
        let mut s = state();
        let mut t = 0;
        while t <= 2_000 {
            s.add(ms(t), &PostAction::Upload, paths(&[&format!("/f{t}")]), None);
            t += 100; // never a 400 ms gap
        }
        assert_eq!(s.next_deadline(), Some(ms(2_000)), "capped at first + max_wait");
        let due = s.take_due(ms(2_000));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].paths.len(), 21);
        // The 21st arrival at 2000 ms would have pushed the deadline to 2400.
        assert_eq!(s.open_batches(), 0);
    }

    #[test]
    fn different_actions_do_not_merge() {
        let mut s = state();
        let w = PostAction::Workflow { workflow: "w".into() };
        let a = s.add(ms(0), &PostAction::Upload, paths(&["/a"]), None);
        let b = s.add(ms(1), &PostAction::Edit, paths(&["/a"]), None);
        let c = s.add(ms(2), &w, paths(&["/a"]), None);
        let d = s.add(ms(3), &PostAction::Upload, paths(&["/b"]), None);
        assert_ne!(a.batch_id, b.batch_id);
        assert_ne!(b.batch_id, c.batch_id);
        assert_eq!(a.batch_id, d.batch_id);
        let mut due = s.take_due(ms(1_000));
        due.sort_by_key(|b| b.id);
        assert_eq!(due.len(), 3);
        assert_eq!(due[0].paths, paths(&["/a", "/b"]));
        assert_eq!(due[1].action, PostAction::Edit);
        assert_eq!(due[2].action, w);
    }

    #[test]
    fn a_full_batch_closes_at_once() {
        let mut s: CoalesceState<u32> = CoalesceState::new(
            CoalesceConfig { max_paths: 3, ..CoalesceConfig::default() },
            RunIds::new(),
        );
        assert!(s.add(ms(0), &PostAction::Upload, paths(&["/a", "/b"]), None).closed.is_none());
        let r = s.add(ms(1), &PostAction::Upload, paths(&["/b", "/c"]), Some(9));
        let closed = r.closed.expect("three distinct paths close the batch");
        assert_eq!(closed.paths, paths(&["/a", "/b", "/c"]));
        assert_eq!(closed.waiters, [9]);
        assert_eq!(r.batch_id, closed.id);
        assert_eq!(s.open_batches(), 0);
        assert!(s.take_due(ms(10_000)).is_empty());
    }

    #[test]
    fn drain_returns_everything_and_deadlines_track_the_earliest() {
        let mut s = state();
        assert_eq!(s.next_deadline(), None);
        s.add(ms(0), &PostAction::Upload, paths(&["/a"]), None);
        s.add(ms(100), &PostAction::Edit, paths(&["/b"]), None);
        assert_eq!(s.next_deadline(), Some(ms(400)));
        let all = s.drain();
        assert_eq!(all.len(), 2);
        assert_eq!(s.next_deadline(), None);
    }

    #[test]
    fn empty_requests_still_join_and_extend_but_add_no_paths() {
        let mut s = state();
        s.add(ms(0), &PostAction::Upload, paths(&["/a"]), None);
        s.add(ms(300), &PostAction::Upload, vec![], Some(1));
        let due = s.take_due(ms(700));
        assert_eq!(due[0].paths, paths(&["/a"]));
        assert_eq!(due[0].waiters, [1]);
    }

    // ---- the threaded coalescer with a fake clock -----------------------------------------

    struct Fixture {
        clock: Arc<FakeClock>,
        co: Coalescer<Sender<u64>>,
        out: Receiver<Batch<Sender<u64>>>,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::new());
        let (tx, out) = mpsc::channel();
        let tx = Mutex::new(tx);
        let co =
            Coalescer::new(CoalesceConfig::default(), RunIds::new(), clock.clone(), move |b| {
                let _ = tx.lock().unwrap().send(b);
            });
        Fixture { clock, co, out }
    }

    impl Fixture {
        fn advance(&self, by: Duration) {
            self.clock.advance(by);
            self.co.poke();
        }
    }

    #[test]
    fn nothing_is_delivered_until_the_fake_clock_passes_the_window() {
        let f = fixture();
        let id = f.co.add(&PostAction::Upload, paths(&["/a"]), None);
        f.co.add(&PostAction::Upload, paths(&["/b"]), None);
        f.advance(ms(399));
        assert!(f.out.recv_timeout(ms(150)).is_err(), "399 ms: still open");
        f.advance(ms(1));
        let b = f.out.recv_timeout(Duration::from_secs(5)).expect("delivered at 400 ms");
        assert_eq!(b.id, id);
        assert_eq!(b.paths, paths(&["/a", "/b"]));
        assert!(f.out.recv_timeout(ms(100)).is_err(), "exactly once");
    }

    #[test]
    fn arrivals_from_many_threads_land_in_one_batch() {
        let f = fixture();
        let co = &f.co;
        let ids: HashSet<u64> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    s.spawn(move || co.add(&PostAction::Upload, paths(&[&format!("/f{i}")]), None))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(ids.len(), 1, "every thread was told the same run id");
        f.advance(ms(400));
        let b = f.out.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(b.paths.len(), 16);
        assert_eq!(b.requests, 16);
    }

    #[test]
    fn waiters_are_handed_over_with_the_batch() {
        let f = fixture();
        let (tx, rx) = mpsc::channel();
        f.co.add(&PostAction::Upload, paths(&["/a"]), Some(tx.clone()));
        f.co.add(&PostAction::Upload, paths(&["/b"]), Some(tx));
        f.advance(ms(400));
        let b = f.out.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(b.waiters.len(), 2);
        for w in &b.waiters {
            w.send(b.id).unwrap();
        }
        assert_eq!(rx.recv().unwrap(), b.id);
        assert_eq!(rx.recv().unwrap(), b.id);
    }

    #[test]
    fn shutdown_flushes_open_batches_and_stops_the_thread() {
        let f = fixture();
        f.co.add(&PostAction::Upload, paths(&["/a"]), None);
        f.co.add(&PostAction::Edit, paths(&["/b"]), None);
        f.co.shutdown();
        let mut got: Vec<_> = f.out.try_iter().collect();
        got.sort_by_key(|b| b.id);
        assert_eq!(got.len(), 2, "files handed over are never dropped at exit");
        f.co.shutdown(); // idempotent
    }

    #[test]
    fn a_panicking_sink_does_not_kill_the_timer_thread() {
        let clock = Arc::new(FakeClock::new());
        let (tx, rx) = mpsc::channel::<u64>();
        let tx = Mutex::new(tx);
        let first = AtomicBool::new(true);
        let co: Coalescer<()> =
            Coalescer::new(CoalesceConfig::default(), RunIds::new(), clock.clone(), move |b| {
                assert!(!first.swap(false, Ordering::SeqCst), "boom");
                let _ = tx.lock().unwrap().send(b.id);
            });
        co.add(&PostAction::Upload, paths(&["/a"]), None);
        clock.advance(ms(400));
        co.poke();
        std::thread::sleep(ms(100));
        co.add(&PostAction::Upload, paths(&["/b"]), None);
        clock.advance(ms(400));
        co.poke();
        let id = rx.recv_timeout(Duration::from_secs(5)).expect("the second batch is delivered");
        assert_eq!(id, 2);
    }

    #[test]
    fn real_time_works_end_to_end_and_the_thread_does_not_spin() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let co: Coalescer<()> = Coalescer::new(
            CoalesceConfig { window: ms(60), max_wait: ms(500), max_paths: 100 },
            RunIds::new(),
            Arc::new(crate::clock::SystemClock::new()),
            move |b| {
                let _ = tx.lock().unwrap().send((Instant::now(), b));
            },
        );
        let t0 = Instant::now();
        co.add(&PostAction::Upload, paths(&["/a"]), None);
        std::thread::sleep(ms(30));
        co.add(&PostAction::Upload, paths(&["/b"]), None);
        let (at, b) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(b.paths.len(), 2);
        assert!(at - t0 >= ms(85), "the window slid to 30 + 60 ms: {:?}", at - t0);
        assert!(rx.recv_timeout(ms(200)).is_err());
    }
}
