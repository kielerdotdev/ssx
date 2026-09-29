//! Output transforms.
//!
//! Screen-copy protocols hand out the output's framebuffer in *scan-out orientation*: on
//! a monitor mounted sideways (`transform 90`) the buffer is 1080x1920 even though the
//! user sees a 1920x1080 desktop. Screenshots must show what the user sees, so we undo
//! the transform here.
//!
//! Convention (matches `wl_output.transform` and wlroots): the compositor produces
//! `buffer = rotate_ccw(k * 90°, flip_x?(upright))` — flip first, then rotate
//! counter-clockwise. [`Transform::undo`] is the exact inverse. The direction was
//! verified against `grim` on real sway outputs (see `tests/sway_live.rs`).

/// Mirrors `wl_output.transform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Transform {
    #[default]
    Normal,
    Rot90,
    Rot180,
    Rot270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    /// From the numeric `wl_output.transform` value; unknown values count as `Normal`.
    pub const fn from_wl(v: u32) -> Self {
        match v {
            1 => Self::Rot90,
            2 => Self::Rot180,
            3 => Self::Rot270,
            4 => Self::Flipped,
            5 => Self::Flipped90,
            6 => Self::Flipped180,
            7 => Self::Flipped270,
            _ => Self::Normal,
        }
    }

    /// Number of counter-clockwise quarter turns.
    const fn quarter_turns(self) -> u32 {
        match self {
            Self::Normal | Self::Flipped => 0,
            Self::Rot90 | Self::Flipped90 => 1,
            Self::Rot180 | Self::Flipped180 => 2,
            Self::Rot270 | Self::Flipped270 => 3,
        }
    }

    const fn flipped(self) -> bool {
        matches!(self, Self::Flipped | Self::Flipped90 | Self::Flipped180 | Self::Flipped270)
    }

    /// `true` if width and height are exchanged.
    pub const fn swaps_axes(self) -> bool {
        self.quarter_turns() % 2 == 1
    }

    /// The size an upright image has when the buffer is `w` x `h`.
    pub const fn upright_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }

    /// The buffer size for an upright `w` x `h` image (used by the test compositor).
    pub const fn buffer_size(self, w: u32, h: u32) -> (u32, u32) {
        self.upright_size(w, h)
    }

    /// Source (buffer) coordinates of upright pixel `(x, y)` in an upright `w` x `h` image.
    const fn buffer_xy(self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
        let xf = if self.flipped() { w - 1 - x } else { x };
        match self.quarter_turns() {
            0 => (xf, y),
            1 => (y, w - 1 - xf),
            2 => (w - 1 - xf, h - 1 - y),
            _ => (h - 1 - y, xf),
        }
    }

    /// Forward transform (upright → buffer) on tightly packed 4-byte pixels.
    pub fn apply(self, upright: &[u8], w: u32, h: u32) -> Vec<u8> {
        if upright.len() != w as usize * h as usize * 4 {
            // Wrong-sized input is a caller bug; return it untouched rather than panic.
            return upright.to_vec();
        }
        let (bw, _bh) = self.buffer_size(w, h);
        let mut out = vec![0u8; upright.len()];
        for y in 0..h {
            for x in 0..w {
                let (bx, by) = self.buffer_xy(x, y, w, h);
                let d = (by as usize * bw as usize + bx as usize) * 4;
                let s = (y as usize * w as usize + x as usize) * 4;
                out[d..d + 4].copy_from_slice(&upright[s..s + 4]);
            }
        }
        out
    }

    /// Undoes the transform on a tightly packed 4-byte-per-pixel buffer of `bw` x `bh`,
    /// returning the upright pixels and their size.
    pub fn undo(self, buffer: Vec<u8>, bw: u32, bh: u32) -> (Vec<u8>, u32, u32) {
        if self == Self::Normal || buffer.len() != bw as usize * bh as usize * 4 {
            // Nothing to undo (or a wrong-sized buffer, which we refuse to index into).
            return (buffer, bw, bh);
        }
        let (w, h) = self.upright_size(bw, bh);
        let mut out = vec![0u8; buffer.len()];
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = self.buffer_xy(x, y, w, h);
                let s = (sy as usize * bw as usize + sx as usize) * 4;
                let d = (y as usize * w as usize + x as usize) * 4;
                out[d..d + 4].copy_from_slice(&buffer[s..s + 4]);
            }
        }
        (out, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::Rot90,
        Transform::Rot180,
        Transform::Rot270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    /// A 3x2 image with unique pixels: value = 10*y + x.
    fn upright() -> Vec<u8> {
        (0..2u8).flat_map(|y| (0..3u8).flat_map(move |x| [10 * y + x, 0, 0, 255])).collect()
    }

    fn firsts(data: &[u8]) -> Vec<u8> {
        data.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn undo_is_the_inverse_of_apply_for_every_transform() {
        for t in ALL {
            let img = upright();
            let buf = t.apply(&img, 3, 2);
            let (bw, bh) = t.buffer_size(3, 2);
            let (back, w, h) = t.undo(buf, bw, bh);
            assert_eq!((w, h), (3, 2), "{t:?}");
            assert_eq!(back, img, "{t:?}");
        }
    }

    #[test]
    fn rot90_is_counter_clockwise() {
        // Upright:      Buffer after 90° CCW (top-right corner goes to top-left):
        //  0 1 2         2 12
        // 10 11 12       1 11
        //                0 10
        let buf = Transform::Rot90.apply(&upright(), 3, 2);
        assert_eq!(firsts(&buf), vec![2, 12, 1, 11, 0, 10]);
    }

    #[test]
    fn rot180_reverses_pixels() {
        let buf = Transform::Rot180.apply(&upright(), 3, 2);
        assert_eq!(firsts(&buf), vec![12, 11, 10, 2, 1, 0]);
    }

    #[test]
    fn rot270_is_clockwise_quarter_turn() {
        let buf = Transform::Rot270.apply(&upright(), 3, 2);
        assert_eq!(firsts(&buf), vec![10, 0, 11, 1, 12, 2]);
    }

    #[test]
    fn flipped_mirrors_horizontally() {
        let buf = Transform::Flipped.apply(&upright(), 3, 2);
        assert_eq!(firsts(&buf), vec![2, 1, 0, 12, 11, 10]);
    }

    #[test]
    fn axes_swap_only_for_odd_quarter_turns() {
        for t in ALL {
            let want = matches!(
                t,
                Transform::Rot90 | Transform::Rot270 | Transform::Flipped90 | Transform::Flipped270
            );
            assert_eq!(t.swaps_axes(), want, "{t:?}");
            assert_eq!(t.upright_size(4, 7), if want { (7, 4) } else { (4, 7) });
        }
    }

    #[test]
    fn from_wl_covers_the_enum_and_tolerates_garbage() {
        for (i, t) in ALL.iter().enumerate() {
            assert_eq!(Transform::from_wl(i as u32), *t);
        }
        assert_eq!(Transform::from_wl(99), Transform::Normal);
    }

    #[test]
    fn wrong_sized_buffers_are_returned_untouched() {
        assert_eq!(Transform::Rot90.undo(vec![1, 2, 3], 2, 2), (vec![1, 2, 3], 2, 2));
        assert_eq!(Transform::Rot90.apply(&[1, 2, 3], 2, 2), vec![1, 2, 3]);
    }

    #[test]
    fn one_pixel_and_empty_images_do_not_panic() {
        for t in ALL {
            let (out, w, h) = t.undo(vec![1, 2, 3, 4], 1, 1);
            assert_eq!((out, w, h), (vec![1, 2, 3, 4], 1, 1));
            let (out, ..) = t.undo(Vec::new(), 0, 0);
            assert!(out.is_empty());
        }
    }
}
