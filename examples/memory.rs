// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Live bytes, peak bytes and allocation calls of feathercritty against `alacritty_terminal`
//! 0.26.0 (the differential test's patched oracle, `tests/oracle/PATCHES.md`) in the same process
//! (design spec sections 6.3 last paragraph, 6.4 resize row and 6.5), measured with a counting
//! `#[global_allocator]` over `System`.
//!
//! Three tables:
//!
//! 1. **Live bytes** of a terminal (126x37, 10,000-line history) after 12,000 lines of each content
//!    kind (spec 6.5), 0.26.0 against the fork, plus bytes per history row. The gated number is the
//!    terminal after the lines minus a freshly created terminal of the same size, for both crates
//!    (project ruling on spec 6.5, 2026-10-25): a new `Term` already holds about 2.3 MB in both
//!    crates (screen, tab stops, the parser's buffers), which is not history and is the same with
//!    or without compact history. The full terminal is printed too.
//! 2. **Peak live bytes** while resizing a terminal holding a full history: 126->100->126 columns
//!    and 200->80 (spec 6.4). Measured above the live bytes before the terminal was created, so the
//!    terminal itself is included.
//! 3. **Allocation calls per scrolled row** once the history is full (spec 6.3, last paragraph):
//!    20,000 more lines are fed to a full terminal and the calls to the allocator (`alloc`,
//!    `alloc_zeroed`, `realloc`) are divided by the lines. The parser's own allocations for new
//!    `CellExtra`s (`underline color` row) are reported but not gated.
//!
//! `--gate` evaluates the "Pass" column of spec 6.5, the peak rule of 6.4 and the zero-allocation
//! rule of 6.3 for the fork and exits non-zero if any row fails. Memory rows are expected to fail
//! until the compact store exists. Without `--gate` the run only reports.
//!
//! | Row | Gate (fork) |
//! |---|---|
//! | short lines | at most 1.5 MB (150 bytes per history row; MB = 10^6 bytes) |
//! | log lines 95 | at most 3 MB (300 bytes per row) |
//! | wide chars | at most 0.5x 0.26.0 |
//! | dense cells | at most 1.0x 0.26.0 |
//! | peak during resizes | below 0.26.0's peak |
//! | allocation calls per scrolled row | zero for plain and colored content |
//!
//! Run with `cargo run --release --example memory [-- --gate]`. To move the comparison to a new
//! upstream release see `benches/common/mod.rs`, which this example includes.

#[path = "../benches/common/mod.rs"]
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use common::{HISTORY, Workload, ac, fc, gate_requested, workload_lines};

/// Counting wrapper over `System`: live bytes, their high-water mark and call count.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: forwards every call unchanged to `System` and only adjusts counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        grow(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        grow(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        if new_size >= layout.size() {
            grow(new_size - layout.size());
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}

fn reset_peak() {
    PEAK.store(live(), Relaxed);
}

/// Both crates behind one interface, so each measurement is written once.
trait Subject {
    fn make(columns: usize, lines: usize) -> Self;
    fn feed(&mut self, bytes: &[u8]);
    fn resize(&mut self, columns: usize, lines: usize);
}

impl Subject for fc::Sim {
    fn make(columns: usize, lines: usize) -> Self {
        fc::Sim::new(columns, lines, HISTORY)
    }

    fn feed(&mut self, bytes: &[u8]) {
        fc::Sim::feed(self, bytes, 64 * 1024)
    }

    fn resize(&mut self, columns: usize, lines: usize) {
        fc::Sim::resize(self, columns, lines)
    }
}

impl Subject for ac::Sim {
    fn make(columns: usize, lines: usize) -> Self {
        ac::Sim::new(columns, lines, HISTORY)
    }

    fn feed(&mut self, bytes: &[u8]) {
        ac::Sim::feed(self, bytes, 64 * 1024)
    }

    fn resize(&mut self, columns: usize, lines: usize) {
        ac::Sim::resize(self, columns, lines)
    }
}

/// Live bytes of a terminal after `buffer` was fed to it, and of the same terminal when it was
/// new (before anything was fed).
fn live_bytes<S: Subject>(columns: usize, lines: usize, buffer: &[u8]) -> (usize, usize) {
    let before = live();
    let mut sim = S::make(columns, lines);
    let fresh = live() - before;
    sim.feed(buffer);
    let bytes = live() - before;
    drop(sim);
    (bytes, fresh)
}

/// Peak live bytes (terminal included) while resizing `steps` after `buffer` was fed.
fn resize_peak<S: Subject>(
    columns: usize,
    lines: usize,
    buffer: &[u8],
    steps: &[(usize, usize)],
) -> usize {
    let before = live();
    let mut sim = S::make(columns, lines);
    sim.feed(buffer);
    reset_peak();
    for &(c, l) in steps {
        sim.resize(c, l);
    }
    let peak = PEAK.load(Relaxed) - before;
    drop(sim);
    peak
}

