//! Shared types for ssx.
//!
//! This crate is deliberately small and dependency-light. It defines:
//!
//! * [`geometry`] — [`Point`], [`Size`] and [`Rect`] in **physical pixels** on the
//!   virtual desktop (origin may be negative on multi-monitor setups).
//! * [`frame`] — [`Frame`], a CPU pixel buffer that always carries its stride, pixel
//!   format, colour space and (for HDR) the SDR white level it was captured at.
//! * [`display`] — [`Monitor`], [`WindowInfo`] and [`HdrInfo`].

pub mod display;
pub mod frame;
pub mod geometry;

pub use display::{HdrInfo, Monitor, WindowInfo};
pub use frame::{ColorSpace, EncodeOptions, Frame, FrameError, ImageFormat, PixelFormat};
pub use geometry::{Point, Rect, Size};
