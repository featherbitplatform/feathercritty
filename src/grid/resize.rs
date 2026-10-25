// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Grid resize and reflow.
//!
//! feathercritty: the algorithms are upstream's, line for line; only the containers differ
//! since the grid holds its screen and its history apart (`Grid` in `mod.rs`). Every container
//! call that differs from upstream is marked `// feathercritty:` so this file can be diffed
//! against upstream's when merging; port upstream changes onto these containers.
//!
//! The column reflow (`grow_columns`, `shrink_columns`) streams (design spec 5.6). Upstream took
//! every row into one dense `Vec` (`Storage::take_all`), built the result in another
//! (`reversed` / `new_raw`) and stored it back (`Storage::replace_inner`). Here:
//!
//! - [`RowSource`] stands for the taken `Vec`: it pops the old history's rows one at a time, oldest
//!   first, decoding each as the loop reaches it, so the old blocks are freed as the pass goes,
//!   then hands out the screen rows.
//! - [`ReflowSink`] stands for the result `Vec`, with exactly the operations upstream used on it.
//!   It keeps only the newest rows dense (a window of `2 * lines + 2` rows) and encodes each row
//!   leaving the window into a new history; reaching back past the window decodes rows back, so the
//!   result never depends on the window size (`reflow_window_one_matches_default` runs with a
//!   window of 1).
//!
//! Peak memory is the old compact store plus the new one plus the window; nothing dense of
//! history size is allocated, and nothing of the pass stays resident after it.
//!
//! When merging upstream: if upstream's reflow starts using another `Vec` operation on these
//! containers (indexing a row other than the newest, `insert`, `retain`, ...), add it to the
//! sink with the same meaning, decoding rows back when they already left the window, and extend
//! `reflow_window_one_matches_default` to reach it.

use std::cmp::{Ordering, max, min};
use std::collections::VecDeque;
use std::mem;

use crate::index::{Boundary, Column, Line};
use crate::term::cell::{Flags, ResetDiscriminant};

use crate::grid::history::History;
use crate::grid::row::Row;
use crate::grid::storage::Storage;
use crate::grid::{Dimensions, Grid, HistoryCell};

/// The rows of a grid taken for a reflow, standing for the `Vec` upstream's `Storage::take_all`
/// returned (feathercritty).
///
/// Ordered as that `Vec`: the front is the bottom screen line, the back the oldest history row.
/// The reflow loop consumes it from the back (`enumerate().rev()`, as upstream), so it sees the
/// rows oldest first: the history rows, each popped from the history (`History::pop_oldest`)
/// and decoded only then, then the screen rows from line 0 down. Popping frees each history
/// block as its last row leaves; once the history is empty its index is freed too.
pub(crate) struct RowSource<'a, T: HistoryCell> {
    history: &'a mut History<T>,

    /// The screen rows, bottom line first.
    screen: std::vec::IntoIter<Row<T>>,
}

impl<'a, T: HistoryCell> RowSource<'a, T> {
    /// The rows of `history` (popped as they are taken) and `screen` (bottom line first, as
    /// `Storage::take_all` returns it).
    pub(crate) fn new(history: &'a mut History<T>, screen: Vec<Row<T>>) -> Self {
        Self { history, screen: screen.into_iter() }
    }
}

impl<T: HistoryCell> Iterator for RowSource<'_, T> {
    type Item = Row<T>;

    /// The newest row left: the bottom screen line first, then the history from its newest row.
    /// (The reflow takes rows from the back; this end exists to make the type an iterator.)
    fn next(&mut self) -> Option<Row<T>> {
        self.screen.next().or_else(|| {
            let mut row = Row::default();
            self.history.pop_newest(&mut row).then_some(row)
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.history.len() + self.screen.len();
        (len, Some(len))
    }
}

impl<T: HistoryCell> DoubleEndedIterator for RowSource<'_, T> {
    /// The oldest row left: the history from its oldest row, then the screen from line 0.
    fn next_back(&mut self) -> Option<Row<T>> {
        if !self.history.is_empty() {
            let mut row = Row::default();
            self.history.pop_oldest(&mut row);
            if self.history.is_empty() {
                // Free the emptied history's index and extras now, not when it is dropped.
                self.history.clear();
            }
            return Some(row);
        }
        self.screen.next_back()
    }
}