/// Allocation calls per scrolled row once the history is full.
fn calls_per_row<S: Subject>(kind: Workload, columns: usize, lines: usize) -> f64 {
    const ROWS: usize = 20_000;
    let fill = workload_lines(kind, 12_000, 3, columns, lines);
    let more = workload_lines(kind, ROWS, 4, columns, lines);
    let mut sim = S::make(columns, lines);
    sim.feed(&fill);
    let before = CALLS.load(Relaxed);
    sim.feed(&more);
    (CALLS.load(Relaxed) - before) as f64 / ROWS as f64
}

/// Label, columns, lines, content, resize steps.
type ResizeCase<'a> = (&'a str, usize, usize, &'a [u8], &'a [(usize, usize)]);

fn verdict(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1e6
}

fn main() {
    let gate = gate_requested();
    let mut failed = 0;
    let (columns, lines) = (126, 37);

    println!(
        "== live bytes after 12,000 lines, {columns}x{lines}, history {HISTORY} (spec 6.5) =="
    );
    println!(
        "{:<28} | {:>10} | {:>10} | {:>9} | {:>9} | {:>6} | gate",
        "content (minus a new Term)", "0.26.0 MB", "fork MB", "ac B/row", "fork B/row", "ratio"
    );
    // (kind, spec 0.26.0 MB, absolute limit in bytes, limit as a fraction of 0.26.0)
    let rows = [
        (Workload::ShortLines, 32.0, Some(1_500_000), None),
        (Workload::LogLines95, 33.0, Some(3_000_000), None),
        (Workload::WideChars, 33.0, None, Some(0.5)),
        (Workload::DenseCells, 33.0, None, Some(1.0)),
    ];
    for (kind, spec_mb, absolute, relative) in rows {
        let buffer = workload_lines(kind, 12_000, 5, columns, lines);
        let (a_full, a_fresh) = live_bytes::<ac::Sim>(columns, lines, &buffer);
        let (f_full, f_fresh) = live_bytes::<fc::Sim>(columns, lines, &buffer);
        let (a, f) = (a_full - a_fresh, f_full - f_fresh);
        let limit = absolute.unwrap_or_else(|| (a as f64 * relative.unwrap()) as usize);
        let ok = f <= limit;
        if !ok {
            failed += 1;
        }
        let rel = match relative {
            Some(r) => format!("<= {r:.1}x 0.26.0"),
            None => format!("<= {:.1} MB", mb(limit)),
        };
        println!(
            "{:<28} | {:>10.2} | {:>10.2} | {:>9} | {:>9} | {:>6.3} | {} ({rel}); spec 0.26.0 \
             ~{spec_mb:.0} MB, measured {:+.0}%; whole Term {:.2} / {:.2} MB",
            kind.name(),
            mb(a),
            mb(f),
            a / HISTORY,
            f / HISTORY,
            f as f64 / a as f64,
            verdict(ok),
            (mb(a) / spec_mb - 1.0) * 100.0,
            mb(a_full),
            mb(f_full),
        );
    }

    println!();
    println!("== peak live bytes during resizes, history full (spec 6.4, 6.5) ==");
    println!("{:<28} | {:>10} | {:>10} | {:>6} | gate", "resize", "0.26.0 MB", "fork MB", "ratio");
    let narrow = workload_lines(Workload::LogLines95, 12_000, 7, 126, 37);
    let wide = workload_lines(Workload::LogLines95, 12_000, 7, 200, 60);
    let cases: [ResizeCase; 2] = [
        ("126->100->126", 126, 37, &narrow, &[(100, 37), (126, 37)]),
        ("200->80", 200, 60, &wide, &[(80, 60)]),
    ];
    for (label, c, l, buffer, steps) in cases {
        let a = resize_peak::<ac::Sim>(c, l, buffer, steps);
        let f = resize_peak::<fc::Sim>(c, l, buffer, steps);
        let ok = f < a;
        if !ok {
            failed += 1;
        }
        println!(
            "{:<28} | {:>10.2} | {:>10.2} | {:>6.3} | {} (< 0.26.0)",
            label,
            mb(a),
            mb(f),
            f as f64 / a as f64,
            verdict(ok)
        );
    }

    println!();
    println!("== allocation calls per scrolled row, history full (spec 6.3) ==");
    println!("{:<28} | {:>10} | {:>10} | gate", "content", "0.26.0", "fork");
    let kinds = [
        (Workload::FullWidthPlain, true),
        (Workload::ShortLines, true),
        (Workload::LogLines95, true),
        (Workload::DenseCells, true),
        (Workload::UnderlineColor, false),
    ];
    for (kind, gated) in kinds {
        let a = calls_per_row::<ac::Sim>(kind, columns, lines);
        let f = calls_per_row::<fc::Sim>(kind, columns, lines);
        let note = if gated {
            let ok = f == 0.0;
            if !ok {
                failed += 1;
            }
            format!("{} (== 0)", verdict(ok))
        } else {
            "- (parser CellExtra allocations, reported separately)".to_owned()
        };
        println!("{:<28} | {:>10.3} | {:>10.3} | {}", kind.name(), a, f, note);
    }

    if gate && failed > 0 {
        eprintln!("{failed} memory row(s) missed their gate");
        std::process::exit(1);
    }
}
