// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Scrollback history: the rows above the screen (feathercritty).
//!
//! Upstream alacritty_terminal keeps the screen and its history in one `Storage` ring and lets
//! anything index any line of it. feathercritty splits the grid in two (`Grid` in `mod.rs`): the
//! screen stays a dense `Storage` ring that every parser write lands in, and the rows above it
//! live in this module's [`History`], reached only through an explicit API:
//!
//! - [`History`] owns the rows above the screen, oldest first. Index `i` counts from the newest
//!   row: `i = 0` is grid line `-1`, `i = len() - 1` is `Grid::topmost_line()`. It never holds more
//!   than `limit()` rows; pushing at the limit evicts the oldest row, a lower limit evicts at once.
//! - [`RowReader`] reads any grid line as `&Row`: the screen row itself for lines `>= 0`, or a copy
//!   of a history row in its own scratch row for lines `< 0`.
//! - [`HistoryCell`] is what a cell type needs for the header bits (`wraps`, `is_clear`).
//!
//! This is phase 1 of the compact history work (design spec section 5): history rows are still
//! stored as dense `Row`s, in a `VecDeque`, so behavior is exactly upstream's. A later phase
//! keeps this API and swaps the internals for an encoded store; the `pub(crate)` helpers and the
//! row lending marked "dense phase" (here, in `RowReader` and in `GridIterator`) exist only to
//! keep phase 1 as fast as upstream (no row copy per scrolled line, per history read or per
//! resize) and go away with it.
//!
//! When merging upstream: nothing in upstream corresponds to this file. Upstream code that reads
//! history lines (`self.raw[line]` or `grid[line]` with `line < 0`) must go through
//! `RowReader`, `Grid::cell`, `Grid::wraps` or `Grid::is_clear`; writes through
//! [`History::update`].

use std::collections::VecDeque;
use std::marker::PhantomData;

use crate::grid::{Dimensions, Grid, GridCell, Row};
use crate::index::Line;
use crate::term::cell::Flags;

/// A cell type the history can store.
///
/// Phase 1 stub: dense rows need nothing beyond the header bits below. The encoded store of a
/// later phase moves this trait next to its row codec and adds the encoding, the decoding and
/// the type of the shared extra data.
pub trait HistoryCell: GridCell + Clone + Default {
    /// Whether `row` continues on the next line: its last cell has `Flags::WRAPLINE`.
    ///
    /// Rows are always as wide as the grid, so this is upstream's
    /// `grid[line][last_column].flags.contains(Flags::WRAPLINE)`.
    #[inline]
    fn row_wraps(row: &Row<Self>) -> bool {
        row.last().is_some_and(|cell| cell.flags().contains(Flags::WRAPLINE))
    }
}

/// The rows above the screen, at most `limit` of them.
///
/// Invariant: `rows.len() <= limit`; `rows` is ordered oldest first, so the newest row (grid
/// line `-1`) is `rows.back()`.
#[derive(Clone, Debug)]
pub struct History<T> {
    rows: VecDeque<Row<T>>,
    limit: usize,
}

impl<T> History<T> {
    /// An empty history that keeps at most `limit` rows.
    pub fn new(limit: usize) -> Self {
        Self { rows: VecDeque::new(), limit }
    }

