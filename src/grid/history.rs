// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Scrollback history: the rows above the screen, stored as compact records in pooled byte
//! blocks (feathercritty).
//!
//! Upstream alacritty_terminal keeps the screen and its history in one `Storage` ring of dense
//! `Row`s and lets anything index any line of it. feathercritty splits the grid in two (`Grid` in
//! `mod.rs`): the screen stays a dense `Storage` ring that every parser write lands in, and the
//! rows above it live in this module's [`History`], reached only through an explicit API:
//!
//! - [`History`] owns the rows above the screen, oldest first. Index `i` counts from the newest
//!   row: `i = 0` is grid line `-1`, `i = len() - 1` is `Grid::topmost_line()`. It never holds more
//!   than `limit()` rows; pushing at the limit evicts the oldest row, a lower limit evicts at once.
//! - [`RowReader`] reads any grid line as `&Row`: the screen row itself for lines `>= 0`, or a
//!   history row decoded into its own scratch row for lines `< 0`.
//! - [`HistoryCell`] (in `compact.rs`, with the row codec) is what a cell type needs to be stored
//!   here: a row encoder and decoder and the type of its shared extras.
//!
//! # Layout (design spec 5.3)
//!
//! A row is encoded once, when it scrolls off the screen (`compact.rs` defines the record), and
//! decoded only when something reads it.
//!
//! - **Blocks.** Records are appended back to back to byte blocks of `block_bytes` (a `Vec<u8>`
//!   allocated with exactly that capacity, [`DEFAULT_BLOCK_BYTES`] unless a test asks for tiny
//!   ones). A record never spans two blocks: one that does not fit in the newest block starts a new
//!   block. A record larger than a quarter block gets its own block of exactly its size
//!   ("oversized"), which is never appended to and is freed with its row. Blocks sit in a
//!   `VecDeque` in creation order; each knows how many index entries point into it (`live`). Blocks
//!   are named by a wrapping `u32` sequence number: the front block is `front_seq`, the next one
//!   `front_seq + 1`, and so on.
//! - **Index.** One 16-byte [`Entry`] per row, oldest first, in a `VecDeque`: the block's sequence
//!   number, the record's offset and length in it, and the position of the row's first extra.
//! - **Extras.** The `T::Extra` values (for `Cell`, its `Arc<CellExtra>`) a row's record refers to
//!   sit in one `VecDeque` shared by the whole history, in index order; positions are wrapping
//!   `u32` counters (`extras_base` is the position of the queue's front). Row `k`'s extras are
//!   exactly the positions from its `first_extra` up to the next row's (or the queue's end for the
//!   newest row), and their count equals the record header's `E`.
//!
//! # Invariants
//!
//! - `index.len() <= limit`. Every entry's block is in `blocks`, and its record lies inside the
//!   block's bytes. A block's `live` is the number of entries pointing into it; the blocks at both
//!   ends of the deque are never empty (`live > 0`), and an empty one in between has no buffer. A
//!   normal block never holds more than `block_bytes` bytes, so it never reallocates.
//! - `first_extra` never decreases from one entry to the next (the extras are in index order).
//! - Records are never edited in place. Bytes no entry points to any more ("dead" bytes, left by
//!   `update`) are reclaimed with their block: when its `live` count reaches zero its buffer goes
//!   to the pool (or is freed) at once, and the emptied block, a placeholder keeping its sequence
//!   number, leaves the deque when it reaches an end. A released record that is the last one in its
//!   block gives its bytes back at once (`pop_newest`, `update` of the newest row).
//!
//! # Eviction and the pool
//!
//! Pushing at the limit first evicts the oldest entry (its extras leave the front of the queue).
//! A block whose `live` count reaches zero gives up its buffer: one of a normal block's capacity
//! is kept as a spare (at most [`MAX_SPARES`]), anything else is freed. New blocks come from the
//! spares first. With a full history every evicted block becomes the next spare, so steady
//! scrolling allocates nothing: no blocks, no index or extras growth (both are at their
//! high-water capacity), and the encode buffer is reused. A cleared or trimmed history
//! (`clear`, a lower `set_limit`) frees the blocks and index capacity it no longer needs and keeps
//! at most the usual spares; dropping the history frees everything. Returning freed memory to
//! the operating system is the host's decision (design spec 5.9): nothing here calls into the
//! allocator beyond ordinary allocation and deallocation.
//!
//! # Why the records hold no pointers
//!
//! Extras are reference-counted pointers; they stay in the typed extras queue, and records refer
//! to them by position. So the blocks are plain bytes (no `unsafe`), and a block plus the extras
//! it refers to can later be written out and read back as is (disk spill, design spec 5.10).
//! Keep it that way: nothing pointer-sized or process-specific goes into a record.
//!
//! # Contiguous extras for the decoder
//!
//! `HistoryCell::decode` takes a row's extras as one slice, but the extras queue is a ring
//! buffer and a row's extras can straddle its wrap point. Reads use `VecDeque::as_slices` and
//! hand the decoder a sub-slice of one half; only a row whose extras straddle the wrap point gets
//! a small copy of them (one clone per extra, for `Cell` an `Arc` increment). That happens for at
//! most one row per wrap of the ring, so `make_contiguous` (O(n) and `&mut`) is never needed.
//!
//! # When merging upstream
//!
//! Nothing in upstream corresponds to this file. Upstream code that reads history lines
//! (`self.raw[line]` or `grid[line]` with `line < 0`) must go through `RowReader`,
//! `Grid::cell`, `Grid::wraps` or `Grid::is_clear`; writes through [`History::update`]. Upstream
//! code that grows or shrinks the history (its `increase_scroll_limit`, `decrease_scroll_limit`,
//! `raw.shrink_lines`) maps to `push_newest`, `remove_oldest` and `set_limit`.

