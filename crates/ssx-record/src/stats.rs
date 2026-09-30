//! Live counters of a recording, readable from any thread while it runs.
//!
//! The invariant the tests check: every *slot* of the constant-rate timeline ends up in
//! exactly one bucket, so `slots == encoded + dropped_backpressure`, and
//! `slots == captured_used + duplicated` (`captured_used` being source frames that were
//! not surplus). Nothing is counted twice and nothing silently vanishes.

use std::{
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Duration,
};

/// Shared atomic counters. Created by the session, updated by the pipeline threads.
#[derive(Debug, Default)]
pub struct Counters {
    /// Frames delivered by the source.
    pub captured: AtomicU64,
    /// Source frames discarded because the slot was already filled (source faster than
    /// the target rate).
    pub surplus_dropped: AtomicU64,
    /// Slots that repeat the previous frame.
    pub duplicated: AtomicU64,
    /// Timeline slots produced by the pacer (real + duplicate).
    pub slots: AtomicU64,
    /// Slots dropped because the encoder could not keep up.
    pub dropped_backpressure: AtomicU64,
    /// Of those, how many were duplicates (dropping them loses no picture).
    pub dropped_duplicates: AtomicU64,
    /// Slots handed to the encoder.
    pub encoded: AtomicU64,
    /// Audio sample frames (per channel) written to the encoder.
    pub audio_samples: AtomicU64,
    /// Audio sample frames discarded (overflow, pause) or synthesised as silence.
    pub audio_silence_samples: AtomicU64,
    /// Bytes of the output file so far (sampled).
    pub file_bytes: AtomicU64,
    /// Highest slot index that reached the encoder (+1), i.e. the encoded timeline length.
    pub encoded_timeline_slots: AtomicU64,
}

impl Counters {
    /// Snapshots the counters.
    pub fn snapshot(&self, recorded: Duration) -> StatsSnapshot {
        StatsSnapshot {
            captured: self.captured.load(Relaxed),
            surplus_dropped: self.surplus_dropped.load(Relaxed),
            duplicated: self.duplicated.load(Relaxed),
            slots: self.slots.load(Relaxed),
            dropped_backpressure: self.dropped_backpressure.load(Relaxed),
            dropped_duplicates: self.dropped_duplicates.load(Relaxed),
            encoded: self.encoded.load(Relaxed),
            audio_samples: self.audio_samples.load(Relaxed),
            audio_silence_samples: self.audio_silence_samples.load(Relaxed),
            file_bytes: self.file_bytes.load(Relaxed),
            recorded,
        }
    }
}

/// A point-in-time copy of [`Counters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Frames delivered by the source.
    pub captured: u64,
    /// Source frames dropped as surplus (source faster than the target rate).
    pub surplus_dropped: u64,
    /// Slots that repeat the previous frame.
    pub duplicated: u64,
    /// Timeline slots produced (real + duplicate).
    pub slots: u64,
    /// Slots dropped under encoder backpressure.
    pub dropped_backpressure: u64,
    /// Of `dropped_backpressure`, the ones that were duplicates.
    pub dropped_duplicates: u64,
    /// Slots encoded.
    pub encoded: u64,
    /// Audio sample frames written.
    pub audio_samples: u64,
    /// Audio sample frames of silence inserted to bridge gaps.
    pub audio_silence_samples: u64,
    /// Output file size in bytes (sampled while running, exact after `stop`).
    pub file_bytes: u64,
    /// Recorded (paused time excluded) duration.
    pub recorded: Duration,
}

impl StatsSnapshot {
    /// Real frames lost to backpressure (the ones that carried new picture content).
    pub fn dropped_real(&self) -> u64 {
        self.dropped_backpressure.saturating_sub(self.dropped_duplicates)
    }

    /// Fraction of slots that were dropped, `0.0..=1.0`.
    pub fn drop_ratio(&self) -> f64 {
        if self.slots == 0 { 0.0 } else { self.dropped_backpressure as f64 / self.slots as f64 }
    }
}
