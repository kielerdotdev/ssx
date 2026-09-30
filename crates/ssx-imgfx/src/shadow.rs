//! Drop shadow, solid border and alpha outline.
//!
//! All three grow the canvas so nothing is clipped and report where the original moved
//! ([`Placed::origin`]); the editor uses that to shift annotations.

use rayon::prelude::*;
use ssx_types::{Frame, Point, Rect};

use crate::{
    BlendMode, BlurMethod, Placed, Result, Rgba, check, composite_over, finite,
    gaussian_blur_premultiplied, i32c, pad, solid_frame,
};

/// Drop shadow settings.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShadowParams {
    /// Horizontal offset of the shadow in pixels (positive = right).
    pub offset_x: i32,
    /// Vertical offset of the shadow in pixels (positive = down).
    pub offset_y: i32,
    /// Blur standard deviation in pixels; 0 gives a hard shadow.
    pub sigma: f32,
    /// Shadow colour; its alpha scales the shadow opacity.
    pub color: Rgba,
}

impl Default for ShadowParams {
    fn default() -> Self {
        Self { offset_x: 4, offset_y: 4, sigma: 4.0, color: [0, 0, 0, 160] }
    }
}

/// Renders `frame` on a larger canvas with a soft shadow of its alpha shape behind it.
pub fn drop_shadow(frame: &Frame, p: &ShadowParams) -> Result<Placed> {
    check(frame)?;
    let sigma = finite(p.sigma, "sigma")?.max(0.0);
    let (w, h) = (frame.width(), frame.height());
    if w == 0 || h == 0 {
        return Ok(Placed { frame: solid_frame(w, h, [0; 4]), origin: Point::new(0, 0) });
    }
    let margin = if sigma > 0.0 { (sigma * 3.0).ceil() as i32 } else { 0 };
    let image = Rect::new(0, 0, w, h);
    let shadow = Rect::new(p.offset_x, p.offset_y, w, h).inflate(margin);
    let out = image.union(shadow);
    let (ow, oh) = (out.width as usize, out.height as usize);
    let origin = Point::new(-out.x, -out.y);

    // Premultiplied shadow layer: the source alpha tinted with the shadow colour.
    let mut layer = vec![0u8; ow * oh * 4];
    let sx0 = (p.offset_x - out.x) as usize;
    let sy0 = (p.offset_y - out.y) as usize;
    layer.par_chunks_mut(ow * 4).enumerate().skip(sy0).take(h as usize).for_each(|(y, row)| {
        let src = frame.row((y - sy0) as u32);
        for (x, s) in src.chunks_exact(4).enumerate() {
            let a = u32::from(s[3]) * u32::from(p.color[3]);
            let a = ((a + 127) / 255) as u8;
            let o = &mut row[(sx0 + x) * 4..(sx0 + x) * 4 + 4];
            for (oc, cc) in o.iter_mut().zip(p.color).take(3) {
                *oc = ((u32::from(cc) * u32::from(a) + 127) / 255) as u8;
            }
            o[3] = a;
        }
    });
    if sigma > 0.0 {
        gaussian_blur_premultiplied(&mut layer, ow, oh, sigma, BlurMethod::Auto);
    }
    // Premultiplied -> straight.
    for px in layer.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in px.iter_mut().take(3) {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut canvas = Frame::from_rgba8(out.width, out.height, layer).expect("exact-size buffer");
    composite_over(&mut canvas, frame, origin, 1.0, BlendMode::Normal)?;
    Ok(Placed { frame: canvas, origin })
}

/// Adds a solid border of `width` pixels around the image.
pub fn add_border(frame: &Frame, width: u32, color: Rgba) -> Result<Placed> {
    pad(frame, width, width, width, width, color)
}

/// Draws an outline `width` pixels thick around the *alpha shape* of the image (useful for
/// stickers/cut-outs), growing the canvas by `width` on every side.
pub fn outline(frame: &Frame, width: u32, color: Rgba) -> Result<Placed> {
    check(frame)?;
    let padded = pad(frame, width, width, width, width, [0; 4])?;
    if width == 0 || frame.width() == 0 || frame.height() == 0 {
        return Ok(padded);
    }
    let src = &padded.frame;
    let (w, h) = (i32c(src.width()), i32c(src.height()));
    let wf = width as f32;
    let reach = i32c(width) + 1;
    let mut offsets: Vec<(i32, i32, f32)> = Vec::new();
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            let cov = (wf + 1.0 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                offsets.push((dx, dy, cov));
            }
        }
    }
    let mut layer = vec![0u8; w as usize * h as usize * 4];
    layer.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
        for (x, o) in row.chunks_exact_mut(4).enumerate() {
            let mut best = 0f32;
            for &(dx, dy, cov) in &offsets {
                let (px, py) = (i32c(x) + dx, i32c(y) + dy);
                if px < 0 || py < 0 || px >= w || py >= h {
                    continue;
                }
                let a = f32::from(src.row(py as u32)[px as usize * 4 + 3]) / 255.0;
                best = best.max(a * cov);
            }
            let a = (best * f32::from(color[3])).round() as u8;
            o.copy_from_slice(&[color[0], color[1], color[2], a]);
        }
    });
    let mut canvas = Frame::from_rgba8(w as u32, h as u32, layer).expect("exact-size buffer");
    composite_over(&mut canvas, src, Point::new(0, 0), 1.0, BlendMode::Normal)?;
    Ok(Placed { frame: canvas, origin: padded.origin })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::px;

    #[test]
    fn shadow_grows_canvas_and_keeps_original() {
        let f = solid_frame(10, 10, [255, 0, 0, 255]);
        let p = drop_shadow(
            &f,
            &ShadowParams { offset_x: 5, offset_y: 3, sigma: 2.0, color: [0, 0, 0, 255] },
        )
        .unwrap();
        // margin = 6, image (0,0,10,10), shadow (5-6, 3-6, 22,22) => union x -1..21, y -3..19
        assert_eq!((p.frame.width(), p.frame.height()), (22, 22));
        assert_eq!(p.origin, Point::new(1, 3));
        assert_eq!(px(&p.frame, 1, 3), [255, 0, 0, 255], "original pixel exact");
        assert_eq!(px(&p.frame, 10, 12), [255, 0, 0, 255]);
        // A pixel well inside the shadow but outside the image is dark and translucent-ish.
        let s = px(&p.frame, 12, 12);
        assert!(s[3] > 200 && s[0] == 0, "{s:?}");
        // Far corner is (almost) transparent.
        assert!(px(&p.frame, 0, 0)[3] < 3);
    }

    #[test]
    fn hard_shadow_no_blur() {
        let f = solid_frame(4, 4, [255, 255, 255, 255]);
        let p = drop_shadow(
            &f,
            &ShadowParams { offset_x: 2, offset_y: 2, sigma: 0.0, color: [0, 0, 0, 255] },
        )
        .unwrap();
        assert_eq!((p.frame.width(), p.frame.height()), (6, 6));
        assert_eq!(p.origin, Point::new(0, 0));
        assert_eq!(px(&p.frame, 5, 5), [0, 0, 0, 255]);
        assert_eq!(px(&p.frame, 5, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn shadow_negative_offset_and_degenerate() {
        let f = solid_frame(3, 3, [1, 2, 3, 255]);
        let p = drop_shadow(
            &f,
            &ShadowParams { offset_x: -4, offset_y: -4, sigma: 1.0, color: [0, 0, 0, 200] },
        )
        .unwrap();
        assert!(p.origin.x >= 4 && p.origin.y >= 4);
        assert_eq!(px(&p.frame, p.origin.x as u32, p.origin.y as u32), [1, 2, 3, 255]);
        let e = drop_shadow(&solid_frame(0, 0, [0; 4]), &ShadowParams::default()).unwrap();
        assert_eq!(e.frame.width(), 0);
        let one =
            drop_shadow(&solid_frame(1, 1, [9, 9, 9, 255]), &ShadowParams::default()).unwrap();
        assert!(one.frame.width() > 1);
        assert!(
            drop_shadow(&f, &ShadowParams { sigma: f32::NAN, ..ShadowParams::default() }).is_err()
        );
    }

    #[test]
    fn border_exact() {
        let f = solid_frame(2, 2, [255, 0, 0, 255]);
        let p = add_border(&f, 3, [0, 0, 255, 255]).unwrap();
        assert_eq!((p.frame.width(), p.frame.height()), (8, 8));
        assert_eq!(px(&p.frame, 0, 0), [0, 0, 255, 255]);
        assert_eq!(px(&p.frame, 3, 3), [255, 0, 0, 255]);
        assert_eq!(px(&p.frame, 4, 4), [255, 0, 0, 255]);
        assert_eq!(px(&p.frame, 5, 5), [0, 0, 255, 255]);
        assert_eq!(p.origin, Point::new(3, 3));
    }

    #[test]
    fn outline_follows_alpha_shape() {
        // Single opaque pixel in a transparent 3x3 image, outline 2.
        let mut f = solid_frame(3, 3, [0; 4]);
        f.data_mut()[(4) * 4..(4) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        let p = outline(&f, 2, [255, 0, 0, 255]).unwrap();
        assert_eq!((p.frame.width(), p.frame.height()), (7, 7));
        // Centre is the original white pixel.
        assert_eq!(px(&p.frame, 3, 3), [255, 255, 255, 255]);
        // Distance 2 straight up is inside the outline, distance 3 is not.
        assert_eq!(px(&p.frame, 3, 1), [255, 0, 0, 255]);
        assert_eq!(px(&p.frame, 3, 0)[3], 0);
        // Diagonal at sqrt(2) is fully inside, sqrt(8) is a faint anti-aliased fringe and
        // sqrt(18) is outside.
        assert_eq!(px(&p.frame, 4, 4), [255, 0, 0, 255]);
        let fringe = px(&p.frame, 5, 5)[3];
        assert!(fringe > 0 && fringe < 128, "{fringe}");
        assert_eq!(px(&p.frame, 0, 0)[3], 0);
    }

    #[test]
    fn outline_degenerate() {
        let f = solid_frame(2, 2, [1, 1, 1, 255]);
        assert_eq!(outline(&f, 0, [0; 4]).unwrap().frame, f);
        let e = outline(&solid_frame(0, 0, [0; 4]), 3, [1; 4]).unwrap();
        assert_eq!(e.frame.width(), 6);
    }
}
