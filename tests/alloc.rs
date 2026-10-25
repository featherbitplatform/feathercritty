// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Allocation checks for the history's block pool (design spec 5.3): once the history is full,
//! scrolling more rows into it allocates nothing, because every evicted block becomes the next
//! spare and the index, the extras queue and the encode buffer are at their high-water capacity.
//!
//! A counting global allocator counts the allocations (and reallocations) made by the thread
//! running a check, so the test harness's own threads do not interfere. It is the only `unsafe`
//! code of the change, and it is test-only.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use feathercritty::grid::{Dimensions, Grid};
use feathercritty::index::{Column, Line};
use feathercritty::term::cell::{Cell, Flags};
use feathercritty::vte::ansi::{Color, NamedColor, Rgb};

struct Counting;

thread_local! {
    /// Allocations made by this thread while `COUNTING` is set.
    static ALLOCATIONS: StdCell<usize> = const { StdCell::new(0) };
    static COUNTING: StdCell<bool> = const { StdCell::new(false) };
}

fn note_allocation() {
    // `try_with`: the allocator also runs while thread-locals are being torn down.
    let _ = COUNTING.try_with(|counting| {
        if counting.get() {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
    });
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees are ours; the bookkeeping touches only `const`-initialized thread-locals, which do
// not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
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

/// Write row number `n` on the bottom line, alternating plain text and colored runs, then
/// scroll it into history.
fn scroll_one(grid: &mut Grid<Cell>, n: usize) {
    let line = Line(grid.screen_lines() as i32 - 1);
    let columns = grid.columns();
    let row = &mut grid.screen_mut()[line];
    let length = 1 + (n * 7) % columns;
    for col in 0..length {
        let cell = &mut row[Column(col)];
        cell.c = char::from(b'a' + ((n + col) % 26) as u8);
        if n % 2 == 1 {
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
    let mut grid = Grid::<Cell>::new_with_block_size(24, columns, limit, block_bytes);

    // Fill the history, then turn it over twice so the block count, the pool and every
    // buffer's capacity reach their steady state.
    for n in 0..3 * limit {
        scroll_one(&mut grid, n);
    }
    assert_eq!(grid.history_size(), limit);

    let count = allocations_in(|| {
        for n in 3 * limit..3 * limit + 10_000 {
            scroll_one(&mut grid, n);
        }
    });
    assert_eq!(count, 0, "{count} allocations while scrolling a full history");
    assert_eq!(grid.history_size(), limit);
}

#[test]
fn steady_scroll_allocates_nothing() {
    // Default 64 KiB blocks at a typical width.
    check(126, 64 * 1024);
    // Tiny blocks: every row fits in a quarter block (narrow rows, at most 2 runs), so blocks
    // fill and are evicted constantly.
    check(8, 256);
}
