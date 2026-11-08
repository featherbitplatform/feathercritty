// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Large histories (design spec 6.8): correctness at 100,000 and 1,000,000 lines, and what they
//! cost. All tests are `#[ignore]` and meant for release builds:
//!
//! ```text
//! cargo test --release --test large_history -- --ignored --nocapture
//! ```
//!
//! - `large_history_matches_oracle_at_100k` drives feathercritty and the differential test's oracle
//!   (`tests/oracle/`, unmodified 0.26.0 plus the two patches of `PATCHES.md`) through a
//!   100,000-line history: fill, column resizes 126 -> 100 -> 126, a lines-only resize, regex
//!   searches (plus selections, `bounds_to_string`, vi motions and the oldest and newest rows),
//!   then lowering the limit to 10,000. After each phase the full `project::Snapshot` of both
//!   terminals (every cell of every history and screen row) and the seeded answers must be equal.
//!   The content is [`block`] with `rich` set: plain, colored, bold, wide-character, underline
//!   color and hyperlink lines, some longer than the terminal is wide, so the rewraps join and
//!   split real logical lines.
//! - `large_history_invariants_at_1m` fills a 1,000,000-line history on feathercritty alone (a
//!   dense oracle that size would take gigabytes) and checks it against text regenerated from
//!   [`block`]: history size and `topmost_line` after every phase, the `wraps` and `is_clear` bits
//!   of every row, and the characters, colors and flags of 1,000 sampled rows plus the newest ones,
//!   through the fill, the column resizes, the lines-only resize and the trim to 10,000 lines.
//!   Which block a row belongs to is computed from the bottom of the grid (see [`expected_at`]), so
//!   rows evicted at the top never shift the expectation.
//! - `large_history_matches_oracle_at_100k_compressed` and
//!   `large_history_invariants_at_1m_compressed` run the two above with `history_compression:
//!   Some(10_000)`, `run_history_jobs()` after the fill and each step, and check that most blocks
//!   are compressed.
//! - `large_history_cost_table` measures (it prints, never asserts): live bytes, resident set size,
//!   resize and trim times for short, log-style and full-width lines at 100,000 and 1,000,000
//!   history lines, and the tables recorded in `benches/RESULTS.md`. Its last table is the history
//!   compression memory gate (that design's 6.3 and the peaks of its 6.4): the realistic
//!   generators, the recorded streams (repeated to fill the history) and the random content,
//!   compression off against on, with the peaks during column resizes and while switching
//!   compression on and off. Each cell runs in a fresh child process (`large_history_cost_cell`,
//!   which does nothing unless the parent sets `LARGE_HISTORY_CELL`), so the resident set size of
//!   one cell does not include the freed memory of the previous one. Live bytes come from a
//!   counting `#[global_allocator]` that counts only the calling thread, so the other tests'
//!   threads do not disturb it. The tests serialize on a lock for the same reason.
//!
//! The allocator below is the only `unsafe` code of this file, as in `tests/alloc.rs`: it
//! forwards every call to `System` and keeps counters in `const` thread-locals.

mod corpora;
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use feathercritty::grid::Dimensions;
use feathercritty::index::{Column, Line, Point};
use feathercritty::term::Config;
use feathercritty::term::cell::{Cell, Flags};
use feathercritty::vte::ansi::{Color, NamedColor};
use support::project::{QAns, QueryPlan, SearchQ, Snapshot};
use support::{Pt, SelTy, Start, Step, ac, fc};

// -- counting allocator ---------------------------------------------------------------------

struct Counting;

thread_local! {
    /// Bytes allocated minus bytes freed by this thread while `COUNTING` is set.
    static LIVE: StdCell<isize> = const { StdCell::new(0) };
    static COUNTING: StdCell<bool> = const { StdCell::new(false) };
    /// The highest `LIVE` since `reset_peak`.
    static PEAK: StdCell<isize> = const { StdCell::new(0) };
}

