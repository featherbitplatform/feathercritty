// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Library-level history reads and resizes, feathercritty against `alacritty_terminal` 0.26.0
//! (the differential test's patched oracle, `tests/oracle/PATCHES.md`) in the same binary
//! (design spec section 6.4). The fbterm-level rows
//! of that table (search across tabs, export, draw list) are measured in the terminal repository.
//!
//! The terminal holds a full 10,000-row history of log lines (about 95 characters, colored) at
//! 126 columns. Rows and thresholds (maximum fork/0.26.0 median ratio):
//!
//! | Row | Threshold |
//! |---|---|
//! | regex search over the whole buffer, no match | 1.5 |
//! | `selection_to_string` over 1,000 history rows | 1.5 |
//! | resize with reflow 126->100->126, and 200->80 | 2.0, or fork under 10 ms |
//! | resize of lines only, one step of up to 10 lines, 126 and 200 columns | fork median at most 50 µs |
//! | resize of lines only, the whole screen down to 1 line and back | fork median at most 1 ms |
//!
//! The lines-only rows have absolute bounds, not ratios (project ruling on spec 6.4,
//! 2026-10-25). 0.26.0 keeps screen and history in one ring, so a lines-only resize there is an
//! O(1) rotation and a few counter updates; feathercritty keeps the history apart (spec 5.5), so
//! the rows that cross between screen and history are moved, one by one. A ratio against an
//! O(1) operation measures nothing useful; the bounds keep the cost far below anything a user
//! notices while resizing. The old ratio row (37->30->37, 2,000 repetitions per run) is still
//! printed, marked informational. When merging upstream: if upstream changes the resize path,
//! re-check these bounds rather than the ratio.
//!
//! The "peak allocated bytes during the resize below 0.26.0" part of the resize gate needs the
//! counting allocator and is checked by `examples/memory.rs --gate`.
//!
//! Prints a table `row | 0.26.0 ms | fork ms | ratio | spread | gate`. `--gate` exits non-zero on
//! a missed threshold; `BENCH_RUNS` and `BENCH_ONLY` as in `benches/ingest.rs`. Run with
//! `cargo bench --bench reads [-- --gate]`; to move the comparison to a new upstream release see
//! `common/mod.rs`.

mod common;

use common::{
    HISTORY, Stats, Workload, ac, compare_ms, fc, runs, selected, table_header, table_row,
    table_row_abs, timed_gate_requested, workload_lines,
};

/// Repetitions of the lines-only resize inside one timed run (one pair takes microseconds).
const LINES_ONLY_REPS: usize = 2000;

macro_rules! sims {
    ($columns:expr, $lines:expr, $buffer:expr) => {{
        let make_ac = || {
            let mut sim = ac::Sim::new($columns, $lines, HISTORY);
            sim.feed($buffer, 64 * 1024);
            assert_eq!(sim.history_size(), HISTORY);
            sim
        };
        let make_fc = || {
            let mut sim = fc::Sim::new($columns, $lines, HISTORY);
            sim.feed($buffer, 64 * 1024);
            assert_eq!(sim.history_size(), HISTORY);
            sim
        };
        (make_ac, make_fc)
    }};
}