use std::collections::VecDeque;
use std::{fmt, mem};

use crate::grid::compact::read_header;
use crate::grid::{Dimensions, Grid, HistoryCell, Row};
use crate::index::{Column, Line};

/// Size of a history block in bytes, unless a test asks for another one.
pub(crate) const DEFAULT_BLOCK_BYTES: usize = 64 * 1024;

/// Most spare blocks a history keeps for its next records.
pub(crate) const MAX_SPARES: usize = 2;

/// A byte block holding records back to back.
struct Block {
    bytes: Vec<u8>,

    /// Index entries pointing into this block.
    live: u32,

    /// Holds a single record larger than a quarter block, in an allocation of exactly its size.
    oversized: bool,
}

/// Where one history row is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    /// Sequence number of the block holding the record.
    block: u32,

    /// Start of the record in the block.
    offset: u32,

    /// Length of the record in bytes (the record header holds no byte length).
    len: u32,

    /// Position of the row's first extra in the extras queue (a wrapping counter).
    first_extra: u32,
}

/// The rows above the screen, at most `limit` of them, as compact records in pooled blocks (see
/// the module docs for the layout and its invariants).
pub struct History<T: HistoryCell> {
    /// One entry per row, oldest first.
    index: VecDeque<Entry>,

    /// The blocks, in creation order; the front one has sequence number `front_seq`.
    blocks: VecDeque<Block>,
    front_seq: u32,

    /// The extras rows refer to, in index order; the front one is at position `extras_base`.
    extras: VecDeque<T::Extra>,
    extras_base: u32,

    /// Empty normal blocks kept for the next records, at most `MAX_SPARES`.
    spares: Vec<Vec<u8>>,

    /// Capacity of a normal block.
    block_bytes: usize,

    /// Most rows kept.
    limit: usize,

    /// Reused buffer the encoder writes a record into before it is copied to its block.
    encoded: Vec<u8>,

    /// Reused row `update` decodes into.
    updated: Row<T>,
}

impl<T: HistoryCell> History<T> {
    /// An empty history that keeps at most `limit` rows. It allocates nothing until a row is
    /// pushed.
    pub fn new(limit: usize) -> Self {
        Self::with_block_size(limit, DEFAULT_BLOCK_BYTES)
    }

    /// An empty history with blocks of `block_bytes` bytes (tests use tiny blocks to reach block
    /// boundaries, spares and oversized rows often).
    pub(crate) fn with_block_size(limit: usize, block_bytes: usize) -> Self {
        assert!(
            block_bytes > 0 && u32::try_from(block_bytes).is_ok(),
            "history block size {block_bytes} out of range"
        );
        Self {
            index: VecDeque::new(),
            blocks: VecDeque::new(),
            front_seq: 0,
            extras: VecDeque::new(),
            extras_base: 0,
            spares: Vec::new(),
            block_bytes,
            limit,
            encoded: Vec::new(),
            updated: Row::default(),
        }
    }

    /// Number of rows stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether no row is stored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Maximum number of rows kept.
    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Capacity of a normal block, for a new history built like this one.
    #[inline]
    pub(crate) fn block_bytes(&self) -> usize {
        self.block_bytes
    }

    /// Change the maximum number of rows; a lower limit evicts the oldest rows now and frees
    /// the blocks and index capacity they used (keeping at most `MAX_SPARES` spare blocks).
    pub fn set_limit(&mut self, limit: usize) {
        let lower = limit < self.limit;
        self.limit = limit;
        self.remove_oldest(self.index.len().saturating_sub(limit));
        if lower {
            self.index.shrink_to(limit);
            self.extras.shrink_to_fit();
        }
    }

    /// Append an encoded copy of `row` as the newest row, evicting the oldest one at the limit.
    /// Stores nothing with a limit of 0. The caller keeps `row` (the screen reuses it).
    pub fn push_newest(&mut self, row: &Row<T>) {
        if self.limit == 0 {
            return;
        }

        // Evict first, so that at the limit the evicted row's block can take the new record.
        if self.index.len() >= self.limit {
            self.remove_oldest(self.index.len() + 1 - self.limit);
        }

        let first_extra = self.extras_end();
        self.encoded.clear();
        T::encode(row, &mut self.encoded, &mut self.extras);
        let entry = self.store(first_extra);
        self.index.push_back(entry);
    }