impl<T: HistoryCell> ExactSizeIterator for RowSource<'_, T> {}

/// The rows a reflow produces, standing for upstream's `reversed` / `new_raw` `Vec`
/// (feathercritty).
///
/// Rows are pushed oldest first; the end is the newest row, as in upstream's `Vec`. Only the
/// newest `window` rows stay dense; a row leaving the window is grown to `grow_to` columns (when
/// set and the row is narrower: upstream's last pass in `grow_columns` grew every row, and no
/// row is touched again once it is not the newest) and encoded into a new history. Operations
/// that reach past the window (`truncate_newest`, `last_mut` after a truncation, `finish`
/// pulling screen rows back) decode rows back with `History::pop_newest`, so the result is the
/// same for any window size. A row decoded back is already grown to `grow_to`: upstream reads a
/// row that stopped being the newest only after its loop, to drop it or to grow it in the final
/// pass, so that is never observable; if upstream starts reading such rows inside the loop, the
/// growing must move to `finish`.
///
/// The new history keeps at most `max_scroll_limit + lines` rows during the pass, evicting the
/// oldest: upstream's final trim (`truncate(max_scroll_limit + lines)` in `shrink_columns`)
/// keeps exactly the newest that many. Eviction before a later `truncate_newest` would differ
/// from upstream, but only `grow_columns` truncates, and it never has more rows than that.
pub(crate) struct ReflowSink<T: HistoryCell> {
    /// The newest rows, oldest first, at most `window` of them after each push.
    dense: VecDeque<Row<T>>,

    /// The rows older than `dense`, encoded.
    history: History<T>,

    window: usize,
    grow_to: Option<usize>,

    /// Whether the history evicted a row (checked against `truncate_newest`, see above).
    evicted: bool,
}

impl<T: HistoryCell> ReflowSink<T> {
    /// A sink keeping `window` (at least 1) rows dense, encoding older rows into `history`
    /// (empty, with a limit of `max_scroll_limit + lines`).
    pub(crate) fn new(window: usize, grow_to: Option<usize>, history: History<T>) -> Self {
        debug_assert!(window > 0 && history.is_empty());
        Self {
            dense: VecDeque::with_capacity(window + 1),
            history,
            window,
            grow_to,
            evicted: false,
        }
    }

    /// Append `row` as the newest row (upstream `Vec::push`).
    pub(crate) fn push(&mut self, row: Row<T>) {
        self.dense.push_back(row);
        if self.dense.len() > self.window {
            if let Some(row) = self.dense.pop_front() {
                self.store(row);
            }
        }
    }

    /// The newest row (upstream `Vec::last_mut`).
    pub(crate) fn last_mut(&mut self) -> Option<&mut Row<T>> {
        if self.dense.is_empty() {
            self.pull_back();
        }
        self.dense.back_mut()
    }

    /// Number of rows (upstream `Vec::len`).
    pub(crate) fn len(&self) -> usize {
        self.dense.len() + self.history.len()
    }

    /// Keep the oldest `len` rows, dropping the newest ones (upstream `Vec::truncate`).
    pub(crate) fn truncate_newest(&mut self, len: usize) {
        debug_assert!(!self.evicted || self.len() <= len, "truncating after an eviction");
        let mut dropped: Row<T> = Row::default();
        while self.len() > len {
            if self.dense.pop_back().is_none() {
                self.history.pop_newest(&mut dropped);
            }
        }
    }