fn note(delta: isize) {
    // `try_with`: the allocator also runs while thread-locals are being torn down.
    let _ = COUNTING.try_with(|counting| {
        if counting.get() {
            let _ = LIVE.try_with(|live| {
                let now = live.get() + delta;
                live.set(now);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
            });
        }
    });
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees are ours; the bookkeeping touches only `const`-initialized thread-locals, which do
// not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size as isize - layout.size() as isize);
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(-(layout.size() as isize));
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn start_counting() {
    LIVE.with(|live| live.set(0));
    COUNTING.with(|counting| counting.set(true));
}

fn live_bytes() -> usize {
    LIVE.with(StdCell::get).max(0) as usize
}

/// Start a new high-water mark at the current live bytes.
fn reset_peak() {
    PEAK.with(|peak| peak.set(LIVE.with(StdCell::get)));
}

/// The highest live bytes since `reset_peak`.
fn peak_bytes() -> usize {
    PEAK.with(StdCell::get).max(0) as usize
}

fn stop_counting() {
    COUNTING.with(|counting| counting.set(false));
}

/// The tests measure memory and time, so they must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

// -- content --------------------------------------------------------------------------------

const COLUMNS: usize = 126;
const LINES: usize = 37;

/// Lines of one [`block`].
const BLOCK_LINES: usize = 10;

/// Length of line `k` of a block when it is not drawn at random: the log line, the exactly
/// full-width line, the blank line, the 40-character line and the line that wraps at 126.
const LOG_LEN: usize = 95;
const FULL_LEN: usize = COLUMNS;
const WRAPPING_LEN: usize = 150;

fn mix(x: u64) -> u64 {
    let mut x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `len` letters and digits (never blank, so a row of them is never `is_clear`).
fn alnum(seed: u64, len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..len).map(|i| ALPHABET[(mix(seed ^ (i as u64) << 20) % 62) as usize] as char).collect()
}

/// A short line: 1 to 20 characters.
fn short_text(seed: u64) -> String {
    alnum(seed, 1 + (mix(seed) % 20) as usize)
}

/// A log-style line of exactly [`LOG_LEN`] characters, and the colors of its runs:
/// timestamp (8), level (`1`, `2` or `3`), module (6), then the message uncolored.
fn log_text(seed: u64) -> (String, u8) {
    let h = mix(seed);
    let (level, color) = match h % 3 {
        0 => ("INFO ", 2),
        1 => ("WARN ", 3),
        _ => ("ERROR", 1),
    };
    let ts = format!(
        "2026-10-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        1 + h % 28,
        (h >> 8) % 24,
        (h >> 16) % 60,
        (h >> 24) % 60,
        (h >> 32) % 1000
    );
    let text = format!("{ts} {level} {}: {}", alnum(seed ^ 1, 18), alnum(seed ^ 2, 44));
    assert_eq!(text.len(), LOG_LEN);
    (text, color)
}

/// Column of the level in a log line, and of the module.
const LEVEL_COL: usize = 25;
const MODULE_COL: usize = 31;

/// The bytes of a log line without its newline: timestamp, level and module colored.
fn log_bytes(seed: u64) -> Vec<u8> {
    let (text, color) = log_text(seed);
    let (ts, rest) = text.split_at(LEVEL_COL);
    let (level, rest) = rest.split_at(MODULE_COL - LEVEL_COL);
    let (module, message) = rest.split_at(20);
    format!("\x1b[38;5;8m{ts}\x1b[38;5;{color}m{level}\x1b[38;5;6m{module}\x1b[0m{message}\x1b[0m")
        .into_bytes()
}

/// One block: ten lines that take [`rows_per_block`] rows (11 at 126 columns). With `rich`, the
/// block also holds a wide-character line, an underline-colored line and a hyperlink, which the
/// text round trip of the 1M test does not model.
///
/// | k | line | rows at 126 / 100 columns |
/// |---|------|-----|
/// | 0 | 1-20 characters, indexed foreground | 1 / 1 |
/// | 1 | log line of 95 characters, four colors | 1 / 1 |
/// | 2 | exactly 126 characters (pending wrap, no wrap bit) | 1 / 2 |
/// | 3 | blank | 1 / 1 |
/// | 4 | 40 plain characters | 1 / 1 |
/// | 5 | 150 characters, bold and indexed foreground | 2 / 2 |
/// | 6-9 | 1-20 plain characters (`rich`: k7 wide characters, k8 underline color, k9 hyperlink) | 1 / 1 |
fn block(b: usize, rich: bool) -> Vec<u8> {
    let seed = |k: usize| mix(((b as u64) << 4) | k as u64);
    let mut out = Vec::new();
    for k in 0..BLOCK_LINES {
        match k {
            0 => out.extend_from_slice(
                format!("\x1b[38;5;{}m{}\x1b[0m", 1 + b % 7, short_text(seed(0))).as_bytes(),
            ),
            1 => out.extend(log_bytes(seed(1))),
            2 => out.extend_from_slice(alnum(seed(2), FULL_LEN).as_bytes()),
            3 => (),
            4 => out.extend_from_slice(alnum(seed(4), 40).as_bytes()),
            5 => out.extend_from_slice(
                format!("\x1b[1;38;5;5m{}\x1b[0m", alnum(seed(5), WRAPPING_LEN)).as_bytes(),
            ),
            7 if rich => {
                let line: String = (0..50 + mix(seed(7)) % 20)
                    .map(|i| char::from_u32(0x4E00 + (mix(seed(7) + i) % 200) as u32).unwrap())
                    .collect();
                out.extend_from_slice(line.as_bytes());
            },
            8 if rich => out.extend_from_slice(
                format!("\x1b[4m\x1b[58;5;{}m{}\x1b[0m", 1 + b % 200, short_text(seed(8)))
                    .as_bytes(),
            ),
            9 if rich => out.extend_from_slice(
                format!(
                    "\x1b]8;;http://example.com/{}\x1b\\{}\x1b]8;;\x1b\\",
                    b % 50,
                    short_text(seed(9))
                )
                .as_bytes(),
            ),
            _ => out.extend_from_slice(short_text(seed(k)).as_bytes()),
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// What line `k` of block `b` contains when it is not drawn from a [`block`] with `rich`.
fn line_text(b: usize, k: usize) -> String {
    let seed = mix(((b as u64) << 4) | k as u64);
    match k {
        0 => short_text(seed),
        1 => log_text(seed).0,
        2 => alnum(seed, FULL_LEN),
        3 => String::new(),
        4 => alnum(seed, 40),
        5 => alnum(seed, WRAPPING_LEN),
        _ => short_text(seed),
    }
}

/// Length of line `k` as drawn (all lines but 2 and 5 are well below 100 columns).
fn line_len(k: usize) -> usize {
    match k {
        2 => FULL_LEN,
        5 => WRAPPING_LEN,
        _ => 0,
    }
}

/// Rows of a block at `columns` columns, as (line `k`, piece of the line).
fn layout(columns: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    for k in 0..BLOCK_LINES {
        let pieces = match line_len(k) {
            0 => 1,
            len => len.div_ceil(columns),
        };
        rows.extend((0..pieces).map(|piece| (k, piece)));
    }
    rows
}

/// A row as the generator says it must look.
struct Expect {
    text: String,
    /// Foreground of column 0 (and of the run starting at the given columns).
    runs: Vec<(usize, Color)>,
    bold: bool,
    wraps: bool,
    clear: bool,
}

/// The expected row `piece` of line `k` of block `b` at `columns` columns.
fn expect_row(b: usize, k: usize, piece: usize, columns: usize) -> Expect {
    let full = line_text(b, k);
    let start = (piece * columns).min(full.len());
    let end = ((piece + 1) * columns).min(full.len());
    let pieces = if full.is_empty() { 1 } else { full.len().div_ceil(columns) };
    let default = Color::Named(NamedColor::Foreground);
    let indexed = |n: u8| Color::Indexed(n);
    let mut runs = vec![(0, default)];
    let mut bold = false;
    match k {
        0 => runs = vec![(0, indexed(1 + (b % 7) as u8))],
        1 => {
            // Columns of the runs in the first (only) piece: timestamp, level, module, message.
            let level = log_text(mix(((b as u64) << 4) | 1)).1;
            runs = vec![
                (0, indexed(8)),
                (LEVEL_COL, indexed(level)),
                (MODULE_COL, indexed(6)),
                (MODULE_COL + 20, default),
            ];
        },
        5 => {
            runs = vec![(0, indexed(5))];
            bold = true;
        },
        _ => (),
    }
    Expect {
        text: full[start..end].to_owned(),
        runs,
        bold,
        wraps: piece + 1 < pieces,
        clear: full.is_empty(),
    }
}

/// The expected row at index `x` counted from the top of the grid (history first, then the
/// screen), for a grid of `total` rows whose last row is the blank cursor row below the final
/// line of block `blocks - 1`. Counting from the bottom keeps the expectation right however many
/// rows were evicted at the top (by the history limit, or by a trim).
///
/// `None` for the cursor row itself or a row above the first block.
fn expected_at(x: usize, total: usize, blocks: usize, columns: usize) -> Option<Expect> {
    let layout = layout(columns);
    let from_end = (total - 2).checked_sub(x)?;
    let block = (blocks - 1).checked_sub(from_end / layout.len())?;
    let (k, piece) = layout[layout.len() - 1 - from_end % layout.len()];
    Some(expect_row(block, k, piece, columns))
}

// -- reading rows ---------------------------------------------------------------------------

type Term = feathercritty::term::Term<feathercritty::event::VoidListener>;

fn size(term: &Term) -> (usize, usize, usize) {
    let grid = term.grid();
    (grid.history_size(), grid.screen_lines(), grid.columns())
}

/// Check the row at index `x` from the top against `expect`.
fn check_row(term: &Term, x: usize, expect: &Expect) -> Result<(), String> {
    let grid = term.grid();
    let line = Line(grid.topmost_line().0 + x as i32);
    let cell = |c: usize| grid.cell(Point::new(line, Column(c)));
    let columns = grid.columns();

    let wrote: Vec<char> = expect.text.chars().collect();
    for c in 0..columns {
        let got = cell(c);
        let want = wrote.get(c).copied().unwrap_or(' ');
        if got.c != want {
            return Err(format!("row {x} col {c}: char {:?}, expected {want:?}", got.c));
        }
        if let Some(&(_, fg)) = expect.runs.iter().rev().find(|(start, _)| *start <= c) {
            // Beyond the text the cells are default.
            let fg = if c < wrote.len() { fg } else { Color::Named(NamedColor::Foreground) };
            if got.fg != fg {
                return Err(format!("row {x} col {c}: fg {:?}, expected {fg:?}", got.fg));
            }
        }
        if c < wrote.len() && got.flags.contains(Flags::BOLD) != expect.bold {
            return Err(format!("row {x} col {c}: bold {}", !expect.bold));
        }
    }
    if grid.wraps(line) != expect.wraps {
        return Err(format!("row {x}: wraps {}, expected {}", grid.wraps(line), expect.wraps));
    }
    if grid.is_clear(line) != expect.clear {
        return Err(format!(
            "row {x}: is_clear {}, expected {}",
            grid.is_clear(line),
            expect.clear
        ));
    }
    Ok(())
}

/// Sizes, ordering and content after a phase: the history and `topmost_line` match `history`,
/// the `wraps` and `is_clear` bits of every row match the generator, and the 1,000 sampled rows
/// (plus the first `skip_top`..`skip_top + 5` and the last 100) match it character by character.
/// `skip_top` leaves out the oldest rows, which a column round trip may have cut in the middle
/// of a line: the 126 -> 100 rewrap evicts rows at the top, and what is left of a line that lost
/// its first piece reflows to a different first row.
fn verify(
    side: &fc::Side,
    phase: &str,
    blocks: usize,
    history: usize,
    columns: usize,
    skip_top: usize,
) {
    let term = &side.term;
    let (h, lines, c) = size(term);
    assert_eq!((h, c), (history, columns), "{phase}: history {h}, columns {c}");
    assert_eq!(term.topmost_line().0, -(h as i32), "{phase}: topmost_line");
    assert_eq!(term.bottommost_line().0, lines as i32 - 1, "{phase}: bottommost_line");
    let total = h + lines;

    // Every row's two bits (cheap), which also proves the order of the wrapped pieces.
    let grid = term.grid();
    let mut wraps = 0usize;
    for x in skip_top..total {
        let line = Line(grid.topmost_line().0 + x as i32);
        match expected_at(x, total, blocks, columns) {
            Some(expect) => {
                assert_eq!(grid.wraps(line), expect.wraps, "{phase}: wraps of row {x}");
                assert_eq!(grid.is_clear(line), expect.clear, "{phase}: is_clear of row {x}");
                wraps += usize::from(expect.wraps);
            },
            None => {
                assert!(x == total - 1, "{phase}: row {x} has no expectation");
                assert!(grid.is_clear(line) && !grid.wraps(line), "{phase}: cursor row");
            },
        }
    }
    println!("  {phase}: {total} rows, {wraps} wrapped, bits of every row match");

    let mut samples: Vec<usize> = (skip_top..skip_top + 5).collect();
    samples.extend(total.saturating_sub(100)..total);
    samples.extend((0..1000).map(|i| skip_top + (mix(i) as usize) % (total - skip_top)));
    for x in samples {
        let Some(expect) = expected_at(x, total, blocks, columns) else { continue };
        if let Err(why) = check_row(term, x, &expect) {
            panic!("{phase}: {why}");
        }
    }
}

// -- driving --------------------------------------------------------------------------------

fn start(history: usize) -> Start {
    Start { lines: LINES, columns: COLUMNS, history, kitty: false, query_seed: 0 }
}

/// 500 blocks per `Step::Bytes`.
fn fill(mut apply: impl FnMut(&Step), blocks: usize, rich: bool) {
    let mut b = 0;
    while b < blocks {
        let end = (b + 500).min(blocks);
        let mut bytes = Vec::new();
        for block_index in b..end {
            bytes.extend(block(block_index, rich));
        }
        apply(&Step::Bytes(bytes));
        b = end;
    }
}

fn resize(lines: usize, columns: usize) -> Step {
    Step::Resize { lines, columns }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Keep-warm rows of the compressed variants.
const WARM: usize = 10_000;

/// Switch compression on for `side` (`Some(WARM)`) at history limit `limit`; the steps of
/// `fc::Side::apply` reset the options to the defaults, so the compressed variants apply their
/// limit changes through [`set_history`].
fn set_history(side: &mut fc::Side, limit: usize, compression: Option<usize>) {
    side.term.set_options(Config {
        scrolling_history: limit,
        history_compression: compression,
        ..Config::default()
    });
    side.term.run_history_jobs();
}

/// Most of the history is compressed, or a compressed variant would prove nothing new.
fn assert_cold(side: &fc::Side, phase: &str) {
    let cold = side.term.grid().cold_blocks_for_test();
    let count = cold.iter().filter(|&&c| c).count();
    println!("  {phase}: {count} of {} blocks compressed", cold.len());
    assert!(count * 2 > cold.len(), "{phase}: only {count} of {} blocks compressed", cold.len());
}

/// Apply `step` and, with compression on, run the jobs it queued (a host does so after each batch
/// of output and each resize).
fn apply(side: &mut fc::Side, step: &Step, compression: Option<usize>) {
    match step {
        Step::SetHistory(limit) => set_history(side, *limit, compression),
        _ => {
            side.apply(step);
            if compression.is_some() {
                side.term.run_history_jobs();
            }
        },
    }
}

// -- 100,000 lines against the oracle -------------------------------------------------------

/// The queries after each phase: whole-history selections of each kind, `bounds_to_string` at
/// both ends, regex searches in both directions (one matching, one not, one bounded), and the
/// motions around the oldest and newest rows.
fn plan() -> QueryPlan {
    let top = Pt::new(-1_000_000, 0);
    let bottom = Pt { line: i32::MAX, col: 125, right: true };
    let middle = Pt::new(-50_000, 40);
    let search = |pattern: &str, origin: Pt, right: bool, max_lines: Option<usize>| SearchQ {
        pattern: pattern.to_owned(),
        origin,
        right,
        side_right: false,
        max_lines,
        unmoved: None,
    };
    QueryPlan {
        selections: vec![
            (SelTy::Simple, top, bottom),
            (SelTy::Lines, top, Pt::new(-99_000, 0)),
            (SelTy::Block, Pt::new(-80_000, 10), Pt::new(-79_000, 60)),
            (SelTy::Semantic, middle, middle),
        ],
        bounds: (top, Pt::new(-1_000_000 + 50, 125)),
        searches: vec![
            search("WARN [A-Za-z0-9]{18}: [a-z]+1", bottom, false, None),
            search("ERROR", top, true, None),
            search("zzzzqqqq[0-9]", middle, true, None),
            search("zzzzqqqq[0-9]", middle, false, None),
            search("example", Pt::new(-60_000, 0), true, Some(30_000)),
            search("2026-10-2[0-9]T0[0-5]", Pt::new(-10, 0), false, Some(60_000)),
        ],
        moved_windows: 0,
        points: vec![top, middle, bottom],
        vi: (Pt::new(-99_990, 5), [0, 11, 14]),
    }
}

/// Compare the two terminals' snapshots and answers to `plan`.
fn compare(phase: &str, a: &mut fc::Side, b: &mut ac::Side, plan: &QueryPlan) {
    let started = Instant::now();
    let (sa, sb): (Snapshot, Snapshot) = (a.snapshot(), b.snapshot());
    if sa != sb {
        panic!("{phase}: {}", support::project::diff(&sa, &sb));
    }
    let rows = sa.rows.len();
    drop((sa, sb));

    let (qa, qb) = (a.queries(plan), b.queries(plan));
    assert_eq!(qa.len(), qb.len(), "{phase}: number of answers");
    let mut oracle_off = 0;
    let first = plan.selections.len() + 1;
    for (i, ((name, ra), (_, rb))) in qa.iter().zip(&qb).enumerate() {
        // Fork change 5: where 0.26.0 cannot answer a search, the fork's answer is checked on
        // its own, as `support::run` does.
        if let Some(search) = i.checked_sub(first).and_then(|k| plan.searches.get(k)) {
            let walked = b.walk(i - first);
            if b.oracle_search_is_off(search, rb, walked.as_ref()) {
                a.check_search_walked(search, search.max_lines, ra, walked.as_ref())
                    .unwrap_or_else(|why| panic!("{phase}: {name} {search:?}: {why}"));
                oracle_off += 1;
                continue;
            }
        }
        if ra != rb {
            let brief = |answer: &QAns| match answer {
                QAns::Text(Some(text)) => format!("Text({} bytes)", text.len()),
                other => format!("{other:?}"),
            };
            panic!("{phase}: {name} differs: feathercritty {}, 0.26.0 {}", brief(ra), brief(rb));
        }
    }
    let matches = qa.iter().filter(|(_, a)| matches!(a, QAns::Match(Some(_)))).count();
    // The plan's matching searches must find something, or the equal answers above would only
    // show that both crates agree on "no match".
    assert!(matches > 0, "{phase}: no search found a match");
    println!(
        "  {phase}: {rows} rows and {} answers equal ({matches} searches found a match, \
         {oracle_off} checked alone), {:.1} s",
        qa.len(),
        started.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "large; run with --release -- --ignored"]
fn large_history_matches_oracle_at_100k() {
    oracle_at_100k(None);
}

#[test]
#[ignore = "large; run with --release -- --ignored"]
fn large_history_matches_oracle_at_100k_compressed() {
    oracle_at_100k(Some(WARM));
}

fn oracle_at_100k(compression: Option<usize>) {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = Instant::now();
    let limit = 100_000;
    // 100,000 history rows and a little more, so the oldest rows are evicted as well.
    let blocks = 9_100;
    let mut a = fc::Side::new(&start(limit));
    let mut b = ac::Side::new(&start(limit));
    if compression.is_some() {
        set_history(&mut a, limit, compression);
    }

    fill(
        |step| {
            apply(&mut a, step, compression);
            b.apply(step);
        },
        blocks,
        true,
    );
    let plan = plan();
    println!(
        "large history vs 0.26.0, {limit}-line limit, {} lines of content",
        blocks * BLOCK_LINES
    );
    compare("fill", &mut a, &mut b, &plan);
    if compression.is_some() {
        assert_cold(&a, "fill");
    }

    // Scrolled back, so the display offset is part of what a resize must keep.
    let phases = [
        ("scroll back 2,000", Step::ScrollView(2_000)),
        ("126 -> 100 columns", resize(LINES, 100)),
        ("100 -> 126 columns", resize(LINES, 126)),
        ("scroll to bottom", Step::ScrollView(-2_000)),
        ("lines only 37 -> 30", resize(30, 126)),
        ("lines only 30 -> 45", resize(45, 126)),
        ("limit to 10,000", Step::SetHistory(10_000)),
    ];
    for (name, step) in phases {
        let t = Instant::now();
        apply(&mut a, &step, compression);
        let fork = t.elapsed();
        let t = Instant::now();
        b.apply(&step);
        let oracle = t.elapsed();
        println!("  {name}: feathercritty {:.1} ms, 0.26.0 {:.1} ms", ms(fork), ms(oracle));
        compare(name, &mut a, &mut b, &plan);
    }
    let history = a.term.grid().history_size();
    assert_eq!(history, 10_000);
    println!("done in {:.1} s", started.elapsed().as_secs_f64());
}

// -- 1,000,000 lines, invariants ------------------------------------------------------------

#[test]
#[ignore = "large; run with --release -- --ignored"]
fn large_history_invariants_at_1m() {
    invariants_at_1m(None);
}

#[test]
#[ignore = "large; run with --release -- --ignored"]
fn large_history_invariants_at_1m_compressed() {
    invariants_at_1m(Some(WARM));
}

fn invariants_at_1m(compression: Option<usize>) {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = Instant::now();
    let limit = 1_000_000;
    // 11 rows a block, and the 37-row screen: 7 rows more than the limit, so 7 are evicted.
    let blocks = 90_913;
    let mut side = fc::Side::new(&start(limit));
    if compression.is_some() {
        set_history(&mut side, limit, compression);
    }
    fill(|step| apply(&mut side, step, compression), blocks, false);
    println!("large history invariants, {limit}-line limit, {blocks} blocks");
    if compression.is_some() {
        assert_cold(&side, "fill");
    }
    verify(&side, "fill", blocks, limit, 126, 0);

    let steps = [
        ("126 -> 100 columns", resize(LINES, 100), Some(limit), 100, 2),
        ("100 -> 126 columns", resize(LINES, 126), None, 126, 2),
    ];
    let mut history = limit;
    for (name, step, expect, columns, skip) in steps {
        let t = Instant::now();
        apply(&mut side, &step, compression);
        let took = t.elapsed();
        history = side.term.grid().history_size();
        match expect {
            // Rows now exceed the limit: it is full, and the oldest rows went.
            Some(limit) => assert_eq!(history, limit, "{name}: history must be at the limit"),
            // The round trip loses what the 100-column pass evicted, and nothing else.
            None => assert!((900_000..limit).contains(&history), "{name}: history {history}"),
        }
        println!("  {name}: {:.0} ms, history {history}", ms(took));
        if compression.is_some() {
            assert_cold(&side, name);
        }
        verify(&side, name, blocks, history, columns, skip);
    }

    // Lines only: the rows move between the screen and the history; the total stays.
    let total = history + LINES;
    apply(&mut side, &resize(30, 126), compression);
    let (h, lines, _) = size(&side.term);
    assert_eq!((h + lines, h), (total, history + 7), "37 -> 30 lines");
    verify(&side, "lines only 37 -> 30", blocks, h, 126, 2);
    apply(&mut side, &resize(LINES, 126), compression);
    let (h, lines, _) = size(&side.term);
    assert_eq!((h + lines, h), (total, history), "30 -> 37 lines");
    verify(&side, "lines only 30 -> 37", blocks, h, 126, 2);

    // Lowering the limit keeps the newest rows.
    let t = Instant::now();
    apply(&mut side, &Step::SetHistory(10_000), compression);
    println!("  limit to 10,000: {:.0} ms", ms(t.elapsed()));
    verify(&side, "limit to 10,000", blocks, 10_000, 126, 0);

    println!("done in {:.1} s", started.elapsed().as_secs_f64());
}

// -- cost -----------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Content {
    Short,
    Log,
    Full,
}

impl Content {
    const ALL: [Content; 3] = [Content::Short, Content::Log, Content::Full];

    fn name(self) -> &'static str {
        match self {
            Content::Short => "short",
            Content::Log => "log",
            Content::Full => "full",
        }
    }

    fn from_name(name: &str) -> Content {
        *Content::ALL.iter().find(|c| c.name() == name).expect("unknown content")
    }

    fn label(self) -> &'static str {
        match self {
            Content::Short => "short lines (0-20 chars)",
            Content::Log => "log lines (95 chars, 4 colors)",
            Content::Full => "full-width lines (126 chars)",
        }
    }

    /// Line `i`, one row at 126 columns.
    fn line(self, i: usize) -> Vec<u8> {
        let seed = mix(i as u64);
        match self {
            Content::Short => {
                let len = (seed % 21) as usize;
                let text = alnum(seed, len);
                if len > 0 && seed >> 8 & 1 == 1 {
                    format!("\x1b[38;5;{}m{text}\x1b[0m", 1 + ((seed >> 16) & 7)).into_bytes()
                } else {
                    text.into_bytes()
                }
            },
            Content::Log => log_bytes(seed),
            Content::Full => alnum(seed, COLUMNS).into_bytes(),
        }
    }
}

/// Resident set size of this process in KiB, through `ps` (works on macOS and Linux).
fn rss_kib() -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Runs only as a child of `large_history_cost_table`: `LARGE_HISTORY_CELL=<content>,<lines>`.
/// Prints one `CELL|...` line.
#[test]
#[ignore = "child of large_history_cost_table"]
fn large_history_cost_cell() {
    let Ok(cell) = std::env::var("LARGE_HISTORY_CELL") else {
        println!("large_history_cost_cell: LARGE_HISTORY_CELL not set, nothing to do");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (content, lines) = cell.split_once(',').expect("<content>,<lines>");
    let content = Content::from_name(content);
    let history: usize = lines.parse().expect("lines");

    // `history + 36` lines and the cursor row make a history of exactly `history` rows.
    let build = || {
        let mut side = fc::Side::new(&start(history));
        let mut buffer = Vec::new();
        for i in 0..history + LINES - 1 {
            buffer.extend(content.line(i));
            buffer.extend_from_slice(b"\r\n");
            if buffer.len() > 64 * 1024 {
                side.apply(&Step::Bytes(std::mem::take(&mut buffer)));
            }
        }
        side.apply(&Step::Bytes(buffer));
        assert_eq!(side.term.grid().history_size(), history);
        side
    };

    let rss_base = rss_kib();
    start_counting();
    let fill_started = Instant::now();
    let mut side = build();
    let fill = fill_started.elapsed();
    let live = live_bytes();
    let rss_fill = rss_kib();

    let time = |side: &mut fc::Side, step: Step| {
        let t = Instant::now();
        side.apply(&step);
        ms(t.elapsed())
    };
    let (mut narrow, mut wide, mut shrink, mut grow) = (vec![], vec![], vec![], vec![]);
    for _ in 0..5 {
        narrow.push(time(&mut side, resize(LINES, 100)));
        wide.push(time(&mut side, resize(LINES, 126)));
    }
    for _ in 0..5 {
        shrink.push(time(&mut side, resize(30, 126)));
        grow.push(time(&mut side, resize(LINES, 126)));
    }
    let live_after_resizes = live_bytes();
    let rss_resized = rss_kib();
    let mut trims = vec![time(&mut side, Step::SetHistory(10_000))];
    let live_trimmed = live_bytes();
    let rss_trimmed = rss_kib();
    stop_counting();
    // The trim destroys what it measures: four more fresh terminals for the median.
    drop(side);
    for _ in 0..4 {
        let mut side = build();
        trims.push(time(&mut side, Step::SetHistory(10_000)));
    }

    let stat = |v: &mut Vec<f64>| {
        let (min, max) =
            (v.iter().copied().fold(f64::MAX, f64::min), v.iter().copied().fold(0.0, f64::max));
        format!("{:.3};{min:.3};{max:.3}", median(v))
    };
    let kib = |v: Option<u64>| v.map_or("n/a".to_owned(), |v| v.to_string());
    println!(
        "CELL|{}|{history}|{live}|{}|{}|{}|{live_after_resizes}|{live_trimmed}|{}|{:.\
         0}|{}|{}|{}|{}|{}",
        content.name(),
        kib(rss_base),
        kib(rss_fill),
        kib(rss_resized),
        kib(rss_trimmed),
        ms(fill),
        stat(&mut narrow),
        stat(&mut wide),
        stat(&mut shrink),
        stat(&mut grow),
        stat(&mut trims),
    );
}

/// One measured cell, as the child printed it.
struct Cell1 {
    content: Content,
    history: usize,
    live: usize,
    rss_base: Option<u64>,
    rss_fill: Option<u64>,
    rss_resized: Option<u64>,
    live_resized: usize,
    live_trimmed: usize,
    fill_ms: f64,
    /// "median;min;max" of each timing.
    times: [String; 5],
}

fn run_cell(content: Content, history: usize) -> Cell1 {
    let exe = std::env::current_exe().expect("test binary");
    let out = Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "large_history_cost_cell",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LARGE_HISTORY_CELL", format!("{},{history}", content.name()))
        .output()
        .expect("spawn the cell");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "cell failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // libtest prints "test <name> ... " first, so the line starts mid-line.
    let line = stdout
        .lines()
        .find_map(|l| l.find("CELL|").map(|at| &l[at..]))
        .unwrap_or_else(|| panic!("no CELL line in:\n{stdout}"));
    let f: Vec<&str> = line.split('|').collect();
    let kib = |s: &str| s.parse::<u64>().ok();
    Cell1 {
        content,
        history,
        live: f[3].parse().unwrap(),
        rss_base: kib(f[4]),
        rss_fill: kib(f[5]),
        rss_resized: kib(f[6]),
        live_resized: f[7].parse().unwrap(),
        live_trimmed: f[8].parse().unwrap(),
        fill_ms: f[10].parse().unwrap(),
        times: [11, 12, 13, 14, 15].map(|i| f[i].to_owned()),
    }
}

fn mb(bytes: usize) -> String {
    format!("{:.1}", bytes as f64 / 1e6)
}

fn mb_kib(kib: Option<u64>) -> String {
    kib.map_or("n/a".to_owned(), |k| format!("{:.1}", k as f64 * 1024.0 / 1e6))
}

fn spread(stat: &str) -> String {
    let p: Vec<&str> = stat.split(';').collect();
    format!("{} ({}-{})", p[0], p[1], p[2])
}

/// The corpora of the compressed columns: name and generator.
type Generator = fn(u64, usize) -> Vec<u8>;

/// A recorded stream repeated until it has `rows` lines (the streams have a few hundred lines;
/// a 64 KiB block holds about one copy, so the repetition helps a block's compression little,
/// see `benches/RESULTS.md`).
fn repeated(name: &str, rows: usize) -> Vec<u8> {
    let stream = corpora::recorded(name);
    let per_copy = stream.iter().filter(|&&b| b == b'\n').count();
    let copies = rows.div_ceil(per_copy);
    stream.repeat(copies)
}

fn recorded_cargo_build(_seed: u64, rows: usize) -> Vec<u8> {
    repeated("cargo-build", rows)
}

fn recorded_git_graph(_seed: u64, rows: usize) -> Vec<u8> {
    repeated("git-graph", rows)
}

/// The uniform random log lines of the tables above (`Content::Log`), which hardly compress.
fn random_log(_seed: u64, rows: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..rows {
        out.extend(Content::Log.line(i));
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The corpora: the realistic generators and recorded streams (gate: compressed at most 40% of
/// off), then the random content (gate: never above off).
const CORPORA: [(&str, Generator); 6] = [
    ("build log", corpora::build_log),
    ("service log", corpora::service_log),
    ("shell session", corpora::shell_session),
    ("recorded cargo build (repeated)", recorded_cargo_build),
    ("recorded git graph (repeated)", recorded_git_graph),
    ("random log lines (large_history.rs)", random_log),
];

/// Index of the random content in [`CORPORA`].
const RANDOM: usize = 5;

/// Live bytes of a `history`-row history of one corpus, as measured by [`corpus_live`].
#[derive(Clone, Copy, Default)]
struct CorpusLive {
    /// After the fill (with compression on, the jobs run after every 64 KiB of input).
    filled: usize,
    /// The highest live bytes during ten column resizes (jobs run after each).
    resize_peak: usize,
    /// After the resizes.
    resized: usize,
    /// Switching compression to the other state after the resizes: the highest live bytes
    /// while switching and running every job, and the live bytes afterwards.
    switch_peak: usize,
    switched: usize,
}

/// Live bytes of a `history`-row history of corpus `corpus` with compression `compression`.
fn corpus_live(corpus: usize, history: usize, compression: Option<usize>) -> CorpusLive {
    // Generated before counting, so the text is not part of the live bytes.
    let bytes = (CORPORA[corpus].1)(corpus as u64 + 1, history + LINES + 10);
    start_counting();
    let mut side = fc::Side::new(&start(history));
    set_history(&mut side, history, compression);
    for chunk in bytes.chunks(64 * 1024) {
        side.apply(&Step::Bytes(chunk.to_vec()));
        side.term.run_history_jobs();
    }
    assert_eq!(side.term.grid().history_size(), history);
    let filled = live_bytes();
    reset_peak();
    for _ in 0..5 {
        apply(&mut side, &resize(LINES, 100), compression);
        apply(&mut side, &resize(LINES, 126), compression);
    }
    let resize_peak = peak_bytes();
    let resized = live_bytes();
    reset_peak();
    let other = if compression.is_some() { None } else { Some(WARM) };
    set_history(&mut side, history, other);
    let switch_peak = peak_bytes();
    let switched = live_bytes();
    stop_counting();
    CorpusLive { filled, resize_peak, resized, switch_peak, switched }
}

/// Runs only as a child of `large_history_cost_table`: `LARGE_HISTORY_CORPUS=<index>,<lines>`.
/// Prints one `CCELL|...` line: [`CorpusLive`] off, then compressed.
#[test]
#[ignore = "child of large_history_cost_table"]
fn large_history_cost_corpus_cell() {
    let Ok(cell) = std::env::var("LARGE_HISTORY_CORPUS") else {
        println!("large_history_cost_corpus_cell: LARGE_HISTORY_CORPUS not set, nothing to do");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (corpus, lines) = cell.split_once(',').expect("<index>,<lines>");
    let (corpus, history): (usize, usize) = (corpus.parse().unwrap(), lines.parse().unwrap());
    let off = corpus_live(corpus, history, None);
    let on = corpus_live(corpus, history, Some(WARM));
    let fields = |c: CorpusLive| {
        format!("{}|{}|{}|{}|{}", c.filled, c.resize_peak, c.resized, c.switch_peak, c.switched)
    };
    println!("CCELL|{corpus}|{history}|{}|{}", fields(off), fields(on));
}

/// One corpus cell as the child printed it: (off, compressed).
fn run_corpus_cell(corpus: usize, history: usize) -> (CorpusLive, CorpusLive) {
    let exe = std::env::current_exe().expect("test binary");
    let out = Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "large_history_cost_corpus_cell",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LARGE_HISTORY_CORPUS", format!("{corpus},{history}"))
        .output()
        .expect("spawn the corpus cell");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "corpus cell failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = stdout
        .lines()
        .find_map(|l| l.find("CCELL|").map(|at| &l[at..]))
        .unwrap_or_else(|| panic!("no CCELL line in:\n{stdout}"));
    let f: Vec<usize> = line.split('|').skip(3).map(|v| v.parse().unwrap()).collect();
    let live = |f: &[usize]| CorpusLive {
        filled: f[0],
        resize_peak: f[1],
        resized: f[2],
        switch_peak: f[3],
        switched: f[4],
    };
    (live(&f[..5]), live(&f[5..]))
}

/// Measures and prints the tables of `benches/RESULTS.md`, "Large histories".
#[test]
#[ignore = "large; run with --release -- --ignored"]
fn large_history_cost_table() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = Instant::now();
    let mut cells = Vec::new();
    for history in [100_000, 1_000_000] {
        for content in Content::ALL {
            cells.push(run_cell(content, history));
        }
    }

    let dense_row = COLUMNS * std::mem::size_of::<Cell>();
    let baseline = cells.iter().filter_map(|c| c.rss_base).max();
    println!(
        "\nMemory (MB = 10^6 bytes). Dense = what 0.26.0's rows take ({dense_row} bytes a row at \
         {COLUMNS} columns). RSS of the process, whose empty baseline was {} MB:\n",
        mb_kib(baseline)
    );
    println!(
        "| content | history lines | live MB | bytes per row | dense MB | RSS MB after fill | RSS \
         MB after 10 resizes | live MB after 10 resizes | live MB after trim to 10,000 |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for c in &cells {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            c.content.label(),
            c.history,
            mb(c.live),
            c.live / c.history,
            mb(c.history * dense_row),
            mb_kib(c.rss_fill),
            mb_kib(c.rss_resized),
            mb(c.live_resized),
            mb(c.live_trimmed),
        );
    }
    println!("\nTime (ms; median of 5 with the range in parentheses; fill once):\n");
    println!(
        "| content | history lines | fill | 126->100 columns | 100->126 columns | 37->30 lines | \
         30->37 lines | trim to 10,000 |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for c in &cells {
        println!(
            "| {} | {} | {:.0} | {} | {} | {} | {} | {} |",
            c.content.label(),
            c.history,
            c.fill_ms,
            spread(&c.times[0]),
            spread(&c.times[1]),
            spread(&c.times[2]),
            spread(&c.times[3]),
            spread(&c.times[4]),
        );
    }

    println!(
        "\nCorpora, compression off against on (keep {WARM} rows warm; live MB; gate at 1,000,000 \
         lines: compressed at most 40% of off, random content never above off; peaks over ten \
         column resizes, jobs run after each; switching: off to on for the off column, on to off \
         for the compressed one):\n"
    );
    println!(
        "| corpus | history lines | off MB | compressed MB | compressed / off | gate | off peak \
         MB in resizes | compressed peak MB in resizes | off MB after resizes | compressed MB \
         after resizes | switching on: peak MB / after | switching off: peak MB / after |"
    );
    println!("|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|");
    let mut failed = Vec::new();
    for history in [100_000, 1_000_000] {
        for (i, (name, _)) in CORPORA.iter().enumerate() {
            let (off, on) = run_corpus_cell(i, history);
            let ratio = on.filled as f64 / off.filled as f64;
            let gate = if history != 1_000_000 {
                "-".to_owned()
            } else {
                let (ok, rule) = if i == RANDOM {
                    (on.filled <= off.filled, "<= off")
                } else {
                    (ratio <= 0.40, "<= 40%")
                };
                // Spec 6.4: the reflow's peak never above compression off's, and switching on
                // never above the uncompressed size (reported in bytes when it is).
                let reflow_ok = on.resize_peak <= off.resize_peak;
                let over = off.switch_peak.saturating_sub(off.resized);
                if !ok || !reflow_ok || over > 0 {
                    failed.push(format!("{name} at {history}"));
                }
                format!(
                    "{} ({rule}); reflow peak {}; switching on {}",
                    if ok { "pass" } else { "FAIL" },
                    if reflow_ok { "pass" } else { "FAIL" },
                    if over == 0 { "pass".to_owned() } else { format!("FAIL (+{over} B)") },
                )
            };
            println!(
                "| {name} | {history} | {} | {} | {:.1}% | {gate} | {} | {} | {} | {} | {} / {} | \
                 {} / {} |",
                mb(off.filled),
                mb(on.filled),
                ratio * 100.0,
                mb(off.resize_peak),
                mb(on.resize_peak),
                mb(off.resized),
                mb(on.resized),
                mb(off.switch_peak),
                mb(off.switched),
                mb(on.switch_peak),
                mb(on.switched),
            );
        }
    }
    println!("\nmeasured in {:.0} s", started.elapsed().as_secs_f64());
    // A measurement, like the tables above: misses are printed, not asserted.
    println!("memory gates missed: {failed:?}");
}
