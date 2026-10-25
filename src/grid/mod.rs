// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! A specialized 2D grid implementation optimized for use in a terminal.

use std::cmp::{max, min};
use std::mem;
use std::ops::{Bound, Deref, Index, IndexMut, Range, RangeBounds};

use crate::index::{Column, Line, Point};
use crate::term::cell::{Flags, ResetDiscriminant};
use crate::vte::ansi::{CharsetIndex, StandardCharset};

mod history;
pub mod resize;
mod row;
mod storage;
#[cfg(test)]
mod tests;

// feathercritty: the history and its read API (see `history.rs`).
pub use self::history::{History, HistoryCell, RowReader};
pub use self::row::Row;
use self::storage::Storage;

pub trait GridCell: Sized {
    /// Check if the cell contains any content.
    fn is_empty(&self) -> bool;

    /// Perform an opinionated cell reset based on a template cell.
    fn reset(&mut self, template: &Self);

    fn flags(&self) -> &Flags;
    fn flags_mut(&mut self) -> &mut Flags;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cursor<T> {
    /// The location of this cursor.
    pub point: Point,

    /// Template cell when using this cursor.
    pub template: T,

    /// Currently configured graphic character sets.
    pub charsets: Charsets,

    /// Tracks if the next call to input will need to first handle wrapping.
    ///
    /// This is true after the last column is set with the input function. Any function that
    /// implicitly sets the line or column needs to set this to false to avoid wrapping twice.
    ///
    /// Tracking `input_needs_wrap` makes it possible to not store a cursor position that exceeds
    /// the number of columns, which would lead to index out of bounds when interacting with arrays
    /// without sanitization.
    pub input_needs_wrap: bool,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct Charsets([StandardCharset; 4]);

impl Index<CharsetIndex> for Charsets {
    type Output = StandardCharset;

    fn index(&self, index: CharsetIndex) -> &StandardCharset {
        &self.0[index as usize]
    }
}

impl IndexMut<CharsetIndex> for Charsets {
    fn index_mut(&mut self, index: CharsetIndex) -> &mut StandardCharset {
        &mut self.0[index as usize]
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Scroll {
    Delta(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Grid based terminal content storage.
///
/// feathercritty: upstream stored every line in one `Storage` ring (`raw`), with a resize
/// buffer and up to 1000 cached rows beyond the history. The grid now has two parts:
///
/// - `screen`: the `lines` visible rows, a `Storage` ring that keeps upstream's hidden rows beyond
///   them but no history, reached through [`Grid::screen`] / [`Grid::screen_mut`] for lines
///   `0..lines`.
/// - `history`: the rows above the screen, at most `history.limit()` (upstream's
///   `max_scroll_limit`), reached through [`RowReader`], [`Grid::cell`], [`Grid::wraps`] and
///   [`Grid::is_clear`].
///
/// `Grid` no longer implements `Index`/`IndexMut` for `Line` and `Point`, so every read or write
/// needs a decision: screen or history. When merging upstream, classify each new indexing site
/// the same way. Line numbers and the diagram below are unchanged: line `-1` is the newest
/// history row, `topmost_line()` the oldest.
///
/// ```notrust
/// ┌─────────────────────────┐  <-- max_scroll_limit + lines
/// │                         │
/// │      UNINITIALIZED      │
/// │                         │
/// ├─────────────────────────┤  <-- self.history_size() + lines
/// │                         │
/// │     SCROLLUP REGION     │
/// │                         │
/// ├─────────────────────────┤v lines
/// │                         │|
/// │     VISIBLE  REGION     │|
/// │                         │|
/// ├─────────────────────────┤^ <-- display_offset
/// │                         │
/// │    SCROLLDOWN REGION    │
/// │                         │
/// └─────────────────────────┘  <-- zero
///                           ^
///                        columns
/// ```
#[derive(Clone, Debug)]
pub struct Grid<T> {
    /// Current cursor for writing data.
    pub cursor: Cursor<T>,

    /// Last saved cursor.
    pub saved_cursor: Cursor<T>,

    /// The visible lines (feathercritty: upstream's `raw` held these and the history).
    screen: Screen<T>,

    /// Lines above the screen, at most `history.limit()` of them (feathercritty: upstream kept
    /// them in `raw` and the limit in `max_scroll_limit`).
    history: History<T>,

    /// Number of columns.
    columns: usize,

    /// Number of visible lines.
    lines: usize,

    /// Offset of displayed area.
    ///
    /// If the displayed region isn't at the bottom of the screen, it stays
    /// stationary while more text is emitted. The scrolling implementation
    /// updates this offset accordingly.
    display_offset: usize,

    /// Lines rotated into history since the grid was created (fbterm patch: lets a host keep
    /// positions stable while output scrolls).
    scrolled_lines: i64,
}

/// The visible lines of a [`Grid`], indexed by `Line` (`0..screen_lines`) and `Point`.
///
/// feathercritty: a thin handle over the `Storage` ring that upstream's `Grid` indexed directly;
/// indexing compiles to the same code. Lines outside the screen fail the `debug_assert!` range
/// checks upstream's `Storage` already has (history lines are read with [`RowReader`]).
#[derive(Clone, Debug)]
pub struct Screen<T> {
    raw: Storage<T>,
}

impl<T> Index<Line> for Screen<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: Line) -> &Row<T> {
        &self.raw[index]
    }
}

impl<T> IndexMut<Line> for Screen<T> {
    #[inline]
    fn index_mut(&mut self, index: Line) -> &mut Row<T> {
        &mut self.raw[index]
    }
}

impl<T> Index<Point> for Screen<T> {
    type Output = T;

    #[inline]
    fn index(&self, point: Point) -> &T {
        &self[point.line][point.column]
    }
}

impl<T> IndexMut<Point> for Screen<T> {
    #[inline]
    fn index_mut(&mut self, point: Point) -> &mut T {
        &mut self[point.line][point.column]
    }
}

impl<T: GridCell + Default + PartialEq> Grid<T> {
    pub fn new(lines: usize, columns: usize, max_scroll_limit: usize) -> Grid<T> {
        Grid {
            screen: Screen { raw: Storage::with_capacity(lines, columns) },
            history: History::new(max_scroll_limit),
            display_offset: 0,
            saved_cursor: Cursor::default(),
            cursor: Cursor::default(),
            lines,
            columns,
            scrolled_lines: 0,
        }
    }

    /// Lines rotated into history since the grid was created (fbterm patch). A line at
    /// `Line(l)` keeps the same `scrolled_lines() + l` while output scrolls it upward and while
    /// the screen grows or shrinks. Can be negative (a grow pulled lines back from history).
    pub fn scrolled_lines(&self) -> i64 {
        self.scrolled_lines
    }

    /// Update the size of the scrollback history.
    pub fn update_history(&mut self, history_size: usize) {
        // feathercritty: upstream shrank `raw` by the excess rows (oldest first) and stored the
        // new `max_scroll_limit`; `History::set_limit` does both.
        self.history.set_limit(history_size);
        self.display_offset = min(self.display_offset, history_size);
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.display_offset = match scroll {
            Scroll::Delta(count) => {
                min(max((self.display_offset as i32) + count, 0) as usize, self.history_size())
            },
            Scroll::PageUp => min(self.display_offset + self.lines, self.history_size()),
            Scroll::PageDown => self.display_offset.saturating_sub(self.lines),
            Scroll::Top => self.history_size(),
            Scroll::Bottom => 0,
        };
    }

    // feathercritty: upstream's `increase_scroll_limit` grew `raw` before `scroll_up` rotated
    // rows into history; `History` grows as `scroll_up` pushes rows, so it is gone.

    /// Remove `count` rows from the oldest end of history.
    ///
    /// feathercritty: upstream shrank `raw` by `count` rows; only `grow_lines` calls this (see
    /// there for what `count` is now).
    fn decrease_scroll_limit(&mut self, count: usize) {
        let count = min(count, self.history_size());
        if count != 0 {
            let mut removed = Row::default();
            for _ in 0..count {
                self.history.pop_oldest(&mut removed);
            }
            self.display_offset = min(self.display_offset, self.history_size());
        }
    }

    #[inline]
    pub fn scroll_down<D>(&mut self, region: &Range<Line>, positions: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // feathercritty: screen-only, as upstream; `self.raw` became `self.screen.raw` and
        // `max_scroll_limit` became `history.limit()`.

        // When rotating the entire region, just reset everything.
        if region.end - region.start <= positions {
            for i in (region.start.0..region.end.0).map(Line::from) {
                self.screen[i].reset(&self.cursor.template);
            }

            return;
        }

        // Which implementation we can use depends on the existence of a scrollback history.
        //
        // Since a scrollback history prevents us from rotating the entire buffer downwards, we
        // instead have to rely on a slower, swap-based implementation.
        if self.history.limit() == 0 {
            // Swap the lines fixed at the bottom to their target positions after rotation.
            //
            // Since we've made sure that the rotation will never rotate away the entire region, we
            // know that the position of the fixed lines before the rotation must already be
            // visible.
            //
            // We need to start from the top, to make sure the fixed lines aren't swapped with each
            // other.
            let screen_lines = self.screen_lines() as i32;
            for i in (region.end.0..screen_lines).map(Line::from) {
                self.screen.raw.swap(i, i - positions as i32);
            }

            // Rotate the entire line buffer downward.
            self.screen.raw.rotate_down(positions);

            // Ensure all new lines are fully cleared.
            for i in (0..positions).map(Line::from) {
                self.screen[i].reset(&self.cursor.template);
            }

            // Swap the fixed lines at the top back into position.
            for i in (0..region.start.0).map(Line::from) {
                self.screen.raw.swap(i, i + positions);
            }
        } else {
            // Subregion rotation.
            let range = (region.start + positions).0..region.end.0;
            for line in range.rev().map(Line::from) {
                self.screen.raw.swap(line, line - positions);
            }

            let range = region.start.0..(region.start + positions).0;
            for line in range.rev().map(Line::from) {
                self.screen[line].reset(&self.cursor.template);
            }
        }
    }

    /// Move lines at the bottom toward the top.
    ///
    /// This is the performance-sensitive part of scrolling.
    pub fn scroll_up<D>(&mut self, region: &Range<Line>, positions: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // When rotating the entire region with fixed lines at the top, just reset everything.
        if region.end - region.start <= positions && region.start != 0 {
            for i in (region.start.0..region.end.0).map(Line::from) {
                self.screen[i].reset(&self.cursor.template);
            }

            return;
        }

        // Update display offset when not pinned to active area.
        if self.display_offset != 0 {
            self.display_offset = min(self.display_offset + positions, self.history.limit());
        }

        // feathercritty: rows rotated through the screen. Upstream rotated its whole ring by
        // `positions`, which for a count above the region's height (only possible when the
        // region starts at the top) also rotated history rows and reset some of them. The screen
        // ring holds no history, so it rotates by at most the region's height and the rest of
        // the count is pushed into history as reset rows below, which is upstream's result.
        let rotated = min(positions, (region.end - region.start).0 as usize);

        // Only rotate the entire history if the active region starts at the top.
        if region.start == 0 {
            // feathercritty: upstream grew the history here (`increase_scroll_limit`) and its
            // rotation below left the departing rows in place as the newest history rows,
            // recycling cache rows (or the oldest history rows at the limit) as the new bottom
            // rows. Here each departing row moves into history oldest first and its slot gets
            // the row `History` hands back (the evicted oldest row at the limit, else a spare
            // screen row or a new one). The rotation then brings the ring's hidden rows, or
            // those slots, to the bottom, where they are reset below as upstream's recycled rows
            // are. With a history limit of 0 nothing is stored and the departing rows are
            // recycled. Which row is recycled is invisible because the `Row` `occ` invariant
            // makes every reset row fully reset (`Row::grow`). `push_newest_recycle` is temporary
            // (dense history only); with an encoded history this pushes a copy and resets the row
            // in place. When merging upstream: if upstream changes this rotation, port it onto
            // these pushes; the differential test checks the result.
            let rotated_into_history = if self.history.limit() == 0 { 0 } else { rotated };
            for i in (0..rotated_into_history).map(Line::from) {
                let row = mem::take(&mut self.screen[i]);
                self.screen[i] = match self.history.push_newest_recycle(row) {
                    Some(recycled) => recycled,
                    None => self.screen.raw.take_spare(self.columns),
                };
            }

            // Swap the lines fixed at the top to their target positions after rotation.
            //
            // Since we've made sure that the rotation will never rotate away the entire region, we
            // know that the position of the fixed lines before the rotation must already be
            // visible.
            //
            // We need to start from the bottom, to make sure the fixed lines aren't swapped with
            // each other.
            for i in (0..region.start.0).rev().map(Line::from) {
                self.screen.raw.swap(i, i + positions);
            }

            // Rotate the entire line buffer upward.
            self.screen.raw.rotate(-(rotated as isize));
            self.scrolled_lines = self.scrolled_lines.wrapping_add(positions as i64);

            // Swap the fixed lines at the bottom back into position.
            let screen_lines = self.screen_lines() as i32;
            for i in (region.end.0..screen_lines).rev().map(Line::from) {
                self.screen.raw.swap(i, i - rotated);
            }
        } else {
            // Rotate lines without moving anything into history.
            for i in (region.start.0..region.end.0 - positions as i32).map(Line::from) {
                self.screen.raw.swap(i, i + positions);
            }
        }

        // Ensure all new lines are fully cleared.
        for i in (region.end.0 - rotated as i32..region.end.0).map(Line::from) {
            self.screen[i].reset(&self.cursor.template);
        }

        // feathercritty: the rest of a count above the region's height (see `rotated`): upstream
        // reset that many rows above the screen, so they enter history as reset rows.
        if positions > rotated && self.history.limit() != 0 {
            for _ in rotated..positions {
                let mut row = Row::new(self.columns);
                row.reset(&self.cursor.template);
                self.history.push_newest_owned(row);
            }
        }
    }

    pub fn clear_viewport<D>(&mut self)
    where
        T: ResetDiscriminant<D> + Clone,
        D: PartialEq,
    {
        // feathercritty: the iterator yields owned cells (`T: Clone`); the loop stops on line -1
        // as upstream's does.
        // Determine how many lines to scroll up by.
        let end = Point::new(Line(self.lines as i32 - 1), Column(self.columns()));
        let mut iter = self.iter_from(end);
        while let Some(cell) = iter.prev() {
            if !cell.is_empty() || cell.point.line < 0 {
                break;
            }
        }
        debug_assert!(iter.point.line >= -1);
        let positions = (iter.point.line.0 + 1) as usize;
        let region = Line(0)..Line(self.lines as i32);

        // Clear the viewport.
        self.scroll_up(&region, positions);

        // Reset rotated lines.
        for line in (0..(self.lines - positions)).map(Line::from) {
            self.screen[line].reset(&self.cursor.template);
        }
    }

    /// Completely reset the grid state.
    pub fn reset<D>(&mut self)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        self.clear_history();

        self.saved_cursor = Cursor::default();
        self.cursor = Cursor::default();
        self.display_offset = 0;

        // Reset all visible lines.
        //
        // feathercritty: `clear_history` above leaves `topmost_line()` at 0, so this resets
        // screen rows only, as upstream did.
        let range = self.topmost_line().0..(self.screen_lines() as i32);
        for line in range.map(Line::from) {
            self.screen[line].reset(&self.cursor.template);
        }
    }
}

impl<T> Grid<T> {
    /// Reset a visible region within the grid.
    pub fn reset_region<D, R: RangeBounds<Line>>(&mut self, bounds: R)
    where
        T: ResetDiscriminant<D> + GridCell + Default,
        D: PartialEq,
    {
        let start = match bounds.start_bound() {
            Bound::Included(line) => *line,
            Bound::Excluded(line) => *line + 1,
            Bound::Unbounded => Line(0),
        };

        let end = match bounds.end_bound() {
            Bound::Included(line) => *line + 1,
            Bound::Excluded(line) => *line,
            Bound::Unbounded => Line(self.screen_lines() as i32),
        };

        debug_assert!(start < self.screen_lines() as i32);
        debug_assert!(end <= self.screen_lines() as i32);

        for line in (start.0..end.0).map(Line::from) {
            self.screen[line].reset(&self.cursor.template);
        }
    }

    #[inline]
    pub fn clear_history(&mut self) {
        // Explicitly purge all lines from history.
        //
        // feathercritty: upstream shrank `raw` by the history size.
        self.history.clear();

        // Reset display offset.
        self.display_offset = 0;
    }

    /// This is used only for initializing after loading ref-tests.
    #[inline]
    pub fn initialize_all(&mut self)
    where
        T: GridCell + Default,
    {
        // Remove all cached lines to clear them of any content.
        self.truncate();

        // Initialize everything with empty new lines.
        //
        // feathercritty: upstream's `raw.initialize` appended blank rows at the oldest end of
        // history until it reached `max_scroll_limit`; the history is rebuilt with them first.
        let missing = self.history.limit() - self.history_size();
        if missing != 0 {
            let mut history = History::new(self.history.limit());
            for _ in 0..missing {
                history.push_newest_owned(Row::new(self.columns));
            }
            let mut row = Row::default();
            while self.history.pop_oldest(&mut row) {
                history.push_newest_owned(mem::take(&mut row));
            }
            self.history = history;
        }
    }

    /// This is used only for truncating before saving ref-tests.
    ///
    /// feathercritty: upstream dropped the cache rows beyond the history and moved its ring's
    /// zero to the start; the screen ring does the same. Serialization writes rows in truncated
    /// order anyway.
    #[inline]
    pub fn truncate(&mut self) {
        self.screen.raw.truncate();
    }

    /// Iterate over all cells in the grid starting at a specific point.
    #[inline]
    pub fn iter_from(&self, point: Point) -> GridIterator<'_, T> {
        let end = Point::new(self.bottommost_line(), self.last_column());
        GridIterator::new(self, point, end)
    }

    /// Iterate over all visible cells.
    ///
    /// This is slightly more optimized than calling `Grid::iter_from` in combination with
    /// `Iterator::take_while`.
    #[inline]
    pub fn display_iter(&self) -> GridIterator<'_, T> {
        let last_column = self.last_column();
        let start = Point::new(Line(-(self.display_offset() as i32) - 1), last_column);
        let end_line = min(start.line + self.screen_lines(), self.bottommost_line());
        let end = Point::new(end_line, last_column);

        GridIterator::new(self, start, end)
    }

    #[inline]
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    #[inline]
    pub fn cursor_cell(&mut self) -> &mut T {
        let point = self.cursor.point;
        &mut self.screen[point.line][point.column]
    }

    /// The visible lines, indexed by `Line` (`0..screen_lines`) and `Point`.
    ///
    /// feathercritty: replaces upstream's `Index<Line>`/`Index<Point>` on `Grid` for screen
    /// lines; history lines are read with [`RowReader`], [`Grid::cell`], [`Grid::wraps`] and
    /// [`Grid::is_clear`].
    #[inline]
    pub fn screen(&self) -> &Screen<T> {
        &self.screen
    }

    /// The visible lines, mutably (feathercritty: replaces upstream's `IndexMut` on `Grid`;
    /// every write the parser makes goes through here).
    #[inline]
    pub fn screen_mut(&mut self) -> &mut Screen<T> {
        &mut self.screen
    }

    /// The rows above the screen (feathercritty).
    #[inline]
    pub(crate) fn history(&self) -> &History<T> {
        &self.history
    }

    /// Change history row `line` (`line < 0`) in place (feathercritty: the one history write,
    /// see `Term::input`).
    #[inline]
    pub(crate) fn update_history_row(&mut self, line: Line, f: impl FnOnce(&mut Row<T>)) {
        debug_assert!(line < 0 && line >= self.topmost_line());
        self.history.update((-line.0 - 1) as usize, f);
    }

    /// Change line `line` in place, screen or history (feathercritty: tests build grids with
    /// history content through this; the library writes history only in `Term::input`).
    #[cfg(test)]
    pub(crate) fn update_line(&mut self, line: Line, f: impl FnOnce(&mut Row<T>)) {
        if line.0 >= 0 { f(&mut self.screen[line]) } else { self.update_history_row(line, f) }
    }

    /// The cell at `point`, by value (feathercritty: replaces upstream's `grid[point]` reads
    /// that can reach history; for many cells of one row, use a [`RowReader`]).
    ///
    /// A line outside `topmost_line()..=bottommost_line()` is "no row" and reads as
    /// `T::default()`. Upstream's indexing failed a `debug_assert!` there in debug builds and
    /// read whichever row its ring held at that index in release builds (a fork behavior
    /// change; the differential test's oracle carries the same rule, see
    /// `tests/oracle/PATCHES.md`). Columns are not checked, as upstream.
    #[inline]
    pub fn cell(&self, point: Point) -> T
    where
        T: Clone + Default,
    {
        if point.line > self.bottommost_line() || point.line < self.topmost_line() {
            T::default()
        } else if point.line.0 >= 0 {
            self.screen[point].clone()
        } else {
            self.history.row((-point.line.0 - 1) as usize)[point.column].clone()
        }
    }

    /// Whether line `line` continues on the next line (its last cell has `WRAPLINE`)
    /// (feathercritty: replaces upstream's
    /// `grid[line][last_column].flags.contains(Flags::WRAPLINE)`). A line outside the grid is
    /// "no row" and does not wrap (see [`Grid::cell`]).
    #[inline]
    pub fn wraps(&self, line: Line) -> bool
    where
        T: HistoryCell,
    {
        if line > self.bottommost_line() || line < self.topmost_line() {
            false
        } else if line.0 >= 0 {
            T::row_wraps(&self.screen[line])
        } else {
            self.history.wraps((-line.0 - 1) as usize)
        }
    }

    /// Whether every cell of line `line` is empty (feathercritty: replaces upstream's
    /// `grid[line].is_clear()`). A line outside the grid is "no row" and is clear (see
    /// [`Grid::cell`]).
    #[inline]
    pub fn is_clear(&self, line: Line) -> bool
    where
        T: GridCell,
    {
        if line > self.bottommost_line() || line < self.topmost_line() {
            true
        } else if line.0 >= 0 {
            self.screen[line].is_clear()
        } else {
            self.history.is_clear((-line.0 - 1) as usize)
        }
    }
}

/// feathercritty: upstream compared the `raw` storage (asserting both rings were truncated and
/// zeroed); this compares the content, screen rows then history rows, which is what that
/// comparison meant.
impl<T: PartialEq> PartialEq for Grid<T> {
    fn eq(&self, other: &Self) -> bool {
        // Compare struct fields and check result of grid comparison.
        self.columns.eq(&other.columns)
            && self.lines.eq(&other.lines)
            && self.display_offset.eq(&other.display_offset)
            && self.history.len() == other.history.len()
            && (0..self.lines as i32)
                .map(Line::from)
                .all(|line| self.screen[line] == other.screen[line])
            && (0..self.history.len()).all(|i| self.history.row(i) == other.history.row(i))
    }
}

/// feathercritty: upstream derived `Serialize`/`Deserialize` on `Grid` and `Storage`. The same
/// format is kept through these private proxies, so ref-test data and other saved grids load
/// unchanged: `raw.inner` lists rows in ring order from `zero` (bottom screen line first, then
/// up through history to the oldest row), `len` rows of it are live. Serializing writes the
/// order upstream writes after `Grid::truncate` (`zero` 0, no cache rows). Deserializing splits
/// the live rows at the grid's `lines`; `visible_lines` is written as `lines` and ignored on
/// read, because old ref-test files carry a stale value there.
#[cfg(feature = "serde")]
mod serde_proxy {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Grid, History, Row, Screen, Storage};
    use crate::index::Line;

