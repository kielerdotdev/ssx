//! Screen recording for ssx.
//!
//! ```text
//!  FrameSource --> capture thread (CfrPacer) --> lossy queue --> convert --> encoder thread --> file
//!  AudioSource --> audio thread (resample, drift, mix) ---------------------> (same encoder)
//! ```
//!
//! * [`source`]: where frames come from (X11, wlroots protocols, the portal + `PipeWire`,
//!   Windows Graphics Capture, a synthetic test pattern);
//! * [`audio`]: system audio and microphone through `cpal`, resampling, drift compensation
//!   and mixing on a shared monotonic clock;
//! * [`encode`]: the [`encode::Encoder`] trait, `FFmpeg` (H.264/HEVC/AV1/VP9 with hardware
//!   encoders probed first) and GIF through gifski;
//! * [`session`]: the pipeline with bounded queues, drop policy, constant-frame-rate pacing,
//!   pause/resume, limits, graceful stop and abort;
//! * [`adapter`]: [`ssx_core::workflow::Recorder`] on top of the session.
//!
//! The README of the crate has the platform matrix, what was verified where, and the build
//! notes for a statically linked `FFmpeg`.

#![deny(unsafe_code)]
// Pixel, sample and timestamp arithmetic converts between `u32`/`usize`/`i32`/`i64`
// constantly; the values are bounded (frame sizes, sample counts of a recording), so the
// wrapping casts cannot occur. The workspace already allows the other numeric cast lints.
#![allow(clippy::cast_possible_wrap)]

pub mod adapter;
pub mod audio;
pub mod convert;
pub mod encode;
pub mod error;
pub mod pacing;
pub mod queue;
pub mod session;
pub mod source;
pub mod stats;
pub mod time;
pub mod timeline;
#[cfg(feature = "ffmpeg")]
pub mod verify;