    /// Append rows of `columns` cells equal to `template` until there are `len` rows, or drop the
    /// newest rows down to `len` (upstream `Vec::resize_with(len, || Row::new(columns))`, with
    /// `template` `T::default()`).
    pub(crate) fn resize_with_blank(&mut self, len: usize, columns: usize, template: &T) {
        self.truncate_newest(len);
        // Counted, not `while self.len() < len`: past `max_scroll_limit + lines` rows a push
        // evicts the oldest and the length stays (the final trim would drop that row anyway).
        for _ in self.len()..len {
            let mut row = Row::new(columns);
            if *template != T::default() {
                row = Row::from_vec(vec![template.clone(); columns], columns);
            }
            self.push(row);
        }
    }

    /// The newest `lines` rows as the screen and the older ones, at most `max_scroll_limit`, as
    /// the history (upstream: reverse the `Vec`, `truncate(max_scroll_limit + lines)` in
    /// `shrink_columns`, `Storage::replace_inner`). There are at least `lines` rows.
    pub(crate) fn finish(
        mut self,
        lines: usize,
        max_scroll_limit: usize,
    ) -> (Storage<T>, History<T>) {
        while self.dense.len() < lines && self.pull_back() {}
        debug_assert!(self.dense.len() >= lines, "a reflow leaves at least `lines` rows");

        let mut dense = mem::take(&mut self.dense);
        for row in dense.drain(..dense.len().saturating_sub(lines)) {
            self.store(row);
        }
        self.history.set_limit(max_scroll_limit);

        let screen = dense.into_iter().rev().map(|row| self.grown(row)).collect();
        (Storage::from_rows(screen), self.history)
    }

    /// Encode `row` as the newest history row.
    fn store(&mut self, row: Row<T>) {
        self.evicted |= self.history.len() >= self.history.limit();
        let row = self.grown(row);
        self.history.push_newest(&row);
    }

    /// `row`, grown to `grow_to` columns when it is narrower.
    fn grown(&self, mut row: Row<T>) -> Row<T> {
        if let Some(columns) = self.grow_to {
            if row.len() < columns {
                row.grow(columns);
            }
        }
        row
    }

    /// Decode the newest history row back to the front of the window. Returns `false` when the
    /// history is empty.
    fn pull_back(&mut self) -> bool {
        let mut row = Row::default();
        let pulled = self.history.pop_newest(&mut row);
        if pulled {
            self.dense.push_front(row);
        }
        pulled
    }
}

impl<T: HistoryCell> Grid<T> {
    /// Move the history out for a reflow (feathercritty), leaving an empty one with the same
    /// limit and block size, so `history.limit()` and `Dimensions` keep working during the pass.
    /// Allocates nothing.
    fn take_history(&mut self) -> History<T> {
        let empty = History::with_block_size(self.history.limit(), self.history.block_bytes());
        mem::replace(&mut self.history, empty)
    }

    /// The sink of a reflow (feathercritty): a window of `2 * lines + 2` rows (the screen, as
    /// many rows `grow_columns` may truncate, and slack), or the test hook's window.
    fn reflow_sink(&self, grow_to: Option<usize>) -> ReflowSink<T> {
        let window = 2 * self.lines + 2;
        #[cfg(any(test, feature = "test-hooks"))]
        let window = self.reflow_window.unwrap_or(window);

        let limit = self.history.limit() + self.lines;
        ReflowSink::new(
            window,
            grow_to,
            History::with_block_size(limit, self.history.block_bytes()),
        )
    }

    /// Store a reflow's rows as the new screen and history (feathercritty: upstream
    /// `self.raw.replace_inner`).
    fn replace_rows(&mut self, rows: ReflowSink<T>) {
        let (screen, history) = rows.finish(self.lines, self.history.limit());
        self.screen.raw = screen;
        self.history = history;
        debug_assert_eq!(self.screen.raw.len(), self.lines);
    }
}

