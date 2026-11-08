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
//! # Credit
//!
//! The idea of keeping the visible screen dense while storing history rows in a compact form,
//! with pooled buffers and an explicit history read API, comes from Adam Mikołajczyk (Razuer),
//! who proposed it in alacritty/alacritty#9049. feathercritty's implementation is independent and
//! was written from the idea only.
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
//!   `front_seq + 1`, and so on. A row is encoded straight into the newest block when the block has
//!   room for the largest record such a row can make (`HistoryCell::max_record_bytes`); otherwise,
//!   near a block's end, into a reused scratch buffer and copied from there to wherever it fits.
//! - **Index.** One 16-byte [`Entry`] per row, oldest first, in a `VecDeque`: the block's sequence
//!   number, the record's offset and length in it (30 bits, the other two hold copies of the record
//!   header's `WRAPS` and `CLEAR` bits, so `wraps` and `is_clear` never decompress), and the
//!   position of the row's first extra. It grows by doubling but never past the limit, so a full
//!   history's index holds at most `limit` entries. After a column reflow it can hold up to `limit`
//!   plus `lines` entries: the reflow builds the new history with a limit of that sum (upstream's
//!   final trim) and lowers the limit at the end without shrinking the index (`trim_to_limit`),
//!   about 16 bytes per screen line.
//! - **Extras.** The `T::Extra` values (for `Cell`, its `Arc<CellExtra>`) a row's record refers to
//!   sit in one `VecDeque` shared by the whole history, in index order; positions are wrapping
//!   `u32` counters (`extras_base` is the position of the queue's front). Row `k`'s extras are
//!   exactly the positions from its `first_extra` up to the next row's (or the queue's end for the
//!   newest row), and their count equals the record header's `E`.
//!
//! # Invariants
//!
//! - `index.len() <= limit`. Every entry's block is in `blocks`, and its record lies inside the
//!   block's bytes (a cold block's uncompressed bytes). A block's `live` is the number of entries
//!   pointing into it; the blocks at both ends of the deque are never empty (`live > 0`), and an
//!   empty one in between has no buffer and is warm. A normal warm block never holds more than
//!   `block_bytes` bytes, so it never reallocates. The newest block is never cold.
//! - `first_extra` never decreases from one entry to the next (the extras are in index order).
//! - Records are never edited in place. Bytes no entry points to any more ("dead" bytes, left by
//!   `update`) are reclaimed with their block: when its `live` count reaches zero its buffer goes
//!   to the pool (or is freed) at once, and the emptied block, a placeholder keeping its sequence
//!   number, leaves the deque when it reaches an end. A released record that is the last one in its
//!   warm block gives its bytes back at once (`pop_newest`, `update` of the newest row); a cold
//!   block is never truncated.
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
//! # Cold blocks (history compression design spec 5.1, 5.2, 5.5)
//!
//! With [`History::set_compression`]`(Some(keep_warm))`, blocks whose rows are all old enough are
//! kept compressed (`cold.rs`): the block's `bytes` hold the compressed form, shrunk to fit, and
//! `cold` is set. Entries are unchanged; offsets still count in the uncompressed bytes.
//!
//! - **Eligibility.** A block should be cold when compression is on, it holds rows, none of them is
//!   among the `keep_warm` newest (every row has `i >= keep_warm`), and it is not the newest block
//!   (new records only go there). Each block counts its rows in that warm window (`warm_live`), so
//!   eligibility is exact even when `update` moved old rows into newer blocks. The counts are kept
//!   only while compression is on: every path that adds or removes an entry adjusts them behind one
//!   `Option` check, and switching compression on recounts them.
//! - **Jobs, never inline.** Blocks change state only through jobs: [`History::next_job`] copies
//!   the bytes of the oldest block whose state disagrees with its eligibility, [`Job::run`]
//!   compresses or decompresses the copy with no access to the history, and [`History::install`]
//!   swaps the result in (a `Vec` swap: it never compresses, decompresses or copies). Setting
//!   changes only mark blocks. `scan` is the oldest block that may disagree (every older one
//!   agrees), so [`History::has_jobs`] is one comparison and the search resumes where it stopped.
//!   Whatever can make a block disagree lowers `scan` to it: a block's warm count reaching zero, a
//!   cold block's leaving zero, a new block making the previous newest one eligible, and a setting
//!   change (from the front). A job left uninstalled is offered again.
//! - **The identity rule.** A job carries its block's sequence number and `generation`, a value
//!   from a history-wide counter that never repeats (not even across `clear`, which resets
//!   `front_seq`). A block gets a new generation when it is created, changes state, loses bytes (a
//!   truncated last record) or becomes the newest block again; bytes are only appended to a block
//!   while it is the newest, which no job targets. So an unchanged identity means the job's copy is
//!   still the block's bytes. `install` also requires the same setting and that the block still
//!   wants the job's state; otherwise it drops the result and returns `false`. Sequence numbers and
//!   generations are only unique within one history: a clone copies them, and a reflow's new
//!   history (`sibling`) starts over at 0. So a job also carries its history's `id`, a process-wide
//!   unique value given to every new history, clone and sibling (kept by `clear`), and `install`
//!   refuses a job from any other history.
//! - **Leaving the cold state.** A cold block whose `live` reaches 0 is freed, never kept as a
//!   spare. A cold block that becomes the newest again (popping rows past the warm window) is
//!   decompressed at once. `update` of a cold row decodes it through the cache and re-encodes it
//!   into the newest block; its old bytes stay as dead bytes in the cold block.
//! - **Read cache.** Every read ends at `record`, which for a cold block takes the bytes from a
//!   cache of at most [`CACHE_BLOCKS`] decompressed blocks keyed by identity, least recently used
//!   first, in a `RefCell` (reads take `&self`; `History` stays `Send`, not `Sync`). A cached block
//!   is an `Arc<Vec<u8>>`, so a read holds its record without holding the `RefCell` borrow, and an
//!   evicted buffer is reused when no read still holds it. The cache is cleared by `clear`, a lower
//!   `set_limit` and switching compression off; an install that warms a cached block, and freeing a
//!   cold block, drop its entry.
//! - **Off means off.** With `None` (the default) and no cold block left, no job exists, `has_jobs`
//!   is false and the paths above cost one `Option` or `cold` check each.
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

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::{Deref, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt, mem};

use crate::grid::compact::{self, PlainText, RowHeader, read_header};
use crate::grid::{Dimensions, Grid, HistoryCell, ReflowInfo, Row, cold};
use crate::index::{Column, Line};
use crate::term::cell::Cell;

/// Size of a history block in bytes, unless a test asks for another one.
pub(crate) const DEFAULT_BLOCK_BYTES: usize = 64 * 1024;

/// Most spare blocks a history keeps for its next records.
pub(crate) const MAX_SPARES: usize = 2;

/// Index capacity a history starts with once it stores a row (it grows by doubling from there,
/// up to the limit).
const MIN_INDEX_CAPACITY: usize = 64;

/// Most decompressed cold blocks a history keeps for reads (design spec 5.5).
pub(crate) const CACHE_BLOCKS: usize = 4;

/// A value no other history in the process has, for `History::id`.
fn new_history_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A byte block holding records back to back.
struct Block {
    /// The records (warm), or their compressed form shrunk to fit (cold, see `cold.rs`).
    bytes: Vec<u8>,

    /// Index entries pointing into this block.
    live: u32,

    /// Index entries pointing into this block whose rows are among the `keep_warm` newest. Kept
    /// only while compression is on (`set_compression` recounts it when switched on).
    warm_live: u32,

    /// Identity, with the sequence number, for jobs: a fresh value from the history's counter
    /// whenever the block is created, changes state, loses bytes or becomes the newest block
    /// again.
    generation: u64,

    /// Holds a single record larger than a quarter block, in an allocation of exactly its size.
    oversized: bool,

    /// `bytes` is the compressed form of the records.
    cold: bool,
}

/// A decompressed cold block, in [`History`]'s read cache.
struct Cached {
    seq: u32,
    generation: u64,
    bytes: Arc<Vec<u8>>,
}

