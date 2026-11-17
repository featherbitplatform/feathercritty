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
//! - **Byte copy** (design spec 5.6, phase 4): a history row the loop would push unchanged but for
//!   its width never reaches the loop; [`RowSource::next_reflow`] (which stands for upstream's
//!   `drain(..).enumerate().rev()`) moves its record to the sink with only `W` changed, under the
//!   exact conditions of [`Pass::byte_copy_width`]. Rows the source decodes go into a row the sink
//!   has finished encoding, and blocks the old history frees go to the new one, so a long pass
//!   allocates little.
//!
//! Peak memory is the old compact store plus the new one plus the window; nothing dense of
//! history size is allocated, and nothing of the pass stays resident after it.
//!
//! - **Cold blocks** (history compression design spec 5.6): the new history is built warm, with
//!   compression off (`History::sibling_warm`), so the pass does no per-row warm-row accounting;
//!   `replace_rows` then applies the old setting, which counts the warm rows once and marks the
//!   blocks for jobs, which run after the resize like any others. Nothing is compressed inline. The
//!   old history's cold blocks are read through its read cache as `RowSource` pops them (byte copy
//!   included) and freed as their last row leaves. The new history has a new identity and the
//!   grid's `history_epoch` is bumped, so jobs taken before the pass are refused.
//!
//! When merging upstream: if upstream's reflow starts using another `Vec` operation on these
//! containers (indexing a row other than the newest, `insert`, `retain`, ...), add it to the
//! sink with the same meaning, decoding rows back when they already left the window, and extend
//! `reflow_window_one_matches_default` to reach it.

use std::cmp::{Ordering, max, min};
use std::collections::{HashMap, VecDeque};
use std::mem;

use crate::index::{Boundary, Column, Line};
use crate::term::cell::{Flags, ResetDiscriminant};

use crate::grid::history::History;
use crate::grid::row::Row;
use crate::grid::storage::Storage;
use crate::grid::{Dimensions, Grid, HistoryCell, ReflowInfo};

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

/// What the reflow loop does with a row, as far as the byte-copy fast path needs to know
/// (feathercritty, see [`RowSource::next_reflow`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pass {
    /// `grow_columns` to `columns` columns.
    Grow { columns: usize, reflow: bool },

    /// `shrink_columns` to `columns` columns; `carried` when cells wrapped off the previous row
    /// wait to be prepended to the next one (upstream's `buffered`).
    Shrink { columns: usize, reflow: bool, carried: bool },
}

impl Pass {
    /// The width a history row described by `info` gets when the reflow loop would push it
    /// unchanged but for its width, so its record can be copied with only `W` changed; `None`
    /// when the loop may change it otherwise (see `RowSource::next_reflow` for the conditions).
    /// `newest_takes_cells(reflow, columns)` is upstream's `should_reflow` on the row before.
    pub(crate) fn byte_copy_width(
        self,
        info: &ReflowInfo,
        newest_takes_cells: impl FnOnce(bool, usize) -> bool,
    ) -> Option<usize> {
        match self {
            Pass::Grow { columns, reflow } => (info.tail_default
                && !newest_takes_cells(reflow, columns))
            .then_some(max(info.width, columns)),
            Pass::Shrink { carried: true, .. } => None,
            Pass::Shrink { columns, .. } if info.width <= columns => Some(info.width),
            Pass::Shrink { columns, reflow, carried: false } => {
                (info.stored < columns && (info.tail_empty || !reflow)).then_some(columns)
            },
        }
    }
}

