//! The arithmetic of reordering a list, separated from egui so it can be tested to death.
//!
//! Two vocabularies are needed and mixing them up is the classic off-by-one bug of drag and
//! drop lists, so they are named apart:
//!
//! * a **position** is where an item *ends up* (`0..len`): used by the move-up / move-down
//!   buttons and keyboard shortcuts ([`move_to`]);
//! * a **gap** is the slot between items a drop lands in (`0..=len`, gap `i` is *before* item
//!   `i`, gap `len` is after the last one): what a pointer position means ([`gap_at`],
//!   [`move_to_gap`]).
//!
//! Dropping item `from` into gap `g` results in position `g` when `g <= from` and `g - 1`
//! otherwise (the item's own removal shifts everything after it); gaps `from` and `from + 1`
//! are no-ops.

/// The position item `from` ends up at when dropped into `gap`.
pub const fn final_position(from: usize, gap: usize) -> usize {
    if gap > from { gap - 1 } else { gap }
}

/// Whether dropping `from` into `gap` changes anything.
pub const fn is_noop(from: usize, gap: usize) -> bool {
    gap == from || gap == from + 1
}

/// Moves the item at `from` to position `to` (clamped to the list). Returns the position it
/// ended up at, or `None` if nothing moved (`from` out of range, or already there).
pub fn move_to<T>(v: &mut Vec<T>, from: usize, to: usize) -> Option<usize> {
    if from >= v.len() {
        return None;
    }
    let to = to.min(v.len() - 1);
    if to == from {
        return None;
    }
    let item = v.remove(from);
    v.insert(to, item);
    Some(to)
}

/// Moves the item at `from` into `gap` (clamped to `0..=len`). Returns its new position, or
/// `None` for a no-op.
pub fn move_to_gap<T>(v: &mut Vec<T>, from: usize, gap: usize) -> Option<usize> {
    if from >= v.len() {
        return None;
    }
    let gap = gap.min(v.len());
    if is_noop(from, gap) {
        return None;
    }
    move_to(v, from, final_position(from, gap))
}

/// One step up. Returns the new position, or `None` at the top.
pub fn move_up<T>(v: &mut Vec<T>, i: usize) -> Option<usize> {
    if i == 0 { None } else { move_to(v, i, i - 1) }
}

/// One step down. Returns the new position, or `None` at the bottom.
pub fn move_down<T>(v: &mut Vec<T>, i: usize) -> Option<usize> {
    move_to(v, i, i + 1)
}

/// The gap a pointer at vertical position `y` points at, given the vertical centres of the
/// items in list order: the number of items whose centre is above the pointer.
pub fn gap_at(centers: &[f32], y: f32) -> usize {
    centers.iter().take_while(|c| **c < y).count()
}

/// The vertical position of the insertion marker for `gap`, given each item's `(top, bottom)`.
/// Between two items it sits in the middle of the space between them.
pub fn marker_y(rows: &[(f32, f32)], gap: usize) -> Option<f32> {
    match (gap.checked_sub(1).and_then(|i| rows.get(i)), rows.get(gap)) {
        (Some(above), Some(below)) => Some(f32::midpoint(above.1, below.0)),
        (None, Some(below)) => Some(below.0),
        (Some(above), None) => Some(above.1),
        (None, None) => None,
    }
}

/// A finished drag or key press: item `from` should go to `gap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// The item's current position.
    pub from: usize,
    /// The gap it was dropped into.
    pub gap: usize,
}

impl Move {
    /// A move by keyboard or button: to the gap just above / below the item's neighbour.
    pub const fn step(from: usize, delta: isize) -> Move {
        let gap = if delta < 0 { from.saturating_sub(1) } else { from + 2 };
        Move { from, gap }
    }

