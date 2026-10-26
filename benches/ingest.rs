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
//! Run with `cargo bench --bench ingest [-- --gate]`. To move the comparison to a new upstream
//! release see `common/mod.rs`.

mod common;

use common::{
    BUFFER_BYTES, HISTORY, SIZES, Workload, ac, compare_ms, fc, interleaved_ms, prefill_buffer,
    runs, selected, table_header, table_row, timed_gate_requested, timed_ms, workload_for,
};

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
    if gate && failed > 0 {
        eprintln!("{failed} ingest row(s) missed their threshold");
        std::process::exit(1);
    }
}
