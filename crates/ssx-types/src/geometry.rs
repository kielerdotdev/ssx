//! Integer geometry in physical pixels.

use serde::{Deserialize, Serialize};

/// A point on the virtual desktop. Coordinates may be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A non-negative size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// An axis-aligned rectangle. `x`/`y` is the top-left corner; the rectangle covers
/// `x..x+width` × `y..y+height` (right/bottom exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    pub const fn from_origin_size(origin: Point, size: Size) -> Self {
        Self { x: origin.x, y: origin.y, width: size.width, height: size.height }
    }

    /// Builds the normalised rectangle spanning two arbitrary corner points, as produced
    /// by dragging a selection in any direction.
    pub fn from_points(a: Point, b: Point) -> Self {
        let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
        let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));
        Self {
            x: x0,
            y: y0,
            width: (i64::from(x1) - i64::from(x0)).min(i64::from(u32::MAX)) as u32,
            height: (i64::from(y1) - i64::from(y0)).min(i64::from(u32::MAX)) as u32,
        }
    }

    pub const fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    pub const fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    /// Exclusive right edge. Widened to `i64` so it cannot overflow.
    pub const fn right(self) -> i64 {
        self.x as i64 + self.width as i64
    }

    /// Exclusive bottom edge. Widened to `i64` so it cannot overflow.
    pub const fn bottom(self) -> i64 {
        self.y as i64 + self.height as i64
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn area(self) -> u64 {
        self.size().area()
    }

    pub fn contains(self, p: Point) -> bool {
        i64::from(p.x) >= i64::from(self.x)
            && i64::from(p.x) < self.right()
            && i64::from(p.y) >= i64::from(self.y)
            && i64::from(p.y) < self.bottom()
    }

    /// The overlapping area of two rectangles, or `None` if they do not overlap.
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let x0 = i64::from(self.x).max(i64::from(other.x));
        let y0 = i64::from(self.y).max(i64::from(other.y));
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Rect { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
    }

    /// The smallest rectangle containing both. Empty rectangles are ignored.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x0 = i64::from(self.x).min(i64::from(other.x));
        let y0 = i64::from(self.y).min(i64::from(other.y));
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect {
            x: x0 as i32,
            y: y0 as i32,
            width: (x1 - x0).min(i64::from(u32::MAX)) as u32,
            height: (y1 - y0).min(i64::from(u32::MAX)) as u32,
        }
    }

    pub fn translate(self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x.saturating_add(dx), y: self.y.saturating_add(dy), ..self }
    }

    /// Grows (or shrinks, for negative `by`) the rectangle equally on all sides.
    pub fn inflate(self, by: i32) -> Rect {
        let w = (i64::from(self.width) + 2 * i64::from(by)).clamp(0, i64::from(u32::MAX));
        let h = (i64::from(self.height) + 2 * i64::from(by)).clamp(0, i64::from(u32::MAX));
        Rect {
            x: self.x.saturating_sub(by),
            y: self.y.saturating_sub(by),
            width: w as u32,
            height: h as u32,
        }
    }

    /// Bounding rectangle of many rectangles; `None` if the iterator is empty.
    pub fn bounding(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
        rects.into_iter().reduce(Rect::union)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_normalises_drag_direction() {
        let r = Rect::from_points(Point::new(50, 60), Point::new(10, 20));
        assert_eq!(r, Rect::new(10, 20, 40, 40));
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 100, 100);
        assert_eq!(a.intersect(b), Some(Rect::new(50, 50, 50, 50)));
        assert_eq!(a.union(b), Rect::new(0, 0, 150, 150));
        assert_eq!(a.intersect(Rect::new(100, 0, 10, 10)), None, "touching edges do not overlap");
    }

    #[test]
    fn negative_origin_multi_monitor() {
        let left = Rect::new(-1920, 0, 1920, 1080);
        let right = Rect::new(0, 0, 2560, 1440);
        assert_eq!(Rect::bounding([left, right]), Some(Rect::new(-1920, 0, 4480, 1440)));
        assert!(left.contains(Point::new(-1, 10)));
        assert!(!left.contains(Point::new(0, 10)));
    }

    #[test]
    fn edges_do_not_overflow() {
        let r = Rect::new(i32::MAX, i32::MAX, u32::MAX, u32::MAX);
        assert!(r.right() > i64::from(i32::MAX));
        let _ = r.union(Rect::new(i32::MIN, i32::MIN, 1, 1));
    }

    #[test]
    fn inflate_clamps_at_zero() {
        assert_eq!(Rect::new(10, 10, 4, 4).inflate(-5).size(), Size::new(0, 0));
        assert_eq!(Rect::new(10, 10, 4, 4).inflate(2), Rect::new(8, 8, 8, 8));
    }

    #[test]
    fn empty_rects_ignored_by_union() {
        let a = Rect::new(5, 5, 0, 0);
        let b = Rect::new(1, 1, 3, 3);
        assert_eq!(a.union(b), b);
        assert_eq!(b.union(a), b);
    }
}
