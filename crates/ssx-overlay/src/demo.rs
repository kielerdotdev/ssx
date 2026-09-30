//! A synthetic desktop for manual testing (`ssx-overlay --demo`), examples and benchmarks.

use ssx_types::{Frame, Point};

/// A busy, deterministic desktop of `w` x `h` pixels: colour ramps, a checker and a 100 px
/// grid with coordinates encoded in the colours, so scaling, dimming, the loupe and pointer
/// mapping problems are all visible by eye.
pub fn demo_frame(w: u32, h: u32, origin: Point) -> Frame {
    let mut data = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let grid = x % 100 == 0 || y % 100 == 0;
            let check = ((x / 8) + (y / 8)) % 2 == 0;
            if grid {
                data.extend_from_slice(&[235, 235, 235, 255]);
            } else {
                let r = (u64::from(x) * 255 / u64::from(w.max(1))) as u8;
                let g = (u64::from(y) * 255 / u64::from(h.max(1))) as u8;
                data.extend_from_slice(&[r, g, if check { 210 } else { 70 }, 255]);
            }
        }
    }
    let mut f = Frame::from_rgba8(w, h, data).expect("buffer sized by construction");
    f.origin = origin;
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_frame_is_deterministic_and_opaque() {
        let a = demo_frame(64, 48, Point::new(-8, 3));
        let b = demo_frame(64, 48, Point::new(-8, 3));
        assert_eq!(a.data(), b.data());
        assert_eq!(a.origin, Point::new(-8, 3));
        assert!(a.data().chunks_exact(4).all(|p| p[3] == 255));
    }
}