    /// Remove the newest row into `into` (reusing its allocation when the width matches).
    /// Returns `false`, leaving `into` untouched, when the history is empty.
    pub fn pop_newest(&mut self, into: &mut Row<T>) -> bool {
        let Some(entry) = self.index.pop_back() else {
            return false;
        };

        self.decode(entry, self.extras_end(), into);
        self.release(entry);
        self.extras.truncate(entry.first_extra.wrapping_sub(self.extras_base) as usize);
        true
    }

    /// Remove the oldest row into `into`. Returns `false`, leaving `into` untouched, when the
    /// history is empty.
    pub fn pop_oldest(&mut self, into: &mut Row<T>) -> bool {
        let Some(&entry) = self.index.front() else {
            return false;
        };

        self.decode(entry, self.extras_after(0), into);
        self.remove_oldest(1);
        true
    }

    /// Remove the `count` oldest rows (all of them if there are fewer) without decoding them.
    pub(crate) fn remove_oldest(&mut self, count: usize) {
        for _ in 0..count.min(self.index.len()) {
            if let Some(entry) = self.index.pop_front() {
                self.release(entry);
            }
        }

        // The extras of the removed rows are those before the new oldest row's.
        let keep = self.index.front().map_or_else(|| self.extras_end(), |entry| entry.first_extra);
        let removed = keep.wrapping_sub(self.extras_base) as usize;
        self.extras.drain(..removed);
        self.extras_base = keep;
    }

    /// Decode row `i` (0 is the newest) into `into`, reusing `into`'s allocation when the width
    /// matches.
    ///
    /// Callers check `i < len()` (a `debug_assert!`). Past the end is "no row": `into` keeps its
    /// width and every cell becomes `T::default()`.
    pub fn read(&self, i: usize, into: &mut Row<T>) {
        match self.position(i) {
            Some(pos) => self.decode(self.index[pos], self.extras_after(pos), into),
            None => into[..].iter_mut().for_each(|cell| *cell = T::default()),
        }
    }

    /// Cell `column` of row `i` (0 is the newest), decoding only what describes it.
    ///
    /// Callers check `i < len()` (a `debug_assert!`); past the end is "no row", a default cell.
    /// A column outside the row panics, as indexing a `Row` does.
    pub fn cell(&self, i: usize, column: Column) -> T {
        match self.position(i) {
            Some(pos) => {
                let entry = self.index[pos];
                let bytes = self.record(entry);
                self.with_extras(entry, self.extras_after(pos), |extras| {
                    T::decode_cell(bytes, extras, column.0)
                })
            },
            None => T::default(),
        }
    }

    /// Change row `i` (0 is the newest).
    ///
    /// The row is decoded, changed by `f`, re-encoded at the end of the newest block and its
    /// entry repointed; the old record's bytes are dead (given back at once when they were the
    /// last bytes of their block, see the module docs). The row keeps its place in the index and
    /// its extras keep their place in the queue.
    ///
    /// Callers check `i < len()` (a `debug_assert!`); past the end there is no row to change.
    pub fn update(&mut self, i: usize, f: impl FnOnce(&mut Row<T>)) {
        let Some(pos) = self.position(i) else {
            return;
        };

        let entry = self.index[pos];
        let old_end = self.extras_after(pos);
        let mut row = mem::take(&mut self.updated);
        self.decode(entry, old_end, &mut row);
        f(&mut row);
        self.release(entry);

        let start = entry.first_extra.wrapping_sub(self.extras_base) as usize;
        let old_count = old_end.wrapping_sub(entry.first_extra) as usize;
        self.encoded.clear();
        if pos + 1 == self.index.len() {
            // The newest row's extras end the queue: replace them in place.
            self.extras.truncate(start);
            T::encode(&row, &mut self.encoded, &mut self.extras);
        } else {
            // The encoder appends the row's extras to the queue; move them into the row's own
            // slots, growing or shrinking those (and moving every newer row's position) when the
            // count changed. O(len) then, but the library's one history write (`Term::input`
            // clearing a flag on the newest row) never gets here.
            let end = self.extras.len();
            T::encode(&row, &mut self.encoded, &mut self.extras);
            let new: Vec<T::Extra> = self.extras.drain(end..).collect();
            let new_count = new.len();
            if new_count == old_count {
                for (k, extra) in new.into_iter().enumerate() {
                    self.extras[start + k] = extra;
                }
            } else {
                self.extras.drain(start..start + old_count);
                for (k, extra) in new.into_iter().enumerate() {
                    self.extras.insert(start + k, extra);
                }
                let delta = new_count.wrapping_sub(old_count) as u32;
                for later in self.index.range_mut(pos + 1..) {
                    later.first_extra = later.first_extra.wrapping_add(delta);
                }
            }
        }

        self.index[pos] = self.store(entry.first_extra);
        self.updated = row;
    }