    #[derive(Serialize)]
    #[serde(rename = "Grid")]
    struct GridRef<'a, T> {
        raw: StorageRef<'a, T>,
        columns: usize,
        lines: usize,
        display_offset: usize,
        max_scroll_limit: usize,
    }

    #[derive(Serialize)]
    #[serde(rename = "Storage")]
    struct StorageRef<'a, T> {
        inner: Vec<&'a Row<T>>,
        zero: usize,
        visible_lines: usize,
        len: usize,
    }

    #[derive(Deserialize)]
    #[serde(rename = "Grid")]
    struct GridOwned<T> {
        raw: StorageOwned<T>,
        columns: usize,
        lines: usize,
        display_offset: usize,
        max_scroll_limit: usize,
    }

    #[derive(Deserialize)]
    #[serde(rename = "Storage")]
    struct StorageOwned<T> {
        inner: Vec<Row<T>>,
        zero: usize,
        #[allow(dead_code)]
        visible_lines: usize,
        len: usize,
    }

    impl<T: Serialize> Serialize for Grid<T> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut inner = Vec::with_capacity(self.lines + self.history.len());
            inner.extend((0..self.lines as i32).rev().map(|line| &self.screen[Line(line)]));
            inner.extend((0..self.history.len()).map(|i| self.history.row(i)));
            let len = inner.len();

            GridRef {
                raw: StorageRef { inner, zero: 0, visible_lines: self.lines, len },
                columns: self.columns,
                lines: self.lines,
                display_offset: self.display_offset,
                max_scroll_limit: self.history.limit(),
            }
            .serialize(serializer)
        }
    }

    impl<'de, T: Deserialize<'de> + Default> Deserialize<'de> for Grid<T> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let GridOwned { raw, columns, lines, display_offset, max_scroll_limit } =
                GridOwned::deserialize(deserializer)?;
            let StorageOwned { mut inner, zero, len, .. } = raw;

            if len < lines || len > inner.len() || (zero != 0 && zero >= inner.len()) {
                return Err(serde::de::Error::custom(format!(
                    "grid rows: len {len}, lines {lines}, {} stored rows, zero {zero}",
                    inner.len()
                )));
            }

            // Ring order from `zero`, live rows only.
            inner.rotate_left(zero);
            inner.truncate(len);
            let history_rows = inner.split_off(lines);

            let mut history = History::new(max_scroll_limit);
            for row in history_rows.into_iter().rev() {
                history.push_newest_owned(row);
            }

            Ok(Grid {
                cursor: Default::default(),
                saved_cursor: Default::default(),
                screen: Screen { raw: Storage::from_rows(inner) },
                history,
                columns,
                lines,
                display_offset,
                scrolled_lines: 0,
            })
        }
    }
}

