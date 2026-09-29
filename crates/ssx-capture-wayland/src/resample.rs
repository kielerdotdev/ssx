//! Resampling of 4-byte-per-pixel images, used when a monitor's native scale is lower
//! than the virtual desktop scale (mixed-DPI layouts, see `docs/wayland-coordinates.md`).
//!
//! Integer up-scaling replicates pixels (keeps text and UI edges crisp, which is what a
//! 1x monitor showing through a 2x desktop looks like). Anything else uses bilinear
//! interpolation with pixel-centre sampling. Only pixel *values* are interpolated; all
//! four channels are treated alike, which is fine because captured frames are opaque.

/// Resamples tightly packed 4-byte pixels from `from` to `to` (`(width, height)`).
///
/// Returns the input unchanged if the sizes match. Empty sizes yield an empty image.
pub fn resample(src: Vec<u8>, from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    if from == to {
        return src;
    }
    let (fw, fh) = (from.0 as usize, from.1 as usize);
    let (tw, th) = (to.0 as usize, to.1 as usize);
    if fw == 0 || fh == 0 || tw == 0 || th == 0 || src.len() < fw * fh * 4 {
        return vec![0; tw * th * 4];
    }
    let mut out = vec![0u8; tw * th * 4];
    let integer_up = tw % fw == 0 && th % fh == 0;
    if integer_up {
        let (sx, sy) = (tw / fw, th / fh);
        for y in 0..th {
            let srow = &src[(y / sy) * fw * 4..(y / sy + 1) * fw * 4];
            let drow = &mut out[y * tw * 4..(y + 1) * tw * 4];
            for (x, d) in drow.chunks_exact_mut(4).enumerate() {
                let s = (x / sx) * 4;
                d.copy_from_slice(&srow[s..s + 4]);
            }
        }
        return out;
    }
    // Bilinear, sampling at pixel centres: src = (dst + 0.5) * from/to - 0.5.
    let xr = fw as f32 / tw as f32;
    let yr = fh as f32 / th as f32;
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * yr - 0.5).clamp(0.0, (fh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(fh - 1);
        let wy = fy - y0 as f32;
        for x in 0..tw {
            let fx = ((x as f32 + 0.5) * xr - 0.5).clamp(0.0, (fw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(fw - 1);
            let wx = fx - x0 as f32;
            let d = (y * tw + x) * 4;
            for c in 0..4 {
                let p = |xx: usize, yy: usize| f32::from(src[(yy * fw + xx) * 4 + c]);
                let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                let bot = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                out[d + c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn same_size_is_identity() {
        let img: Vec<u8> = (0..16).collect();
        assert_eq!(resample(img.clone(), (2, 2), (2, 2)), img);
    }

    #[test]
    fn integer_upscale_replicates_pixels() {
        let img: Vec<u8> = [px(1), px(2), px(3), px(4)].concat();
        let out = resample(img, (2, 2), (4, 4));
        let firsts: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(firsts, vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn bilinear_preserves_flat_colour_and_range() {
        let img: Vec<u8> = std::iter::repeat_n(px(200), 9).flatten().collect();
        let out = resample(img, (3, 3), (5, 7));
        assert_eq!(out.len(), 5 * 7 * 4);
        assert!(out.chunks_exact(4).all(|p| p == px(200)));
    }

    #[test]
    fn bilinear_is_monotonic_on_a_ramp() {
        let img: Vec<u8> = [px(0), px(100), px(200)].concat();
        let out = resample(img, (3, 1), (5, 1));
        let v: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(v.first(), Some(&0));
        assert_eq!(v.last(), Some(&200));
        assert!(v.windows(2).all(|w| w[0] <= w[1]), "{v:?}");
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        assert_eq!(resample(Vec::new(), (0, 0), (2, 2)), vec![0; 16]);
        assert!(resample(vec![0; 4], (1, 1), (0, 5)).is_empty());
        // Truncated source buffer.
        assert_eq!(resample(vec![1, 2, 3, 4], (2, 2), (4, 4)).len(), 64);
    }
}
