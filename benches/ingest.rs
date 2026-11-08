// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Ingest throughput of `Processor::advance` on a `Term` with a full 10,000-row history,
//! feathercritty against `alacritty_terminal` 0.26.0 in the same binary (design spec section 6.3).
//! The comparison crate is the differential test's oracle: the published 0.26.0 source with the
//! two patches of `tests/oracle/PATCHES.md`, which do not change ingest timing measurably.
//!
//! Each row feeds a pre-generated buffer of at least 50 MB (see `common::workload_for`) to a
//! fresh terminal whose history was filled first (untimed), at 126x37 and 200x60, and prints the
//! median of `BENCH_RUNS` (default 7) interleaved runs of both crates, the spread
//! ((max - min) / median, larger of the two crates) and the verdict against the threshold.
//!
//! `--gate` exits non-zero when a row misses its threshold:
//!
//! | Workload | Allowed fork/0.26.0 median ratio |
//! |---|---|
//! | short lines | 1.10 |
//! | full-width plain, uniform rows, dense cells, region scroll | 1.05 |
//! | alternate-screen redraw, typing echo | 1.01, or the A/A spread (below) |
//!
//! **Noise ruling for the 1% rows** (project ruling, 2026-10-25): such a row runs at least 31
//! interleaved rounds (15 rounds are below this combined binary's noise; see
//! `benches/RESULTS.md`) and also times 0.26.0 against itself (A/A) in the same rounds. It passes
//! when the fork's median ratio is within `max(1%, A/A spread)`, the A/A spread being how far the
//! two 0.26.0 medians differ (`|A'/A - 1|`): a difference the same binary shows against itself
//! is noise, not a cost of the fork.
//!
//! Log lines and wide characters are measured but not gated (spec 6.3 lists no threshold).
//! `BENCH_ONLY=<substring>` limits the rows, for quick runs; `--gate` refuses it (and any filter
//! argument) and refuses `BENCH_RUNS` below 5, with an error and exit status 2.
//!
//! # History compression (README change 9; its design spec 6.4)
//!
//! Three more sections, feathercritty only, at a 1,000,000-line history of log lines (126x37,
//! `history_compression: Some(10_000)`), each run with a history worker on a second thread that
//! shares the terminal through a `parking_lot::Mutex`, as fbterm does (`common::worker`):
//!
//! | Row | Gate |
//! |---|---|
//! | parser-side ingest of the log-line buffer, compression on with the worker running, against compression off with the same locking | at most 1.03x |
//! | the worker's lock holds (`next_history_job` with its copy, `install_history_job`) while a 1,000,000-line history fills from empty, 5 fills | p99 at most 0.1 ms (median of the fills) |
//! | a sustained flood of log lines (`FLOOD_SECS`, default 60): warm blocks and the worker's CPU share | recorded; the backlog must stay bounded (most warm blocks in the second half of the flood at most 2 above the first half's) |
//!
//! "Off means off" (the same spec, 6.2: compression off within 1% of the fork's `main` on every
//! workload) needs the fork's `main` in the same binary and is measured outside this file; see
//! `benches/RESULTS.md`, "History compression".
//!
//! Run with `cargo bench --bench ingest [-- --gate]`. To move the comparison to a new upstream
//! release see `common/mod.rs`.

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::{
    BIG_HISTORY, BUFFER_BYTES, HISTORY, KEEP_WARM, SIZES, Stats, Workload, ac, big_prefill,
    compare_ms, fc, fc_sim, interleaved_ms, percentile, prefill_buffer, runs, selected,
    table_header, table_row, timed_gate_requested, timed_ms, worker, workload_for,
};
use parking_lot::Mutex;

/// Spec 6.3 thresholds (maximum fork/0.26.0 ratio), `None` where the spec sets none.
fn threshold(kind: Workload) -> Option<f64> {
    match kind {
        Workload::ShortLines => Some(1.10),
        Workload::FullWidthPlain
        | Workload::UniformRows
        | Workload::DenseCells
        | Workload::RegionScroll => Some(1.05),
        Workload::AltScreenRedraw | Workload::TypingEcho => Some(1.01),
        _ => None,
    }
}