/// Grid dimensions.
pub trait Dimensions {
    /// Total number of lines in the buffer, this includes scrollback and visible lines.
    fn total_lines(&self) -> usize;

    /// Height of the viewport in lines.
    fn screen_lines(&self) -> usize;

    /// Width of the terminal in columns.
    fn columns(&self) -> usize;

    /// Index for the last column.
    #[inline]
    fn last_column(&self) -> Column {
        Column(self.columns() - 1)
    }

    /// Line farthest up in the grid history.
    #[inline]
    fn topmost_line(&self) -> Line {
        Line(-(self.history_size() as i32))
    }

    /// Line farthest down in the grid history.
    #[inline]
    fn bottommost_line(&self) -> Line {
        Line(self.screen_lines() as i32 - 1)
    }

    /// Number of invisible lines part of the scrollback history.
    #[inline]
    fn history_size(&self) -> usize {
        self.total_lines().saturating_sub(self.screen_lines())
    }
}

impl<G> Dimensions for Grid<G> {
    /// feathercritty: upstream returned `raw.len()`; it is the screen plus the history.
    #[inline]
    fn total_lines(&self) -> usize {
        self.lines + self.history.len()
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.lines
    }

    #[inline]
    fn columns(&self) -> usize {
        self.columns
    }
}

