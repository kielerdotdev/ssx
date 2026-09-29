//! Stitching per-monitor frames into one virtual-desktop frame.

use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Rect};

#[derive(Debug, thiserror::Error)]
pub enum CompositeError {
    #[error("no frames to composite")]
    Empty,
    #[error(
        "frames have mixed pixel formats/colour spaces ({0:?}/{1:?} vs {2:?}/{3:?}); \
         convert them to a common format first"
    )]
    MixedFormats(PixelFormat, ColorSpace, PixelFormat, ColorSpace),
}

/// Copies `src` into `dst`, positioned by their virtual-desktop origins. Parts of `src`
/// outside `dst` are clipped. Both frames must share format and colour space.
pub fn blit(dst: &mut Frame, src: &Frame) -> Result<(), CompositeError> {
    if (dst.format(), dst.color_space()) != (src.format(), src.color_space()) {
        return Err(CompositeError::MixedFormats(
            dst.format(),
            dst.color_space(),
            src.format(),
            src.color_space(),
        ));
    }
    let Some(overlap) = dst.rect().intersect(src.rect()) else { return Ok(()) };
    let bpp = dst.format().bytes_per_pixel();
    let row_len = overlap.width as usize * bpp;
    for y in 0..overlap.height {
        let sy = (i64::from(overlap.y) - i64::from(src.origin.y)) as u32 + y;
        let dy = (i64::from(overlap.y) - i64::from(dst.origin.y)) as u32 + y;
        let sx = (i64::from(overlap.x) - i64::from(src.origin.x)) as usize * bpp;
        let dx = (i64::from(overlap.x) - i64::from(dst.origin.x)) as usize * bpp;
        let s = &src.row(sy)[sx..sx + row_len];
        dst.row_mut(dy)[dx..dx + row_len].copy_from_slice(s);
    }
    Ok(())
}

/// Stitches frames onto a canvas covering their bounding box. Gaps between monitors are
/// left transparent black. All frames must share pixel format and colour space.
pub fn composite(frames: &[Frame]) -> Result<Frame, CompositeError> {
    let first = frames.first().ok_or(CompositeError::Empty)?;
    let bounds = Rect::bounding(frames.iter().map(Frame::rect)).ok_or(CompositeError::Empty)?;
    let mut canvas = Frame::new(bounds.size(), first.format(), first.color_space());
    canvas.origin = Point::new(bounds.x, bounds.y);
    canvas.scale_factor = first.scale_factor;
    canvas.sdr_white_nits = first.sdr_white_nits;
    for f in frames {
        blit(&mut canvas, f)?;
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssx_types::Size;

    fn solid(w: u32, h: u32, origin: Point, px: [u8; 4]) -> Frame {
        let data = px.iter().copied().cycle().take((w * h * 4) as usize).collect();
        let mut f = Frame::from_rgba8(w, h, data).unwrap();
        f.origin = origin;
        f
    }

    #[test]
    fn stitches_side_by_side_with_negative_origin() {
        let left = solid(2, 2, Point::new(-2, 0), [1, 0, 0, 255]);
        let right = solid(3, 2, Point::new(0, 0), [0, 2, 0, 255]);
        let c = composite(&[left, right]).unwrap();
        assert_eq!(c.origin, Point::new(-2, 0));
        assert_eq!(c.size(), Size::new(5, 2));
        assert_eq!(&c.row(0)[0..4], &[1, 0, 0, 255]);
        assert_eq!(&c.row(1)[16..20], &[0, 2, 0, 255]);
    }

    #[test]
    fn gaps_stay_transparent() {
        let a = solid(1, 1, Point::new(0, 0), [9, 9, 9, 255]);
        let b = solid(1, 1, Point::new(2, 0), [7, 7, 7, 255]);
        let c = composite(&[a, b]).unwrap();
        assert_eq!(c.size(), Size::new(3, 1));
        assert_eq!(&c.row(0)[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn mixed_formats_are_rejected() {
        let a = solid(1, 1, Point::default(), [1, 1, 1, 255]);
        let b = Frame::new(Size::new(1, 1), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear);
        assert!(matches!(composite(&[a, b]), Err(CompositeError::MixedFormats(..))));
        assert!(matches!(composite(&[]), Err(CompositeError::Empty)));
    }
}