    /// Whether row `i` (0 is the newest) continues on the next line (the record header's
    /// `WRAPS` bit); "no row" past the end does not.
    pub fn wraps(&self, i: usize) -> bool {
        self.position(i).is_some_and(|pos| read_header(self.record(self.index[pos])).wraps)
    }

    /// Whether every cell of row `i` (0 is the newest) is empty (the record header's `CLEAR`
    /// bit); "no row" past the end is.
    pub fn is_clear(&self, i: usize) -> bool {
        self.position(i).is_none_or(|pos| read_header(self.record(self.index[pos])).clear)
    }

    /// Remove every row; the limit stays. Frees the index, the extras and every block but at
    /// most `MAX_SPARES` spares.
    pub fn clear(&mut self) {
        self.index = VecDeque::new();
        self.extras = VecDeque::new();
        self.extras_base = 0;
        for block in mem::take(&mut self.blocks) {
            self.recycle(block.bytes);
        }
        self.front_seq = 0;
    }

    /// Position of row `i` (0 is the newest) in `index`, `None` past the end ("no row"; a
    /// caller bug, so debug builds stop there).
    #[inline]
    fn position(&self, i: usize) -> Option<usize> {
        let len = self.index.len();
        debug_assert!(i < len, "history row {i} out of range (len {len})");
        (i < len).then(|| len - 1 - i)
    }

    /// Position one past the last extra in the queue.
    #[inline]
    fn extras_end(&self) -> u32 {
        self.extras_base.wrapping_add(self.extras.len() as u32)
    }

    /// Position one past the last extra of the row at `pos` in `index`.
    #[inline]
    fn extras_after(&self, pos: usize) -> u32 {
        self.index.get(pos + 1).map_or_else(|| self.extras_end(), |next| next.first_extra)
    }

    /// The record `entry` points to.
    #[inline]
    fn record(&self, entry: Entry) -> &[u8] {
        let block = &self.blocks[entry.block.wrapping_sub(self.front_seq) as usize];
        &block.bytes[entry.offset as usize..][..entry.len as usize]
    }

    /// Call `f` with the extras of the row `entry` points to, which end at position `end`, as
    /// one slice (see "Contiguous extras for the decoder" in the module docs).
    #[inline]
    fn with_extras<R>(&self, entry: Entry, end: u32, f: impl FnOnce(&[T::Extra]) -> R) -> R {
        let count = end.wrapping_sub(entry.first_extra) as usize;
        if count == 0 {
            return f(&[]);
        }

        let start = entry.first_extra.wrapping_sub(self.extras_base) as usize;
        let (front, back) = self.extras.as_slices();
        if start + count <= front.len() {
            f(&front[start..start + count])
        } else if start >= front.len() {
            f(&back[start - front.len()..][..count])
        } else {
            let copy: Vec<T::Extra> = self.extras.range(start..start + count).cloned().collect();
            f(&copy)
        }
    }

    /// Decode the row `entry` points to, whose extras end at position `extras_end`, into `into`.
    fn decode(&self, entry: Entry, extras_end: u32, into: &mut Row<T>) {
        let bytes = self.record(entry);
        let used = self.with_extras(entry, extras_end, |extras| T::decode(bytes, extras, into));
        // A wrong offset or length must not decode silently.
        debug_assert_eq!(used, entry.len as usize, "history record length disagrees with index");
    }

    /// Copy the record in `encoded` to a block and return its entry: into the newest block when
    /// it fits there, else into a new block (a spare when there is one); a record larger than a
    /// quarter block into a block of its own.
    fn store(&mut self, first_extra: u32) -> Entry {
        let len = self.encoded.len();
        let oversized = len > self.block_bytes / 4;
        let fits = |block: &Block| !block.oversized && block.bytes.len() + len <= self.block_bytes;
        if oversized {
            let bytes = Vec::with_capacity(len);
            self.blocks.push_back(Block { bytes, live: 0, oversized: true });
        } else if !self.blocks.back().is_some_and(fits) {
            let bytes = self.spares.pop().unwrap_or_else(|| Vec::with_capacity(self.block_bytes));
            self.blocks.push_back(Block { bytes, live: 0, oversized: false });
        }

        let block_seq = self.front_seq.wrapping_add(self.blocks.len() as u32 - 1);
        let block = self.blocks.back_mut().expect("a block was just ensured");
        let offset = block.bytes.len();
        block.bytes.extend_from_slice(&self.encoded);
        block.live += 1;
        debug_assert!(oversized || block.bytes.len() <= self.block_bytes);

        Entry { block: block_seq, offset: offset as u32, len: len as u32, first_extra }
    }