#[cfg(test)]
impl Dimensions for (usize, usize) {
    fn total_lines(&self) -> usize {
        self.0
    }

    fn screen_lines(&self) -> usize {
        self.0
    }

    fn columns(&self) -> usize {
        self.1
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Indexed<T> {
    pub point: Point,
    pub cell: T,
}

impl<T> Deref for Indexed<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.cell
    }
}

/// Grid cell iterator.
///
/// feathercritty: upstream yielded `Indexed<&T>` borrowed from the grid. Encoded history rows have
/// no cells to borrow, so the iterator yields `Indexed<T>` by value (a clone of the cell) and
/// `cell()` borrows from the iterator. It looks a row up once when it enters it and keeps a
/// reference to it: the screen row, or in the dense phase the stored history row (the encoded
/// store decodes a history row once into the iterator instead, never per cell). Iterating never
/// allocates.
pub struct GridIterator<'a, T> {
    /// Immutable grid reference.
    grid: &'a Grid<T>,

    /// Current position of the iterator within the grid.
    point: Point,

    /// Last cell included in the iterator.
    end: Point,

    /// The row `point` is on, `None` while `point` is outside the grid (feathercritty).
    row: Option<&'a Row<T>>,
}

impl<'a, T> GridIterator<'a, T> {
    fn new(grid: &'a Grid<T>, point: Point, end: Point) -> Self {
        Self { grid, point, end, row: Self::row_at(grid, point.line) }
    }