fn main() {
    let gate = timed_gate_requested();
    let kinds = [
        Workload::ShortLines,
        Workload::FullWidthPlain,
        Workload::UniformRows,
        Workload::DenseCells,
        Workload::RegionScroll,
        Workload::AltScreenRedraw,
        Workload::TypingEcho,
        Workload::LogLines95,
        Workload::WideChars,
    ];
    let mut failed = 0;
    table_header();
    for (columns, lines) in SIZES {
        let prefill = prefill_buffer(columns);
        for kind in kinds {
            let label = format!("{} {columns}x{lines}", kind.name());
            if !selected(&label) {
                continue;
            }
            let buffer = workload_for(kind, BUFFER_BYTES, 42, columns, lines);
            let chunk = kind.chunk();
            let mut setup_ac = || {
                let mut sim = ac::Sim::new(columns, lines, HISTORY);
                sim.feed(&prefill, 64 * 1024);
                assert_eq!(sim.history_size(), HISTORY);
                sim
            };
            let mut setup_fc = || {
                let mut sim = fc::Sim::new(columns, lines, HISTORY);
                sim.feed(&prefill, 64 * 1024);
                assert_eq!(sim.history_size(), HISTORY);
                sim
            };
            let mut run_ac = |mut sim: ac::Sim| {
                sim.feed(&buffer, chunk);
                sim
            };
            let mut run_fc = |mut sim: fc::Sim| {
                sim.feed(&buffer, chunk);
                sim
            };

            let ok = if threshold(kind).is_some_and(|t| t <= 1.01) {
                // The 1% rows: at least 31 rounds, with an A/A comparison (see the module docs).
                let mut setup_ac2 = setup_ac;
                let mut run_ac2 = run_ac;
                let stats = interleaved_ms(runs().max(31), &mut [
                    &mut || timed_ms(&mut setup_ac, &mut run_ac),
                    &mut || timed_ms(&mut setup_fc, &mut run_fc),
                    &mut || timed_ms(&mut setup_ac2, &mut run_ac2),
                ]);
                let aa = (stats[2].median / stats[0].median - 1.0).abs();
                let allowed = 1.0 + aa.max(0.01);
                let ok = table_row(&label, stats[0], stats[1], Some(allowed), None);
                println!(
                    "{:<34} | {:>12.3} | {:>10} | {:>6.3} | {:>7.1}% | A/A spread {:.2}%",
                    format!("  A/A {label}"),
                    stats[2].median,
                    "",
                    stats[2].median / stats[0].median,
                    stats[2].spread_pct(),
                    aa * 100.0
                );
                ok
            } else {
                let (ac_stats, fc_stats) = compare_ms(runs(), setup_ac, run_ac, setup_fc, run_fc);
                table_row(&label, ac_stats, fc_stats, threshold(kind), None)
            };
            if !ok {
                failed += 1;
            }
        }
    }
    failed += compression();
    if gate && failed > 0 {
        eprintln!("{failed} ingest row(s) missed their threshold");
        std::process::exit(1);
    }
}

