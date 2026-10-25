// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Defines the Row type which makes up lines in the grid.

use std::cmp::{max, min};
use std::ops::{Index, IndexMut, Range, RangeFrom, RangeFull, RangeTo, RangeToInclusive};
use std::{ptr, slice};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::grid::GridCell;
use crate::index::Column;
use crate::term::cell::ResetDiscriminant;

/// A row in the grid.
///
/// feathercritty: upstream derived `Default` and `Clone`. They are written out so that
/// `Row::default()` (an empty row, no allocation) needs no `T: Default`, and so that
/// `clone_from` reuses the target's allocation (`History::push_newest` and `History::read` copy
/// rows into existing ones). When merging upstream: keep both in step with the fields.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Row<T> {
    inner: Vec<T>,

    /// Maximum number of occupied entries.
    ///
    /// This is the upper bound on the number of elements in the row, which have been modified
    /// since the last reset. All cells after this point are guaranteed to be equal.
    pub(crate) occ: usize,
}

impl<T> Default for Row<T> {
    #[inline]
    fn default() -> Self {
        Row { inner: Vec::new(), occ: 0 }
    }
}

impl<T: Clone> Clone for Row<T> {
    #[inline]
    fn clone(&self) -> Self {
        Row { inner: self.inner.clone(), occ: self.occ }
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.inner.clone_from(&source.inner);
        self.occ = source.occ;
    }
}

impl<T: PartialEq> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Default> Row<T> {
    /// Create a new terminal row.
    ///
    /// Ideally the `template` should be `Copy` in all performance sensitive scenarios.
    pub fn new(columns: usize) -> Row<T> {
        debug_assert!(columns >= 1);

        let mut inner: Vec<T> = Vec::with_capacity(columns);

        // This is a slightly optimized version of `std::vec::Vec::resize`.
        unsafe {
            let mut ptr = inner.as_mut_ptr();

            for _ in 1..columns {
                ptr::write(ptr, T::default());
                ptr = ptr.offset(1);
            }
            ptr::write(ptr, T::default());

            inner.set_len(columns);
        }

        Row { inner, occ: 0 }
    }

    /// Increase the number of columns in the row.
    #[inline]
    pub fn grow(&mut self, columns: usize) {
        if self.inner.len() >= columns {
            return;
        }

        // feathercritty: upstream padded with default cells and left `occ` alone, so cells from
        // `occ` on were no longer all equal (a reset tail followed by default padding), and a
        // later `reset` left the old tail in place whenever the template matched the padding.
        // What a reset row looked like then depended on which row was recycled. Raising `occ`
        // to the old length keeps the invariant. The differential test's oracle carries the
        // same fix (`tests/oracle/PATCHES.md`). When merging upstream: drop this if upstream
        // fixed it; re-run `grid::row` tests.
        self.occ = max(self.occ, self.inner.len());
        self.inner.resize_with(columns, T::default);
    }

    /// Reduce the number of columns in the row.
    ///
    /// This will return all non-empty cells that were removed.
    pub fn shrink(&mut self, columns: usize) -> Option<Vec<T>>
    where
        T: GridCell,
    {
        if self.inner.len() <= columns {
            return None;
        }

        // Split off cells for a new row.
        let mut new_row = self.inner.split_off(columns);
        let index = new_row.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
        new_row.truncate(index);

        self.occ = min(self.occ, columns);

        if new_row.is_empty() { None } else { Some(new_row) }
    }

    /// Reset all cells in the row to the `template` cell.
    #[inline]
    pub fn reset<D>(&mut self, template: &T)
    where
        T: ResetDiscriminant<D> + GridCell,
        D: PartialEq,
    {
        debug_assert!(!self.inner.is_empty());

        // Mark all cells as dirty if template cell changed.
        let len = self.inner.len();
        if self.inner[len - 1].discriminant() != template.discriminant() {
            self.occ = len;
        }

        // Reset every dirty cell in the row.
        for item in &mut self.inner[0..self.occ] {
            item.reset(template);
        }

        self.occ = 0;
    }
}

#[allow(clippy::len_without_is_empty)]
impl<T> Row<T> {
    #[inline]
    pub fn from_vec(vec: Vec<T>, occ: usize) -> Row<T> {
        Row { inner: vec, occ }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn last(&self) -> Option<&T> {
        self.inner.last()
    }

    #[inline]
    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.occ = self.inner.len();
        self.inner.last_mut()
    }

    #[inline]
    pub fn append(&mut self, vec: &mut Vec<T>)
    where
        T: GridCell,
    {
        // feathercritty: upstream added `vec.len()` to `occ`, which leaves the appended cells
        // beyond `occ` whenever `occ` was below the old length (the same broken invariant as in
        // `grow`). Every appended cell is now inside `occ`. The oracle carries the same fix.
        if !vec.is_empty() {
            self.occ = self.inner.len() + vec.len();
        }
        self.inner.append(vec);
    }

    #[inline]
    pub fn append_front(&mut self, mut vec: Vec<T>) {
        self.occ += vec.len();

        vec.append(&mut self.inner);
        self.inner = vec;
    }

    /// Check if all cells in the row are empty.
    #[inline]
    pub fn is_clear(&self) -> bool
    where
        T: GridCell,
    {
        self.inner.iter().all(GridCell::is_empty)
    }