    /// Number of rows stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no row is stored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Maximum number of rows kept.
    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Change the maximum number of rows; a lower limit evicts the oldest rows now.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.evict_over_limit();
    }

    /// Append a copy of `row` as the newest row, evicting the oldest one at the limit. Stores
    /// nothing with a limit of 0. The caller keeps `row` (the screen reuses it).
    pub fn push_newest(&mut self, row: &Row<T>)
    where
        T: Clone,
    {
        if self.limit == 0 {
            return;
        }

        // Reuse the evicted row's allocation when there is one.
        let mut new = if self.rows.len() >= self.limit {
            self.rows.pop_front().unwrap_or_default()
        } else {
            Row::default()
        };
        new.clone_from(row);
        self.rows.push_back(new);
        self.evict_over_limit();
    }

    /// Dense phase: move `row` in as the newest row and return a row for the caller to reset
    /// and reuse: the evicted oldest row at the limit, `row` itself with a limit of 0 (nothing
    /// is stored), and `None` below the limit (the caller supplies a row).
    ///
    /// Same result as [`History::push_newest`] without copying the cells, which keeps
    /// `Grid::scroll_up` as cheap as upstream's ring rotation. Temporary: it exists only while
    /// history rows are dense; with the encoded store `Grid::scroll_up` calls `push_newest` and
    /// resets the row it keeps. Which row comes back does not matter: the `Row` `occ` invariant
    /// makes every reset row fully reset (`Row::grow`).
    #[inline]
    pub(crate) fn push_newest_recycle(&mut self, row: Row<T>) -> Option<Row<T>> {
        if self.limit == 0 {
            return Some(row);
        }

        // `rows.len() <= limit` holds, so popping one at the limit keeps it.
        let evicted = if self.rows.len() >= self.limit { self.rows.pop_front() } else { None };
        self.rows.push_back(row);
        evicted
    }

    /// Dense phase: append `row` as the newest row without copying it (resize hands the
    /// reflowed rows back this way). Evicts the oldest row at the limit.
    pub(crate) fn push_newest_owned(&mut self, row: Row<T>) {
        if self.limit == 0 {
            return;
        }

        self.rows.push_back(row);
        self.evict_over_limit();
    }

    /// Remove the newest row into `into`. Returns `false`, leaving `into` untouched, when the
    /// history is empty.
    pub fn pop_newest(&mut self, into: &mut Row<T>) -> bool {
        match self.rows.pop_back() {
            Some(row) => {
                *into = row;
                true
            },
            None => false,
        }
    }

    /// Remove the oldest row into `into`. Returns `false`, leaving `into` untouched, when the
    /// history is empty.
    pub fn pop_oldest(&mut self, into: &mut Row<T>) -> bool {
        match self.rows.pop_front() {
            Some(row) => {
                *into = row;
                true
            },
            None => false,
        }
    }

    /// Copy row `i` (0 is the newest) into `into`, reusing `into`'s allocation.
    ///
    /// Panics when `i >= len()`.
    pub fn read(&self, i: usize, into: &mut Row<T>)
    where
        T: Clone,
    {
        into.clone_from(self.row(i));
    }

    /// Change row `i` (0 is the newest) in place.
    ///
    /// Panics when `i >= len()`.
    pub fn update(&mut self, i: usize, f: impl FnOnce(&mut Row<T>)) {
        let index = self.index(i);
        f(&mut self.rows[index]);
    }

    /// Whether row `i` (0 is the newest) continues on the next line.
    pub fn wraps(&self, i: usize) -> bool
    where
        T: HistoryCell,
    {
        T::row_wraps(self.row(i))
    }

    /// Whether every cell of row `i` (0 is the newest) is empty.
    pub fn is_clear(&self, i: usize) -> bool
    where
        T: GridCell,
    {
        self.row(i).is_clear()
    }

    /// Remove every row; the limit stays.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Dense phase: row `i` (0 is the newest) by reference, for `Grid::cell`, equality and
    /// serialization. The encoded store of a later phase has no row to lend; those callers then
    /// decode with [`History::read`].
    ///
    /// Panics when `i >= len()`.
    #[inline]
    pub(crate) fn row(&self, i: usize) -> &Row<T> {
        &self.rows[self.index(i)]
    }

    /// Position of row `i` (0 is the newest) in `rows`.
    #[inline]
    fn index(&self, i: usize) -> usize {
        assert!(i < self.rows.len(), "history row {i} out of range (len {})", self.rows.len());
        self.rows.len() - 1 - i
    }

    fn evict_over_limit(&mut self) {
        while self.rows.len() > self.limit {
            self.rows.pop_front();
        }
    }
}

/// Reads grid rows from any line, screen or history, as `&Row`.
///
/// [`RowReader::row`] returns the screen row itself for lines `>= 0`. For history lines, the
/// dense phase lends the stored row too, so a reader costs nothing; the encoded store of a later
/// phase gives the reader a scratch row and decodes history rows into it (reusing its
/// allocation), which is why the API takes `&mut self` and why callers should keep one reader for
/// a loop over many lines.
#[derive(Debug)]
pub struct RowReader<T> {
    /// Dense phase: nothing to keep (the encoded store keeps its scratch row here).
    marker: PhantomData<Row<T>>,
}

impl<T> Default for RowReader<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RowReader<T> {
    /// A reader; it allocates nothing until it has to copy a row.
    pub fn new() -> Self {
        Self { marker: PhantomData }
    }

