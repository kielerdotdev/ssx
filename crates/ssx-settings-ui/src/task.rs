//! Background work for the UI thread: run a closure on a worker thread, poll for its result
//! each frame, and wake the UI when it is done.
//!
//! Everything that can block (file dialogs, the OS credential store, network uploads, the
//! diagnostics probe, database queries on a big history) goes through [`Task`], so no page
//! ever waits on I/O inside its `ui()` function. A task that is dropped before it finishes
//! simply has its result discarded; the worker is not interrupted (callers that need
//! cancellation pass a `CancelToken` into the closure).

use std::{
    fmt,
    sync::mpsc::{self, Receiver, TryRecvError},
};

/// Asks the UI to repaint (`ctx.request_repaint()`), from any thread.
pub type Waker = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A waker that does nothing (tests).
pub fn no_wake() -> Waker {
    std::sync::Arc::new(|| {})
}

/// A value being computed on another thread.
pub struct Task<T> {
    rx: Receiver<T>,
    done: bool,
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").field("done", &self.done).finish()
    }
}

impl<T: Send + 'static> Task<T> {
    /// Runs `work` on a new thread; `wake` is called when it has finished.
    pub fn spawn(wake: &Waker, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let wake = wake.clone();
        let spawned = std::thread::Builder::new().name("ssx-task".to_owned()).spawn(move || {
            let value = work();
            let _ = tx.send(value);
            wake();
        });
        // If the OS refuses a thread the sender is dropped with the closure: the task then
        // reports `None` forever, which callers treat as "never finished".
        let _ = spawned;
        Self { rx, done: false }
    }
}

impl<T> Task<T> {
    /// The result, once, when it is ready.
    pub fn poll(&mut self) -> Option<T> {
        if self.done {
            return None;
        }
        match self.rx.try_recv() {
            Ok(v) => {
                self.done = true;
                Some(v)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                None
            }
        }
    }

    /// `true` while the worker has not delivered and the task has not been abandoned.
    pub fn is_running(&self) -> bool {
        !self.done
    }

    /// Blocks until the result arrives or `timeout` passes (tests).
    pub fn wait(&mut self, timeout: std::time::Duration) -> Option<T> {
        if self.done {
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(v) => {
                self.done = true;
                Some(v)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.done = true;
                None
            }
        }
    }
}

/// A task slot that pages keep: at most one result in flight, replaced when a new one starts.
pub struct Slot<T> {
    task: Option<Task<T>>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self { task: None }
    }
}

impl<T> fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("running", &self.task.as_ref().is_some_and(Task::is_running))
            .finish()
    }
}

impl<T: Send + 'static> Slot<T> {
    /// Starts `work`, abandoning the previous task's result.
    pub fn start(&mut self, wake: &Waker, work: impl FnOnce() -> T + Send + 'static) {
        self.task = Some(Task::spawn(wake, work));
    }
}

impl<T> Slot<T> {
    /// The result, once, when ready.
    pub fn poll(&mut self) -> Option<T> {
        let t = self.task.as_mut()?;
        let r = t.poll();
        if !t.is_running() {
            self.task = None;
        }
        r
    }

    /// Whether something is running.
    pub fn running(&self) -> bool {
        self.task.as_ref().is_some_and(Task::is_running)
    }

    /// Blocks for the result (tests).
    pub fn wait(&mut self, timeout: std::time::Duration) -> Option<T> {
        let t = self.task.as_mut()?;
        let r = t.wait(timeout);
        if !t.is_running() {
            self.task = None;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    #[test]
    fn result_arrives_once_and_wakes() {
        let n = std::sync::Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let wake: Waker = std::sync::Arc::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        let mut t = Task::spawn(&wake, || 21 * 2);
        assert_eq!(t.wait(Duration::from_secs(5)), Some(42));
        assert_eq!(t.poll(), None, "delivered once");
        assert!(!t.is_running());
        // the wake-up follows the send, so give the worker a moment to make it
        for _ in 0..200 {
            if n.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn poll_is_non_blocking() {
        let (tx, rx) = mpsc::channel::<()>();
        let mut t = Task::spawn(&no_wake(), move || {
            let _ = rx.recv();
            7
        });
        assert_eq!(t.poll(), None);
        assert!(t.is_running());
        tx.send(()).unwrap();
        assert_eq!(t.wait(Duration::from_secs(5)), Some(7));
    }

    #[test]
    fn a_panicking_worker_ends_the_task_without_a_result() {
        let mut t = Task::<u8>::spawn(&no_wake(), || panic!("boom"));
        assert_eq!(t.wait(Duration::from_secs(5)), None);
        assert!(!t.is_running());
    }

    #[test]
    fn a_slot_keeps_only_the_newest_task() {
        let mut s = Slot::default();
        let (tx, rx) = mpsc::channel::<()>();
        s.start(&no_wake(), move || {
            let _ = rx.recv();
            1
        });
        assert!(s.running());
        s.start(&no_wake(), || 2);
        assert_eq!(s.wait(Duration::from_secs(5)), Some(2));
        drop(tx);
        assert!(!s.running());
        assert_eq!(s.poll(), None);
    }
}