    #[inline]
    pub fn front_split_off(&mut self, at: usize) -> Vec<T> {
        self.occ = self.occ.saturating_sub(at);

        let mut split = self.inner.split_off(at);
        std::mem::swap(&mut split, &mut self.inner);
        split
    }
}

impl<'a, T> IntoIterator for &'a Row<T> {
    type IntoIter = slice::Iter<'a, T>;
    type Item = &'a T;

    #[inline]
    fn into_iter(self) -> slice::Iter<'a, T> {
        self.inner.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Row<T> {
    type IntoIter = slice::IterMut<'a, T>;
    type Item = &'a mut T;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.occ = self.len();
        self.inner.iter_mut()
    }
}

impl<T> Index<Column> for Row<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Column) -> &T {
        &self.inner[index.0]
    }
}

impl<T> IndexMut<Column> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Column) -> &mut T {
        self.occ = max(self.occ, *index + 1);
        &mut self.inner[index.0]
    }
}

impl<T> Index<Range<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: Range<Column>) -> &[T] {
        &self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> IndexMut<Range<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Range<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end);
        &mut self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> Index<RangeTo<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeTo<Column>) -> &[T] {
        &self.inner[..(index.end.0)]
    }
}

impl<T> IndexMut<RangeTo<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeTo<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end);
        &mut self.inner[..(index.end.0)]
    }
}

impl<T> Index<RangeFrom<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeFrom<Column>) -> &[T] {
        &self.inner[(index.start.0)..]
    }
}

impl<T> IndexMut<RangeFrom<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeFrom<Column>) -> &mut [T] {
        self.occ = self.len();
        &mut self.inner[(index.start.0)..]
    }
}

impl<T> Index<RangeFull> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, _: RangeFull) -> &[T] {
        &self.inner[..]
    }
}

impl<T> IndexMut<RangeFull> for Row<T> {
    #[inline]
    fn index_mut(&mut self, _: RangeFull) -> &mut [T] {
        self.occ = self.len();
        &mut self.inner[..]
    }
}

impl<T> Index<RangeToInclusive<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeToInclusive<Column>) -> &[T] {
        &self.inner[..=(index.end.0)]
    }
}

impl<T> IndexMut<RangeToInclusive<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeToInclusive<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end + 1);
        &mut self.inner[..=(index.end.0)]
    }
}

/// feathercritty: the `occ` invariant ("all cells from `occ` on are equal") for every `Row`
/// mutator; `grow` broke it upstream (fork change, see `Row::grow`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::cell::Cell;
    use crate::vte::ansi::{Color, NamedColor};

    fn assert_occ_invariant(row: &Row<Cell>) {
        let tail = &row.inner[row.occ.min(row.inner.len())..];
        assert!(tail.windows(2).all(|pair| pair[0] == pair[1]), "cells from occ on differ");
    }

    fn red() -> Cell {
        Cell::from(Color::Named(NamedColor::Red))
    }

    fn text(c: char) -> Cell {
        Cell { c, ..Cell::default() }
    }

    /// A row of `len` cells reset with a red background (`occ` 0, every cell red).
    fn red_row(len: usize) -> Row<Cell> {
        let mut row = Row::new(len);
        row.reset(&red());
        row
    }

    #[test]
    fn grow_raises_occ_to_old_len() {
        let mut row = red_row(3);
        row.grow(5);
        assert_eq!(row.occ, 3);
        assert_occ_invariant(&row);

        // A later reset with the default template now clears the red cells as well.
        row.reset(&Cell::default());
        assert!(row.inner.iter().all(|cell| *cell == Cell::default()));
    }

    #[test]
    fn shrink_keeps_occ_invariant() {
        let mut row = red_row(5);
        row[Column(3)] = text('x');
        row.shrink(2);
        assert_occ_invariant(&row);
    }

    #[test]
    fn reset_keeps_occ_invariant() {
        let mut row = red_row(4);
        row[Column(1)] = text('x');
        row.reset(&Cell::default());
        assert_eq!(row.occ, 0);
        assert_occ_invariant(&row);
    }

    #[test]
    fn append_keeps_occ_invariant() {
        let mut row = red_row(3);
        row.append(&mut vec![text('a'), text('b')]);
        assert_occ_invariant(&row);
    }

    #[test]
    fn append_front_keeps_occ_invariant() {
        let mut row = red_row(3);
        row.append_front(vec![text('a'), text('b')]);
        assert_occ_invariant(&row);
    }

    #[test]
    fn front_split_off_keeps_occ_invariant() {
        let mut row = red_row(5);
        row[Column(3)] = text('x');
        let _ = row.front_split_off(2);
        assert_occ_invariant(&row);
    }

    #[test]
    fn index_mut_writes_keep_occ_invariant() {
        let mut row = red_row(6);
        row[Column(2)] = text('x');
        assert_occ_invariant(&row);
        row[Column(0)..Column(4)][3] = text('y');
        assert_occ_invariant(&row);
        row[..Column(5)][4] = text('z');
        assert_occ_invariant(&row);
        row[..=Column(5)][5] = text('w');
        assert_occ_invariant(&row);
    }
}