// feathercritty: upstream's bound was `T: GridCell + Default + PartialEq`, which `HistoryCell`
// includes.
impl<T: HistoryCell> Grid<T> {
    /// Resize the grid's width and/or height.
    pub fn resize<D>(&mut self, reflow: bool, lines: usize, columns: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // Use empty template cell for resetting cells due to resize.
        let template = mem::take(&mut self.cursor.template);

        match self.lines.cmp(&lines) {
            Ordering::Less => self.grow_lines(lines),
            Ordering::Greater => self.shrink_lines(lines),
            Ordering::Equal => (),
        }

        match self.columns.cmp(&columns) {
            Ordering::Less => self.grow_columns(reflow, columns),
            Ordering::Greater => self.shrink_columns(reflow, columns),
            Ordering::Equal => (),
        }

        // Restore template cell.
        self.cursor.template = template;
    }

    /// Add lines to the visible area.
    ///
    /// Alacritty keeps the cursor at the bottom of the terminal as long as there
    /// is scrollback available. Once scrollback is exhausted, new lines are
    /// simply added to the bottom of the screen.
    fn grow_lines<D>(&mut self, target: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        let lines_added = target - self.lines;

        // feathercritty: `history_size` and `from_history` are computed before growing the
        // screen (upstream: after, with the same values, since its growth kept the history size).
        let history_size = self.history_size();
        let from_history = min(history_size, lines_added);

        // Need to resize before updating buffer.
        //
        // feathercritty: upstream grew its ring by `lines_added` rows at the oldest end, which
        // moved the `from_history` newest history rows onto the screen's new top lines and left
        // blank rows above them (on the screen when history ran short, in history otherwise).
        // The screen ring grows by `lines_added` top rows (its hidden rows or new ones, stale
        // content included, as upstream's cache rows were) and the newest history rows are
        // decoded into the lowest of them, giving the same screen. The stale rows left above
        // only ever reach history through `scroll_up` below and are removed again, as
        // upstream's were.
        self.screen.raw.grow_visible_lines(target);
        self.lines = target;
        for line in (lines_added - from_history..lines_added).rev().map(|line| Line(line as i32)) {
            self.history.pop_newest(&mut self.screen[line]);
        }

        // Move existing lines up for every line that couldn't be pulled from history.
        if from_history != lines_added {
            let delta = lines_added - from_history;
            self.scroll_up(&(Line(0)..Line(target as i32)), delta);
        }

        // fbterm patch: existing lines move down by `from_history` rows; `scroll_up` above
        // counted `delta` rotations that only undo the growth, so take back all `lines_added`.
        self.scrolled_lines = self.scrolled_lines.wrapping_sub(lines_added as i64);

        // Move cursor down for every line pulled from history.
        self.saved_cursor.point.line += from_history;
        self.cursor.point.line += from_history;

        self.display_offset = self.display_offset.saturating_sub(lines_added);

        // feathercritty: upstream removed `lines_added` rows from the oldest end of history:
        // the blank rows its growth had added there. Here the history already lost the
        // `from_history` rows it gave to the screen; what is left to remove are the blank rows
        // `scroll_up` above pushed (as upstream, all of them).
        self.decrease_scroll_limit(lines_added - from_history);

        // feathercritty: upstream's `decrease_scroll_limit(lines_added)` always removed rows
        // (unless history and its limit were 0, where the display offset is 0 anyway), so it
        // always clamped the display offset to the history size. The call above may remove
        // nothing, so clamp here; the offset can exceed the history size, since `scroll_up`
        // with fixed lines at the top moves it without adding history.
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Remove lines from the visible area.
    ///
    /// The behavior in Terminal.app and iTerm.app is to keep the cursor at the
    /// bottom of the screen. This is achieved by pushing history "out the top"
    /// of the terminal window.
    ///
    /// Alacritty takes the same approach.
    fn shrink_lines<D>(&mut self, target: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // Scroll up to keep content inside the window.
        let required_scrolling = (self.cursor.point.line.0 as usize + 1).saturating_sub(target);
        if required_scrolling > 0 {
            self.scroll_up(&(Line(0)..Line(self.lines as i32)), required_scrolling);

            // Clamp cursors to the new viewport size.
            self.cursor.point.line = min(self.cursor.point.line, Line(target as i32 - 1));
        }

        // Clamp saved cursor, since only primary cursor is scrolled into viewport.
        self.saved_cursor.point.line = min(self.saved_cursor.point.line, Line(target as i32 - 1));

        // feathercritty: `self.raw` became `self.screen.raw`; the bottom `lines - target` rows
        // become hidden rows of the screen ring, as upstream's did.
        self.screen.raw.rotate((self.lines - target) as isize);
        self.screen.raw.shrink_visible_lines(target);
        self.lines = target;
    }

    /// Grow number of columns in each row, reflowing if necessary.
    fn grow_columns(&mut self, reflow: bool, columns: usize) {
        // Check if a row needs to be wrapped.
        let should_reflow = |row: &Row<T>| -> bool {
            let len = Column(row.len());
            reflow && len.0 > 0 && len < columns && row[len - 1].flags().contains(Flags::WRAPLINE)
        };

        self.columns = columns;

        // feathercritty: upstream `Vec::with_capacity(self.raw.len())`; see `ReflowSink`.
        let mut reversed = self.reflow_sink(Some(columns));
        let mut cursor_line_delta = 0;

        // Remove the linewrap special case, by moving the cursor outside of the grid.
        if self.cursor.input_needs_wrap && reflow {
            self.cursor.input_needs_wrap = false;
            self.cursor.point.column += 1;
        }

        // feathercritty: upstream `self.raw.take_all()` and `rows.drain(..)`; see `RowSource`.
        let mut history = self.take_history();
        let rows = RowSource::new(&mut history, self.screen.raw.take_all());

        for (i, mut row) in rows.enumerate().rev() {
            // Check if reflowing should be performed.
            let last_row = match reversed.last_mut() {
                Some(last_row) if should_reflow(last_row) => last_row,
                _ => {
                    reversed.push(row);
                    continue;
                },
            };

            // Remove wrap flag before appending additional cells.
            if let Some(cell) = last_row.last_mut() {
                cell.flags_mut().remove(Flags::WRAPLINE);
            }

            // Remove leading spacers when reflowing wide char to the previous line.
            let mut last_len = last_row.len();
            if last_len >= 1
                && last_row[Column(last_len - 1)].flags().contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                last_row.shrink(last_len - 1);
                last_len -= 1;
            }

            // Don't try to pull more cells from the next line than available.
            let mut num_wrapped = columns - last_len;
            let len = min(row.len(), num_wrapped);

            // Insert leading spacer when there's not enough room for reflowing wide char.
            let mut cells = if row[Column(len - 1)].flags().contains(Flags::WIDE_CHAR) {
                num_wrapped -= 1;

                let mut cells = row.front_split_off(len - 1);

                let mut spacer = T::default();
                spacer.flags_mut().insert(Flags::LEADING_WIDE_CHAR_SPACER);
                cells.push(spacer);

                cells
            } else {
                row.front_split_off(len)
            };

            // Add removed cells to previous row and reflow content.
            last_row.append(&mut cells);

            let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;

            if i == cursor_buffer_line && reflow {
                // Resize cursor's line and reflow the cursor if necessary.
                let mut target = self.cursor.point.sub(self, Boundary::Cursor, num_wrapped);

                // Clamp to the last column, if no content was reflown with the cursor.
                if target.column.0 == 0 && row.is_clear() {
                    self.cursor.input_needs_wrap = true;
                    target = target.sub(self, Boundary::Cursor, 1);
                }
                self.cursor.point.column = target.column;

                // Get required cursor line changes. Since `num_wrapped` is smaller than `columns`
                // this will always be either `0` or `1`.
                let line_delta = self.cursor.point.line - target.line;

                if line_delta != 0 && row.is_clear() {
                    continue;
                }

                cursor_line_delta += line_delta.0 as usize;
            } else if row.is_clear() {
                if i < self.display_offset {
                    // Since we removed a line, rotate down the viewport.
                    self.display_offset = self.display_offset.saturating_sub(1);
                }

                // Rotate cursor down if content below them was pulled from history.
                if i < cursor_buffer_line {
                    self.cursor.point.line += 1;
                }

                // Don't push line into the new buffer.
                continue;
            }

            if let Some(cell) = last_row.last_mut() {
                // Set wrap flag if next line still has cells.
                cell.flags_mut().insert(Flags::WRAPLINE);
            }

            reversed.push(row);
        }

        // Make sure we have at least the viewport filled.
        if reversed.len() < self.lines {
            let delta = (self.lines - reversed.len()) as i32;
            self.cursor.point.line = max(self.cursor.point.line - delta, Line(0));
            // feathercritty: upstream `reversed.resize_with(self.lines, || Row::new(columns))`.
            reversed.resize_with_blank(self.lines, columns, &T::default());
        }

        // Pull content down to put cursor in correct position, or move cursor up if there's no
        // more lines to delete below the cursor.
        if cursor_line_delta != 0 {
            let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
            let available = min(cursor_buffer_line, reversed.len() - self.lines);
            let overflow = cursor_line_delta.saturating_sub(available);
            // feathercritty: upstream `reversed.truncate`.
            reversed.truncate_newest(reversed.len() + overflow - cursor_line_delta);
            self.cursor.point.line = max(self.cursor.point.line - overflow, Line(0));
        }

        // Reverse iterator and fill all rows that are still too short.
        //
        // feathercritty: upstream reversed `reversed` into `new_raw`, growing every row narrower
        // than `columns`, then called `self.raw.replace_inner(new_raw)`. The sink grows each row
        // as it leaves the window and in `finish`, which also reverses the rows into the screen
        // and the history (no trim here: `grow_columns` never has more rows than it started
        // with).
        self.replace_rows(reversed);

        // Clamp display offset in case lines above it got merged.
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Shrink number of columns in each row, reflowing if necessary.
    fn shrink_columns(&mut self, reflow: bool, columns: usize) {
        self.columns = columns;

        // Remove the linewrap special case, by moving the cursor outside of the grid.
        if self.cursor.input_needs_wrap && reflow {
            self.cursor.input_needs_wrap = false;
            self.cursor.point.column += 1;
        }

        // feathercritty: upstream `Vec::with_capacity(self.raw.len())`; see `ReflowSink`.
        let mut new_raw = self.reflow_sink(None);
        let mut buffered: Option<Vec<T>> = None;

        // feathercritty: upstream `self.raw.take_all()` and `rows.drain(..)`; see `RowSource`.
        let mut history = self.take_history();
        let rows = RowSource::new(&mut history, self.screen.raw.take_all());
        for (i, mut row) in rows.enumerate().rev() {
            // Append lines left over from the previous row.
            if let Some(buffered) = buffered.take() {
                // Add a column for every cell added before the cursor, if it goes beyond the new
                // width it is then later reflown.
                let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                if i == cursor_buffer_line {
                    self.cursor.point.column += buffered.len();
                }

                row.append_front(buffered);
            }

            loop {
                // Remove all cells which require reflowing.
                let mut wrapped = match row.shrink(columns) {
                    Some(wrapped) if reflow => wrapped,
                    _ => {
                        let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                        if reflow && i == cursor_buffer_line && self.cursor.point.column > columns {
                            // If there are empty cells before the cursor, we assume it is explicit
                            // whitespace and need to wrap it like normal content.
                            Vec::new()
                        } else {
                            // Since it fits, just push the existing line without any reflow.
                            new_raw.push(row);
                            break;
                        }
                    },
                };

                // Insert spacer if a wide char would be wrapped into the last column.
                if row.len() >= columns
                    && row[Column(columns - 1)].flags().contains(Flags::WIDE_CHAR)
                {
                    let mut spacer = T::default();
                    spacer.flags_mut().insert(Flags::LEADING_WIDE_CHAR_SPACER);

                    let wide_char = mem::replace(&mut row[Column(columns - 1)], spacer);
                    wrapped.insert(0, wide_char);
                }

                // Remove wide char spacer before shrinking.
                let len = wrapped.len();
                if len > 0 && wrapped[len - 1].flags().contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    if len == 1 {
                        row[Column(columns - 1)].flags_mut().insert(Flags::WRAPLINE);
                        new_raw.push(row);
                        break;
                    } else {
                        // Remove the leading spacer from the end of the wrapped row.
                        wrapped[len - 2].flags_mut().insert(Flags::WRAPLINE);
                        wrapped.truncate(len - 1);
                    }
                }

                new_raw.push(row);

                // Set line as wrapped if cells got removed.
                if let Some(cell) = new_raw.last_mut().and_then(|r| r.last_mut()) {
                    cell.flags_mut().insert(Flags::WRAPLINE);
                }

                if wrapped
                    .last()
                    .map(|c| c.flags().contains(Flags::WRAPLINE) && i >= 1)
                    .unwrap_or(false)
                    && wrapped.len() < columns
                {
                    // Make sure previous wrap flag doesn't linger around.
                    if let Some(cell) = wrapped.last_mut() {
                        cell.flags_mut().remove(Flags::WRAPLINE);
                    }

                    // Add removed cells to start of next row.
                    buffered = Some(wrapped);
                    break;
                } else {
                    // Reflow cursor if a line below it is deleted.
                    let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                    if (i == cursor_buffer_line && self.cursor.point.column < columns)
                        || i < cursor_buffer_line
                    {
                        self.cursor.point.line = max(self.cursor.point.line - 1, Line(0));
                    }

                    // Reflow the cursor if it is on this line beyond the width.
                    if i == cursor_buffer_line && self.cursor.point.column >= columns {
                        // Since only a single new line is created, we subtract only `columns`
                        // from the cursor instead of reflowing it completely.
                        self.cursor.point.column -= columns;
                    }

                    // Make sure new row is at least as long as new width.
                    let occ = wrapped.len();
                    if occ < columns {
                        wrapped.resize_with(columns, T::default);
                    }
                    row = Row::from_vec(wrapped, occ);

                    if i < self.display_offset {
                        // Since we added a new line, rotate up the viewport.
                        self.display_offset += 1;
                    }
                }
            }
        }

        // Reverse iterator and use it as the new grid storage.
        //
        // feathercritty: upstream collected `new_raw` reversed into `reversed`, called
        // `reversed.truncate(self.max_scroll_limit + self.lines)` and
        // `self.raw.replace_inner(reversed)`; `ReflowSink::finish` does all three.
        self.replace_rows(new_raw);

        // Clamp display offset in case some lines went off.
        self.display_offset = min(self.display_offset, self.history_size());

        // Reflow the primary cursor, or clamp it if reflow is disabled.
        if !reflow {
            self.cursor.point.column = min(self.cursor.point.column, Column(columns - 1));
        } else if self.cursor.point.column == columns
            // feathercritty: upstream `self[...]`; the cursor is on the screen.
            && !self.screen[self.cursor.point.line][Column(columns - 1)]
                .flags()
                .contains(Flags::WRAPLINE)
        {
            self.cursor.input_needs_wrap = true;
            self.cursor.point.column -= 1;
        } else {
            self.cursor.point = self.cursor.point.grid_clamp(self, Boundary::Cursor);
        }

        // Clamp the saved cursor to the grid.
        self.saved_cursor.point.column = min(self.saved_cursor.point.column, Column(columns - 1));
    }
}
