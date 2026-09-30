//! Bounded queues between the pipeline stages.
//!
//! Two flavours share one implementation:
//!
//! * a **lossy** push for the capture -> convert edge: when the queue is full the
//!   cheapest item is sacrificed (a duplicate slot before a real frame, and the newest
//!   real frame before anything queued), so a slow encoder costs frames, never memory;
//! * a **blocking** push for later edges, which propagates backpressure upstream so the
//!   drop happens at exactly one place and is counted exactly once.
//!
//! `std::sync::mpsc` cannot do this: it can neither evict a queued item nor be closed
//! from the consumer side, and both matter for graceful stop and abort.

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// How valuable a queued item is when the queue overflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// A repeat of the previous picture: dropping it loses nothing visible.
    Cheap,
    /// A real frame.
    Real,
}

/// Result of a lossy push.
#[derive(Debug, PartialEq, Eq)]
pub enum Push<T> {
    /// The item was queued.
    Queued,
    /// The queue was full: this item (the evicted or the offered one) was dropped.
    Dropped(T),
    /// The queue is closed; the item is returned.
    Closed(T),
}

#[derive(Debug)]
struct Inner<T> {
    items: VecDeque<(Rank, T)>,
    closed: bool,
}

/// A bounded multi-producer multi-consumer queue with eviction. See the module docs.
#[derive(Debug)]
pub struct Queue<T> {
    inner: Mutex<Inner<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
}

impl<T> Queue<T> {
    /// A queue holding at most `capacity` items (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { items: VecDeque::new(), closed: false }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Capacity in items.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current length.
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Queues `item`, evicting on overflow (see the module docs). Never blocks.
    pub fn push_lossy(&self, rank: Rank, item: T) -> Push<T> {
        let mut g = self.lock();
        if g.closed {
            return Push::Closed(item);
        }
        if g.items.len() >= self.capacity {
            // Prefer to evict the oldest cheap item; else drop the offered item.
            if let Some(pos) = g.items.iter().position(|(r, _)| *r == Rank::Cheap) {
                let (_, evicted) = g.items.remove(pos).expect("position() just found this index");
                g.items.push_back((rank, item));
                drop(g);
                self.not_empty.notify_one();
                return Push::Dropped(evicted);
            }
            return Push::Dropped(item);
        }
        g.items.push_back((rank, item));
        drop(g);
        self.not_empty.notify_one();
        Push::Queued
    }

    /// Queues `item`, waiting while the queue is full. Returns the item back if the queue
    /// was closed or `give_up` returned `true` while waiting.
    pub fn push_blocking(&self, item: T, give_up: impl Fn() -> bool) -> Result<(), T> {
        let mut g = self.lock();
        loop {
            if g.closed || give_up() {
                return Err(item);
            }
            if g.items.len() < self.capacity {
                g.items.push_back((Rank::Real, item));
                drop(g);
                self.not_empty.notify_one();
                return Ok(());
            }
            g = self
                .not_full
                .wait_timeout(g, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Queues without any capacity check (for tiny, must-not-lose items such as audio
    /// blocks); the caller bounds the volume itself. Returns the item if closed.
    pub fn push_unbounded(&self, item: T) -> Result<(), T> {
        let mut g = self.lock();
        if g.closed {
            return Err(item);
        }
        g.items.push_back((Rank::Real, item));
        drop(g);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Pops the oldest item, waiting up to `timeout`. `Ok(None)` means timeout;
    /// `Err(Closed)` means the queue is closed **and** drained.
    pub fn pop(&self, timeout: Duration) -> Result<Option<T>, Closed> {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            if let Some((_, item)) = g.items.pop_front() {
                drop(g);
                self.not_full.notify_one();
                return Ok(Some(item));
            }
            if g.closed {
                return Err(Closed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            g = self
                .not_empty
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Marks the queue closed: producers are refused, consumers drain what is left.
    pub fn close(&self) {
        self.lock().closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Closes and discards everything queued (abort).
    pub fn close_and_clear(&self) -> usize {
        let mut g = self.lock();
        g.closed = true;
        let n = g.items.len();
        g.items.clear();
        drop(g);
        self.not_empty.notify_all();
        self.not_full.notify_all();
        n
    }
}

/// The queue is closed and empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use super::*;

    #[test]
    fn lossy_push_evicts_cheap_before_real() {
        let q = Queue::new(3);
        assert_eq!(q.push_lossy(Rank::Real, 1), Push::Queued);
        assert_eq!(q.push_lossy(Rank::Cheap, 2), Push::Queued);
        assert_eq!(q.push_lossy(Rank::Real, 3), Push::Queued);
        // Full: a real frame evicts the queued cheap one.
        assert_eq!(q.push_lossy(Rank::Real, 4), Push::Dropped(2));
        // Full of real frames: the offered item is dropped.
        assert_eq!(q.push_lossy(Rank::Real, 5), Push::Dropped(5));
        assert_eq!(q.push_lossy(Rank::Cheap, 6), Push::Dropped(6));
        let got: Vec<_> = std::iter::from_fn(|| q.pop(Duration::ZERO).unwrap()).collect();
        assert_eq!(got, vec![1, 3, 4]);
    }

    #[test]
    fn close_drains_then_reports_closed() {
        let q = Queue::new(4);
        q.push_lossy(Rank::Real, 1);
        q.close();
        assert_eq!(q.push_lossy(Rank::Real, 2), Push::Closed(2));
        assert_eq!(q.pop(Duration::from_millis(10)), Ok(Some(1)));
        assert_eq!(q.pop(Duration::from_millis(10)), Err(Closed));
    }

    #[test]
    fn pop_times_out_when_empty() {
        let q: Queue<u8> = Queue::new(1);
        let t = Instant::now();
        assert_eq!(q.pop(Duration::from_millis(30)), Ok(None));
        assert!(t.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn blocking_push_waits_for_space_and_honours_give_up() {
        let q = Arc::new(Queue::new(1));
        q.push_blocking(1, || false).unwrap();
        let q2 = Arc::clone(&q);
        let h = thread::spawn(move || q2.push_blocking(2, || false));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(q.pop(Duration::from_millis(10)), Ok(Some(1)));
        assert!(h.join().unwrap().is_ok());
        assert_eq!(q.pop(Duration::from_millis(100)), Ok(Some(2)));
        // give_up unblocks a stuck producer.
        q.push_blocking(3, || false).unwrap();
        assert_eq!(q.push_blocking(4, || true), Err(4));
    }

    #[test]
    fn close_wakes_blocked_producer() {
        let q = Arc::new(Queue::new(1));
        q.push_blocking(1, || false).unwrap();
        let q2 = Arc::clone(&q);
        let h = thread::spawn(move || q2.push_blocking(2, || false));
        thread::sleep(Duration::from_millis(30));
        q.close();
        assert_eq!(h.join().unwrap(), Err(2));
    }

    #[test]
    fn close_and_clear_discards() {
        let q = Queue::new(4);
        q.push_lossy(Rank::Real, 1);
        q.push_lossy(Rank::Real, 2);
        assert_eq!(q.close_and_clear(), 2);
        assert_eq!(q.pop(Duration::ZERO), Err(Closed));
    }
}