    /// Applies it to `v`; returns the item's new position (`None` for a no-op).
    pub fn apply<T>(&self, v: &mut Vec<T>) -> Option<usize> {
        move_to_gap(v, self.from, self.gap)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn list(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    #[test]
    fn positions_and_gaps() {
        assert_eq!(final_position(2, 0), 0);
        assert_eq!(final_position(2, 2), 2);
        assert_eq!(final_position(2, 3), 2);
        assert_eq!(final_position(2, 4), 3);
        assert!(is_noop(2, 2) && is_noop(2, 3));
        assert!(!is_noop(2, 1) && !is_noop(2, 4));
    }

    #[test]
    fn move_to_examples() {
        let mut v = list(5);
        assert_eq!(move_to(&mut v, 0, 3), Some(3));
        assert_eq!(v, [1, 2, 3, 0, 4]);
        assert_eq!(move_to(&mut v, 4, 0), Some(0));
        assert_eq!(v, [4, 1, 2, 3, 0]);
        assert_eq!(move_to(&mut v, 2, 2), None);
        assert_eq!(move_to(&mut v, 9, 0), None);
        assert_eq!(move_to(&mut v, 0, 99), Some(4), "clamped to the end");
        assert_eq!(v, [1, 2, 3, 0, 4]);
    }

    #[test]
    fn move_to_gap_examples() {
        let mut v = list(4);
        assert_eq!(move_to_gap(&mut v, 0, 4), Some(3), "after the last item");
        assert_eq!(v, [1, 2, 3, 0]);
        assert_eq!(move_to_gap(&mut v, 3, 0), Some(0), "before the first");
        assert_eq!(v, [0, 1, 2, 3]);
        assert_eq!(move_to_gap(&mut v, 1, 1), None);
        assert_eq!(move_to_gap(&mut v, 1, 2), None);
        assert_eq!(move_to_gap(&mut v, 1, 3), Some(2));
        assert_eq!(v, [0, 2, 1, 3]);
        assert_eq!(move_to_gap(&mut v, 0, 99), Some(3), "gap is clamped");
    }

    #[test]
    fn up_and_down_stop_at_the_ends() {
        let mut v = list(3);
        assert_eq!(move_up(&mut v, 0), None);
        assert_eq!(move_down(&mut v, 2), None);
        assert_eq!(move_down(&mut v, 0), Some(1));
        assert_eq!(v, [1, 0, 2]);
        assert_eq!(move_up(&mut v, 2), Some(1));
        assert_eq!(v, [1, 2, 0]);
        let mut empty: Vec<u8> = vec![];
        assert_eq!(move_up(&mut empty, 0), None);
        assert_eq!(move_down(&mut empty, 0), None);
    }

    #[test]
    fn steps_are_moves_of_one_position() {
        for n in 1..6 {
            for from in 0..n {
                let mut a = list(n);
                let mut b = list(n);
                let r1 = Move::step(from, -1).apply(&mut a);
                let r2 = move_up(&mut b, from);
                assert_eq!((r1, &a), (r2, &b), "up n={n} from={from}");
                let mut a = list(n);
                let mut b = list(n);
                let r1 = Move::step(from, 1).apply(&mut a);
                let r2 = move_down(&mut b, from);
                assert_eq!((r1, &a), (r2, &b), "down n={n} from={from}");
            }
        }
    }

    #[test]
    fn pointer_to_gap() {
        let centers = [10.0, 30.0, 50.0];
        assert_eq!(gap_at(&centers, -5.0), 0);
        assert_eq!(gap_at(&centers, 10.0), 0, "exactly on a centre counts as above it");
        assert_eq!(gap_at(&centers, 20.0), 1);
        assert_eq!(gap_at(&centers, 49.0), 2);
        assert_eq!(gap_at(&centers, 51.0), 3);
        assert_eq!(gap_at(&centers, 1e9), 3);
        assert_eq!(gap_at(&[], 5.0), 0);
    }

    #[test]
    fn marker_position() {
        let rows = [(0.0, 20.0), (24.0, 44.0), (48.0, 68.0)];
        assert_eq!(marker_y(&rows, 0), Some(0.0));
        assert_eq!(marker_y(&rows, 1), Some(22.0));
        assert_eq!(marker_y(&rows, 2), Some(46.0));
        assert_eq!(marker_y(&rows, 3), Some(68.0));
        assert_eq!(marker_y(&[], 0), None);
    }

    proptest! {
        #[test]
        fn move_to_is_a_permutation_that_places_the_item(n in 1usize..40, from in 0usize..60, to in 0usize..60) {
            let mut v = list(n);
            let r = move_to(&mut v, from, to);
            let mut sorted = v.clone();
            sorted.sort_unstable();
            prop_assert_eq!(sorted, list(n));
            if from < n {
                let target = to.min(n - 1);
                if target == from {
                    prop_assert_eq!(r, None);
                    prop_assert_eq!(v, list(n));
                } else {
                    prop_assert_eq!(r, Some(target));
                    prop_assert_eq!(v[target], from);
                    // the others keep their relative order
                    let others: Vec<usize> = v.iter().copied().filter(|x| *x != from).collect();
                    let expect: Vec<usize> = list(n).into_iter().filter(|x| *x != from).collect();
                    prop_assert_eq!(others, expect);
                }
            } else {
                prop_assert_eq!(r, None);
                prop_assert_eq!(v, list(n));
            }
        }

        #[test]
        fn dropping_into_a_gap_lands_in_the_predicted_position(n in 1usize..40, from in 0usize..40, gap in 0usize..41) {
            prop_assume!(from < n && gap <= n);
            let mut v = list(n);
            let r = move_to_gap(&mut v, from, gap);
            if is_noop(from, gap) {
                prop_assert_eq!(r, None);
                prop_assert_eq!(v, list(n));
            } else {
                let p = final_position(from, gap);
                prop_assert_eq!(r, Some(p));
                prop_assert_eq!(v[p], from);
                // the item sits between the items that surrounded the gap before the drop
                if gap < n && gap != from {
                    // its successor is the item that was at `gap` (unless that is `from` itself)
                    prop_assert_eq!(v.get(p + 1).copied(), Some(gap));
                }
                if gap > 0 && gap - 1 != from {
                    prop_assert_eq!(v[p - 1], gap - 1);
                }
            }
        }

        #[test]
        fn undoing_a_move_restores_the_list(n in 2usize..30, from in 0usize..30, to in 0usize..30) {
            prop_assume!(from < n);
            let mut v = list(n);
            if let Some(p) = move_to(&mut v, from, to) {
                prop_assert_eq!(move_to(&mut v, p, from), if p == from { None } else { Some(from) });
            }
            prop_assert_eq!(v, list(n));
        }

        #[test]
        fn pointer_gaps_are_monotonic_and_bounded(n in 0usize..20, ys in proptest::collection::vec(-100.0f32..1000.0, 2..10)) {
            let centers: Vec<f32> = (0..n).map(|i| 10.0 + 24.0 * i as f32).collect();
            let mut sorted = ys.clone();
            sorted.sort_by(f32::total_cmp);
            let gaps: Vec<usize> = sorted.iter().map(|y| gap_at(&centers, *y)).collect();
            prop_assert!(gaps.windows(2).all(|w| w[0] <= w[1]));
            prop_assert!(gaps.iter().all(|g| *g <= n));
        }
    }
}
