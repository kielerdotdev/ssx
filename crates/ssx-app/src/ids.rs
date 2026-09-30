//! Run ids: one counter shared by everything that starts a run (the supervisor, the
//! `PostFiles` coalescer), so an id from [`ssx_core::ipc::Response::Accepted`] is unique
//! for the life of the daemon.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Hands out run ids, starting at 1. Cheap to clone; all clones share the counter.
#[derive(Debug, Clone)]
pub struct RunIds(Arc<AtomicU64>);

impl RunIds {
    /// A counter whose first id is 1.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }

    /// The next id.
    pub fn next_id(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

impl Default for RunIds {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_shared_between_clones() {
        let a = RunIds::new();
        let b = a.clone();
        assert_eq!(a.next_id(), 1);
        assert_eq!(b.next_id(), 2);
        assert_eq!(a.next_id(), 3);
    }

    #[test]
    fn ids_are_unique_across_threads() {
        let ids = RunIds::new();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ids = ids.clone();
                std::thread::spawn(move || (0..200).map(|_| ids.next_id()).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 1600);
    }
}