    /// Account for `entry` no longer pointing into its block: give its bytes back when they are
    /// the block's last ones, recycle the block's buffer when no entry points into it any more,
    /// and remove the emptied blocks at either end of the deque.
    fn release(&mut self, entry: Entry) {
        let block = &mut self.blocks[entry.block.wrapping_sub(self.front_seq) as usize];
        block.live -= 1;
        if entry.offset as usize + entry.len as usize == block.bytes.len() {
            block.bytes.truncate(entry.offset as usize);
        }
        if block.live == 0 {
            // A block in the middle of the deque stays as an empty placeholder (its sequence
            // number must stay taken) until it reaches an end, but its buffer is reused now.
            let bytes = mem::take(&mut block.bytes);
            self.recycle(bytes);
        }

        while self.blocks.front().is_some_and(|block| block.live == 0) {
            self.blocks.pop_front();
            self.front_seq = self.front_seq.wrapping_add(1);
        }
        while self.blocks.back().is_some_and(|block| block.live == 0) {
            self.blocks.pop_back();
        }
    }

    /// Keep a block buffer no entry points into as a spare if it has a normal block's capacity
    /// and there is room, else free it.
    fn recycle(&mut self, mut bytes: Vec<u8>) {
        if bytes.capacity() != self.block_bytes || self.spares.len() >= MAX_SPARES {
            return;
        }

        bytes.clear();
        if self.spares.capacity() == 0 {
            self.spares.reserve_exact(MAX_SPARES);
        }
        self.spares.push(bytes);
    }
}

/// Copies the rows; spares and scratch buffers are not copied. Normal blocks keep their full
/// capacity, so the copy appends without reallocating, as the original does.
impl<T: HistoryCell> Clone for History<T> {
    fn clone(&self) -> Self {
        let blocks = self
            .blocks
            .iter()
            .map(|block| {
                let capacity = match block {
                    Block { live: 0, .. } => 0,
                    Block { oversized: true, bytes, .. } => bytes.len(),
                    Block { oversized: false, .. } => self.block_bytes,
                };
                let mut bytes = Vec::with_capacity(capacity);
                bytes.extend_from_slice(&block.bytes);
                Block { bytes, live: block.live, oversized: block.oversized }
            })
            .collect();

        Self {
            index: self.index.clone(),
            blocks,
            front_seq: self.front_seq,
            extras: self.extras.clone(),
            extras_base: self.extras_base,
            spares: Vec::new(),
            block_bytes: self.block_bytes,
            limit: self.limit,
            encoded: Vec::new(),
            updated: Row::default(),
        }
    }
}

/// A summary: the rows themselves are bytes (read them with [`History::read`]).
impl<T: HistoryCell> fmt::Debug for History<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("History")
            .field("len", &self.index.len())
            .field("limit", &self.limit)
            .field("blocks", &self.blocks.len())
            .field("bytes", &self.blocks.iter().map(|block| block.bytes.len()).sum::<usize>())
            .field("spares", &self.spares.len())
            .field("extras", &self.extras.len())
            .field("block_bytes", &self.block_bytes)
            .finish()
    }
}

/// Reads grid rows from any line, screen or history, as `&Row`.
///
/// [`RowReader::row`] returns the screen row itself for lines `>= 0` and decodes history rows
/// into the reader's scratch row (reusing its allocation), which is why the API takes `&mut self`
/// and why callers should keep one reader for a loop over many lines.
#[derive(Debug)]
pub struct RowReader<T> {
    /// The last history row read.
    scratch: Row<T>,

    /// A row of default cells, the grid's width, returned for "no row" (made on first use).
    blank: Row<T>,
}

impl<T> Default for RowReader<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RowReader<T> {
    /// A reader; it allocates nothing until it has to decode a row.
    pub fn new() -> Self {
        Self { scratch: Row::default(), blank: Row::default() }
    }