    /// Row `line` of `grid`, for any line from `grid.topmost_line()` to
    /// `grid.bottommost_line()`. Callers clamp; a line outside that range panics (in debug
    /// builds with this message, like upstream's indexing). No library path calls it with such
    /// a line: reads outside the grid are "no row" (`Grid::cell`).
    pub fn row<'a>(&'a mut self, grid: &'a Grid<T>, line: Line) -> &'a Row<T>
    where
        T: Clone,
    {
        debug_assert!(
            line >= grid.topmost_line() && line <= grid.bottommost_line(),
            "RowReader: line {} outside the grid ({}..={}); callers must clamp",
            line.0,
            grid.topmost_line().0,
            grid.bottommost_line().0,
        );

        if line.0 >= 0 {
            &grid.screen()[line]
        } else {
            // Dense phase: lend the stored row (the encoded store decodes it into a scratch row).
            grid.history().row((-line.0 - 1) as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Column;

    fn row(value: usize) -> Row<usize> {
        let mut row = Row::new(2);
        row[Column(0)] = value;
        row
    }

    fn first(history: &History<usize>, i: usize) -> usize {
        let mut into = Row::default();
        history.read(i, &mut into);
        into[Column(0)]
    }

    #[test]
    fn push_evicts_oldest_at_limit() {
        let mut history = History::new(3);
        for value in 1..=5 {
            history.push_newest(&row(value));
        }

        assert_eq!(history.len(), 3);
        assert_eq!((first(&history, 0), first(&history, 1), first(&history, 2)), (5, 4, 3));
    }

    #[test]
    fn push_at_limit_zero_stores_nothing() {
        let mut history = History::new(0);
        history.push_newest(&row(1));
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn set_limit_lower_evicts_now() {
        let mut history = History::new(5);
        for value in 1..=5 {
            history.push_newest(&row(value));
        }

        history.set_limit(2);

        assert_eq!(history.limit(), 2);
        assert_eq!(history.len(), 2);
        assert_eq!((first(&history, 0), first(&history, 1)), (5, 4));

        // A higher limit keeps every row and lets the history grow again.
        history.set_limit(4);
        history.push_newest(&row(6));
        assert_eq!(history.len(), 3);
        assert_eq!(first(&history, 2), 4);
    }

    #[test]
    fn pop_newest_returns_last_pushed() {
        let mut history = History::new(3);
        history.push_newest(&row(1));
        history.push_newest(&row(2));

        let mut into = Row::default();
        assert!(history.pop_newest(&mut into));
        assert_eq!(into[Column(0)], 2);
        assert!(history.pop_newest(&mut into));
        assert_eq!(into[Column(0)], 1);
        assert!(!history.pop_newest(&mut into));
        assert_eq!(into[Column(0)], 1, "a failed pop leaves `into` untouched");
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn pop_oldest_returns_first_pushed() {
        let mut history = History::new(3);
        history.push_newest(&row(1));
        history.push_newest(&row(2));

        let mut into = Row::default();
        assert!(history.pop_oldest(&mut into));
        assert_eq!(into[Column(0)], 1);
        assert_eq!(history.len(), 1);
        assert_eq!(first(&history, 0), 2);
    }

    #[test]
    fn read_zero_is_newest() {
        let mut history = History::new(3);
        history.push_newest(&row(1));
        history.push_newest(&row(2));

        assert_eq!(first(&history, 0), 2);
        assert_eq!(first(&history, 1), 1);
    }

    #[test]
    fn update_changes_one_row_only() {
        let mut history = History::new(3);
        for value in 1..=3 {
            history.push_newest(&row(value));
        }

        history.update(1, |row| row[Column(1)] = 9);

        let mut into = Row::default();
        history.read(1, &mut into);
        assert_eq!((into[Column(0)], into[Column(1)]), (2, 9));
        history.read(0, &mut into);
        assert_eq!((into[Column(0)], into[Column(1)]), (3, 0));
        history.read(2, &mut into);
        assert_eq!((into[Column(0)], into[Column(1)]), (1, 0));
    }

    #[test]
    fn clear_empties() {
        let mut history = History::new(3);
        history.push_newest(&row(1));
        history.push_newest(&row(2));

        history.clear();

        assert_eq!(history.len(), 0);
        assert_eq!(history.limit(), 3);
        let mut into = Row::default();
        assert!(!history.pop_newest(&mut into));
    }

    #[test]
    fn push_newest_recycle_hands_back_a_row_to_reuse() {
        let mut history = History::new(1);

        // Below the limit the caller supplies the row to reuse.
        assert!(history.push_newest_recycle(row(1)).is_none());

        // At the limit it gets the evicted oldest row back.
        let evicted = history.push_newest_recycle(row(2)).unwrap();
        assert_eq!(evicted[Column(0)], 1);
        assert_eq!(history.len(), 1);
        assert_eq!(first(&history, 0), 2);

        // At limit zero nothing is stored and the row comes back.
        let mut history = History::new(0);
        assert_eq!(history.push_newest_recycle(row(3)).unwrap()[Column(0)], 3);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn wraps_and_is_clear_read_the_row() {
        use crate::term::cell::{Cell, Flags};

        let mut history = History::new(2);
        let mut wrapped = Row::<Cell>::new(2);
        wrapped[Column(0)].c = 'a';
        wrapped[Column(1)].flags.insert(Flags::WRAPLINE);
        history.push_newest(&wrapped);
        history.push_newest(&Row::new(2));

        assert!(!history.wraps(0));
        assert!(history.is_clear(0));
        assert!(history.wraps(1));
        assert!(!history.is_clear(1));
    }
}