    /// Row `line` of `grid`, `None` outside the grid (feathercritty).
    #[inline]
    fn row_at(grid: &'a Grid<T>, line: Line) -> Option<&'a Row<T>> {
        if line > grid.bottommost_line() || line < grid.topmost_line() {
            None
        } else if line.0 >= 0 {
            Some(&grid.screen[line])
        } else {
            // Dense phase: lend the stored history row.
            Some(grid.history.row((-line.0 - 1) as usize))
        }
    }

    /// Current iterator position.
    pub fn point(&self) -> Point {
        self.point
    }

    /// Cell at the current iterator position.
    ///
    /// feathercritty: borrows from the iterator (upstream: `&'a T` from the grid). Panics outside
    /// the grid, as upstream's indexing did in debug builds.
    #[inline]
    pub fn cell(&self) -> &T {
        let row = self.row.expect("GridIterator: position outside the grid");
        &row[self.point.column]
    }
}

impl<T: Clone> Iterator for GridIterator<'_, T> {
    type Item = Indexed<T>;

    fn next(&mut self) -> Option<Self::Item> {
        // Stop once we've reached the end of the grid.
        if self.point >= self.end {
            return None;
        }

        match self.point {
            Point { column, .. } if column >= self.grid.last_column() => {
                self.point.column = Column(0);
                self.point.line += 1;
                // feathercritty: look the new row up once.
                self.row = Self::row_at(self.grid, self.point.line);
            },
            _ => self.point.column += Column(1),
        }

        Some(Indexed { cell: self.cell().clone(), point: self.point })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.point >= self.end {
            return (0, Some(0));
        }

        let size = if self.point.line == self.end.line {
            (self.end.column - self.point.column).0
        } else {
            let cols_on_first_line = self.grid.columns.saturating_sub(self.point.column.0 + 1);
            let middle_lines = (self.end.line - self.point.line).0 as usize - 1;
            let cols_on_last_line = self.end.column + 1;

            cols_on_first_line + middle_lines * self.grid.columns + cols_on_last_line.0
        };

        (size, Some(size))
    }
}

/// Bidirectional iterator.
pub trait BidirectionalIterator: Iterator {
    fn prev(&mut self) -> Option<Self::Item>;
}

impl<T: Clone> BidirectionalIterator for GridIterator<'_, T> {
    fn prev(&mut self) -> Option<Self::Item> {
        let topmost_line = self.grid.topmost_line();
        let last_column = self.grid.last_column();

        // Stop once we've reached the end of the grid.
        if self.point <= Point::new(topmost_line, Column(0)) {
            return None;
        }

        match self.point {
            Point { column: Column(0), .. } => {
                self.point.column = last_column;
                self.point.line -= 1;
                // feathercritty: look the new row up once.
                self.row = Self::row_at(self.grid, self.point.line);
            },
            _ => self.point.column -= Column(1),
        }

        Some(Indexed { cell: self.cell().clone(), point: self.point })
    }
}