/// Decompressed cold blocks for reads, least recently used first, at most [`CACHE_BLOCKS`].
#[derive(Default)]
struct Cache {
    entries: Vec<Cached>,
}

impl Cache {
    /// Forget the block `seq` (it was warmed or freed).
    fn remove(&mut self, seq: u32) {
        self.entries.retain(|cached| cached.seq != seq);
    }
}

/// The bytes of one record: borrowed from a warm block, or held in a decompressed cold block
/// (which a read can keep while the cache moves on).
enum Record<'a> {
    Warm(&'a [u8]),
    Cold(Arc<Vec<u8>>, Range<usize>),
}

impl Deref for Record<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            Record::Warm(bytes) => bytes,
            Record::Cold(block, range) => &block[range.clone()],
        }
    }
}

/// A block that needs to change state (go cold, or come back warm), with a copy of its bytes:
/// from [`History::next_job`], run anywhere with [`Job::run`], installed with
/// [`History::install`].
pub struct Job {
    /// The history the job was taken from (`History::id`).
    history: u64,
    seq: u32,
    generation: u64,
    compression: Option<usize>,
    to_cold: bool,
    bytes: Vec<u8>,

    /// Capacity of the warm block a warming job makes.
    capacity: usize,
}

/// The result of a [`Job`], for [`History::install`].
pub struct Done {
    history: u64,
    seq: u32,
    generation: u64,
    compression: Option<usize>,
    to_cold: bool,
    bytes: Vec<u8>,
}

impl Job {
    /// Compress or decompress the copied block. Needs no access to the history, so it can run
    /// outside the terminal's lock.
    pub fn run(self) -> Done {
        let bytes = if self.to_cold {
            let mut cold = Vec::new();
            cold::compress(&self.bytes, &mut cold);
            cold.shrink_to_fit();
            cold
        } else {
            let mut warm = Vec::with_capacity(self.capacity);
            cold::decompress(&self.bytes, &mut warm);
            warm
        };
        Done {
            history: self.history,
            seq: self.seq,
            generation: self.generation,
            compression: self.compression,
            to_cold: self.to_cold,
            bytes,
        }
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("history", &self.history)
            .field("seq", &self.seq)
            .field("generation", &self.generation)
            .field("to_cold", &self.to_cold)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl fmt::Debug for Done {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Done")
            .field("history", &self.history)
            .field("seq", &self.seq)
            .field("generation", &self.generation)
            .field("to_cold", &self.to_cold)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Where one history row is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    /// Sequence number of the block holding the record.
    block: u32,

    /// Start of the record in the block.
    offset: u32,

    /// Length of the record in bytes in the low 30 bits (the record header holds no byte
    /// length), with the header's `WRAPS` bit at bit 31 and `CLEAR` bit at bit 30, so that
    /// `wraps` and `is_clear` never read (or decompress) a record. Use the accessors.
    len_flags: u32,

    /// Position of the row's first extra in the extras queue (a wrapping counter).
    first_extra: u32,
}

impl Entry {
    const CLEAR: u32 = 1 << 30;
    const LEN: u32 = Self::CLEAR - 1;
    const WRAPS: u32 = 1 << 31;

    /// An entry for the record `record` of `len` bytes: the header bits are copied from it.
    #[inline]
    fn new(block: u32, offset: usize, record: &[u8], first_extra: u32) -> Entry {
        let len = record.len();
        debug_assert!(len < 1 << 30, "history record of {len} bytes");
        let header = read_header(record);
        let mut len_flags = len as u32;
        if header.wraps {
            len_flags |= Self::WRAPS;
        }
        if header.clear {
            len_flags |= Self::CLEAR;
        }
        Entry { block, offset: offset as u32, len_flags, first_extra }
    }

    /// Length of the record in bytes.
    #[inline]
    fn len(self) -> usize {
        (self.len_flags & Self::LEN) as usize
    }

    /// The record header's `WRAPS` bit.
    #[inline]
    fn wraps(self) -> bool {
        self.len_flags & Self::WRAPS != 0
    }

    /// The record header's `CLEAR` bit.
    #[inline]
    fn clear(self) -> bool {
        self.len_flags & Self::CLEAR != 0
    }
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

    /// Rows kept warm (see "Cold blocks" in the module docs); `None` never compresses.
    compression: Option<usize>,

    /// The last block generation handed out.
    generation: u64,

    /// This history's identity for jobs: unique in the process, fresh for a clone and for a
    /// `sibling`, kept by `clear` (see "The identity rule" in the module docs).
    id: u64,

    /// Sequence number of the oldest block that may disagree with its eligibility; every older
    /// block agrees. `None` when every block agrees (`has_jobs` is false).
    scan: Option<u32>,

    /// Decompressed cold blocks for reads.
    cache: RefCell<Cache>,
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
            compression: None,
            generation: 0,
            id: new_history_id(),
            scan: None,
            cache: RefCell::default(),
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

    /// Change the maximum number of rows; a lower limit evicts the oldest rows now and frees
    /// the blocks and index capacity they used (keeping at most `MAX_SPARES` spare blocks).
    pub fn set_limit(&mut self, limit: usize) {
        let lower = limit < self.limit;
        self.limit = limit;
        self.remove_oldest(self.index.len().saturating_sub(limit));
        if lower {
            self.index.shrink_to(limit);
            self.extras.shrink_to_fit();
            self.cache.get_mut().entries.clear();
        }
    }

    /// Keep the `keep_warm` newest rows uncompressed and compress the blocks older than that;
    /// `None` never compresses (and warms the cold blocks there are). It only marks blocks for
    /// jobs ([`History::next_job`]): nothing is compressed or decompressed here.
    pub fn set_compression(&mut self, keep_warm: Option<usize>) {
        if keep_warm == self.compression {
            return;
        }

        self.compression = keep_warm;
        match keep_warm {
            Some(keep) => {
                // Recount the warm rows of every block (not kept while compression was off).
                self.blocks.iter_mut().for_each(|block| block.warm_live = 0);
                for entry in self.index.iter().rev().take(keep) {
                    self.blocks[entry.block.wrapping_sub(self.front_seq) as usize].warm_live += 1;
                }
            },
            None => self.cache.get_mut().entries.clear(),
        }
        self.scan = (!self.blocks.is_empty()).then_some(self.front_seq);
    }

    /// Rows kept warm, `None` when compression is off (see [`History::set_compression`]).
    #[inline]
    pub fn compression(&self) -> Option<usize> {
        self.compression
    }

    /// Whether [`History::next_job`] may have a job. False means every block is in the state
    /// the setting asks for. O(1), so a host can check after every batch of output.
    #[inline]
    pub fn has_jobs(&self) -> bool {
        self.scan.is_some()
    }

    /// The next block that needs to change state, with a copy of its bytes, or `None` when every
    /// block is in the state the setting asks for. Taking a job changes nothing: until it is
    /// installed, the next call offers the same block again.
    pub fn next_job(&mut self) -> Option<Job> {
        let start = self.position_of(self.scan?);
        let Some(at) = (start..self.blocks.len()).find(|&at| self.disagrees(at)) else {
            self.scan = None;
            return None;
        };

        let seq = self.front_seq.wrapping_add(at as u32);
        self.scan = Some(seq);
        let block = &self.blocks[at];
        let capacity = match block {
            Block { cold: true, oversized: true, bytes, .. } => cold::uncompressed_len(bytes),
            _ => self.block_bytes,
        };
        Some(Job {
            history: self.id,
            seq,
            generation: block.generation,
            compression: self.compression,
            to_cold: !block.cold,
            bytes: block.bytes.clone(),
            capacity,
        })
    }

    /// Swap the result of a job in, if it was taken from this history, its block still exists
    /// with the same identity, the setting is the one the job was taken under and the block still
    /// wants that state. Returns whether it did; otherwise the result is dropped (the job came
    /// from another history, or the block was evicted, cleared, changed or the setting changed
    /// meanwhile). Never compresses, decompresses or copies a block.
    pub fn install(&mut self, done: Done) -> bool {
        if done.history != self.id {
            return false;
        }

        let at = done.seq.wrapping_sub(self.front_seq) as usize;
        let current = self.blocks.get(at).is_some_and(|block| {
            block.live > 0 && block.generation == done.generation && block.cold != done.to_cold
        });
        if !current || done.compression != self.compression || self.wants_cold(at) != done.to_cold {
            return false;
        }

        let generation = self.next_generation();
        let block = &mut self.blocks[at];
        let old = mem::replace(&mut block.bytes, done.bytes);
        block.cold = done.to_cold;
        block.generation = generation;
        if done.to_cold {
            // The warm buffer goes to the pool like an evicted block's.
            if !block.oversized {
                self.recycle(old);
            }
        } else {
            self.cache.get_mut().remove(done.seq);
        }
        true
    }

    /// Run and install every job now (tests, and hosts that want no background work).
    pub fn run_jobs(&mut self) {
        self.run_jobs_with(Job::run);
    }

    /// `run_jobs`, running each job with `run`. Nothing changes between taking a job and
    /// installing it here, so an install is never refused; if one ever were, the same job would
    /// be offered again forever, so it stops there instead (the tests' `run_and_check` asserts
    /// that no job is left).
    fn run_jobs_with(&mut self, mut run: impl FnMut(Job) -> Done) {
        while let Some(job) = self.next_job() {
            if !self.install(run(job)) {
                break;
            }
        }
    }

    /// An empty history built like this one: the same block size and compression setting, with
    /// limit `limit` (a reflow's new history, or the empty one standing in while it runs). It has
    /// its own identity, so jobs taken from this history are refused there. Allocates nothing.
    pub(crate) fn sibling(&self, limit: usize) -> Self {
        let mut sibling = Self::with_block_size(limit, self.block_bytes);
        sibling.compression = self.compression;
        sibling
    }

    /// Whether each block, oldest first, is cold (test hook, not a stable API).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn cold_blocks(&self) -> Vec<bool> {
        self.blocks.iter().map(|block| block.cold).collect()
    }

    /// Decompressed blocks in the read cache (test hook, not a stable API).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn cached_blocks(&self) -> usize {
        self.cache.borrow().entries.len()
    }

