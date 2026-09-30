//! Region clipping and raw-buffer plumbing shared by all effects.
//!
//! Effects that operate on a sub-rectangle copy it into a tight buffer, filter it, and
//! write it back. The copies are memory-bound and cheap next to the filters themselves,
//! and they make the "region op == crop, filter, paste" contract obvious.

use rayon::prelude::*;
use ssx_types::{Frame, Rect};

/// Clips an optional frame-local region to the frame bounds. `None` means the whole
/// frame. Returns `None` when the clipped region is empty (callers treat that as a
/// no-op). The result is always inside `0..width` × `0..height`.
pub fn clip_region(width: u32, height: u32, region: Option<Rect>) -> Option<Rect> {
    let bounds = Rect::new(0, 0, width, height);
    if bounds.is_empty() {
        return None;
    }
    match region {
        None => Some(bounds),
        Some(r) => r.intersect(bounds),
    }
}

/// Copies `r` (already clipped) out of `frame` into a tight RGBA buffer.
pub(crate) fn extract(frame: &Frame, r: Rect) -> Vec<u8> {
    let rw = r.width as usize * 4;
    let mut out = vec![0u8; rw * r.height as usize];
    if rw == 0 {
        return out;
    }
    out.par_chunks_mut(rw).enumerate().for_each(|(y, dst)| {
        let src = frame.row(r.y as u32 + y as u32);
        let x0 = r.x as usize * 4;
        dst.copy_from_slice(&src[x0..x0 + rw]);
    });
    out
}

/// Writes a tight RGBA buffer back at `r` (already clipped).
pub(crate) fn write_back(frame: &mut Frame, r: Rect, data: &[u8]) {
    let rw = r.width as usize * 4;
    if rw == 0 {
        return;
    }
    let stride = frame.stride();
    let (x0, y0) = (r.x as usize, r.y as usize);
    frame.data_mut().par_chunks_mut(stride).enumerate().skip(y0).take(r.height as usize).for_each(
        |(y, row)| {
            let src = &data[(y - y0) * rw..(y - y0 + 1) * rw];
            row[x0 * 4..x0 * 4 + rw].copy_from_slice(src);
        },
    );
}

/// Runs `f` on the pixel bytes of every row of `r` (already clipped) in parallel. The
/// slice handed to `f` covers exactly the region's pixels in that row; the row index
/// is frame-local.
pub(crate) fn for_rows_mut<F>(frame: &mut Frame, r: Rect, f: F)
where
    F: Fn(u32, &mut [u8]) + Sync,
{
    let stride = frame.stride();
    let (x0, y0) = (r.x as usize, r.y as usize);
    let rw = r.width as usize * 4;
    if rw == 0 {
        return;
    }
    frame
        .data_mut()
        .par_chunks_mut(stride)
        .enumerate()
        .skip(y0)
        .take(r.height as usize)
        .for_each(|(y, row)| f(y as u32, &mut row[x0 * 4..x0 * 4 + rw]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::noise;

    #[test]
    fn clip_cases() {
        assert_eq!(clip_region(10, 10, None), Some(Rect::new(0, 0, 10, 10)));
        assert_eq!(clip_region(0, 10, None), None);
        assert_eq!(
            clip_region(10, 10, Some(Rect::new(-5, -5, 10, 10))),
            Some(Rect::new(0, 0, 5, 5))
        );
        assert_eq!(clip_region(10, 10, Some(Rect::new(10, 0, 5, 5))), None);
        assert_eq!(clip_region(10, 10, Some(Rect::new(2, 2, 0, 5))), None);
        assert_eq!(
            clip_region(10, 10, Some(Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX))),
            Some(Rect::new(0, 0, 10, 10))
        );
    }

    #[test]
    fn extract_and_write_back_round_trip() {
        let mut f = noise(9, 7, true, 1);
        let orig = f.clone();
        let r = Rect::new(2, 1, 5, 4);
        let buf = extract(&f, r);
        assert_eq!(buf.len(), 5 * 4 * 4);
        write_back(&mut f, r, &buf);
        assert_eq!(f, orig);
    }
}
