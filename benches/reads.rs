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
//! # History compression (README change 9; its design spec 6.4, library rows)
//!
//! A second table, feathercritty only: compression off (`0.26.0` column) against
//! `history_compression: Some(10_000)` with every job run (`fork` column), at a 1,000,000-line
//! history, 126x37. The terminals are built once per subject and read repeatedly (a read leaves
//! only the 4-block read cache behind; a sweep starts at the far end, so it misses anyway).
//!
//! | Row | Gate |
//! |---|---|
//! | regex search over the whole buffer, no match (log lines) | at most 2.0x |
//! | `selection_to_string` over 1,000 rows 500,000 rows up (log lines), 20 depths so the read cache misses | recorded (the spec has no selection row) |
//! | every history row read through `RowReader` and turned into text, the way an export reads (log lines) | at most 2.0x |
//! | resize with reflow, 126->100 and 100->126 columns, service-log corpus (`tests/corpora`), jobs run between passes | at most off plus the time to decompress the old history once, and at most 1 s, per pass |
//! | switching compression on, then off, for a full 1,000,000-line history, with the jobs run the way the worker runs them | recorded: total time and the longest lock holds (p99 at most 0.1 ms, as for the worker in `benches/ingest.rs`) |
//! | decompressing one 64 KiB block, as `cold.rs` does it (a new zstd context and a zeroed buffer per call) and with a reused context and buffer | recorded |
//!
//! Peak live bytes during the reflow, and memory while switching, need the counting allocator
//! and are measured by `tests/large_history.rs` (`large_history_cost_table`).
//!
//! Prints a table `row | 0.26.0 ms | fork ms | ratio | spread | gate`. `--gate` exits non-zero on
//! a missed threshold; `BENCH_RUNS` and `BENCH_ONLY` as in `benches/ingest.rs`. Run with
//! `cargo bench --bench reads [-- --gate]`; to move the comparison to a new upstream release see
//! `common/mod.rs`.

mod common;
#[path = "../tests/corpora/mod.rs"]
mod corpora;

use std::sync::Arc;
use std::time::Instant;

use common::{
    BIG_HISTORY, HISTORY, KEEP_WARM, Stats, Workload, ac, big_prefill, compare_ms, fc, fc_sim,
    interleaved_ms, percentile, runs, selected, table_header, table_row, table_row_abs,
    timed_gate_requested, worker, workload_lines,
};
use feathercritty::grid::{Dimensions, RowReader, cold_frame_for_test, decompress_cold_for_test};
use feathercritty::index::{Column, Line, Point, Side};
use feathercritty::selection::{Selection, SelectionType};
use feathercritty::term::cell::Cell;
use parking_lot::Mutex;

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

    failed += compression();
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

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Times `off` and `on` interleaved, `runs` rounds after a warm-up each.
fn persistent(runs: usize, off: &mut dyn FnMut(), on: &mut dyn FnMut()) -> (Stats, Stats) {
    let stats = interleaved_ms(runs, &mut [
        &mut || {
            let start = Instant::now();
            off();
            ms_since(start)
        },
        &mut || {
            let start = Instant::now();
            on();
            ms_since(start)
        },
    ]);
    (stats[0], stats[1])
}

/// `selection_to_string` over `rows` rows starting `up` rows above the screen; its length.
fn select_at(sim: &mut fc::Sim, up: usize, rows: usize) -> usize {
    let columns = sim.term.columns();
    let start = Point::new(Line(-(up as i32)), Column(0));
    let end = Point::new(Line(-(up as i32) + rows as i32 - 1), Column(columns - 1));
    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
    selection.update(end, Side::Right);
    sim.term.selection = Some(selection);
    let text = sim.term.selection_to_string().unwrap_or_default();
    sim.term.selection = None;
    text.len()
}

/// Every history row through `RowReader`, as text (an export's reads); total bytes of text.
fn export_text(sim: &fc::Sim) -> usize {
    let grid = sim.term.grid();
    let mut reader: RowReader<Cell> = RowReader::new();
    let mut line = String::new();
    let mut total = 0;
    for l in grid.topmost_line().0..0 {
        line.clear();
        line.extend(reader.row(grid, Line(l)).into_iter().map(|cell| cell.c));
        total += line.trim_end().len() + 1;
    }
    total
}

/// The cold blocks of `sim`'s history, as stored.
fn cold_blocks(sim: &fc::Sim) -> Vec<Vec<u8>> {
    let grid = sim.term.grid();
    grid.history_blocks_for_test()
        .into_iter()
        .zip(grid.cold_blocks_for_test())
        .filter(|&(_, cold)| cold)
        .map(|(bytes, _)| bytes.to_vec())
        .collect()
}

