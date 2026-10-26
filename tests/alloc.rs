// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Allocation checks for the history's block pool (design spec 5.3): once the history is full,
//! scrolling more rows into it allocates nothing, because every evicted block becomes the next
//! spare and the index, the extras queue and the encode buffer are at their high-water capacity.
//! And the peak memory of a reflow (design spec 5.6): rows stream from the old store into a new
//! one, so no dense copy of the history is ever held.
//!
//! A counting global allocator counts the allocations (and reallocations) and the live bytes of
//! the thread running a check, so the test harness's own threads do not interfere. It is the only
//! `unsafe` code of the change, and it is test-only.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;
use std::sync::Arc;

use feathercritty::grid::{Dimensions, Grid};
use feathercritty::index::{Column, Line};
use feathercritty::term::cell::{Cell, CellExtra, Flags, Hyperlink};
use feathercritty::vte::ansi::{Color, NamedColor, Rgb};

struct Counting;

thread_local! {
    /// Allocations made by this thread while `COUNTING` is set.
    static ALLOCATIONS: StdCell<usize> = const { StdCell::new(0) };
    static COUNTING: StdCell<bool> = const { StdCell::new(false) };
    /// Bytes allocated minus bytes freed by this thread while `COUNTING` is set, and the
    /// highest value it reached.
    static LIVE: StdCell<isize> = const { StdCell::new(0) };
    static PEAK: StdCell<isize> = const { StdCell::new(0) };
}

/// Count an allocation (when `new` is set) and a change of `delta` live bytes.
fn note_allocation(new: bool, delta: isize) {
    // `try_with`: the allocator also runs while thread-locals are being torn down.
    let _ = COUNTING.try_with(|counting| {
        if counting.get() {
            if new {
                let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            }
            let _ = LIVE.try_with(|live| {
                live.set(live.get() + delta);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
            });
        }
    });
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees are ours; the bookkeeping touches only `const`-initialized thread-locals, which do
// not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation(true, layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation(true, layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation(true, new_size as isize - layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_allocation(false, -(layout.size() as isize));
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` makes on this thread.
fn allocations_in(f: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|counting| counting.set(true));
    f();
    COUNTING.with(|counting| counting.set(false));
    ALLOCATIONS.with(|count| count.get())
}

/// Start counting live bytes on this thread from 0.
fn start_counting() {
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    COUNTING.with(|counting| counting.set(true));
}

/// Live bytes since `start_counting`, and the peak since the last `reset_peak`.
fn live_and_peak() -> (isize, isize) {
    (LIVE.with(StdCell::get), PEAK.with(StdCell::get))
}

/// Restart the peak from the current live bytes.
fn reset_peak() {
    PEAK.with(|peak| peak.set(LIVE.with(StdCell::get)));
}

/// Cell extras the rows share (as the parser shares one hyperlink across the cells it covers):
/// two hyperlinks, a combining mark and an underline color. Built before counting, so scrolling
/// only clones the `Arc`s.
fn shared_extras() -> Vec<Arc<CellExtra>> {
    let mut extras = Vec::new();
    for uri in ["https://example.com/a", "https://example.com/b"] {
        let mut cell = Cell::default();
        cell.set_hyperlink(Some(Hyperlink::new(None::<String>, uri.to_owned())));
        extras.extend(cell.extra);
    }
    let mut cell = Cell::default();
    cell.push_zerowidth('\u{301}');
    extras.extend(cell.extra);
    let mut cell = Cell::default();
    cell.set_underline_color(Some(Color::Indexed(3)));
    extras.extend(cell.extra);
    extras
}

/// Write row number `n` on the bottom line, cycling through plain text, colored runs and runs
/// with extras (hyperlinks, a zero-width character, an underline color), then scroll it into
/// history.
fn scroll_one(grid: &mut Grid<Cell>, n: usize, extras: &[Arc<CellExtra>]) {
    let line = Line(grid.screen_lines() as i32 - 1);
    let columns = grid.columns();
    let row = &mut grid.screen_mut()[line];
    let length = 1 + (n * 7) % columns;
    for col in 0..length {
        let cell = &mut row[Column(col)];
        cell.c = char::from(b'a' + ((n + col) % 26) as u8);
        if n % 3 == 2 && col % 5 < 3 {
            cell.extra = Some(extras[(n + col / 5) % extras.len()].clone());
        }
        if n % 3 == 1 {
            // Colored: a few runs of different colors and flags.
            match col / 4 % 3 {
                0 => cell.fg = Color::Indexed((n % 16) as u8),
                1 => {
                    cell.fg = Color::Spec(Rgb { r: 200, g: 40, b: n as u8 });
                    cell.flags.insert(Flags::BOLD);
                },
                _ => cell.bg = Color::Named(NamedColor::Blue),
            }
        }
    }
    grid.scroll_up::<Color>(&(Line(0)..Line(grid.screen_lines() as i32)), 1);
}

fn check(columns: usize, block_bytes: usize) {
    let limit = 1000;
    let extras = shared_extras();
    let mut grid = Grid::<Cell>::new_with_block_size(24, columns, limit, block_bytes);

    // Fill the history, then turn it over twice so the block count, the pool and every
    // buffer's capacity (the extras queue's too) reach their steady state.
    for n in 0..3 * limit {
        scroll_one(&mut grid, n, &extras);
    }
    assert_eq!(grid.history_size(), limit);

    let count = allocations_in(|| {
        for n in 3 * limit..3 * limit + 10_000 {
            scroll_one(&mut grid, n, &extras);
        }
    });
    assert_eq!(count, 0, "{count} allocations while scrolling a full history");
    assert_eq!(grid.history_size(), limit);
}

#[test]
fn steady_scroll_allocates_nothing() {
    // Default 64 KiB blocks at a typical width.
    check(126, 64 * 1024);
    // Tiny blocks, so blocks fill and are evicted constantly. Rows of up to 8 cells keep every
    // record within a quarter block (a larger one gets a block of its own, allocated and freed
    // with its row).
    check(8, 256);
}

/// A reflow streams the history: during 126->100->126 columns with a full 10,000-row history,
/// the grid's peak live bytes stay below what the history rows alone take as dense rows, and the
/// old store is released as the pass goes (design spec 5.6, 6.4).
#[test]
fn reflow_peak_below_dense() {
    let (lines, columns, limit) = (37, 126, 10_000);
    start_counting();
    let extras = shared_extras();
    let mut grid = Grid::<Cell>::new(lines, columns, limit);
    for n in 0..limit + 2 * lines {
        scroll_one(&mut grid, n, &extras);
    }
    assert_eq!(grid.history_size(), limit);
    let (before, _) = live_and_peak();

    reset_peak();
    grid.resize(true, lines, 100);
    grid.resize(true, lines, columns);
    let (after, peak) = live_and_peak();
    COUNTING.with(|counting| counting.set(false));

    let dense = limit * columns * std::mem::size_of::<Cell>();
    let compact = before as usize;
    println!("grid {before} B before, {after} B after, peak {peak} B; dense history {dense} B");
    assert!((peak as usize) < dense, "peak {peak} B not below the dense history {dense} B");
    // Old and new store side by side would be about twice the grid; streaming frees the old
    // blocks as the new ones fill, so the peak stays well below that.
    assert!(
        (peak as usize) < compact + compact / 2,
        "peak {peak} B: the old store was not released during the pass (grid {compact} B)"
    );
    // Nothing of the pass stays resident.
    assert!(
        (after as usize) < compact + compact / 4,
        "{after} B after the resize, {compact} B before"
    );
}