    /// Row `line` of `grid`, for any line from `grid.topmost_line()` to
    /// `grid.bottommost_line()`. Callers clamp: a line outside that range fails a
    /// `debug_assert!` (a caller bug, as upstream's indexing treated it) and in release builds
    /// reads as "no row", a row of default cells (see `Grid::cell`), never a stale scratch row.
    pub fn row<'a>(&'a mut self, grid: &'a Grid<T>, line: Line) -> &'a Row<T>
    where
        T: HistoryCell,
    {
        let inside = line >= grid.topmost_line() && line <= grid.bottommost_line();
        debug_assert!(
            inside,
            "RowReader: line {} outside the grid ({}..={}); callers must clamp",
            line.0,
            grid.topmost_line().0,
            grid.bottommost_line().0,
        );

        if !inside {
            if self.blank.len() != grid.columns() {
                self.blank = Row::new(grid.columns());
            }
            &self.blank
        } else if line.0 >= 0 {
            &grid.screen()[line]
        } else {
            grid.history().read((-line.0 - 1) as usize, &mut self.scratch);
            &self.scratch
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

    // Block store tests (design spec 5.3). Most use 256-byte blocks, so rows larger than 64
    // bytes are oversized and a few dozen rows fill a block.

    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    use crate::term::cell::{Cell, Hyperlink};

    const TINY: usize = 256;

    /// A `usize` row `width` cells wide whose cells are all `value` (each cell a varint).
    fn wide(width: usize, value: usize) -> Row<usize> {
        let mut row = Row::new(width);
        row[..].iter_mut().for_each(|cell| *cell = value);
        row
    }

    fn read(history: &History<usize>, i: usize) -> Row<usize> {
        let mut into = Row::default();
        history.read(i, &mut into);
        into
    }

    /// The store's invariants, checked after every operation of the tests below.
    fn check_store<T: HistoryCell>(history: &History<T>) {
        assert!(history.len() <= history.limit());
        assert!(history.spares.len() <= MAX_SPARES);
        for spare in &history.spares {
            assert!(spare.is_empty());
            assert_eq!(spare.capacity(), history.block_bytes);
        }

        let mut live = vec![0; history.blocks.len()];
        for entry in &history.index {
            let at = entry.block.wrapping_sub(history.front_seq) as usize;
            let block = &history.blocks[at];
            let end = entry.offset as usize + entry.len as usize;
            assert!(end <= block.bytes.len(), "a record reaches past its block");
            live[at] += 1;
        }
        for (block, live) in history.blocks.iter().zip(live) {
            assert_eq!(block.live as usize, live, "live count");
            if live == 0 {
                assert_eq!(block.bytes.capacity(), 0, "an empty block kept its buffer");
            } else if block.oversized {
                assert_eq!(block.bytes.len(), block.bytes.capacity());
                assert!(block.bytes.len() > history.block_bytes / 4);
            } else {
                assert!(block.bytes.len() <= history.block_bytes);
                assert_eq!(block.bytes.capacity(), history.block_bytes, "a block grew");
            }
        }
        if let (Some(front), Some(back)) = (history.blocks.front(), history.blocks.back()) {
            assert!(front.live > 0 && back.live > 0, "an empty block at an end of the deque");
        }

        // Extras: first positions never decrease, and each row's extras are inside the queue.
        let end = history.extras_base.wrapping_add(history.extras.len() as u32);
        let mut previous = history.extras_base;
        for entry in &history.index {
            let first = entry.first_extra.wrapping_sub(history.extras_base);
            assert!(first >= previous.wrapping_sub(history.extras_base));
            assert!(first <= end.wrapping_sub(history.extras_base));
            previous = entry.first_extra;
        }
    }

    #[test]
    fn index_entry_is_small() {
        assert!(std::mem::size_of::<Entry>() <= 16);
    }

    #[test]
    fn rows_never_span_blocks() {
        let mut rng = ChaCha8Rng::seed_from_u64(8);
        let mut history = History::with_block_size(500, TINY);
        let mut model = VecDeque::new();
        for n in 0..2000 {
            // Up to 20 cells of 1 to 3 bytes: some rows are oversized (over 64 bytes).
            let width = rng.gen_range(1..=20);
            let row = wide(width, rng.gen_range(0..20_000));
            history.push_newest(&row);
            model.push_front(row);
            model.truncate(500);
            if n % 97 == 0 {
                check_store(&history);
            }
        }
        check_store(&history);
        assert!(history.blocks.len() > 10);
        for (i, row) in model.iter().enumerate() {
            assert_eq!(&read(&history, i), row, "row {i}");
        }
    }

    #[test]
    fn oversized_row_gets_own_block() {
        let mut history = History::with_block_size(10, TINY);
        history.push_newest(&wide(2, 1));
        // 40 cells of 2 bytes each: about 90 bytes, more than a quarter of the block.
        let big = wide(40, 300);
        history.push_newest(&big);
        history.push_newest(&wide(2, 2));
        check_store(&history);

        assert_eq!(history.blocks.len(), 3);
        let oversized = &history.blocks[1];
        assert!(oversized.oversized);
        assert_eq!(oversized.live, 1);
        assert_eq!(oversized.bytes.len(), history.index[1].len as usize);
        assert_eq!(oversized.bytes.capacity(), oversized.bytes.len());
        assert_eq!(read(&history, 1), big);
        assert_eq!(read(&history, 0), wide(2, 2));
        assert_eq!(read(&history, 2), wide(2, 1));

        // Evicted, an oversized block is freed, never kept as a spare.
        history.set_limit(1);
        check_store(&history);
        assert!(history.spares.iter().all(|spare| spare.capacity() == TINY));
        assert_eq!(read(&history, 0), wide(2, 2));
    }

    #[test]
    fn evicted_block_becomes_spare_max_two() {
        let mut history = History::with_block_size(100, TINY);
        for value in 0..100 {
            history.push_newest(&wide(2, value));
        }
        assert!(history.spares.is_empty());
        let oldest = history.blocks[0].bytes.as_ptr();

        // Push until the oldest block's rows are all evicted: its buffer is reused, as a spare
        // or as the block now taking new rows.
        let mut reused = false;
        for value in 100..1000 {
            history.push_newest(&wide(2, value));
            check_store(&history);
            reused |= history.spares.iter().any(|spare| spare.as_ptr() == oldest)
                || history.blocks.iter().any(|block| block.bytes.as_ptr() == oldest);
            assert!(history.spares.len() <= MAX_SPARES);
        }
        assert!(reused, "the evicted block was not recycled");
        assert_eq!(read(&history, 0), wide(2, 999));
        assert_eq!(read(&history, 99), wide(2, 900));
    }

    #[test]
    fn pop_newest_gives_bytes_back_when_last() {
        let mut history = History::with_block_size(10, TINY);
        for value in 1..=3 {
            history.push_newest(&wide(3, value));
        }
        let used = history.blocks[0].bytes.len();
        let last = history.index.back().unwrap().len as usize;

        let mut into = Row::default();
        assert!(history.pop_newest(&mut into));
        assert_eq!(into, wide(3, 3));
        assert_eq!(history.blocks[0].bytes.len(), used - last);
        check_store(&history);

        assert!(history.pop_newest(&mut into));
        assert!(history.pop_newest(&mut into));
        assert_eq!(into, wide(3, 1));
        assert!(history.blocks.is_empty());
        assert_eq!(history.spares.len(), 1, "the emptied block is kept as a spare");
        assert!(!history.pop_newest(&mut into));
    }

    #[test]
    fn update_reencodes_at_end_and_reclaims_dead_bytes() {
        let mut history = History::with_block_size(100, TINY);
        // 8-byte records, 32 per block: rows 0..32 fill the first block, 32..40 the second.
        for value in 0..40 {
            history.push_newest(&wide(2, value));
        }
        assert_eq!(history.blocks.len(), 2);
        let first_len = history.blocks[0].bytes.len();

        // The newest row's record is last in its block: it is re-encoded in place.
        let before = history.blocks[1].bytes.len();
        history.update(0, |row| row[Column(0)] = 1000);
        check_store(&history);
        assert_eq!(history.blocks[1].bytes.len(), before + 1, "1000 takes one more byte");

        // Row 5 (index 34 from the newest) moves to the end of the last block; its old bytes in
        // the first block are dead.
        history.update(34, |row| row[Column(1)] = 7);
        check_store(&history);
        let entry = history.index[5];
        assert_eq!(entry.block, history.front_seq.wrapping_add(1));
        assert_eq!(entry.offset as usize + entry.len as usize, history.blocks[1].bytes.len());
        assert_eq!(history.blocks[0].live, 31);
        assert_eq!(history.blocks[0].bytes.len(), first_len, "never edited in place");
        let mut expected = wide(2, 5);
        expected[Column(1)] = 7;
        assert_eq!(read(&history, 34), expected);
        assert_eq!(read(&history, 33), wide(2, 6));
        assert_eq!(read(&history, 35), wide(2, 4));
        assert_eq!(read(&history, 0)[Column(0)], 1000);

        // Evicting the other rows of the first block frees it, dead bytes included.
        let mut into = Row::default();
        for _ in 0..6 {
            history.pop_oldest(&mut into);
        }
        assert_eq!(into, expected);
        assert_eq!(history.blocks[0].live, 26);
        for _ in 0..26 {
            history.pop_oldest(&mut into);
        }
        check_store(&history);
        assert_eq!(history.blocks.len(), 1);
        assert_eq!(history.spares.len(), 1);
        assert_eq!(history.len(), 8);
        assert_eq!(read(&history, 0)[Column(0)], 1000);
    }

    #[test]
    fn clear_keeps_at_most_two_spares() {
        let mut history = History::with_block_size(1000, TINY);
        for value in 0..1000 {
            history.push_newest(&wide(2, value));
        }
        assert!(history.blocks.len() > 20);

        history.clear();
        check_store(&history);
        assert_eq!(history.len(), 0);
        assert!(history.blocks.is_empty());
        assert!(history.extras.is_empty());
        assert_eq!(history.spares.len(), MAX_SPARES);
        assert_eq!(history.index.capacity(), 0, "the index is freed");

        // The history works as before.
        history.push_newest(&wide(2, 1));
        assert_eq!(read(&history, 0), wide(2, 1));
        assert_eq!(history.spares.len(), MAX_SPARES - 1, "new rows go to a spare first");
        check_store(&history);
    }

    #[test]
    fn set_limit_lower_frees_blocks() {
        let mut history = History::with_block_size(1000, TINY);
        for value in 0..1000 {
            history.push_newest(&wide(2, value));
        }
        let before = history.blocks.len();

        history.set_limit(40);
        check_store(&history);
        assert_eq!(history.len(), 40);
        assert!(history.blocks.len() <= 3, "{} of {before} blocks kept", history.blocks.len());
        assert!(history.spares.len() <= MAX_SPARES);
        assert!(history.index.capacity() < 1000, "the index shrinks with the limit");
        assert_eq!(read(&history, 39), wide(2, 960));
    }

    #[test]
    fn history_clear_during_reader_use() {
        let mut grid = Grid::<usize>::new_with_block_size(2, 2, 10, TINY);
        grid.screen_mut()[Line(0)][Column(0)] = 7;
        grid.scroll_up::<usize>(&(Line(0)..Line(2)), 1);
        let mut reader = RowReader::new();
        assert_eq!(reader.row(&grid, Line(-1))[Column(0)], 7);

        grid.clear_history();
        assert_eq!(grid.topmost_line(), Line(0));

        // Line -1 is gone: debug builds stop at the caller bug, release builds read "no row",
        // never the reader's stale copy of the old row.
        let stale = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader.row(&grid, Line(-1)).clone()
        }));
        match stale {
            Err(_) => {
                let debug = cfg!(debug_assertions);
                assert!(debug, "only debug builds stop there");
            },
            Ok(row) => assert_eq!(row, Row::new(2)),
        }

