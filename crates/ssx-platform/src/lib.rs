//! Wires the OS capture backends and HDR tone-mapping into one [`ScreenSource`].
//!
//! Backends (`ssx-capture-*`) return frames in their **native** format, which on an HDR
//! Windows display is linear float scRGB. Everything above this crate wants ordinary 8-bit
//! sRGB, so [`ScreenSource`] converts each frame with `ssx-hdr` *per monitor, before*
//! stitching: on a mixed HDR + SDR setup the HDR monitor is tone-mapped and the SDR monitor
//! passes through untouched, and only then are they composed into one desktop image.
//!
//! [`detect_backend`] picks the backend for the current session, and reports why each
//! candidate that was tried was rejected, so "no backend" errors are actionable.

mod detect;
mod source;

pub use detect::{Attempt, BackendKind, Detected, detect_backend};
pub use source::{ScreenSource, to_sdr};