    /// Drop every decompressed block from the read cache (tests).
    #[cfg(test)]
    pub(crate) fn clear_cache(&mut self) {
        self.cache.get_mut().entries = Vec::new();
    }

    /// Whether the block at `at` in `blocks` should be cold: compression is on, it holds rows,
    /// none of them is among the `keep_warm` newest, and it is not the newest block.
    #[inline]
    fn wants_cold(&self, at: usize) -> bool {
        let block = &self.blocks[at];
        self.compression.is_some()
            && block.live > 0
            && block.warm_live == 0
            && at + 1 < self.blocks.len()
    }

    /// Whether the block at `at` holds rows and is not in the state it should be in.
    #[inline]
    fn disagrees(&self, at: usize) -> bool {
        self.blocks[at].live > 0 && self.blocks[at].cold != self.wants_cold(at)
    }

    /// Position in `blocks` of the block `seq`, 0 for one already gone from the front.
    #[inline]
    fn position_of(&self, seq: u32) -> usize {
        let at = seq.wrapping_sub(self.front_seq);
        if (at as i32) < 0 { 0 } else { at as usize }
    }

    /// Note that the block `seq` may now disagree with its eligibility.
    fn mark(&mut self, seq: u32) {
        match self.scan {
            Some(scan) if self.position_of(scan) <= self.position_of(seq) => {},
            _ => self.scan = Some(seq),
        }
    }

    /// A fresh block generation.
    #[inline]
    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// One more row of the block `seq` is among the warm rows.
    fn warm_enter(&mut self, seq: u32) {
        let block = &mut self.blocks[seq.wrapping_sub(self.front_seq) as usize];
        block.warm_live += 1;
        if block.warm_live == 1 && block.cold {
            self.mark(seq);
        }
    }

    /// One row of the block `seq` left the warm rows.
    fn warm_leave(&mut self, seq: u32) {
        let at = seq.wrapping_sub(self.front_seq) as usize;
        let block = &mut self.blocks[at];
        block.warm_live -= 1;
        if block.warm_live == 0 && !block.cold && at + 1 < self.blocks.len() {
            self.mark(seq);
        }
    }

    /// Account for the row just pushed as the newest: it is warm, and the row now `keep` rows
    /// from the newest left the warm rows.
    fn pushed(&mut self, keep: usize) {
        let len = self.index.len();
        if keep == 0 {
            return;
        }
        self.warm_enter(self.index[len - 1].block);
        if len > keep {
            self.warm_leave(self.index[len - 1 - keep].block);
        }
    }