        // New history reads as itself.
        grid.screen_mut()[Line(0)][Column(0)] = 9;
        grid.scroll_up::<usize>(&(Line(0)..Line(2)), 1);
        assert_eq!(reader.row(&grid, Line(-1))[Column(0)], 9);
    }

    /// A row of `Cell`s with text, colors and extras (hyperlinks from `links`, so rows share
    /// them), different for each `seed`.
    fn cell_row(rng: &mut ChaCha8Rng, links: &[Cell], width: usize) -> Row<Cell> {
        let mut row = Row::new(width);
        let used = rng.gen_range(0..=width);
        let mut col = 0;
        while col < used {
            let run = rng.gen_range(1..=6).min(used - col);
            let mut template = if rng.gen_bool(0.3) {
                links[rng.gen_range(0..links.len())].clone()
            } else {
                Cell::default()
            };
            template.fg = crate::vte::ansi::Color::Indexed(rng.gen_range(0..16));
            for cell in &mut row[Column(col)..Column(col + run)] {
                *cell = template.clone();
                cell.c = rng.gen_range('a'..='z');
            }
            col += run;
        }
        row
    }

    fn cell_links() -> Vec<Cell> {
        (0..5)
            .map(|i| {
                let mut cell = Cell::default();
                cell.set_hyperlink(Some(Hyperlink::new(None::<String>, format!("https://e/{i}"))));
                cell
            })
            .collect()
    }

    /// Random pushes, pops, updates, limit changes and clears on tiny blocks, against a dense
    /// model: every row reads back as the model's, extras included (the extras queue wraps
    /// around its ring buffer many times).
    #[test]
    fn matches_dense_model() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x5704e);
        let links = cell_links();
        let mut history = History::<Cell>::with_block_size(60, TINY);
        let mut model: VecDeque<Row<Cell>> = VecDeque::new();
        let mut into = Row::default();
        for step in 0..20_000 {
            let width = rng.gen_range(1..=12);
            match rng.gen_range(0..100) {
                0..=69 => {
                    let row = cell_row(&mut rng, &links, width);
                    history.push_newest(&row);
                    if history.limit() > 0 {
                        model.push_front(row);
                        model.truncate(history.limit());
                    }
                },
                70..=79 => {
                    let popped = history.pop_newest(&mut into);
                    assert_eq!(popped, !model.is_empty());
                    if let Some(row) = model.pop_front() {
                        assert_eq!(into, row, "step {step}: pop_newest");
                    }
                },
                80..=84 => {
                    let popped = history.pop_oldest(&mut into);
                    assert_eq!(popped, !model.is_empty());
                    if let Some(row) = model.pop_back() {
                        assert_eq!(into, row, "step {step}: pop_oldest");
                    }
                },
                85..=96 if !model.is_empty() => {
                    let i = rng.gen_range(0..model.len());
                    let new = cell_row(&mut rng, &links, model[i].len());
                    history.update(i, |row| *row = new.clone());
                    model[i] = new;
                },
                97..=98 => {
                    let limit = rng.gen_range(0..=80);
                    history.set_limit(limit);
                    model.truncate(limit);
                },
                99 => {
                    history.clear();
                    model.clear();
                },
                _ => {},
            }

            check_store(&history);
            assert_eq!(history.len(), model.len(), "step {step}");
            if step % 50 == 0 {
                for (i, row) in model.iter().enumerate() {
                    history.read(i, &mut into);
                    assert_eq!(&into, row, "step {step}: row {i}");
                    for col in 0..row.len() {
                        assert_eq!(history.cell(i, Column(col)), row[Column(col)]);
                    }
                    assert_eq!(history.wraps(i), Cell::row_wraps(row));
                    assert_eq!(history.is_clear(i), row.is_clear());
                }
            }
        }
    }

    #[test]
    fn clone_keeps_block_capacity() {
        let mut history = History::with_block_size(100, TINY);
        for value in 0..50 {
            history.push_newest(&wide(2, value));
        }
        let mut copy = history.clone();
        check_store(&copy);
        for value in 50..200 {
            copy.push_newest(&wide(2, value));
            check_store(&copy);
        }
        assert_eq!(read(&copy, 0), wide(2, 199));
        assert_eq!(read(&history, 0), wide(2, 49));
    }
}
