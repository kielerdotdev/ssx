//! CPU pixel buffers.

use std::{io::Cursor, path::Path, time::Duration};

use image::{ImageEncoder, codecs};
use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect, Size};

/// In-memory channel layout of a [`Frame`]. All layouts are 4 channels per pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit red, green, blue, alpha.
    Rgba8,
    /// 8-bit blue, green, red, alpha (native order of most OS capture APIs).
    Bgra8,
    /// IEEE-754 half floats (little endian), red, green, blue, alpha. Used for HDR.
    Rgba16F,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
            PixelFormat::Rgba16F => 8,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, PixelFormat::Rgba16F)
    }
}

/// Meaning of the numeric channel values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorSpace {
    /// Gamma-encoded sRGB, display referred. What every image file expects.
    Srgb,
    /// Linear light, Rec.709 primaries, **1.0 = 80 nits**; values above 1.0 are HDR
    /// highlights and channels may be negative (wide gamut). This is Windows' scRGB.
    ScRgbLinear,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("buffer too small: need {need} bytes for {size:?} stride {stride}, have {have}")]
    BufferTooSmall { need: usize, have: usize, size: Size, stride: usize },
    #[error("stride {stride} is smaller than one row ({row} bytes)")]
    StrideTooSmall { stride: usize, row: usize },
    #[error("crop rectangle {0:?} is outside the frame")]
    CropOutOfBounds(Rect),
    #[error("operation requires an 8-bit sRGB frame but this is {0:?}/{1:?}; tonemap it first")]
    NotSdr8(PixelFormat, ColorSpace),
    #[error("image encode/decode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported image format for path {0:?}")]
    UnknownExtension(String),
}

/// Output image container for [`Frame::encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
    /// Lossless WebP.
    WebP,
    Bmp,
}

impl ImageFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::WebP => "webp",
            ImageFormat::Bmp => "bmp",
        }
    }

    pub const fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Bmp => "image/bmp",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "webp" => Some(ImageFormat::WebP),
            "bmp" => Some(ImageFormat::Bmp),
            _ => None,
        }
    }
}

/// Encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeOptions {
    pub format: ImageFormat,
    /// JPEG quality 1–100.
    pub jpeg_quality: u8,
    /// PNG: `true` favours speed (fast compression), `false` favours size.
    pub png_fast: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self { format: ImageFormat::Png, jpeg_quality: 90, png_fast: false }
    }
}

impl EncodeOptions {
    pub fn new(format: ImageFormat) -> Self {
        Self { format, ..Self::default() }
    }
}