/// Decompress every block of `cold` with `cold.rs` itself: a new context and a zeroed buffer per
/// block.
fn decompress_all(cold: &[Vec<u8>], out: &mut Vec<u8>) {
    for block in cold {
        decompress_cold_for_test(block, out);
    }
}

/// [`decompress_all`] with one reused context, into a reused buffer that is not zeroed.
fn decompress_all_reused(
    cold: &[Vec<u8>],
    context: &mut zstd::bulk::Decompressor<'_>,
    out: &mut Vec<u8>,
) {
    for block in cold {
        let (len, frame) = cold_frame_for_test(block);
        out.clear();
        out.reserve(len);
        let written = context.decompress_to_buffer(frame, out).unwrap();
        assert_eq!(written, len);
    }
}

/// The history compression table (see the module docs); returns the number of missed gates.
fn compression() -> usize {
    let labels = [
        "1M: regex search, no match",
        "1M: selection_to_string 1000 rows",
        "1M: export-equivalent read",
        "1M: reflow 126->100",
        "1M: reflow 100->126",
        "1M: switch on",
        "1M: switch off",
        "decompress one block",
    ];
    if !labels.iter().any(|label| selected(label)) {
        return 0;
    }

    let mut failed = 0;
    let (columns, lines) = (126, 37);
    println!(
        "\n== history compression, 1,000,000-line history (feathercritty only; the 0.26.0 column \
         is compression off, the fork column compression on) =="
    );
    table_header();

    let prefill = big_prefill(columns);
    let filled = |keep_warm: Option<usize>| {
        let mut sim = fc_sim(columns, lines, BIG_HISTORY, keep_warm);
        sim.feed(&prefill, 64 * 1024);
        assert_eq!(sim.history_size(), BIG_HISTORY);
        sim.term.run_history_jobs();
        sim
    };

    if labels[..3].iter().any(|label| selected(label)) {
        let mut off = filled(None);
        let mut on = filled(Some(KEEP_WARM));
        assert!(cold_blocks(&on).len() > 1000, "the history is not compressed");

        let label = labels[0];
        if selected(label) {
            let stats = persistent(runs(), &mut || assert!(off.search_no_match()), &mut || {
                assert!(on.search_no_match())
            });
            failed += usize::from(!table_row(label, stats.0, stats.1, Some(2.0), None));
        }

        let label = labels[1];
        if selected(label) {
            let expected = select_at(&mut off, 500_000, 1000);
            assert_eq!(select_at(&mut on, 500_000, 1000), expected);
            // 20 selections per sample, each at another depth, so the read cache misses.
            let depth = |k: usize| 500_000 + k * 20_000;
            let stats = persistent(
                runs(),
                &mut || {
                    for k in 0..20 {
                        std::hint::black_box(select_at(&mut off, depth(k), 1000));
                    }
                },
                &mut || {
                    for k in 0..20 {
                        std::hint::black_box(select_at(&mut on, depth(k), 1000));
                    }
                },
            );
            // Recorded, not gated (spec 6.4 has no selection row): each selection here starts
            // with an empty read cache, so it pays for about four blocks of decompression,
            // which takes longer than reading those rows warm. A selection made by dragging
            // over the rows has just read them, and its blocks are in the cache.
            table_row(label, stats.0, stats.1, None, None);
        }

        let label = labels[2];
        if selected(label) {
            assert_eq!(export_text(&off), export_text(&on));
            let stats = persistent(
                runs(),
                &mut || {
                    std::hint::black_box(export_text(&off));
                },
                &mut || {
                    std::hint::black_box(export_text(&on));
                },
            );
            failed += usize::from(!table_row(label, stats.0, stats.1, Some(2.0), None));
        }
    }

    if labels[3..5].iter().any(|label| selected(label)) {
        // The service-log corpus: realistic, compressible, and some lines wider than 100
        // columns, so the reflow both copies records and rewraps rows.
        let corpus = corpora::service_log(2, BIG_HISTORY + 40);
        let build = |keep_warm: Option<usize>| {
            let mut sim = fc_sim(columns, lines, BIG_HISTORY, keep_warm);
            sim.feed(&corpus, 64 * 1024);
            assert_eq!(sim.history_size(), BIG_HISTORY);
            sim.term.run_history_jobs();
            sim
        };
        let (mut off, mut on) = (build(None), build(Some(KEEP_WARM)));
        let mut out = Vec::new();
        let (mut narrow, mut wide, mut decompress) =
            ((Vec::new(), Vec::new()), (Vec::new(), Vec::new()), (Vec::new(), Vec::new()));
        for round in 0..runs() + 1 {
            for (to, times, slot) in [(100, &mut narrow, 0), (126, &mut wide, 1)] {
                let cold = cold_blocks(&on);
                let start = Instant::now();
                decompress_all(&cold, &mut out);
                let decompressed = ms_since(start);
                // Alternate which subject goes first.
                let order: [bool; 2] = if round % 2 == 0 { [false, true] } else { [true, false] };
                let (mut t_off, mut t_on) = (0.0, 0.0);
                for compressed in order {
                    let sim = if compressed { &mut on } else { &mut off };
                    let start = Instant::now();
                    sim.resize(to, lines);
                    let t = ms_since(start);
                    if compressed {
                        t_on = t;
                        sim.term.run_history_jobs();
                    } else {
                        t_off = t;
                    }
                }
                if round > 0 {
                    times.0.push(t_off);
                    times.1.push(t_on);
                    if slot == 0 {
                        decompress.0.push(decompressed);
                    } else {
                        decompress.1.push(decompressed);
                    }
                }
            }
        }
        for (label, times, decompressed) in
            [(labels[3], narrow, decompress.0), (labels[4], wide, decompress.1)]
        {
            let (off, on) = (Stats::from_samples(times.0), Stats::from_samples(times.1));
            let decompressed = Stats::from_samples(decompressed);
            let bound = (off.median + decompressed.median).min(1000.0);
            let ok = on.median <= bound;
            failed += usize::from(!ok);
            println!(
                "{label:<34} | {:>12.3} | {:>10.3} | {:>6.3} | {:>7.1}% | {} (<= off + decompress \
                 {:.3} ms = {bound:.3} ms, and <= 1000 ms)",
                off.median,
                on.median,
                on.median / off.median,
                off.spread_pct().max(on.spread_pct()),
                if ok { "pass" } else { "FAIL" },
                decompressed.median,
            );
        }
    }

    for (label, from, to) in
        [(labels[5], None, Some(KEEP_WARM)), (labels[6], Some(KEEP_WARM), None)]
    {
        if !selected(label) {
            continue;
        }
        let (mut totals, mut p99s, mut maxes, mut jobs) = (vec![], vec![], vec![], vec![]);
        for _ in 0..runs().max(5) {
            let sim = Arc::new(Mutex::new(filled(from)));
            let mut report = worker::Report::default();
            let start = Instant::now();
            {
                let mut term = sim.lock();
                term.term.set_options(feathercritty::term::Config {
                    scrolling_history: BIG_HISTORY,
                    history_compression: to,
                    ..Default::default()
                });
            }
            worker::drain(&sim, &mut report);
            totals.push(ms_since(start));
            assert!(!sim.lock().term.has_history_jobs());
            assert_eq!(report.refused, 0);
            let mut holds = report.holds;
            p99s.push(percentile(&mut holds, 99.0));
            maxes.push(percentile(&mut holds, 100.0));
            jobs.push(report.jobs as f64);
        }
        let (total, p99) = (Stats::from_samples(totals), Stats::from_samples(p99s));
        let ok = p99.median <= 0.1;
        failed += usize::from(!ok);
        println!(
            "{label:<34} | total {:.1} ms ({:.1}-{:.1}) | {:.0} jobs | holds p99 {:.4} ms, max \
             {:.4} ms | {} (p99 <= 0.1 ms)",
            total.median,
            total.min,
            total.max,
            Stats::from_samples(jobs).median,
            p99.median,
            Stats::from_samples(maxes).median,
            if ok { "pass" } else { "FAIL" },
        );
    }

    let label = labels[7];
    if selected(label) {
        let cold = cold_blocks(&filled(Some(KEEP_WARM)));
        let (mut out, mut reused) = (Vec::new(), Vec::new());
        let mut context = zstd::bulk::Decompressor::new().unwrap();
        let stats = persistent(runs(), &mut || decompress_all(&cold, &mut out), &mut || {
            decompress_all_reused(&cold, &mut context, &mut reused)
        });
        let per = |ms: f64| ms * 1000.0 / cold.len() as f64;
        println!(
            "{label:<34} | as cold.rs {:.2} us | reused context and buffer {:.2} us | {:.3} | {} \
             blocks | -",
            per(stats.0.median),
            per(stats.1.median),
            stats.1.median / stats.0.median,
            cold.len(),
        );
    }
    failed
}