impl<T: HistoryCell> RowSource<'_, T> {
    /// The next row for the reflow loop with its index, as upstream's
    /// `rows.drain(..).enumerate().rev()` yields it (feathercritty).
    ///
    /// Before that, history rows the loop would leave untouched go straight to `sink` as bytes
    /// (the byte-copy fast path of design spec 5.6): their record is copied with only the width
    /// changed (`History::move_oldest_to`), never decoded or re-encoded, and the loop never sees
    /// them. The index of the row returned stays upstream's (the rows left after it), so the
    /// rows passed over are accounted for. A history row is passed over when
    /// (`HistoryCell::reflow_info`):
    ///
    /// - **growing**: its tail is the default cell (growing appends default cells, so only `W`
    ///   changes, and the row cannot wrap into the next one), and the row before it would not take
    ///   cells from it (upstream's `should_reflow` on the sink's newest row is false), so the loop
    ///   would push it unchanged;
    /// - **shrinking**: no cells are carried over from the previous row, and either the row is not
    ///   wider than `columns` (it is pushed as is) or every cell it loses is a tail cell (`P <
    ///   columns`) and, when reflowing, empty (so `Row::shrink` returns nothing and the loop pushes
    ///   the cut row). History rows are never the cursor's line, the one other case the loop treats
    ///   apart.
    ///
    /// These conditions are a correct superset of the text of design spec 5.6, each refinement
    /// taken from upstream's loop: shrinking accepts any empty tail (`GridCell::is_empty`, for
    /// example a `'\t'` tail or a space with `BOLD`, exactly what `Row::shrink` drops), not only
    /// the default one; shrinking without reflow (`!reflow`, the `_` arm that pushes the cut row
    /// either way) copies too; and growing uses upstream's whole `should_reflow` (`reflow`,
    /// `len < columns` and a wrapping last cell), not only "the previous row does not wrap".
    ///
    /// The result is exactly upstream's: such a row's record with `W` changed is the record of
    /// the row the loop would have pushed (`byte_copy_matches_decode_and_encode`), and the
    /// differential test runs it with a reflow window of 1. When merging upstream: if the loop
    /// starts doing anything else to a row it pushes unchanged, narrow these conditions.
    pub(crate) fn next_reflow(
        &mut self,
        sink: &mut ReflowSink<T>,
        pass: Pass,
        copied: &mut dyn FnMut(usize, usize, bool),
    ) -> Option<(usize, Row<T>)> {
        while sink.byte_copy {
            let Some(info) = self.history.oldest_reflow_info() else {
                break;
            };
            let takes_cells = |reflow, columns| sink.newest_takes_cells(reflow, columns);
            let Some(width) = pass.byte_copy_width(&info, takes_cells) else {
                break;
            };

            // feathercritty: report each copied row to `copied(i, stream, dropped)` — the
            // caller's anchor hook (see `AnchorTracker::byte_copied`); `i` is upstream's index
            // for it (rows remaining after this one, counted from the bottom line), matching
            // what the decoded path below returns; `dropped` is true when the push evicted the
            // row at the history limit.
            let evicted_before = sink.evicted;
            sink.push_moved(self.history, width);
            copied(self.len(), sink.stream_len() - 1, sink.evicted > evicted_before);
            if self.history.is_empty() {
                self.history.clear();
            }
        }

        // The oldest row left, as `next_back`, but decoded into a row the sink is done with.
        let row = if self.history.is_empty() {
            self.screen.next_back()?
        } else {
            let mut row = sink.spare_row.take().unwrap_or_default();
            self.history.pop_oldest(&mut row);
            if self.history.is_empty() {
                self.history.clear();
            }
            sink.history.take_spares(self.history);
            row
        };
        Some((self.len(), row))
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

    /// How many rows the history evicted at its limit (checked against `truncate_newest`, see
    /// above; also counted by `stream_len` so anchor positions stay in produced-row coordinates).
    evicted: usize,

    /// Whether `RowSource::next_reflow` may copy records instead of decoding them (always,
    /// unless a test turns it off to compare, `Grid::set_reflow_byte_copy`).
    pub(crate) byte_copy: bool,
    /// A row encoded into the history and no longer needed, for `RowSource::next_reflow` to
    /// decode the next history row into (saves an allocation per row).
    spare_row: Option<Row<T>>,
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
            evicted: 0,
            byte_copy: true,
            spare_row: None,
        }
    }

    /// Whether upstream's `grow_columns` would move cells of the next row into the newest row
    /// (its `should_reflow` on `last_mut()`), read from the header when the newest row is
    /// already encoded.
    ///
    /// The header branch reads the width the row was stored with, which is the grown width
    /// (`store` grows rows to `grow_to` first), not the length upstream's `last_mut()` would see.
    /// That is only correct because the window is empty inside the loop solely right after
    /// `push_moved`, so the encoded newest row is a byte-copied row: its tail is the default cell,
    /// so it never wraps and the answer is `false` whatever its width. The `debug_assert!` holds
    /// that invariant; if a change empties the window inside the loop with a decoded row at the
    /// newest position (for example a `truncate_newest` inside the loop after an upstream merge),
    /// decode that row back (`pull_back`) instead of reading its header.
    fn newest_takes_cells(&self, reflow: bool, columns: usize) -> bool {
        let (len, wraps) = match self.dense.back() {
            Some(row) => (row.len(), T::row_wraps(row)),
            None => match self.history.newest_header() {
                Some(header) => {
                    debug_assert!(
                        !header.wraps,
                        "the encoded newest row of a reflow sink is a byte-copied row, which \
                         never wraps"
                    );
                    (header.width as usize, header.wraps)
                },
                None => return false,
            },
        };
        reflow && len > 0 && len < columns && wraps
    }

    /// Append `source`'s oldest row as the newest row without decoding it, with its width changed
    /// to `width` (see `RowSource::next_reflow`). The dense rows are encoded first, so the rows
    /// stay in order.
    fn push_moved(&mut self, source: &mut History<T>, width: usize) {
        while let Some(row) = self.dense.pop_front() {
            self.store(row);
        }
        if self.history.len() >= self.history.limit() {
            self.evicted += 1;
        }
        source.move_oldest_to(&mut self.history, width);
        self.history.take_spares(source);
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

    /// Rows produced including those already evicted at the limit — stream coordinates for
    /// feathercritty's anchor tracking (see `AnchorTracker`).
    pub(crate) fn stream_len(&self) -> usize {
        self.len() + self.evicted
    }

    /// Keep the oldest `len` rows, dropping the newest ones (upstream `Vec::truncate`).
    pub(crate) fn truncate_newest(&mut self, len: usize) {
        debug_assert!(self.evicted == 0 || self.len() <= len, "truncating after an eviction");
        while self.len() > len {
            if self.dense.pop_back().is_none() {
                self.history.remove_newest();
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
        self.history.trim_to_limit(max_scroll_limit);

        let screen = dense.into_iter().rev().map(|row| self.grown(row)).collect();
        (Storage::from_rows(screen), self.history)
    }

    /// Encode `row` as the newest history row.
    fn store(&mut self, row: Row<T>) {
        if self.history.len() >= self.history.limit() {
            self.evicted += 1;
        }
        let row = self.grown(row);
        self.history.push_newest(&row);
        self.spare_row = Some(row);
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
    /// limit, block size and compression (`History::sibling`), so `history.limit()` and
    /// `Dimensions` keep working during the pass. Allocates nothing.
    fn take_history(&mut self) -> History<T> {
        let empty = self.history.sibling(self.history.limit());
        mem::replace(&mut self.history, empty)
    }

    /// The sink of a reflow (feathercritty): a window of `2 * lines + 2` rows (the screen, as
    /// many rows `grow_columns` may truncate, and slack), or the test hook's window.
    fn reflow_sink(&self, grow_to: Option<usize>) -> ReflowSink<T> {
        let window = 2 * self.lines + 2;
        #[cfg(any(test, feature = "test-hooks"))]
        let window = self.reflow_window.unwrap_or(window);

        let limit = self.history.limit() + self.lines;
        #[allow(unused_mut)]
        let mut sink = ReflowSink::new(window, grow_to, self.history.sibling_warm(limit));
        #[cfg(any(test, feature = "test-hooks"))]
        {
            sink.byte_copy = self.reflow_byte_copy;
        }
        sink
    }

    /// Store a reflow's rows as the new screen and history (feathercritty: upstream
    /// `self.raw.replace_inner`), then resolve the anchor tracker's outcomes against the result
    /// and hand it back (see `resize_tracked`; `None` when there was nothing to track).
    fn replace_rows(
        &mut self,
        rows: ReflowSink<T>,
        pending: Option<AnchorTracker>,
    ) -> Option<AnchorTracker> {
        let produced = rows.stream_len();
        let (screen, history) = rows.finish(self.lines, self.history.limit());
        let compression = self.history.compression();
        let kept = history.len() + self.lines;
        self.screen.raw = screen;
        self.history = history;
        self.history.set_compression(compression);
        self.history_epoch += 1;
        debug_assert_eq!(self.screen.raw.len(), self.lines);
        let dropped_oldest = produced.saturating_sub(kept);
        if let Some(mut pending) = pending {
            pending.resolve(self.scrolled_lines, self.lines, produced, dropped_oldest);
            Some(pending)
        } else {
            None
        }
    }
}

// feathercritty: upstream's bound was `T: GridCell + Default + PartialEq`, which `HistoryCell`
// includes.
// feathercritty: upstream's bound was `T: GridCell + Default + PartialEq`, which `HistoryCell`
// includes.
impl<T: HistoryCell> Grid<T> {
    /// Resize the grid's width and/or height.
    pub fn resize<D>(&mut self, reflow: bool, lines: usize, columns: usize)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        self.resize_impl::<D>(reflow, lines, columns, None);
    }

    /// Resize, reporting where anchor cells went (feathercritty; not upstream).
    ///
    /// `anchors` are `(stable line, column)` cells on this grid before the resize — hosts pin
    /// image placements to text with these. A row-only resize keeps every stable line, so those
    /// outcomes echo the inputs. A column resize maps each anchor through the reflow exactly as
    /// the loop moves content: the outcome is `(new stable line, new column, false)` when the
    /// cell followed its row's content, or `(line, column, true)` when it cannot follow — its
    /// cells were cut at the new width (no reflow), its line was merged away, or its content was
    /// destroyed by the height shrink — with the line nearest where it used to sit, so the caller
    /// can mark its place instead of losing it silently. Outcomes come out in input order and
    /// are appended to `out` (which must start empty for them).
    pub fn resize_tracked<D>(
        &mut self,
        reflow: bool,
        lines: usize,
        columns: usize,
        anchors: &[(i64, usize)],
        out: &mut Vec<(i64, usize, bool)>,
    ) where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        debug_assert_eq!(out.len(), 0);
        // The tracking state borrows `anchors`/`out` only for the duration of this call; the
        // `'static` cast keeps its type simple (neither buffer escapes through the grid).
        let track = (!anchors.is_empty()).then(|| ReflowTracking {
            anchors: anchors.to_vec(),
            // SAFETY: `track`, and everything that reads `out` through it, is dropped before
            // this returns.
            out: unsafe { &mut *(&mut *out as *mut Vec<(i64, usize, bool)>) },
            pending: None,
        });
        self.resize_impl::<D>(reflow, lines, columns, track);
        debug_assert_eq!(out.len(), anchors.len());
    }

    fn resize_impl<D>(
        &mut self,
        reflow: bool,
        lines: usize,
        columns: usize,
        mut track: Option<ReflowTracking>,
    ) where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // Use empty template cell for resetting cells due to resize.
        let template = mem::take(&mut self.cursor.template);

        match self.lines.cmp(&lines) {
            Ordering::Less => self.grow_lines(lines),
            Ordering::Greater => self.shrink_lines::<D>(lines, &mut track),
            Ordering::Equal => (),
        }

        match self.columns.cmp(&columns) {
            // feathercritty: only a column pass moves content between rows; a row-only resize
            // keeps stable lines, so its anchors are answered with their inputs.
            Ordering::Less => {
                let mut tracker = track.as_mut().map(|track| {
                    track.lend(self.scrolled_lines, self.lines, columns, |_| ());
                    track.pending.take().expect("just lent")
                });
                self.grow_columns(reflow, columns, &mut tracker);
                if let Some(track) = track.as_mut() {
                    if let Some(tracker) = tracker.take() {
                        track.absorb(tracker);
                    }
                }
            },
            Ordering::Greater => {
                let mut tracker = track.as_mut().map(|track| {
                    track.lend(self.scrolled_lines, self.lines, columns, |_| ());
                    track.pending.take().expect("just lent")
                });
                self.shrink_columns(reflow, columns, &mut tracker);
                if let Some(track) = track.as_mut() {
                    if let Some(tracker) = tracker.take() {
                        track.absorb(tracker);
                    }
                }
            },
            Ordering::Equal => {
                if let Some(track) = &mut track {
                    track.fill_identity();
                }
            },
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
            // feathercritty: no anchor observer here. This rotation only runs with an empty
            // scrolled-back view (the history was shorter than the growth), where every buffer
            // index a surviving row holds is unchanged by it — `resize_tracked`'s index map
            // stays correct without a report.
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
    fn shrink_lines<D>(&mut self, target: usize, track: &mut Option<ReflowTracking>)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // Scroll up to keep content inside the window.
        let required_scrolling = (self.cursor.point.line.0 as usize + 1).saturating_sub(target);
        if required_scrolling > 0 {
            // feathercritty: this scroll destroys the viewport's top rows and rotates survivors
            // into history; `resize_tracked`'s tracker remaps its anchors to match, as every
            // other caller of `scroll_up` keeps placements in step.
            match track {
                Some(track) => {
                    let scrolled = self.scrolled_lines;
                    let lines = self.lines;
                    track.lend(scrolled, lines, self.columns, |t| {
                        self.scroll_up_observed(
                            &(Line(0)..Line(self.lines as i32)),
                            required_scrolling,
                            &mut |positions: usize, evicted: usize, history_before: usize| {
                                t.scroll_up(positions, evicted, history_before);
                            },
                        );
                    });
                },
                None => self.scroll_up(&(Line(0)..Line(self.lines as i32)), required_scrolling),
            }

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
    ///
    /// `track` carries the anchor tracker built by `resize_impl` (feathercritty; see
    /// `resize_tracked`); its state threads through the loop and it is resolved in
    /// `replace_rows`.
    fn grow_columns(&mut self, reflow: bool, columns: usize, track: &mut Option<AnchorTracker>) {
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

        // feathercritty: upstream `self.raw.take_all()`, then `for (i, mut row) in
        // rows.drain(..).enumerate().rev()`; see `RowSource::next_reflow`, which also passes
        // rows this loop would push unchanged straight to `reversed` as bytes.
        let mut history = self.take_history();
        let mut rows = RowSource::new(&mut history, self.screen.raw.take_all());

        while let Some((i, mut row)) = rows.next_reflow(
            &mut reversed,
            Pass::Grow { columns, reflow },
            // feathercritty: byte-copied rows are reported through the tracker (see
            // `RowSource::next_reflow`).
            &mut |i, stream, dropped| {
                if let Some(track) = track.as_mut() {
                    track.byte_copied(i, stream, dropped, Pass::Grow { columns, reflow });
                }
            },
        ) {
            // feathercritty: the stream position of the row `last_row` sits at (the sink's
            // newest row), captured before any borrow it is read under.
            let last_stream = reversed.stream_len().wrapping_sub(1);

            // Check if reflowing should be performed.
            let last_row = match reversed.last_mut() {
                Some(last_row) if should_reflow(last_row) => last_row,
                _ => {
                    // feathercritty: pushed whole at its own stream position: anchors keep
                    // their columns.
                    if let Some(track) = track.as_mut() {
                        let stream = reversed.stream_len();
                        track.piece_pushed(i, stream, usize::MAX, row.len(), false);
                    }
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

            // feathercritty: the tail cell this merge pulls, if a wide char, keeps its visual
            // column (it moves to the target row's end unchanged); see `AnchorTracker::merge`.
            let wide_tail = row[Column(len - 1)].flags().contains(Flags::WIDE_CHAR);

            // Insert leading spacer when there's not enough room for reflowing wide char.
            let mut cells = if wide_tail {
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

            // feathercritty: `pulled` real cells moved from this row's front to the target
            // piece's end, landing at columns shifted by `last_len`; a wide tail cell stays on
            // this row (its spacer went to the target) and reappears at the rest's column 0.
            let pulled = len - usize::from(wide_tail);
            if let Some(track) = track.as_mut() {
                track.merge(i, last_stream, pulled, last_len);
            }

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
                    // feathercritty: the rest of this row (blanks) is dropped.
                    if let Some(track) = track.as_mut() {
                        track.row_dropped(i, last_stream, last_len, last_len + pulled);
                    }
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

                // feathercritty: the rest of this row (blanks) is dropped.
                if let Some(track) = track.as_mut() {
                    track.row_dropped(i, last_stream, last_len, last_len + pulled);
                }

                // Don't push line into the new buffer.
                continue;
            }

            if let Some(cell) = last_row.last_mut() {
                // Set wrap flag if next line still has cells.
                cell.flags_mut().insert(Flags::WRAPLINE);
            }

            // feathercritty: the rest of this row becomes its own row after the merged part.
            let stream = reversed.stream_len();
            if let Some(track) = track.as_mut() {
                track.piece_pushed(i, stream, usize::MAX, row.len(), false);
            }
            reversed.push(row);
        }

        // Make sure we have at least the viewport filled.
        if reversed.len() < self.lines {
            let delta = (self.lines - reversed.len()) as i32;
            self.cursor.point.line = max(self.cursor.point.line - delta, Line(0));
            // feathercritty: upstream `reversed.resize_with(self.lines, || Row::new(columns))`.
            // Blank filler rows carry no anchors.
            reversed.resize_with_blank(self.lines, columns, &T::default());
        }

        // Pull content down to put cursor in correct position, or move cursor up if there's no
        // more lines to delete below the cursor.
        if cursor_line_delta != 0 {
            let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
            let available = min(cursor_buffer_line, reversed.len() - self.lines);
            let overflow = cursor_line_delta.saturating_sub(available);
            // feathercritty: upstream `reversed.truncate`. Rows dropped here resolve their
            // anchors nearest the last surviving row.
            let kept = reversed.len() + overflow - cursor_line_delta;
            if let Some(track) = track.as_mut() {
                track.truncated(kept, reversed.stream_len().saturating_sub(kept));
            }
            reversed.truncate_newest(kept);
            self.cursor.point.line = max(self.cursor.point.line - overflow, Line(0));
        }

        // Reverse iterator and fill all rows that are still too short.
        //
        // feathercritty: upstream reversed `reversed` into `new_raw`, growing every row narrower
        // than `columns`, then called `self.raw.replace_inner(new_raw)`. The sink grows each row
        // as it leaves the window and in `finish`, which also reverses the rows into the screen
        // and the history (no trim here: `grow_columns` never has more rows than it started
        // with).
        *track = self.replace_rows(reversed, track.take());

        // Clamp display offset in case lines above it got merged.
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Shrink number of columns in each row, reflowing if necessary.
    ///
    /// `track` is the anchor tracker (see `grow_columns`).
    fn shrink_columns(&mut self, reflow: bool, columns: usize, track: &mut Option<AnchorTracker>) {
        self.columns = columns;

        // Remove the linewrap special case, by moving the cursor outside of the grid.
        if self.cursor.input_needs_wrap && reflow {
            self.cursor.input_needs_wrap = false;
            self.cursor.point.column += 1;
        }

        // feathercritty: upstream `Vec::with_capacity(self.raw.len())`; see `ReflowSink`.
        let mut new_raw = self.reflow_sink(None);
        let mut buffered: Option<Vec<T>> = None;
        // feathercritty: anchors of the cells in `buffered`, carried to the row they prepend to.
        let mut carry: Vec<usize> = Vec::new();

        // feathercritty: upstream `self.raw.take_all()`, then `for (i, mut row) in
        // rows.drain(..).enumerate().rev()`; see `RowSource::next_reflow`, which also passes
        // rows this loop would push unchanged straight to `new_raw` as bytes.
        let mut history = self.take_history();
        let mut rows = RowSource::new(&mut history, self.screen.raw.take_all());
        while let Some((i, mut row)) = rows.next_reflow(
            &mut new_raw,
            Pass::Shrink { columns, reflow, carried: buffered.is_some() },
            // feathercritty: byte-copied rows are reported through the tracker (see
            // `RowSource::next_reflow`).
            &mut |i, stream, dropped| {
                if let Some(track) = track.as_mut() {
                    track.byte_copied(i, stream, dropped, Pass::Shrink {
                        columns,
                        reflow,
                        carried: false,
                    });
                }
            },
        ) {
            // Append lines left over from the previous row.
            if let Some(buffered) = buffered.take() {
                // Add a column for every cell added before the cursor, if it goes beyond the new
                // width it is then later reflown.
                let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                if i == cursor_buffer_line {
                    self.cursor.point.column += buffered.len();
                }

                // feathercritty: prepending shifts this row's cells right by the carried
                // cells' count and adopts their anchors (the carried indices re-key to this
                // row). The shift applies whether or not any anchor rode along.
                let n = buffered.len();
                if let Some(track) = track.as_mut() {
                    track.prepend(i, n, &carry);
                }
                carry.clear();
                row.append_front(buffered);
            }

            // feathercritty: the stream position new rows land at, captured outside the loop's
            // borrows of `new_raw`.
            let mut stream = new_raw.stream_len();

            loop {
                // feathercritty: this row's content end for the anchor hooks' "no cell there"
                // guard (see `AnchorTracker::piece_committed`): rows are padded to full width,
                // so its trailing empty cells do not count — they are what `Row::shrink` drops.
                let mut content_end = row.len();
                while content_end > 0 && row[Column(content_end - 1)].is_empty() {
                    content_end -= 1;
                }

                // Remove all cells which require reflowing.
                // feathercritty: `shrunk`/`truncated` are read for the anchor hooks below;
                // computing them here does not change what the loop does.
                let shrunk = row.shrink(columns);
                let truncated = shrunk.is_some();
                let mut wrapped = match shrunk {
                    Some(wrapped) if reflow => wrapped,
                    _ => {
                        let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                        if reflow && i == cursor_buffer_line && self.cursor.point.column > columns {
                            // If there are empty cells before the cursor, we assume it is explicit
                            // whitespace and need to wrap it like normal content.
                            Vec::new()
                        } else {
                            // Since it fits, just push the existing line without any reflow.
                            // feathercritty: with reflow off a wider row's tail was already cut
                            // and discarded (`truncated`), so its anchors cannot follow; with
                            // reflow on nothing was cut and everything stays.
                            if let Some(track) = track.as_mut() {
                                let moved_from = if truncated { columns } else { usize::MAX };
                                track.piece_pushed(i, stream, moved_from, content_end, false);
                            }
                            new_raw.push(row);
                            break;
                        }
                    },
                };

                // Insert spacer if a wide char would be wrapped into the last column.
                // feathercritty: read before the mutable borrow (see `AnchorTracker` hooks).
                let wide_at_edge = row.len() >= columns
                    && row[Column(columns - 1)].flags().contains(Flags::WIDE_CHAR);
                if wide_at_edge {
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
                        // feathercritty: the only wrapped cell is the drained spacer at this
                        // row's old end — anchors up to `columns` stay, the spacer's cannot.
                        if let Some(track) = track.as_mut() {
                            track.piece_pushed(i, stream, columns, content_end, false);
                        }
                        new_raw.push(row);
                        break;
                    } else {
                        // Remove the leading spacer from the end of the wrapped row.
                        wrapped[len - 2].flags_mut().insert(Flags::WRAPLINE);
                        wrapped.truncate(len - 1);
                    }
                }

                // feathercritty: the boundary between cells that stay on this row and cells
                // that follow `wrapped`: with a spacer inserted, the wide char at the edge is
                // in `wrapped` (an anchor on it follows the character to the run's column 0).
                let moved_from = columns - usize::from(wide_at_edge);

                new_raw.push(row);
                stream = new_raw.stream_len();

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

                    // feathercritty: the pushed row is final; its cut cells wait in `buffered`
                    // with their anchors, which prepend to the next row.
                    if let Some(track) = track.as_mut() {
                        carry = track.piece_pushed(i, stream - 1, moved_from, content_end, true);
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

                    // feathercritty: cells up to `moved_from` are final on the pushed row; the
                    // rest continues as a new row with their anchors (they stay pending on the
                    // row, at columns relative to the run that follows — the loop iterates
                    // again for them).
                    if let Some(track) = track.as_mut() {
                        track.piece_committed(i, stream - 1, moved_from, content_end, true);
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

        // feathercritty: cells still buffered after the last row have no older row to prepend
        // into; their anchors cannot follow and resolve nearest their last piece.
        if !carry.is_empty() {
            if let Some(track) = track.as_mut() {
                track.stash_final(&carry);
            }
        }

        // Reverse iterator and use it as the new grid storage.
        //
        // feathercritty: upstream collected `new_raw` reversed into `reversed`, called
        // `reversed.truncate(self.max_scroll_limit + self.lines)` and
        // `self.raw.replace_inner(reversed)`; `ReflowSink::finish` does all three.
        *track = self.replace_rows(new_raw, track.take());

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
// --- anchor tracking (feathercritty; not upstream) -------------------------------------------

/// The anchor list and outcome buffer of one [`Grid::resize_tracked`] call.
struct ReflowTracking {
    anchors: Vec<(i64, usize)>,
    out: &'static mut Vec<(i64, usize, bool)>,
    /// Built on first use while the resize runs; `shrink_lines` lends it to its scroll report
    /// and a column pass takes it out and resolves it in `replace_rows`.
    pending: Option<AnchorTracker>,
}

impl ReflowTracking {
    /// Run `f` with the tracker, building it from the grid's current state on first use
    /// (`scrolled`/`lines` are the grid's values now; a row at buffer index `i`, 0 oldest, sits
    /// at stable `scrolled + lines - 1 - i`).
    fn lend(
        &mut self,
        scrolled: i64,
        lines: usize,
        columns: usize,
        f: impl FnOnce(&mut AnchorTracker),
    ) {
        if self.pending.is_none() {
            let mut slots: Vec<Option<Slot>> = (0..self.anchors.len()).map(|_| None).collect();
            let mut rows: HashMap<usize, Vec<usize>> = HashMap::new();
            for (index, &(stable, col)) in self.anchors.iter().enumerate() {
                let i = (scrolled + lines as i64 - 1 - stable).max(0) as usize;
                slots[index] = Some(Slot::Pending { i, col });
                rows.entry(i).or_default().push(index);
            }
            self.pending = Some(AnchorTracker {
                scrolled,
                lines,
                columns,
                slots,
                rows,
                last_piece: HashMap::new(),
            });
        }
        // A later piece of the same resize (the column pass after a row pass): rebase the
        // buffer-index coordinates onto the grid's current values; pending anchors move to
        // their rows' new indices.
        if let Some(tracker) = self.pending.as_mut() {
            if tracker.scrolled != scrolled || tracker.lines != lines {
                let delta =
                    (scrolled + lines as i64 - 1) - (tracker.scrolled + tracker.lines as i64 - 1);
                let keys: Vec<usize> = tracker.rows.keys().copied().collect();
                for old_i in keys {
                    let indices = tracker.rows.remove(&old_i).expect("key just listed");
                    for &index in &indices {
                        if let Some(Slot::Pending { i, col }) = tracker.slots[index] {
                            let i = (i as i64 + delta).max(0) as usize;
                            tracker.slots[index] = Some(Slot::Pending { i, col });
                        }
                    }
                    tracker.rows.insert((old_i as i64 + delta).max(0) as usize, indices);
                }
                tracker.scrolled = scrolled;
                tracker.lines = lines;
            }
            tracker.columns = columns;
            f(tracker);
        }
    }

    /// Answer every anchor with its input (a resize that did not reflow: stable lines survive
    /// it; scrolls and destruction were already resolved through the tracker), in input order.
    fn fill_identity(&mut self) {
        match self.pending.take() {
            Some(tracker) => self.absorb(tracker),
            None => self.out.extend(self.anchors.iter().map(|&(stable, col)| (stable, col, false))),
        }
    }

    /// Append a resolved tracker's outcomes to `out` in input order.
    fn absorb(&mut self, tracker: AnchorTracker) {
        let out = &mut *self.out;
        tracker.collect_into(&self.anchors, out);
    }
}

/// Where an anchor cell's content went, fed hook by hook from the reflow loop's own decisions
/// (see [`Grid::resize_tracked`]). The tracker cannot desync from the grid: every decision that
/// moves a cell has a hook, and anchors for rows the loop never touches keep their stable line.
pub(crate) struct AnchorTracker {
    /// Buffer-index coordinates at construction: a row at buffer index `i` (0 oldest) sat at
    /// stable `scrolled + lines_before - 1 - i`; stream-keyed slots resolve through it once the
    /// pass ends (`scroll_up` keeps that mapping current when a row pass moved content).
    scrolled: i64,
    /// The grid's height when the tracker was built (row passes run before any hook needs
    /// this field, so it stays current).
    lines: usize,
    columns: usize,
    /// One slot per anchor, in input order.
    slots: Vec<Option<Slot>>,
    /// Anchor indices per buffer index; `scroll_up` moves whole rows by re-keying this map.
    rows: HashMap<usize, Vec<usize>>,
    /// Each row's last surviving piece `(stream position, length)`: anchors that cannot follow
    /// resolve nearest it.
    last_piece: HashMap<usize, (usize, usize)>,
}

/// One anchor's state mid-resize. A `line` marked `stream_keyed` holds a produced-row stream
/// position that [`AnchorTracker::resolve`] maps onto a stable line once the pass ends; an
/// unmarked one is already final.
#[derive(Clone, Copy)]
enum Slot {
    /// Cells still in play: buffer index of the row holding them, column within their piece.
    Pending { i: usize, col: usize },
    /// Followed its content (line stream-keyed until `resolve`).
    Placed { line: i64, col: usize },
    /// Cannot follow; `line`/`col` are the nearest surviving cell — its own cell when the row's
    /// content was destroyed outright (then never stream-keyed).
    Lost { line: i64, col: usize, stream_keyed: bool },
}

impl AnchorTracker {
    /// [`Grid::scroll_up_observed`]'s report (see there for the arguments); only whole-viewport
    /// rotations arrive. The rotation moves every surviving row up by `positions` lines — its
    /// buffer index (upstream's reflow numbering, distance from the bottom line) grows by that
    /// much whatever end of the buffer it sits at — and evicts the `evicted` oldest rows: those
    /// whose index reached `lines + history_before - evicted`. Their anchors cannot follow and
    /// report their own cell (the caller makes a placeholder or a removal from it). `self`'s
    /// coordinates are the grid's before the rotation (see `ReflowTracking::lend`).
    fn scroll_up(&mut self, positions: usize, evicted: usize, history_before: usize) {
        if self.rows.is_empty() {
            return;
        }

        if evicted > 0 {
            let doomed_base = self.lines + history_before.saturating_sub(evicted);
            let doomed: Vec<(usize, Vec<usize>)> = self
                .rows
                .iter()
                .filter(|&(&i, _)| i >= doomed_base)
                .map(|(&i, indices)| (i, indices.clone()))
                .collect();
            for (i, indices) in doomed {
                let stable = self.scrolled + self.lines as i64 - 1 - i as i64;
                for &index in &indices {
                    if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                        self.slots[index] =
                            Some(Slot::Lost { line: stable, col, stream_keyed: false });
                    }
                }
                self.rows.remove(&i);
            }
        }

        let keys: Vec<usize> = self.rows.keys().copied().collect();
        for old_i in keys {
            let indices = self.rows.remove(&old_i).expect("key just listed");
            for &index in &indices {
                if let Some(Slot::Pending { i, col }) = self.slots[index] {
                    self.slots[index] = Some(Slot::Pending { i: i + positions, col });
                }
            }
            self.rows.insert(old_i + positions, indices);
        }
    }

    /// A history row moved to stream position `stream` as bytes, unchanged except its recorded
    /// width (see `RowSource::next_reflow`): nothing merged into it and nothing will (a copied
    /// row never wraps into the next one), so its anchors keep their columns at that row. Two
    /// exceptions: a copy evicted at the history limit (`dropped_row`) loses its content
    /// outright, and shrinking keeps only the columns below the new width — those anchors clamp
    /// to the last visible column of their (still present) line.
    fn byte_copied(&mut self, i: usize, stream: usize, dropped_row: bool, pass: Pass) {
        let Some(indices) = self.rows.remove(&i) else {
            return;
        };
        for index in indices {
            match self.slots[index] {
                Some(Slot::Pending { col, .. }) => {
                    if dropped_row {
                        // The copy itself was evicted: its line leaves the buffer, and the
                        // placeholder or prune the caller derives from this verdict replaces it.
                        let stable = self.scrolled + self.lines as i64 - 1 - i as i64;
                        self.slots[index] =
                            Some(Slot::Lost { line: stable, col, stream_keyed: false });
                    } else {
                        match pass {
                            Pass::Grow { .. } => {
                                self.slots[index] = Some(Slot::Placed { line: stream as i64, col });
                            },
                            Pass::Shrink { columns, .. } if col < columns => {
                                self.slots[index] = Some(Slot::Placed { line: stream as i64, col });
                            },
                            Pass::Shrink { columns, .. } => {
                                // The record keeps cells the grid will not show; the anchor
                                // clamps to the last visible column of its (still present) line.
                                let stable = self.scrolled + self.lines as i64 - 1 - i as i64;
                                self.slots[index] = Some(Slot::Lost {
                                    line: stable,
                                    col: columns - 1,
                                    stream_keyed: false,
                                });
                            },
                        }
                    }
                },
                // A scroll report already resolved this anchor (an evicted row whose index a
                // survivor now holds): leave the verdict.
                Some(Slot::Lost { .. }) | None => {},
                Some(Slot::Placed { .. }) => {},
            }
        }
    }

    /// Cells `[0, pulled)` of row `i` were appended to the piece at stream position `target`,
    /// landing after its `last_len` cells; a wide tail cell stays on this row (only its spacer
    /// went) and reappears at the rest's column 0. The rest of the row follows separately, at
    /// columns shifted left by `pulled`.
    fn merge(&mut self, i: usize, target: usize, pulled: usize, last_len: usize) {
        let Some(indices) = self.rows.remove(&i) else {
            return;
        };
        for &index in &indices {
            if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                self.slots[index] = if col < pulled {
                    Some(Slot::Placed { line: target as i64, col: col + last_len })
                } else {
                    Some(Slot::Pending { i, col: col - pulled })
                };
            }
        }
        self.rows.insert(i, indices);
        self.last_piece.insert(i, (target, last_len + pulled));
    }

    /// The `n` cells carried from a newer row were prepended to row `i`: the carried anchors
    /// re-key to this row keeping their columns at the piece's start, and this row's own cells
    /// shift right by `n`.
    fn prepend(&mut self, i: usize, n: usize, carried: &[usize]) {
        if let Some(indices) = self.rows.get_mut(&i) {
            for &index in indices.iter() {
                if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                    self.slots[index] = Some(Slot::Pending { i, col: col + n });
                }
            }
        }
        for &index in carried {
            // The anchor rode in under its source row's key; move it to this row's.
            for indices in self.rows.values_mut() {
                indices.retain(|&other| other != index);
            }
            if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                self.slots[index] = Some(Slot::Pending { i, col });
            }
            self.rows.entry(i).or_default().push(index);
        }
    }

    /// Row `i`'s current piece `[0, moved_from)` was pushed at stream position `stream` and is
    /// final: its anchors follow. The cells from `moved_from` on went to the cut run; their
    /// anchor indices are returned so the caller can carry them (to `buffered`). They stay
    /// pending at columns relative to the run's start until [`prepend`](Self::prepend) re-keys
    /// them to the row they land on. `carries` is false when the cut cells were discarded
    /// (shrinking without reflow): their anchors cannot follow. `content_end` is the row's
    /// content length before the cut: an anchor at or beyond it, and beyond the new width, sits
    /// on no cell that moves and cannot follow either.
    fn piece_pushed(
        &mut self,
        i: usize,
        stream: usize,
        moved_from: usize,
        content_end: usize,
        carries: bool,
    ) -> Vec<usize> {
        self.piece_committed(i, stream, moved_from, content_end, carries)
    }

    /// [`piece_pushed`](Self::piece_pushed) for a push whose cut run continues as another row
    /// over the same `i` in the loop: identical, only the carried indices are not needed.
    fn piece_committed(
        &mut self,
        i: usize,
        stream: usize,
        moved_from: usize,
        content_end: usize,
        carries: bool,
    ) -> Vec<usize> {
        let mut carried = Vec::new();
        if let Some(indices) = self.rows.get(&i) {
            for &index in indices.iter() {
                if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                    if col >= content_end && col >= self.columns {
                        // No cell there at the new width: nothing carries it. Nearest is this
                        // row's line, clamped to its last visible column.
                        self.slots[index] = Some(Slot::Lost {
                            line: stream as i64,
                            col: self.columns - 1,
                            stream_keyed: true,
                        });
                    } else if col < moved_from {
                        // Kept on the pushed row at its unchanged visual column.
                        self.slots[index] = Some(Slot::Placed { line: stream as i64, col });
                    } else if carries {
                        self.slots[index] = Some(Slot::Pending { i, col: col - moved_from });
                        carried.push(index);
                    } else {
                        // A real tail cell, cut and discarded with reflow off.
                        self.slots[index] = Some(Slot::Lost {
                            line: stream as i64,
                            col: col.min(self.columns - 1),
                            stream_keyed: true,
                        });
                    }
                }
            }
        }
        self.last_piece.insert(i, (stream, moved_from));
        carried
    }

    /// The cells left on row `i` after [`merge`](Self::merge) were dropped (the loop removed a
    /// clear row): their anchors cannot follow and land clamped onto the merged run's end at
    /// the target piece. Their columns are already relative to the rest, as `merge` rewrote
    /// them; `run_len` is the merged run's visible length on the target row.
    fn row_dropped(&mut self, i: usize, target: usize, last_len: usize, run_len: usize) {
        if let Some(indices) = self.rows.remove(&i) {
            for index in indices {
                if let Some(Slot::Pending { col, .. }) = self.slots[index] {
                    let col = (col + last_len).clamp(0, run_len.max(1) - 1);
                    self.slots[index] =
                        Some(Slot::Lost { line: target as i64, col, stream_keyed: true });
                }
            }
        }
    }

    /// Pieces at produced stream positions `[kept, kept + dropped)` were dropped (a grow's
    /// cursor pull-down): anchors on them, and still-pending anchors of rows whose last piece
    /// was one, cannot follow; they resolve nearest the newest surviving piece.
    fn truncated(&mut self, kept: usize, dropped: usize) {
        if dropped == 0 {
            return;
        }
        let in_dropped = |stream: i64| stream >= kept as i64 && stream < (kept + dropped) as i64;

        // The newest surviving piece, for the nearest line and column clamp.
        let nearest = self
            .last_piece
            .iter()
            .map(|(&i, &(stream, len))| (i, stream, len))
            .filter(|&(_, stream, _)| stream < kept)
            .max_by_key(|&(_, stream, _)| stream)
            .map(|(_, stream, len)| (stream, len));

        for slot in self.slots.iter_mut() {
            let doomed = match *slot {
                Some(Slot::Placed { line, .. }) => in_dropped(line),
                Some(Slot::Pending { i, .. }) => {
                    self.last_piece.get(&i).is_some_and(|&(stream, _)| in_dropped(stream as i64))
                },
                _ => false,
            };
            if doomed {
                let col = match *slot {
                    Some(Slot::Placed { col, .. }) | Some(Slot::Pending { col, .. }) => col,
                    _ => 0,
                };
                let (line, len) = nearest.unwrap_or((kept.saturating_sub(1), 1));
                *slot = Some(Slot::Lost {
                    line: line as i64,
                    col: nearest_col(col, len, self.columns),
                    stream_keyed: true,
                });
            }
        }
    }

    /// Carried cells left over after the last processed row have no older row to prepend into:
    /// their anchors cannot follow and resolve nearest their source row's last piece.
    fn stash_final(&mut self, carried: &[usize]) {
        for &index in carried {
            if let Some(Slot::Pending { i, col }) = self.slots[index] {
                let (stream, len) = self.last_piece.get(&i).copied().unwrap_or((0, 1));
                let col = nearest_col(col, len, self.columns);
                self.slots[index] =
                    Some(Slot::Lost { line: stream as i64, col, stream_keyed: true });
            }
        }
    }

    /// Finalize: map every stream-keyed line onto a stable line — produced row `s` sits at
    /// `scrolled + lines_after - 1 - (produced - 1 - s)` from the bottom of the finished buffer,
    /// whose newest row is stream `produced - 1` — and turn placements whose piece was evicted
    /// at the scrollback limit (`s < dropped`) into losses. Already-final lines (evicted rows,
    /// cleared content) are untouched.
    fn resolve(&mut self, scrolled: i64, lines_after: usize, produced: usize, dropped: usize) {
        let stable =
            |stream: i64| scrolled + lines_after as i64 - 1 - (produced as i64 - 1 - stream);
        for slot in &mut self.slots {
            match *slot {
                Some(Slot::Placed { line: stream, col }) => {
                    if stream < dropped as i64 {
                        *slot = Some(Slot::Lost { line: stable(stream), col, stream_keyed: false });
                    } else {
                        *slot = Some(Slot::Placed { line: stable(stream), col });
                    }
                },
                Some(Slot::Lost { line, col, stream_keyed: true }) => {
                    *slot = Some(Slot::Lost { line: stable(line), col, stream_keyed: false });
                },
                _ => {},
            }
        }
    }

    /// Append the resolved outcomes to `out` in input order (after `resolve`).
    fn collect_into(self, anchors: &[(i64, usize)], out: &mut Vec<(i64, usize, bool)>) {
        for (index, slot) in self.slots.into_iter().enumerate() {
            match slot {
                None | Some(Slot::Pending { .. }) => {
                    // Untouched: the row survived the resize unchanged (byte-copied within the
                    // new width, or never reflowed).
                    let (stable, col) = anchors[index];
                    out.push((stable, col, false));
                },
                Some(Slot::Placed { line, col }) => out.push((line, col, false)),
                Some(Slot::Lost { line, col, .. }) => out.push((line, col, true)),
            }
        }
    }
}

/// The column an anchor that cannot follow lands on: the last cell of its nearest piece that
/// the grid still shows.
fn nearest_col(col: usize, len: usize, columns: usize) -> usize {
    let visible = len.min(columns).saturating_sub(1);
    col.min(visible)
}
