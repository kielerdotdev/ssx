//! Screen recording for ssx.

#![deny(unsafe_code)]

pub mod audio;
pub mod convert;
pub mod encode;
pub mod error;
pub mod pacing;
pub mod queue;
pub mod source;
pub mod stats;
pub mod time;
pub mod timeline;
#[cfg(feature = "ffmpeg")]
pub mod verify;