    /// Account for the newest row `entry`, just removed from the index (still in its block):
    /// it leaves the warm rows, and the row `keep - 1` rows from the newest comes back in.
    fn popped(&mut self, keep: usize, entry: Entry) {
        let len = self.index.len();
        if keep == 0 {
            return;
        }
        self.warm_leave(entry.block);
        if len >= keep {
            self.warm_enter(self.index[len - keep].block);
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
        let entry = match self.encode_in_place(row, first_extra) {
            Some(entry) => entry,
            None => {
                self.encoded.clear();
                T::encode(row, &mut self.encoded, &mut self.extras);
                self.store(first_extra)
            },
        };
        self.reserve_index();
        self.index.push_back(entry);
        if let Some(keep) = self.compression {
            self.pushed(keep);
        }
    }

    /// Encode `row` straight into the newest block when that block surely has room for it
    /// (`HistoryCell::max_record_bytes`), saving the copy from the scratch buffer; `None`, having
    /// done nothing, when it may not. A record that turns out larger than a quarter block is
    /// moved to a block of its own, as `store` would have placed it.
    #[inline]
    fn encode_in_place(&mut self, row: &Row<T>, first_extra: u32) -> Option<Entry> {
        let bound = T::max_record_bytes(row.len())?;
        let block_seq = self.front_seq.wrapping_add(self.blocks.len() as u32).wrapping_sub(1);
        let block = self
            .blocks
            .back_mut()
            .filter(|block| !block.oversized && self.block_bytes - block.bytes.len() >= bound)?;

        let offset = block.bytes.len();
        T::encode(row, &mut block.bytes, &mut self.extras);
        debug_assert_eq!(block.bytes.capacity(), self.block_bytes, "a block reallocated");
        let len = block.bytes.len() - offset;
        if len > self.block_bytes / 4 {
            self.encoded.clear();
            self.encoded.extend_from_slice(&block.bytes[offset..]);
            block.bytes.truncate(offset);
            return Some(self.store(first_extra));
        }

        block.live += 1;
        Some(Entry::new(block_seq, offset, &block.bytes[offset..], first_extra))
    }

    /// Make room for one more index entry. The index grows by doubling, as a `VecDeque` does,
    /// but never past the limit, so a full history's index holds exactly `limit` entries (a
    /// plain doubling would reach 16,384 entries, 256 KiB, for a limit of 10,000).
    #[inline]
    fn reserve_index(&mut self) {
        let len = self.index.len();
        if len == self.index.capacity() {
            let target = (len * 2).max(MIN_INDEX_CAPACITY).min(self.limit).max(len + 1);
            self.index.reserve_exact(target - len);
        }
    }

    /// Remove the newest row into `into` (reusing its allocation when the width matches).
    /// Returns `false`, leaving `into` untouched, when the history is empty.
    pub fn pop_newest(&mut self, into: &mut Row<T>) -> bool {
        let Some(entry) = self.index.pop_back() else {
            return false;
        };

        self.decode(entry, self.extras_end(), into);
        if let Some(keep) = self.compression {
            self.popped(keep, entry);
        }
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
        let count = count.min(self.index.len());
        if let Some(keep) = self.compression {
            for _ in 0..count {
                if let Some(entry) = self.index.pop_front() {
                    // It was warm when every row was.
                    if self.index.len() < keep {
                        self.warm_leave(entry.block);
                    }
                    self.release(entry);
                }
            }
        } else {
            for _ in 0..count {
                if let Some(entry) = self.index.pop_front() {
                    self.release(entry);
                }
            }
        }

        // The extras of the removed rows are those before the new oldest row's (usually none or
        // a few, which popping one by one removes faster than a `drain`).
        let keep = self.index.front().map_or_else(|| self.extras_end(), |entry| entry.first_extra);
        let removed = keep.wrapping_sub(self.extras_base) as usize;
        if removed <= 8 {
            for _ in 0..removed {
                self.extras.pop_front();
            }
        } else {
            self.extras.drain(..removed);
        }
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
                    T::decode_cell(&bytes, extras, column.0)
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
        let warm = self.compression.is_some_and(|keep| self.index.len() - pos <= keep);
        if warm {
            self.warm_leave(entry.block);
        }
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

        let new = self.store(entry.first_extra);
        self.index[pos] = new;
        if warm {
            self.warm_enter(new.block);
        }
        self.updated = row;
    }

    /// [`HistoryCell::reflow_info`] of the oldest row, `None` when the history is empty or the
    /// format has no reflow fast path.
    pub(crate) fn oldest_reflow_info(&self) -> Option<ReflowInfo> {
        self.index.front().and_then(|&entry| T::reflow_info(&self.record(entry)))
    }

    /// The header of the newest row, `None` when the history is empty.
    pub(crate) fn newest_header(&self) -> Option<RowHeader> {
        self.index.back().map(|&entry| read_header(&self.record(entry)))
    }

    /// Move the oldest row to `to` as its newest row without decoding it: its record is copied
    /// with the width changed to `width` (`compact::copy_with_width`), its extras move along.
    /// The caller guarantees that the row with its width changed is exactly the record with `W`
    /// changed (see `HistoryCell::reflow_info`). `to` evicts its oldest row at its limit, as
    /// `push_newest` does.
    pub(crate) fn move_oldest_to(&mut self, to: &mut History<T>, width: usize) {
        let Some(entry) = self.index.pop_front() else {
            return;
        };

        to.encoded.clear();
        compact::copy_with_width(&self.record(entry), width, &mut to.encoded);
        if self.compression.is_some_and(|keep| self.index.len() < keep) {
            self.warm_leave(entry.block);
        }
        self.release(entry);

        // The oldest row's extras are at the front of the queue (`remove_oldest` drains the ones
        // before the oldest row's first).
        debug_assert_eq!(entry.first_extra, self.extras_base);
        let end = self.index.front().map_or_else(|| self.extras_end(), |next| next.first_extra);
        let count = end.wrapping_sub(entry.first_extra) as usize;
        self.extras_base = end;
        to.push_encoded(self.extras.drain(..count));
    }

    /// Append the record in `encoded`, whose extras are `extras`, as the newest row (evicting the
    /// oldest at the limit, storing nothing at a limit of 0).
    fn push_encoded(&mut self, extras: impl Iterator<Item = T::Extra>) {
        if self.limit == 0 {
            return;
        }
        if self.index.len() >= self.limit {
            self.remove_oldest(self.index.len() + 1 - self.limit);
        }

        let first_extra = self.extras_end();
        self.extras.extend(extras);
        let entry = self.store(first_extra);
        self.reserve_index();
        self.index.push_back(entry);
        if let Some(keep) = self.compression {
            self.pushed(keep);
        }
    }

    /// Remove the newest row without decoding it (a reflow dropping rows it truncates). Returns
    /// `false` when the history is empty.
    pub(crate) fn remove_newest(&mut self) -> bool {
        let Some(entry) = self.index.pop_back() else {
            return false;
        };

        if let Some(keep) = self.compression {
            self.popped(keep, entry);
        }
        self.release(entry);
        self.extras.truncate(entry.first_extra.wrapping_sub(self.extras_base) as usize);
        true
    }

    /// Take `other`'s spare blocks while this history has room for them and they are its block
    /// size: a reflow hands the blocks its old history frees to the new one instead of returning
    /// them to the allocator and allocating again.
    pub(crate) fn take_spares(&mut self, other: &mut History<T>) {
        while self.spares.len() < MAX_SPARES && other.block_bytes == self.block_bytes {
            let Some(spare) = other.spares.pop() else {
                break;
            };
            self.recycle(spare);
        }
    }

    /// Lower the limit to `limit` and evict the rows above it, keeping the index and extras
    /// capacity (a reflow's history, built with a limit `lines` above the grid's, gets the
    /// grid's limit at the end; shrinking the index there would only make it grow again).
    pub(crate) fn trim_to_limit(&mut self, limit: usize) {
        debug_assert!(limit <= self.limit);
        self.limit = limit;
        self.remove_oldest(self.index.len().saturating_sub(limit));
    }

    /// Whether row `i` (0 is the newest) continues on the next line (the record header's
    /// `WRAPS` bit); "no row" past the end does not.
    pub fn wraps(&self, i: usize) -> bool {
        self.position(i).is_some_and(|pos| self.index[pos].wraps())
    }

    /// Whether every cell of row `i` (0 is the newest) is empty (the record header's `CLEAR`
    /// bit); "no row" past the end is.
    pub fn is_clear(&self, i: usize) -> bool {
        self.position(i).is_none_or(|pos| self.index[pos].clear())
    }

    /// Blocks that still hold a buffer (tests: the old store's blocks are freed while a reflow
    /// pops its rows).
    #[cfg(test)]
    pub(crate) fn blocks_held(&self) -> usize {
        self.blocks.iter().filter(|block| block.bytes.capacity() > 0).count()
    }

    /// Remove every row; the limit and the compression setting stay. Frees the index, the
    /// extras, the read cache and every block but at most `MAX_SPARES` spares (never a cold one).
    pub fn clear(&mut self) {
        self.index = VecDeque::new();
        self.extras = VecDeque::new();
        self.extras_base = 0;
        for block in mem::take(&mut self.blocks) {
            if !block.cold {
                self.recycle(block.bytes);
            }
        }
        self.front_seq = 0;
        self.next_generation();
        self.scan = None;
        self.cache.get_mut().entries = Vec::new();
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

    /// The record `entry` points to, read through the cache when its block is cold.
    #[inline]
    fn record(&self, entry: Entry) -> Record<'_> {
        let block = &self.blocks[entry.block.wrapping_sub(self.front_seq) as usize];
        let range = entry.offset as usize..entry.offset as usize + entry.len();
        if block.cold {
            Record::Cold(self.decompressed(entry.block, block), range)
        } else {
            Record::Warm(&block.bytes[range])
        }
    }

    /// The decompressed bytes of the cold block `seq`, from the cache, or decompressed into it
    /// (evicting the least recently used block, whose buffer is reused when no read holds it).
    #[cold]
    #[inline(never)]
    fn decompressed(&self, seq: u32, block: &Block) -> Arc<Vec<u8>> {
        let mut cache = self.cache.borrow_mut();
        let entries = &mut cache.entries;
        let hit = entries
            .iter()
            .position(|cached| cached.seq == seq && cached.generation == block.generation);
        if let Some(at) = hit {
            let cached = entries.remove(at);
            let bytes = Arc::clone(&cached.bytes);
            entries.push(cached);
            return bytes;
        }

        if entries.capacity() == 0 {
            entries.reserve_exact(CACHE_BLOCKS);
        }
        let mut bytes =
            if entries.len() >= CACHE_BLOCKS { entries.remove(0).bytes } else { Arc::default() };
        match Arc::get_mut(&mut bytes) {
            Some(buffer) => cold::decompress(&block.bytes, buffer),
            None => {
                let mut buffer = Vec::new();
                cold::decompress(&block.bytes, &mut buffer);
                bytes = Arc::new(buffer);
            },
        }
        entries.push(Cached { seq, generation: block.generation, bytes: Arc::clone(&bytes) });
        bytes
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
        // The entry's copies of the header bits must be the record's (`wraps` and `is_clear`
        // answer from them).
        debug_assert!(
            {
                let header = read_header(&bytes);
                (header.wraps, header.clear) == (entry.wraps(), entry.clear())
            },
            "history entry header bits disagree with its record"
        );
        let used = self.with_extras(entry, extras_end, |extras| T::decode(&bytes, extras, into));
        // A wrong offset or length must not decode silently.
        debug_assert_eq!(used, entry.len(), "history record length disagrees with index");
    }

    /// Copy the record in `encoded` to a block and return its entry: into the newest block when
    /// it fits there, else into a new block (a spare when there is one); a record larger than a
    /// quarter block into a block of its own.
    fn store(&mut self, first_extra: u32) -> Entry {
        let len = self.encoded.len();
        let oversized = len > self.block_bytes / 4;
        let fits = |block: &Block| !block.oversized && block.bytes.len() + len <= self.block_bytes;
        if oversized || !self.blocks.back().is_some_and(fits) {
            let bytes = if oversized {
                Vec::with_capacity(len)
            } else {
                self.spares.pop().unwrap_or_else(|| Vec::with_capacity(self.block_bytes))
            };
            self.push_block(bytes, oversized);
        }

        let block_seq = self.front_seq.wrapping_add(self.blocks.len() as u32 - 1);
        let block = self.blocks.back_mut().expect("a block was just ensured");
        debug_assert!(!block.cold, "the newest block is cold");
        let offset = block.bytes.len();
        block.bytes.extend_from_slice(&self.encoded);
        block.live += 1;
        debug_assert!(oversized || block.bytes.len() <= self.block_bytes);

        Entry::new(block_seq, offset, &self.encoded, first_extra)
    }

    /// Append a new, empty block with buffer `bytes` as the newest block. The block that was
    /// newest may now be eligible to go cold.
    fn push_block(&mut self, bytes: Vec<u8>, oversized: bool) {
        if self.compression.is_some() && self.blocks.back().is_some_and(|back| back.warm_live == 0)
        {
            self.mark(self.front_seq.wrapping_add(self.blocks.len() as u32 - 1));
        }
        let generation = self.next_generation();
        let block = Block { bytes, live: 0, warm_live: 0, generation, oversized, cold: false };
        self.blocks.push_back(block);
    }

    /// Account for `entry` no longer pointing into its block: give its bytes back when they are
    /// the last ones of a warm block, recycle the block's buffer (free it, when cold) when no
    /// entry points into it any more, and remove the emptied blocks at either end of the deque.
    /// A block that becomes the newest again gets a new generation, and is warmed now if cold.
    fn release(&mut self, entry: Entry) {
        let generation = self.generation + 1;
        let block = &mut self.blocks[entry.block.wrapping_sub(self.front_seq) as usize];
        block.live -= 1;
        if !block.cold && entry.offset as usize + entry.len() == block.bytes.len() {
            block.bytes.truncate(entry.offset as usize);
            // A job may hold a copy of the longer bytes.
            block.generation = generation;
            self.generation = generation;
        }
        if block.live == 0 {
            // A block in the middle of the deque stays as an empty placeholder (its sequence
            // number must stay taken) until it reaches an end, but its buffer is reused now.
            let bytes = mem::take(&mut block.bytes);
            if mem::take(&mut block.cold) {
                self.cache.get_mut().remove(entry.block);
            } else {
                self.recycle(bytes);
            }
        }

        while self.blocks.front().is_some_and(|block| block.live == 0) {
            self.blocks.pop_front();
            self.front_seq = self.front_seq.wrapping_add(1);
        }
        if self.blocks.back().is_some_and(|block| block.live == 0) {
            while self.blocks.back().is_some_and(|block| block.live == 0) {
                self.blocks.pop_back();
            }
            self.newest_again();
        }
    }

    /// The newest block was removed: the block now newest will take records again, so its
    /// copies in jobs go stale, and it must be warm (rare: popping rows past the warm window).
    fn newest_again(&mut self) {
        let generation = self.next_generation();
        let block_bytes = self.block_bytes;
        let seq = self.front_seq.wrapping_add(self.blocks.len() as u32).wrapping_sub(1);
        let Some(block) = self.blocks.back_mut() else {
            return;
        };
        block.generation = generation;
        if !block.cold {
            return;
        }

        let mut warm = match block.oversized {
            true => Vec::with_capacity(cold::uncompressed_len(&block.bytes)),
            false => self.spares.pop().unwrap_or_else(|| Vec::with_capacity(block_bytes)),
        };
        cold::decompress(&block.bytes, &mut warm);
        block.bytes = warm;
        block.cold = false;
        self.cache.get_mut().remove(seq);
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

#[cfg(feature = "test-hooks")]
impl<T: HistoryCell> History<T> {
    /// The bytes of every block, oldest first, partly filled ones included, as stored (cold
    /// blocks compressed) (test hook, not a stable API: it exists so tests can measure
    /// compressors on real block contents).
    pub fn block_bytes_for_test(&self) -> Vec<&[u8]> {
        self.blocks.iter().map(|block| block.bytes.as_slice()).collect()
    }
}

impl History<Cell> {
    /// `f` of row `i` (0 is the newest) as plain text read straight from its record, when it
    /// qualifies (see `compact::plain_text`); `None` when it does not or is not there. A callback,
    /// because a cold row's record lives in the read cache only while it is read.
    pub(crate) fn plain_text<R>(&self, i: usize, f: impl FnOnce(PlainText<'_>) -> R) -> Option<R> {
        let pos = self.position(i)?;
        compact::plain_text(&self.record(self.index[pos])).map(f)
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
                    Block { cold: true, bytes, .. } | Block { oversized: true, bytes, .. } => {
                        bytes.len()
                    },
                    Block { .. } => self.block_bytes,
                };
                let mut bytes = Vec::with_capacity(capacity);
                bytes.extend_from_slice(&block.bytes);
                Block { bytes, ..*block }
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
            compression: self.compression,
            generation: self.generation,
            id: new_history_id(),
            scan: self.scan,
            cache: RefCell::default(),
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
            .field("compression", &self.compression)
            .field("cold", &self.blocks.iter().filter(|block| block.cold).count())
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

        // Each block's uncompressed bytes, decompressed here (not through the read cache, which
        // tests count).
        let warm: Vec<Vec<u8>> = history
            .blocks
            .iter()
            .map(|block| match block.cold {
                true => {
                    let mut warm = Vec::new();
                    cold::decompress(&block.bytes, &mut warm);
                    assert_eq!(warm.len(), cold::uncompressed_len(&block.bytes));
                    warm
                },
                false => block.bytes.clone(),
            })
            .collect();

        let mut live = vec![0; history.blocks.len()];
        let mut warm_live = vec![0; history.blocks.len()];
        for (pos, entry) in history.index.iter().enumerate() {
            let at = entry.block.wrapping_sub(history.front_seq) as usize;
            let end = entry.offset as usize + entry.len();
            assert!(end <= warm[at].len(), "a record reaches past its block");
            let header = read_header(&warm[at][entry.offset as usize..end]);
            assert_eq!(
                (header.wraps, header.clear),
                (entry.wraps(), entry.clear()),
                "index entry {pos}: header bits disagree with its record"
            );
            live[at] += 1;
            if history.compression.is_some_and(|keep| history.index.len() - pos <= keep) {
                warm_live[at] += 1;
            }
        }
        for (at, (block, live)) in history.blocks.iter().zip(live).enumerate() {
            assert_eq!(block.live as usize, live, "live count");
            if history.compression.is_some() {
                assert_eq!(block.warm_live, warm_live[at], "warm count of block {at}");
            }
            if live == 0 {
                assert_eq!(block.bytes.capacity(), 0, "an empty block kept its buffer");
                assert!(!block.cold, "an empty block is cold");
            } else if block.cold {
                assert!(at + 1 < history.blocks.len(), "the newest block is cold");
                assert_eq!(block.bytes.len(), block.bytes.capacity(), "cold bytes not shrunk");
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
        assert_eq!(oversized.bytes.len(), history.index[1].len());
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
        let last = history.index.back().unwrap().len();

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
        assert_eq!(entry.offset as usize + entry.len(), history.blocks[1].bytes.len());
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
        matches_dense_model_with(TINY, 12);
    }

    /// The same with 4 KiB blocks, where most rows are encoded straight into their block
    /// (`encode_in_place`; with tiny blocks the bound never fits and every row goes through the
    /// scratch buffer), and rows up to 160 cells, some of them oversized.
    #[test]
    fn matches_dense_model_in_place() {
        matches_dense_model_with(4096, 160);
    }

    /// The same with compression switched on, off and changed at random, and jobs taken, run,
    /// installed, dropped or installed late at random points: rows still read as the model's,
    /// and once every job is run each block's state agrees with its eligibility.
    #[test]
    fn matches_dense_model_compressed() {
        matches_dense_model_compressed_with(TINY, 12);
        matches_dense_model_compressed_with(4096, 160);
    }

    fn matches_dense_model_with(block_bytes: usize, max_width: usize) {
        dense_model(block_bytes, max_width, false);
    }

    fn matches_dense_model_compressed_with(block_bytes: usize, max_width: usize) {
        dense_model(block_bytes, max_width, true);
    }

    fn dense_model(block_bytes: usize, max_width: usize, compress: bool) {
        let mut rng = ChaCha8Rng::seed_from_u64(0x5704e);
        let links = cell_links();
        let mut history = History::<Cell>::with_block_size(60, block_bytes);
        let mut model: VecDeque<Row<Cell>> = VecDeque::new();
        let mut into = Row::default();
        let mut held: Option<Done> = None;
        for step in 0..20_000 {
            let width = rng.gen_range(1..=max_width);
            if compress {
                match rng.gen_range(0..100) {
                    0..=1 => {
                        let keep = [None, Some(0), Some(1), Some(5), Some(37)];
                        history.set_compression(keep[rng.gen_range(0..keep.len())]);
                    },
                    // Take a job and keep its result for later (installed after whatever the
                    // next steps do, or dropped).
                    2..=9 => held = history.next_job().map(Job::run),
                    10..=17 => {
                        if let Some(done) = held.take() {
                            history.install(done);
                        }
                    },
                    18..=29 => {
                        if let Some(job) = history.next_job() {
                            assert!(history.install(job.run()), "step {step}: a fresh job");
                        }
                    },
                    30 => run_and_check(&mut history),
                    _ => {},
                }
            }
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

    /// `push_newest` encodes straight into the newest block when it has room for the largest
    /// record of the row, through the scratch buffer when it has not (the record still lands in
    /// that block when it fits), and moves a record that turns out oversized to its own block.
    #[test]
    fn encode_in_place_conditions() {
        let bound = Cell::max_record_bytes(100).unwrap();
        let block_bytes = 4 * 1024;
        assert!(bound < block_bytes && bound > block_bytes / 2);
        let mut history = History::<Cell>::with_block_size(100, block_bytes);
        let mut row = Row::<Cell>::new(100);
        row[Column(0)].c = 'a';

        // The first row starts a block (scratch path, there is no block yet); the second goes
        // in place, right behind it.
        history.push_newest(&row);
        history.push_newest(&row);
        check_store(&history);
        assert_eq!(history.blocks.len(), 1);
        let (first, second) = (history.index[0], history.index[1]);
        assert_eq!(second.offset, first.offset + first.len() as u32);
        assert!(history.encoded.capacity() > 0, "the first row went through the scratch buffer");

        // Fill the block until less than `bound` is left: the next small row takes the scratch
        // path but still lands in the same block.
        while block_bytes - history.blocks[0].bytes.len() >= bound {
            history.push_newest(&row);
        }
        let used = history.blocks[0].bytes.len();
        history.encoded = Vec::new();
        history.push_newest(&row);
        check_store(&history);
        assert_eq!(history.blocks.len(), 1, "a small record still fits the block");
        assert!(history.blocks[0].bytes.len() > used);
        assert!(history.encoded.capacity() > 0, "it went through the scratch buffer");

        // A record above a quarter block, encoded in place in a fresh block, moves to a block of
        // its own: every cell a different color.
        let mut history = History::<Cell>::with_block_size(100, block_bytes);
        let mut small = Row::<Cell>::new(100);
        small[Column(0)].c = 'b';
        history.push_newest(&small);
        let mut dense = Row::<Cell>::new(100);
        for (col, cell) in dense[..].iter_mut().enumerate() {
            cell.c = 'd';
            cell.fg = crate::vte::ansi::Color::Indexed(col as u8);
        }
        history.push_newest(&dense);
        history.push_newest(&small);
        check_store(&history);
        assert_eq!(history.blocks.len(), 3);
        assert!(history.blocks[1].oversized);
        assert!(history.index[1].len() > block_bytes / 4);
        let mut into = Row::default();
        history.read(1, &mut into);
        assert_eq!(into, dense);
        history.read(0, &mut into);
        assert_eq!(into, small);
        history.read(2, &mut into);
        assert_eq!(into, small);
    }

    /// The index never holds more entries than the limit (no doubling past it), and a small
    /// limit gets a small index.
    #[test]
    fn index_capacity_stops_at_limit() {
        for limit in [1, 10, 64, 100, 10_000] {
            let mut history = History::new(limit);
            for value in 0..limit + limit / 5 + 3 {
                history.push_newest(&wide(2, value));
            }
            assert_eq!(history.index.capacity(), limit, "limit {limit}");
        }
        let mut history = History::new(10_000);
        history.push_newest(&wide(2, 1));
        assert!(history.index.capacity() <= MIN_INDEX_CAPACITY);
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

    // Cold block tests (design spec 5.1, 5.5): tiny blocks, so a few hundred rows make dozens of
    // blocks, and small warm windows.

    /// A `usize` row of 1 to 20 cells with varied values (some of them zero, so some rows are
    /// clear; rows over 64 bytes are oversized).
    fn varied_row(rng: &mut ChaCha8Rng) -> Row<usize> {
        let mut row = Row::new(rng.gen_range(1..=20));
        if rng.gen_bool(0.9) {
            row[..].iter_mut().for_each(|cell| *cell = rng.gen_range(0..3000));
        }
        row
    }

    /// A history of `rows` varied rows on tiny blocks, with room for 5,000.
    fn varied(rows: usize) -> History<usize> {
        let mut rng = ChaCha8Rng::seed_from_u64(0xc01d);
        let mut history = History::with_block_size(5000, TINY);
        for _ in 0..rows {
            history.push_newest(&varied_row(&mut rng));
        }
        history
    }

    /// Every read of `history` equals the same read of `warm`.
    fn assert_same_rows<T: HistoryCell + fmt::Debug>(history: &History<T>, warm: &History<T>) {
        assert_eq!(history.len(), warm.len());
        let (mut a, mut b) = (Row::default(), Row::default());
        for i in 0..warm.len() {
            history.read(i, &mut a);
            warm.read(i, &mut b);
            assert_eq!(a, b, "row {i}");
            for col in 0..b.len() {
                assert_eq!(history.cell(i, Column(col)), b[Column(col)], "row {i} cell {col}");
            }
            assert_eq!(history.wraps(i), warm.wraps(i), "row {i} wraps");
            assert_eq!(history.is_clear(i), warm.is_clear(i), "row {i} clear");
        }
    }

    /// Which blocks should be cold with `keep` warm rows: those holding rows, other than the
    /// newest block, none of whose rows is among the `keep` newest.
    fn eligible<T: HistoryCell>(history: &History<T>, keep: usize) -> Vec<bool> {
        let mut eligible: Vec<bool> = history.blocks.iter().map(|block| block.live > 0).collect();
        if let Some(newest) = eligible.last_mut() {
            *newest = false;
        }
        for entry in history.index.iter().rev().take(keep) {
            eligible[entry.block.wrapping_sub(history.front_seq) as usize] = false;
        }
        eligible
    }

    /// Run every job and check each block's state against eligibility.
    fn run_and_check<T: HistoryCell>(history: &mut History<T>) {
        history.run_jobs();
        assert!(!history.has_jobs());
        let expected = match history.compression() {
            Some(keep) => eligible(history, keep),
            None => vec![false; history.blocks.len()],
        };
        assert_eq!(history.cold_blocks(), expected);
        check_store(history);
    }

    #[test]
    fn cold_rows_read_identically() {
        let mut history = varied(2000);
        let warm = history.clone();
        history.set_compression(Some(10));
        assert_eq!(history.compression(), Some(10));
        assert!(history.has_jobs());
        assert!(history.cold_blocks().iter().all(|&cold| !cold), "nothing compressed inline");
        run_and_check(&mut history);
        assert!(history.cold_blocks().iter().filter(|&&cold| cold).count() > 20);
        assert_same_rows(&history, &warm);

        // Cells with extras (hyperlinks): the extras queue is untouched by compression.
        let mut rng = ChaCha8Rng::seed_from_u64(0xc01e);
        let links = cell_links();
        let mut history = History::<Cell>::with_block_size(1000, 4096);
        for _ in 0..1000 {
            let width = rng.gen_range(1..=80);
            history.push_newest(&cell_row(&mut rng, &links, width));
        }
        let warm = history.clone();
        history.set_compression(Some(37));
        run_and_check(&mut history);
        assert!(history.cold_blocks().iter().any(|&cold| cold));
        assert_same_rows(&history, &warm);
        for i in 0..warm.len() {
            let expected = warm.plain_text(i, |p| (p.text.to_vec(), p.tail, p.width, p.wraps));
            let got = history.plain_text(i, |p| (p.text.to_vec(), p.tail, p.width, p.wraps));
            assert_eq!(got, expected, "row {i} plain text");
        }
    }

    #[test]
    fn newest_block_never_cold() {
        for keep in [0, 1, 10, 37] {
            let mut history = varied(600);
            history.set_compression(Some(keep));
            run_and_check(&mut history);
            assert_eq!(history.cold_blocks().last(), Some(&false), "keep {keep}");

            // Pushing keeps compressing as rows leave the warm window.
            let mut rng = ChaCha8Rng::seed_from_u64(keep as u64);
            for _ in 0..300 {
                history.push_newest(&varied_row(&mut rng));
            }
            assert!(history.has_jobs());
            run_and_check(&mut history);
            assert_eq!(history.cold_blocks().last(), Some(&false), "keep {keep}");
        }
    }

    /// A history with compression at 10 warm rows and its first job taken, plus a warm copy.
    fn with_job() -> (History<usize>, History<usize>, Job) {
        let mut history = varied(600);
        let warm = history.clone();
        history.set_compression(Some(10));
        let job = history.next_job().expect("a block to compress");
        (history, warm, job)
    }

    #[test]
    fn stale_job_refused_after_eviction() {
        let (mut history, mut warm, job) = with_job();
        let oldest = history.front_seq;
        history.set_limit(600);
        warm.set_limit(600);
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        while history.front_seq == oldest {
            let row = varied_row(&mut rng);
            history.push_newest(&row);
            warm.push_newest(&row);
        }
        assert!(!history.install(job.run()));
        assert_same_rows(&history, &warm);
        run_and_check(&mut history);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn wraps_and_clear_never_decompress() {
        let mut rng = ChaCha8Rng::seed_from_u64(0xbee5);
        let links = cell_links();
        let mut history = History::<Cell>::with_block_size(1000, 4096);
        for _ in 0..1000 {
            let width = rng.gen_range(1..=80);
            let mut row = cell_row(&mut rng, &links, width);
            if rng.gen_bool(0.4) {
                row[Column(width - 1)].flags.insert(crate::term::cell::Flags::WRAPLINE);
            }
            history.push_newest(&row);
        }
        history.set_compression(Some(10));
        history.run_jobs();
        assert!(history.cold_blocks().iter().any(|&cold| cold));
        history.clear_cache();
        assert_eq!(history.cached_blocks(), 0);

        let answers: Vec<(bool, bool)> =
            (0..history.len()).map(|i| (history.wraps(i), history.is_clear(i))).collect();
        assert_eq!(history.cached_blocks(), 0, "wraps or is_clear decompressed a block");
        assert!(answers.iter().any(|a| a.0) && answers.iter().any(|a| !a.0));
        assert!(answers.iter().any(|a| a.1) && answers.iter().any(|a| !a.1));

        for (i, &(wraps, clear)) in answers.iter().enumerate() {
            let header = read_header(&history.record(history.index[history.len() - 1 - i]));
            assert_eq!((header.wraps, header.clear), (wraps, clear), "row {i}");
        }
    }

    #[test]
    fn stale_job_refused_after_clear() {
        let (mut history, warm, job) = with_job();
        history.clear();
        assert_eq!(history.cached_blocks(), 0);
        // The same rows again: the first block has the same sequence number, not the same
        // identity.
        let mut rng = ChaCha8Rng::seed_from_u64(0xc01d);
        for _ in 0..600 {
            history.push_newest(&varied_row(&mut rng));
        }
        assert!(!history.install(job.run()));
        assert_same_rows(&history, &warm);
        run_and_check(&mut history);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn stale_job_refused_after_set_limit() {
        let (mut history, mut warm, job) = with_job();
        history.set_limit(100);
        warm.set_limit(100);
        assert!(!history.install(job.run()));
        assert_same_rows(&history, &warm);
        run_and_check(&mut history);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn stale_job_refused_after_setting_change() {
        let (mut history, warm, job) = with_job();
        history.set_compression(None);
        assert!(!history.install(job.run()));
        assert_same_rows(&history, &warm);

        // A cold block's warming job, refused once compression is back on.
        history.set_compression(Some(10));
        run_and_check(&mut history);
        history.set_compression(None);
        let job = history.next_job().expect("a block to warm");
        history.set_compression(Some(10));
        assert!(!history.install(job.run()));
        assert_same_rows(&history, &warm);
        run_and_check(&mut history);
    }

    #[test]
    fn switching_off_warms_every_block() {
        let mut history = varied(2000);
        let warm = history.clone();
        history.set_compression(Some(0));
        run_and_check(&mut history);
        history.set_compression(None);
        assert!(history.has_jobs());
        assert_eq!(history.cached_blocks(), 0);
        run_and_check(&mut history);
        assert!(history.cold_blocks().iter().all(|&cold| !cold));
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn raising_keep_warm_warms_blocks() {
        let mut history = varied(2000);
        let warm = history.clone();
        history.set_compression(Some(10));
        run_and_check(&mut history);
        let cold = history.cold_blocks().iter().filter(|&&cold| cold).count();
        history.set_compression(Some(1000));
        run_and_check(&mut history);
        assert!(history.cold_blocks().iter().filter(|&&cold| cold).count() < cold);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn lowering_keep_warm_cools_blocks() {
        let mut history = varied(2000);
        let warm = history.clone();
        history.set_compression(Some(1000));
        run_and_check(&mut history);
        let cold = history.cold_blocks().iter().filter(|&&cold| cold).count();
        history.set_compression(Some(10));
        run_and_check(&mut history);
        assert!(history.cold_blocks().iter().filter(|&&cold| cold).count() > cold);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn update_on_cold_row() {
        let mut history = varied(600);
        history.set_compression(Some(10));
        run_and_check(&mut history);
        let mut warm = history.clone();
        warm.set_compression(None);
        warm.run_jobs();

        let i = 500;
        let pos = history.len() - 1 - i;
        let block = history.index[pos].block.wrapping_sub(history.front_seq) as usize;
        assert!(history.cold_blocks()[block], "row {i} is in a cold block");
        history.update(i, |row| row[Column(0)] = 7777);
        warm.update(i, |row| row[Column(0)] = 7777);
        check_store(&history);
        assert_eq!(read(&history, i)[Column(0)], 7777);
        assert_same_rows(&history, &warm);
        run_and_check(&mut history);
        assert_same_rows(&history, &warm);
    }

    #[test]
    fn pop_newest_into_cold_block() {
        let mut history = varied(600);
        let mut warm = history.clone();
        history.set_compression(Some(0));
        run_and_check(&mut history);
        let (mut a, mut b) = (Row::default(), Row::default());
        while warm.pop_newest(&mut b) {
            assert!(history.pop_newest(&mut a));
            assert_eq!(a, b, "{} rows left", warm.len());
            check_store(&history);
            assert_eq!(history.cold_blocks().last().copied(), history.blocks.back().map(|_| false));
        }
        assert!(!history.pop_newest(&mut a));
        assert!(history.blocks.is_empty());
    }

    #[test]
    fn cache_holds_at_most_four_blocks() {
        let mut history = varied(2000);
        history.set_compression(Some(0));
        run_and_check(&mut history);
        assert_eq!(history.cached_blocks(), 0);

        // The oldest row of each of the first 7 cold blocks.
        let cold: Vec<usize> = (0..history.blocks.len())
            .filter(|&block| history.cold_blocks()[block])
            .take(7)
            .map(|block| {
                let seq = history.front_seq.wrapping_add(block as u32);
                let pos = history.index.iter().position(|entry| entry.block == seq).unwrap();
                history.len() - 1 - pos
            })
            .collect();
        let cached = |history: &History<usize>| -> Vec<u32> {
            history.cache.borrow().entries.iter().map(|cached| cached.seq).collect()
        };
        let seq = |history: &History<usize>, i: usize| history.index[history.len() - 1 - i].block;

        for (n, &i) in cold[..6].iter().enumerate() {
            read(&history, i);
            assert_eq!(history.cached_blocks(), (n + 1).min(CACHE_BLOCKS));
        }
        // Least recently used first: reading block 3 again moves it last, so block 7 evicts
        // block 4.
        let order: Vec<u32> = cold[2..6].iter().map(|&i| seq(&history, i)).collect();
        assert_eq!(cached(&history), order);
        read(&history, cold[2]);
        read(&history, cold[6]);
        let order: Vec<u32> =
            [cold[4], cold[5], cold[2], cold[6]].iter().map(|&i| seq(&history, i)).collect();
        assert_eq!(cached(&history), order);

        // Lowering the limit and switching off clear the cache.
        history.set_limit(1500);
        assert_eq!(history.cached_blocks(), 0);
        assert_eq!(history.cold_blocks().first(), Some(&true));
        read(&history, history.len() - 1);
        assert_eq!(history.cached_blocks(), 1);
        history.set_compression(None);
        assert_eq!(history.cached_blocks(), 0);
    }

    /// A job is installed only into the history it was taken from: a clone, whose blocks have
    /// the same sequence numbers and generations, refuses it.
    #[test]
    fn job_refused_by_a_clone() {
        let (mut history, warm, job) = with_job();
        let mut clone = history.clone();
        let at = job.seq.wrapping_sub(clone.front_seq) as usize;
        assert_eq!(clone.blocks[at].generation, job.generation, "same identity in the clone");
        assert!(!clone.install(job.run()));
        assert_same_rows(&clone, &warm);
        run_and_check(&mut clone);
        assert_same_rows(&clone, &warm);

        let job = history.next_job().expect("the same block again");
        assert!(history.install(job.run()));
        assert_same_rows(&history, &warm);
    }

    /// A reflow's new history (`sibling`) starts over at sequence number 0 and generation 0, so
    /// its first block can carry the identity of a job taken from the old history, with other
    /// bytes. It refuses that job.
    #[test]
    fn job_refused_by_a_sibling() {
        let (history, _, job) = with_job();
        let mut sibling = history.sibling(5000);
        assert_eq!(sibling.compression(), Some(10));
        assert_eq!(sibling.block_bytes, TINY);
        assert!(sibling.is_empty());

        let mut rng = ChaCha8Rng::seed_from_u64(0x5eed);
        for _ in 0..600 {
            sibling.push_newest(&varied_row(&mut rng));
        }
        let warm = sibling.clone();
        let at = job.seq.wrapping_sub(sibling.front_seq) as usize;
        assert_eq!(sibling.blocks[at].generation, job.generation, "same identity in the sibling");
        assert!(sibling.wants_cold(at));
        assert!(!sibling.install(job.run()));
        assert_same_rows(&sibling, &warm);
        run_and_check(&mut sibling);
        assert_same_rows(&sibling, &warm);
    }

    /// The reflow's byte copy (`move_oldest_to`, `push_encoded`) with compression on in both
    /// histories: cold source rows are read through the cache and their blocks freed as the last
    /// row leaves, the new history (with a lower limit, so it evicts too) only marks jobs, and
    /// every counter and block state stays exact.
    #[test]
    fn move_oldest_to_with_compression() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x0b1e);
        let links = cell_links();
        let mut old = History::<Cell>::with_block_size(3000, 1024);
        for _ in 0..3000 {
            let width = rng.gen_range(1..=80);
            let mut row = cell_row(&mut rng, &links, width);
            if rng.gen_bool(0.3) {
                row[Column(width - 1)].flags.insert(crate::term::cell::Flags::WRAPLINE);
            }
            old.push_newest(&row);
        }
        let warm = old.clone();
        old.set_compression(Some(37));
        run_and_check(&mut old);
        assert!(old.cold_blocks().iter().filter(|&&cold| cold).count() > 10);

        let mut new = old.sibling(2000);
        assert_eq!(new.compression(), Some(37));
        let (mut row, mut moved) = (Row::default(), 0);
        for step in 1.. {
            if old.is_empty() {
                break;
            }
            let cold_before = new.cold_blocks().iter().filter(|&&cold| cold).count();
            match old.oldest_reflow_info() {
                Some(info) if rng.gen_bool(0.8) => {
                    old.move_oldest_to(&mut new, info.width);
                    moved += 1;
                },
                _ => {
                    assert!(old.pop_oldest(&mut row));
                    new.push_newest(&row);
                },
            }
            let cold_after = new.cold_blocks().iter().filter(|&&cold| cold).count();
            assert!(cold_after <= cold_before, "a push compressed a block inline");
            if step % 7 == 0 {
                check_store(&old);
                check_store(&new);
            }
            if step % 449 == 0 {
                run_and_check(&mut new);
            }
        }
        assert!(moved > 1000, "{moved} rows copied as bytes");
        assert_eq!(old.blocks_held(), 0);
        check_store(&old);
        check_store(&new);
        assert!(new.has_jobs());
        run_and_check(&mut new);
        assert!(new.cold_blocks().iter().any(|&cold| cold));

        let mut reference = History::<Cell>::with_block_size(2000, 1024);
        for i in (0..warm.len()).rev() {
            warm.read(i, &mut row);
            reference.push_newest(&row);
        }
        assert_same_rows(&new, &reference);
    }

    /// `check_store` and `decode` catch an index entry whose header bits disagree with its
    /// record (every path that rewrites entries runs under them in the randomized tests).
    #[test]
    #[should_panic(expected = "header bits")]
    fn check_store_catches_wrong_header_bits() {
        let mut history = varied(100);
        history.index[50].len_flags ^= Entry::WRAPS;
        check_store(&history);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "header bits")]
    fn decode_catches_wrong_header_bits() {
        let mut history = varied(100);
        history.index[50].len_flags ^= Entry::CLEAR;
        read(&history, 49);
    }

    /// `run_jobs` stops at a refused install instead of offering the same job forever.
    #[test]
    fn run_jobs_stops_when_install_refuses() {
        let mut history = varied(600);
        history.set_compression(Some(10));
        let mut runs = 0;
        history.run_jobs_with(|job| {
            runs += 1;
            let mut done = job.run();
            done.generation = u64::MAX;
            done
        });
        assert_eq!(runs, 1);
        assert!(history.has_jobs());
        run_and_check(&mut history);
    }

    #[test]
    fn off_does_nothing() {
        let mut history = History::with_block_size(1000, TINY);
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        for _ in 0..10_000 {
            history.push_newest(&varied_row(&mut rng));
        }
        assert_eq!(history.compression(), None);
        assert!(!history.has_jobs());
        assert!(history.next_job().is_none());
        history.set_compression(None);
        assert!(!history.has_jobs());
        assert!(history.cold_blocks().iter().all(|&cold| !cold));
    }

    /// `History` stays `Send` (fbterm's worker moves `Job`s and `Done`s across threads too).
    #[test]
    fn history_and_jobs_are_send() {
        fn send<S: Send>() {}
        send::<History<Cell>>();
        send::<Job>();
        send::<Done>();
    }
}
