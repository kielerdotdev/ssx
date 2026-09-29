//! Bounded-size PNG thumbnails.
//!
//! Thumbnails are stored in the database, so their size is capped in *bytes* as well as
//! pixels: a noisy 256x256 screenshot can compress badly, and the history window loads many
//! of them at once. If the first encode is too big the edge is shrunk until it fits.

use image::{ImageEncoder, RgbaImage, codecs::png, imageops::FilterType};
use ssx_types::Frame;

/// Thumbnail limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailOptions {
    /// Longest edge in pixels (never upscales).
    pub max_edge: u32,
    /// Encoded size cap in bytes.
    pub max_bytes: usize,
}

impl Default for ThumbnailOptions {
    fn default() -> Self {
        Self { max_edge: 256, max_bytes: 256 * 1024 }
    }
}

/// Why a thumbnail could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    /// The source image has no pixels.
    #[error("cannot make a thumbnail of an empty image")]
    Empty,
    /// Even the smallest size exceeded the byte cap.
    #[error("thumbnail does not fit in {0} bytes even at the minimum size")]
    TooLarge(usize),
    /// The frame is HDR or float and must be tonemapped first.
    #[error("frame is not 8-bit sRGB (tonemap it first): {0}")]
    NotSdr(String),
    /// Decoding or encoding failed.
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
}

const MIN_EDGE: u32 = 16;

/// Thumbnail of an 8-bit sRGB frame, PNG encoded.
pub fn thumbnail_from_frame(
    frame: &Frame,
    opts: ThumbnailOptions,
) -> Result<Vec<u8>, ThumbnailError> {
    let img = frame.to_image().map_err(|e| ThumbnailError::NotSdr(e.to_string()))?;
    thumbnail_from_image(&img, opts)
}

/// Thumbnail of an encoded image file's bytes (PNG/JPEG/WebP/BMP/GIF).
pub fn thumbnail_from_bytes(
    bytes: &[u8],
    opts: ThumbnailOptions,
) -> Result<Vec<u8>, ThumbnailError> {
    let img = image::load_from_memory(bytes)?.into_rgba8();
    thumbnail_from_image(&img, opts)
}

/// Thumbnail of an RGBA image.
pub fn thumbnail_from_image(
    img: &RgbaImage,
    opts: ThumbnailOptions,
) -> Result<Vec<u8>, ThumbnailError> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(ThumbnailError::Empty);
    }
    let mut edge = opts.max_edge.max(MIN_EDGE);
    loop {
        let scaled = scale_to_edge(img, edge);
        let png = encode_png(&scaled)?;
        if png.len() <= opts.max_bytes {
            return Ok(png);
        }
        if edge <= MIN_EDGE {
            return Err(ThumbnailError::TooLarge(opts.max_bytes));
        }
        edge = (edge * 3 / 4).max(MIN_EDGE);
    }
}

fn scale_to_edge(img: &RgbaImage, edge: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest <= edge {
        return img.clone();
    }
    let scale = f64::from(edge) / f64::from(longest);
    let nw = ((f64::from(w) * scale).round() as u32).max(1);
    let nh = ((f64::from(h) * scale).round() as u32).max(1);
    // Box-ish (Triangle) filtering: good quality for large reductions and fast.
    image::imageops::resize(img, nw, nh, FilterType::Triangle)
}

fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    let mut out = Vec::new();
    png::PngEncoder::new_with_quality(
        &mut out,
        png::CompressionType::Best,
        png::FilterType::Adaptive,
    )
    .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]))
    }

    fn noise(w: u32, h: u32) -> RgbaImage {
        let mut state = 0x1234_5678_u32;
        RgbaImage::from_fn(w, h, |_, _| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = state.to_le_bytes();
            image::Rgba([b[0], b[1], b[2], 255])
        })
    }

    fn dims(png: &[u8]) -> (u32, u32) {
        image::load_from_memory(png).unwrap().to_rgba8().dimensions()
    }

    #[test]
    fn downscales_keeping_aspect_ratio() {
        let png = thumbnail_from_image(&gradient(1000, 500), ThumbnailOptions::default()).unwrap();
        assert_eq!(dims(&png), (256, 128));
        let png = thumbnail_from_image(&gradient(300, 1200), ThumbnailOptions::default()).unwrap();
        assert_eq!(dims(&png), (64, 256));
    }

    #[test]
    fn never_upscales() {
        let png = thumbnail_from_image(&gradient(40, 30), ThumbnailOptions::default()).unwrap();
        assert_eq!(dims(&png), (40, 30));
    }

    #[test]
    fn extreme_aspect_ratio_keeps_one_pixel() {
        let png = thumbnail_from_image(&gradient(5000, 1), ThumbnailOptions::default()).unwrap();
        let (w, h) = dims(&png);
        assert_eq!((w, h), (256, 1));
    }

    #[test]
    fn size_cap_shrinks_until_it_fits() {
        let big_noise = noise(512, 512);
        let opts = ThumbnailOptions { max_edge: 256, max_bytes: 40_000 };
        let png = thumbnail_from_image(&big_noise, opts).unwrap();
        assert!(png.len() <= 40_000, "{}", png.len());
        let (w, _) = dims(&png);
        assert!(w < 256, "had to shrink, got {w}");
    }

    #[test]
    fn impossible_cap_is_an_error() {
        let opts = ThumbnailOptions { max_edge: 64, max_bytes: 10 };
        assert!(matches!(
            thumbnail_from_image(&noise(64, 64), opts),
            Err(ThumbnailError::TooLarge(10))
        ));
    }

    #[test]
    fn empty_image_rejected() {
        let img = RgbaImage::new(0, 10);
        assert!(matches!(
            thumbnail_from_image(&img, ThumbnailOptions::default()),
            Err(ThumbnailError::Empty)
        ));
    }

    #[test]
    fn from_frame_and_bytes() {
        let img = gradient(600, 400);
        let frame = Frame::from_image(img.clone());
        let a = thumbnail_from_frame(&frame, ThumbnailOptions::default()).unwrap();
        assert_eq!(dims(&a), (256, 171));
        let encoded = frame.encode(ssx_types::EncodeOptions::default()).unwrap();
        let b = thumbnail_from_bytes(&encoded, ThumbnailOptions::default()).unwrap();
        assert_eq!(dims(&b), (256, 171));
        assert!(thumbnail_from_bytes(b"not an image", ThumbnailOptions::default()).is_err());
    }

    #[test]
    fn hdr_frames_are_rejected_with_guidance() {
        let f = Frame::new(
            ssx_types::Size::new(4, 4),
            ssx_types::PixelFormat::Rgba16F,
            ssx_types::ColorSpace::ScRgbLinear,
        );
        let e = thumbnail_from_frame(&f, ThumbnailOptions::default()).unwrap_err();
        assert!(matches!(e, ThumbnailError::NotSdr(_)));
        assert!(e.to_string().contains("tonemap"));
    }
}
