//! wgpu compute pipeline for HDR to SDR tonemapping and video-frame conversion.
//!
//! * [`GpuContext`]: headless device, adapter policy, device-loss recovery.
//! * [`GpuTonemapper`]: `Rgba16F` scRGB to `Rgba8` sRGB, matching [`ssx_hdr`].
//! * [`YuvConverter`]: RGB / tonemapped HDR to NV12 or I420, and back.
//! * [`GpuFx`]: Gaussian blur, pixelate and resize for the editor.
//!
//! See the crate README for the parity statement and integration notes.

#![forbid(unsafe_code)]

pub mod context;
pub mod convert;
pub mod error;
pub mod fx;
pub mod shaders;
pub mod tonemap;
mod util;
pub mod yuv;

pub use context::{BackendChoice, DeviceHandle, GpuContext, GpuOptions};
pub use convert::{PlaneLayout, YuvConverter, YuvInput, YuvPass};
pub use error::{GpuError, Result};
pub use fx::{GpuFx, ResizeFilter};
pub use tonemap::{GpuTonemapper, TonemapPass};
pub use util::TileLimits;
pub use yuv::{ChromaSiting, ColorMatrix, YuvFrame, YuvLayout, YuvOptions, YuvRange};