fn main() {
    let gate = timed_gate_requested();
    let mut failed = 0;
    let mut row =
        |label: &str, (a, f): (common::Stats, common::Stats), max: f64, floor: Option<f64>| {
            if !table_row(label, a, f, Some(max), floor) {
                failed += 1;
            }
        };
    table_header();

    let narrow = workload_lines(Workload::LogLines95, 12_000, 7, 126, 37);
    let wide = workload_lines(Workload::LogLines95, 12_000, 7, 200, 60);

    let label = "regex search, no match 126x37";
    if selected(label) {
        let (make_ac, make_fc) = sims!(126, 37, &narrow);
        let stats = compare_ms(
            runs(),
            make_ac,
            |sim| {
                assert!(sim.search_no_match());
                sim
            },
            make_fc,
            |sim| {
                assert!(sim.search_no_match());
                sim
            },
        );
        row(label, stats, 1.5, None);
    }

    let label = "selection_to_string 1000 rows 126x37";
    if selected(label) {
        let (make_ac, make_fc) = sims!(126, 37, &narrow);
        let stats = compare_ms(
            runs(),
            make_ac,
            |mut sim| {
                assert!(sim.select_history_rows(1000) > 0);
                sim
            },
            make_fc,
            |mut sim| {
                assert!(sim.select_history_rows(1000) > 0);
                sim
            },
        );
        row(label, stats, 1.5, None);
    }

    let label = "resize reflow 126->100->126";
    if selected(label) {
        let (make_ac, make_fc) = sims!(126, 37, &narrow);
        let stats = compare_ms(
            runs(),
            make_ac,
            |mut sim| {
                sim.resize(100, 37);
                sim.resize(126, 37);
                sim
            },
            make_fc,
            |mut sim| {
                sim.resize(100, 37);
                sim.resize(126, 37);
                sim
            },
        );
        row(label, stats, 2.0, Some(10.0));
    }

    let label = "resize reflow 200->80";
    if selected(label) {
        let (make_ac, make_fc) = sims!(200, 60, &wide);
        let stats = compare_ms(
            runs(),
            make_ac,
            |mut sim| {
                sim.resize(80, 60);
                sim
            },
            make_fc,
            |mut sim| {
                sim.resize(80, 60);
                sim
            },
        );
        row(label, stats, 2.0, Some(10.0));
    }

    let label = "lines only 37->30->37 x2000 (info)";
    if selected(label) {
        let (make_ac, make_fc) = sims!(126, 37, &narrow);
        let stats = compare_ms(
            runs(),
            make_ac,
            |mut sim| {
                for _ in 0..LINES_ONLY_REPS {
                    sim.resize(126, 30);
                    sim.resize(126, 37);
                }
                sim
            },
            make_fc,
            |mut sim| {
                for _ in 0..LINES_ONLY_REPS {
                    sim.resize(126, 30);
                    sim.resize(126, 37);
                }
                sim
            },
        );
        table_row(label, stats.0, stats.1, None, None);
    }

    // Absolute bounds for lines-only resizes (see the module docs). Each sample is one resize
    // step (or one shrink-then-grow pair), timed on its own over `LINES_ONLY_REPS` steps.
    for (columns, lines, buffer) in [(126, 37, &narrow), (200, 60, &wide)] {
        let label = format!("lines only step <=10 {columns}x{lines}");
        if selected(&label) {
            let (make_ac, make_fc) = sims!(columns, lines, buffer);
            let (mut a, mut f) = (make_ac(), make_fc());
            let step = |n: usize| if n % 2 == 0 { lines - 10 } else { lines };
            let ac = step_samples(LINES_ONLY_REPS, |n| a.resize(columns, step(n)));
            let fc = step_samples(LINES_ONLY_REPS, |n| f.resize(columns, step(n)));
            if !table_row_abs(&label, ac, fc, 0.050) {
                failed += 1;
            }
        }

        let label = format!("lines only {lines}->1->{lines} {columns}x{lines}");
        if selected(&label) {
            let (make_ac, make_fc) = sims!(columns, lines, buffer);
            let (mut a, mut f) = (make_ac(), make_fc());
            let reps = LINES_ONLY_REPS / 10;
            let ac = step_samples(reps, |_| {
                a.resize(columns, 1);
                a.resize(columns, lines);
            });
            let fc = step_samples(reps, |_| {
                f.resize(columns, 1);
                f.resize(columns, lines);
            });
            if !table_row_abs(&label, ac, fc, 1.0) {
                failed += 1;
            }
        }
    }

    if gate && failed > 0 {
        eprintln!("{failed} read row(s) missed their threshold");
        std::process::exit(1);
    }
}

/// `reps` calls of `step` (given the call's index), each timed on its own, in milliseconds,
/// after `reps / 10` untimed warm-up calls.
fn step_samples(reps: usize, mut step: impl FnMut(usize)) -> Stats {
    for n in 0..reps / 10 {
        step(n);
    }
    let samples = (0..reps)
        .map(|n| {
            let start = std::time::Instant::now();
            step(n);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    Stats::from_samples(samples)
}
