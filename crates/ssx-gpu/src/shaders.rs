//! The WGSL sources, composed at compile time.
//!
//! WGSL has no `#include`, so shared code (the tonemap maths, the YUV core) is
//! concatenated in front of the shaders that need it. The composed strings are public so
//! they can be validated without a device (see `tests/shader_validation.rs`) and reused by
//! embedders that build their own pipelines.

/// HDR `Rgba16Float` texture to SDR `Rgba8Unorm` storage texture.
pub const TONEMAP_WGSL: &str =
    concat!(include_str!("shaders/tonemap_common.wgsl"), "\n", include_str!("shaders/tonemap.wgsl"));

/// 8-bit RGB(A) texture to NV12 / I420 planes (storage buffer).
pub const RGB_TO_YUV_WGSL: &str =
    concat!(include_str!("shaders/yuv_core.wgsl"), "\n", include_str!("shaders/rgb_fetch.wgsl"));

/// HDR texture, tonemapped and converted to NV12 / I420 planes in one dispatch.
pub const TONEMAP_TO_YUV_WGSL: &str = concat!(
    include_str!("shaders/tonemap_common.wgsl"),
    "\n",
    include_str!("shaders/yuv_core.wgsl"),
    "\n",
    include_str!("shaders/tonemap_fetch.wgsl")
);

/// NV12 / I420 planes (storage buffer) to an `Rgba8Unorm` storage texture.
pub const YUV_TO_RGB_WGSL: &str = include_str!("shaders/yuv_to_rgb.wgsl");

/// Weighted separable filter passes (Gaussian blur, Lanczos/bilinear resize).
pub const RESAMPLE_WGSL: &str = include_str!("shaders/resample.wgsl");

/// Block-average pixelate.
pub const PIXELATE_WGSL: &str = include_str!("shaders/pixelate.wgsl");

/// Every shader module with its name and the entry points it must expose (used by the
/// device-less validation test).
pub const ALL: &[(&str, &str, &[&str])] = &[
    ("tonemap", TONEMAP_WGSL, &["main"]),
    ("rgb_to_yuv", RGB_TO_YUV_WGSL, &["luma", "chroma"]),
    ("tonemap_to_yuv", TONEMAP_TO_YUV_WGSL, &["luma", "chroma"]),
    ("yuv_to_rgb", YUV_TO_RGB_WGSL, &["main"]),
    ("resample", RESAMPLE_WGSL, &["pass_h", "pass_v"]),
    ("pixelate", PIXELATE_WGSL, &["main"]),
];
