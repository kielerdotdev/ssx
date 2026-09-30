//! The crate error type.
//!
//! Every wgpu failure mode that can be provoked by input or by the environment (no
//! adapter, device loss, validation/out-of-memory errors caught by error scopes, mapping
//! failures) is mapped to a variant here so callers never see a panic from the GPU layer.

use ssx_types::{ColorSpace, FrameError, PixelFormat};

/// Errors from the GPU pipeline.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    /// No suitable adapter (GPU or software rasteriser) was found.
    #[error(
        "no suitable GPU adapter found ({0}); set SSX_GPU_BACKEND / SSX_GPU_ADAPTER, install a \
         Vulkan driver (e.g. mesa-vulkan-drivers) or use the CPU tonemapper in ssx-hdr"
    )]
    NoAdapter(String),
    /// The adapter refused to create a device.
    #[error("could not create a GPU device: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),
    /// The device was lost (driver reset, GPU removed, explicitly destroyed). The context
    /// recreates the device on the next call; resources of the old device are invalid.
    #[error("the GPU device was lost: {0}")]
    DeviceLost(String),
    /// wgpu reported a validation error (a bug in this crate or an unsupported request).
    #[error("GPU validation error: {0}")]
    Validation(String),
    /// The GPU ran out of memory; retry with a smaller frame or a smaller tile limit.
    #[error("GPU out of memory")]
    OutOfMemory,
    /// wgpu reported an internal error.
    #[error("internal GPU error: {0}")]
    Internal(String),
    /// Waiting for the GPU failed.
    #[error("waiting for the GPU failed: {0}")]
    Poll(#[from] wgpu::PollError),
    /// Mapping a buffer for readback failed.
    #[error("GPU readback failed: {0}")]
    Readback(String),
    /// The tonemap settings are invalid.
    #[error(transparent)]
    Settings(#[from] ssx_hdr::HdrError),
    /// The frame's format / colour space is not accepted by this operation.
    #[error("unsupported frame: {format:?} / {space:?} ({expected})")]
    UnsupportedFrame {
        /// Format of the offending frame.
        format: PixelFormat,
        /// Colour space of the offending frame.
        space: ColorSpace,
        /// What the operation accepts.
        expected: &'static str,
    },
    /// The frame's `sdr_white_nits` is not a positive finite number.
    #[error("invalid SDR white level {0} nits; expected a positive finite value")]
    InvalidSdrWhite(f32),
    /// An argument is out of range (empty region, zero size, sigma <= 0, ...).
    #[error("invalid argument `{what}`: {reason}")]
    InvalidArgument {
        /// Name of the argument.
        what: &'static str,
        /// Why it is invalid.
        reason: String,
    },
    /// The image cannot be processed within the device limits even when tiled.
    #[error("image of {width}x{height} exceeds the device limits: {reason}")]
    TooLarge {
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
        /// Which limit is hit.
        reason: String,
    },
    /// Building a `Frame` from GPU output failed (internal size mismatch).
    #[error("frame construction failed: {0}")]
    Frame(#[from] FrameError),
}

/// Convenience alias.
pub type Result<T, E = GpuError> = std::result::Result<T, E>;

impl GpuError {
    pub(crate) fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidArgument { what, reason: reason.into() }
    }

    /// `true` if the device was lost and the operation may simply be retried (the context
    /// has already scheduled re-creation).
    pub fn is_device_lost(&self) -> bool {
        matches!(self, Self::DeviceLost(_))
    }
}