/// The history compression sections (see the module docs); returns the number of missed gates.
fn compression() -> usize {
    let (columns, lines) = (126, 37);
    let ingest = "log lines 95 126x37, 1M history, on+worker vs off";
    let holds = "worker lock holds, 1M-line fill";
    let flood = "flood with the worker";
    if ![ingest, holds, flood].iter().any(|label| selected(label)) {
        return 0;
    }

    let mut failed = 0;
    let prefill = big_prefill(columns);
    let filled = |keep_warm: Option<usize>| {
        let mut sim = fc_sim(columns, lines, BIG_HISTORY, keep_warm);
        sim.feed(&prefill, 64 * 1024);
        assert_eq!(sim.history_size(), BIG_HISTORY);
        sim.term.run_history_jobs();
        Arc::new(Mutex::new(sim))
    };
    let buffer = workload_for(Workload::LogLines95, BUFFER_BYTES, 42, columns, lines);

    println!(
        "\n== history compression (feathercritty only; the 0.26.0 column is compression off) =="
    );
    table_header();
    if selected(ingest) {
        // Both subjects run the same locking and worker; with compression off it never wakes.
        let run = |sim: Arc<Mutex<fc::Sim>>| {
            let worker = worker::Worker::start(Arc::clone(&sim));
            worker::feed(&sim, &worker, &buffer, 64 * 1024);
            (sim, worker)
        };
        let mut backlog = Vec::new();
        let (off, on) = compare_ms(
            runs(),
            || filled(None),
            |sim| {
                let (sim, worker) = run(sim);
                assert_eq!(worker.stop().jobs, 0, "jobs with compression off");
                sim
            },
            || filled(Some(KEEP_WARM)),
            |sim| {
                let (sim, worker) = run(sim);
                // Untimed from here: the parser side is done.
                backlog.push(sim.lock().term.has_history_jobs());
                let report = worker.stop();
                assert_eq!(report.refused, 0);
                sim
            },
        );
        if !table_row(ingest, off, on, Some(1.03), None) {
            failed += 1;
        }
        let behind = backlog.iter().filter(|&&b| b).count();
        println!("  jobs left when the parser finished: {behind} of {} runs", backlog.len());
    }

    if selected(holds) {
        // A 1,000,000-line fill from empty with the worker running, five times.
        let (mut p99s, mut maxes, mut counts) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..runs().max(5) {
            let sim = Arc::new(Mutex::new(fc_sim(columns, lines, BIG_HISTORY, Some(KEEP_WARM))));
            let worker = worker::Worker::start(Arc::clone(&sim));
            worker::feed(&sim, &worker, &prefill, 64 * 1024);
            let mut report = worker.stop();
            assert_eq!(report.refused, 0);
            counts.push(report.holds.len() as f64);
            p99s.push(percentile(&mut report.holds, 99.0));
            maxes.push(percentile(&mut report.holds, 100.0));
        }
        let p99 = Stats::from_samples(p99s);
        let ok = p99.median <= 0.1;
        failed += usize::from(!ok);
        println!(
            "{holds:<34} | p99 {:.4} ms (fills {:.4}-{:.4}) | max {:.4} ms (median of fills) | \
             {:.0} holds per fill | {} (p99 <= 0.1 ms)",
            p99.median,
            p99.min,
            p99.max,
            Stats::from_samples(maxes).median,
            Stats::from_samples(counts).median,
            if ok { "pass" } else { "FAIL" },
        );
    }

    if selected(flood) {
        let secs: f64 =
            std::env::var("FLOOD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60.0);
        let sim = filled(Some(KEEP_WARM));
        let worker = worker::Worker::start(Arc::clone(&sim));
        let warm_blocks = |sim: &Mutex<fc::Sim>| {
            sim.lock().term.grid().cold_blocks_for_test().iter().filter(|&&cold| !cold).count()
        };
        let started = Instant::now();
        let (mut first, mut second, mut bytes) = (0, 0, 0usize);
        while started.elapsed().as_secs_f64() < secs {
            for piece in buffer.chunks(64 * 1024 * 32) {
                worker::feed(&sim, &worker, piece, 64 * 1024);
                bytes += piece.len();
                let warm = warm_blocks(&sim);
                if started.elapsed().as_secs_f64() < secs / 2.0 {
                    first = first.max(warm);
                } else {
                    second = second.max(warm);
                }
            }
        }
        let wall = started.elapsed().as_secs_f64() * 1000.0;
        let report = worker.stop();
        let busy = report.run_ms + report.holds.iter().sum::<f64>();
        let ok = second <= first + 2;
        failed += usize::from(!ok);
        println!(
            "{flood:<34} | {:.0} s, {:.0} MB/s | most warm blocks: first half {first}, second \
             half {second}, after stop {} | worker {} jobs, {:.1}% of one core | {} (bounded)",
            wall / 1000.0,
            bytes as f64 / 1e6 / (wall / 1000.0),
            warm_blocks(&sim),
            report.jobs,
            busy / wall * 100.0,
            if ok { "pass" } else { "FAIL" },
        );
    }
    failed
}