/// A CPU pixel buffer with explicit stride, format and colour space.
#[derive(Clone, PartialEq)]
pub struct Frame {
    size: Size,
    stride: usize,
    format: PixelFormat,
    color_space: ColorSpace,
    data: Vec<u8>,
    /// Top-left position on the virtual desktop, in physical pixels.
    pub origin: Point,
    /// UI scale factor of the source display (1.0 when unknown).
    pub scale_factor: f64,
    /// For HDR frames: nits that SDR white maps to (`None` when not applicable).
    pub sdr_white_nits: Option<f32>,
    /// Capture timestamp relative to the start of a stream (`None` for stills).
    pub timestamp: Option<Duration>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("size", &self.size)
            .field("stride", &self.stride)
            .field("format", &self.format)
            .field("color_space", &self.color_space)
            .field("origin", &self.origin)
            .field("scale_factor", &self.scale_factor)
            .field("sdr_white_nits", &self.sdr_white_nits)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Frame {
    /// Wraps an existing buffer. `stride` is in bytes and may exceed one row.
    pub fn from_raw(
        size: Size,
        stride: usize,
        format: PixelFormat,
        color_space: ColorSpace,
        data: Vec<u8>,
    ) -> Result<Self, FrameError> {
        let row = size.width as usize * format.bytes_per_pixel();
        if stride < row {
            return Err(FrameError::StrideTooSmall { stride, row });
        }
        let need = if size.height == 0 { 0 } else { stride * (size.height as usize - 1) + row };
        if data.len() < need {
            return Err(FrameError::BufferTooSmall { need, have: data.len(), size, stride });
        }
        Ok(Self {
            size,
            stride,
            format,
            color_space,
            data,
            origin: Point::default(),
            scale_factor: 1.0,
            sdr_white_nits: None,
            timestamp: None,
        })
    }

    /// A zero-filled (transparent black) tightly packed frame.
    pub fn new(size: Size, format: PixelFormat, color_space: ColorSpace) -> Self {
        let stride = size.width as usize * format.bytes_per_pixel();
        let data = vec![0u8; stride * size.height as usize];
        Self::from_raw(size, stride, format, color_space, data).expect("valid by construction")
    }

    /// Tightly packed 8-bit sRGB RGBA.
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Result<Self, FrameError> {
        Self::from_raw(
            Size::new(width, height),
            width as usize * 4,
            PixelFormat::Rgba8,
            ColorSpace::Srgb,
            data,
        )
    }

    pub fn size(&self) -> Size {
        self.size
    }
    pub fn width(&self) -> u32 {
        self.size.width
    }
    pub fn height(&self) -> u32 {
        self.size.height
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn format(&self) -> PixelFormat {
        self.format
    }
    pub fn color_space(&self) -> ColorSpace {
        self.color_space
    }
    /// The frame's bounds on the virtual desktop.
    pub fn rect(&self) -> Rect {
        Rect::from_origin_size(self.origin, self.size)
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// `true` for 8-bit sRGB (either channel order), i.e. directly encodable.
    pub fn is_sdr8(&self) -> bool {
        !self.format.is_float() && self.color_space == ColorSpace::Srgb
    }

    fn row_bytes(&self) -> usize {
        self.size.width as usize * self.format.bytes_per_pixel()
    }

    /// The pixel bytes of row `y` (exactly one row, excluding stride padding).
    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.stride;
        &self.data[start..start + self.row_bytes()]
    }

    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let start = y as usize * self.stride;
        let n = self.row_bytes();
        &mut self.data[start..start + n]
    }

    /// Crops to `rect` given in **frame-local** pixel coordinates.
    pub fn crop(&self, rect: Rect) -> Result<Frame, FrameError> {
        let bounds = Rect::new(0, 0, self.size.width, self.size.height);
        let inter = rect.intersect(bounds).filter(|i| *i == rect);
        let Some(r) = inter else { return Err(FrameError::CropOutOfBounds(rect)) };
        let bpp = self.format.bytes_per_pixel();
        let row = r.width as usize * bpp;
        let mut out = Vec::with_capacity(row * r.height as usize);
        for y in 0..r.height {
            let start = (r.y as usize + y as usize) * self.stride + r.x as usize * bpp;
            out.extend_from_slice(&self.data[start..start + row]);
        }
        let mut f = Frame::from_raw(r.size(), row, self.format, self.color_space, out)?;
        f.origin = Point::new(self.origin.x + r.x, self.origin.y + r.y);
        f.scale_factor = self.scale_factor;
        f.sdr_white_nits = self.sdr_white_nits;
        f.timestamp = self.timestamp;
        Ok(f)
    }

    /// Crops using **virtual-desktop** coordinates (as reported by [`Frame::rect`]).
    pub fn crop_desktop(&self, rect: Rect) -> Result<Frame, FrameError> {
        self.crop(rect.translate(-self.origin.x, -self.origin.y))
    }

    /// Converts an 8-bit sRGB frame to tightly packed [`PixelFormat::Rgba8`].
    /// Float / non-sRGB frames must be tonemapped first (see the `ssx-hdr` crate).
    pub fn into_rgba8(self) -> Result<Frame, FrameError> {
        if !self.is_sdr8() {
            return Err(FrameError::NotSdr8(self.format, self.color_space));
        }
        let mut out = Vec::with_capacity(self.row_bytes() * self.size.height as usize);
        for y in 0..self.size.height {
            let row = self.row(y);
            match self.format {
                PixelFormat::Rgba8 => out.extend_from_slice(row),
                PixelFormat::Bgra8 => {
                    for px in row.chunks_exact(4) {
                        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                    }
                }
                PixelFormat::Rgba16F => unreachable!("rejected above"),
            }
        }
        let mut f = Frame::from_raw(
            self.size,
            self.size.width as usize * 4,
            PixelFormat::Rgba8,
            ColorSpace::Srgb,
            out,
        )?;
        f.origin = self.origin;
        f.scale_factor = self.scale_factor;
        f.sdr_white_nits = self.sdr_white_nits;
        f.timestamp = self.timestamp;
        Ok(f)
    }

    /// Forces every pixel opaque (capture APIs often leave alpha undefined).
    pub fn set_opaque(&mut self) {
        let (w, h, stride, fmt) =
            (self.size.width as usize, self.size.height, self.stride, self.format);
        match fmt {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => {
                for y in 0..h as usize {
                    let row = &mut self.data[y * stride..y * stride + w * 4];
                    for px in row.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                }
            }
            PixelFormat::Rgba16F => {
                let one = half::f16::from_f32(1.0).to_le_bytes();
                for y in 0..h as usize {
                    let row = &mut self.data[y * stride..y * stride + w * 8];
                    for px in row.chunks_exact_mut(8) {
                        px[6] = one[0];
                        px[7] = one[1];
                    }
                }
            }
        }
    }

    /// Copies into an [`image::RgbaImage`]. Requires an 8-bit sRGB frame.
    pub fn to_image(&self) -> Result<image::RgbaImage, FrameError> {
        let f = self.clone().into_rgba8()?;
        Ok(image::RgbaImage::from_raw(f.width(), f.height(), f.data)
            .expect("into_rgba8 produces exact-size buffer"))
    }

    /// Wraps an [`image::RgbaImage`] as an sRGB [`Frame`].
    pub fn from_image(img: image::RgbaImage) -> Frame {
        let (w, h) = img.dimensions();
        Frame::from_rgba8(w, h, img.into_raw()).expect("exact-size buffer")
    }

    /// Encodes to an in-memory image file. Requires an 8-bit sRGB frame.
    pub fn encode(&self, opts: EncodeOptions) -> Result<Vec<u8>, FrameError> {
        let img = self.to_image()?;
        let (w, h) = img.dimensions();
        let mut out = Vec::new();
        match opts.format {
            ImageFormat::Png => {
                let (compression, filter) = if opts.png_fast {
                    (codecs::png::CompressionType::Fast, codecs::png::FilterType::Sub)
                } else {
                    (codecs::png::CompressionType::Best, codecs::png::FilterType::Adaptive)
                };
                codecs::png::PngEncoder::new_with_quality(&mut out, compression, filter)
                    .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)?;
            }
            ImageFormat::Jpeg => {
                // JPEG has no alpha channel.
                let rgb = image::DynamicImage::ImageRgba8(img).into_rgb8();
                codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut out,
                    opts.jpeg_quality.clamp(1, 100),
                )
                .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)?;
            }
            ImageFormat::WebP => {
                codecs::webp::WebPEncoder::new_lossless(&mut out).write_image(
                    img.as_raw(),
                    w,
                    h,
                    image::ExtendedColorType::Rgba8,
                )?;
            }
            ImageFormat::Bmp => {
                img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Bmp)?;
            }
        }
        Ok(out)
    }

    /// Encodes and writes to `path`; the container is chosen from the extension.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), FrameError> {
        let path = path.as_ref();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        let format = ImageFormat::from_extension(ext)
            .ok_or_else(|| FrameError::UnknownExtension(path.display().to_string()))?;
        std::fs::write(path, self.encode(EncodeOptions::new(format))?)?;
        Ok(())
    }

    /// Decodes an image file's bytes into an sRGB [`Frame`].
    pub fn decode(bytes: &[u8]) -> Result<Frame, FrameError> {
        Ok(Frame::from_image(image::load_from_memory(bytes)?.into_rgba8()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Frame {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[x as u8, y as u8, (x + y) as u8, 255]);
            }
        }
        Frame::from_rgba8(w, h, data).unwrap()
    }

    #[test]
    fn rejects_bad_buffers() {
        assert!(matches!(
            Frame::from_raw(Size::new(4, 4), 16, PixelFormat::Rgba8, ColorSpace::Srgb, vec![0; 10]),
            Err(FrameError::BufferTooSmall { .. })
        ));
        assert!(matches!(
            Frame::from_raw(Size::new(4, 4), 8, PixelFormat::Rgba8, ColorSpace::Srgb, vec![0; 100]),
            Err(FrameError::StrideTooSmall { .. })
        ));
    }

    #[test]
    fn last_row_may_omit_stride_padding() {
        // 2x2 px, stride 12 (4 bytes padding); last row needs only 8 bytes -> 12+8 = 20.
        assert!(
            Frame::from_raw(Size::new(2, 2), 12, PixelFormat::Rgba8, ColorSpace::Srgb, vec![0; 20])
                .is_ok()
        );
    }

    #[test]
    fn crop_respects_stride_and_origin() {
        let mut f = gradient(8, 8);
        f.origin = Point::new(-100, 50);
        let c = f.crop(Rect::new(2, 3, 3, 2)).unwrap();
        assert_eq!(c.size(), Size::new(3, 2));
        assert_eq!(c.origin, Point::new(-98, 53));
        assert_eq!(&c.row(0)[0..4], &[2, 3, 5, 255]);
        assert_eq!(&c.row(1)[8..12], &[4, 4, 8, 255]);
        assert!(f.crop(Rect::new(6, 6, 4, 4)).is_err());
        let d = f.crop_desktop(Rect::new(-98, 53, 3, 2)).unwrap();
        assert_eq!(d.data(), c.data());
    }

    #[test]
    fn bgra_swizzle_and_encode_roundtrip() {
        let data = vec![10, 20, 30, 255, 40, 50, 60, 255]; // BGRA
        let f = Frame::from_raw(Size::new(2, 1), 8, PixelFormat::Bgra8, ColorSpace::Srgb, data)
            .unwrap();
        let rgba = f.clone().into_rgba8().unwrap();
        assert_eq!(rgba.data(), &[30, 20, 10, 255, 60, 50, 40, 255]);
        for fmt in [ImageFormat::Png, ImageFormat::WebP, ImageFormat::Bmp] {
            let bytes = f.encode(EncodeOptions::new(fmt)).unwrap();
            let back = Frame::decode(&bytes).unwrap();
            assert_eq!(back.data(), rgba.data(), "{fmt:?} must be lossless");
        }
        let jpg = f.encode(EncodeOptions::new(ImageFormat::Jpeg)).unwrap();
        assert_eq!(Frame::decode(&jpg).unwrap().size(), Size::new(2, 1));
    }

    #[test]
    fn float_frames_refuse_encoding() {
        let f = Frame::new(Size::new(2, 2), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
        assert!(matches!(f.encode(EncodeOptions::default()), Err(FrameError::NotSdr8(..))));
    }

    #[test]
    fn set_opaque_handles_float() {
        let mut f = Frame::new(Size::new(1, 1), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
        f.set_opaque();
        let a = half::f16::from_le_bytes([f.data()[6], f.data()[7]]);
        // 1.0 is exactly representable in f16, so an exact comparison is the point of the test.
        assert!((a.to_f32() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn save_picks_format_from_extension() {
        let dir = std::env::temp_dir().join(format!("ssx-types-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = gradient(4, 4);
        f.save(dir.join("a.png")).unwrap();
        f.save(dir.join("a.JPG")).unwrap();
        assert!(f.save(dir.join("a.xyz")).is_err());
        assert!(std::fs::metadata(dir.join("a.png")).unwrap().len() > 0);
        std::fs::remove_dir_all(dir).ok();
    }
}
