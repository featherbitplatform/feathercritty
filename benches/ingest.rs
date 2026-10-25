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
//! | alternate-screen redraw, typing echo | 1.01 |
//!
//! Log lines and wide characters are measured but not gated (spec 6.3 lists no threshold).
//! `BENCH_ONLY=<substring>` limits the rows, for quick runs.
//!
//! Run with `cargo bench --bench ingest [-- --gate]`. To move the comparison to a new upstream
//! release see `common/mod.rs`.

mod common;

use common::{
    BUFFER_BYTES, HISTORY, SIZES, Workload, ac, compare_ms, fc, gate_requested, prefill_buffer,
    runs, selected, table_header, table_row, workload_for,
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
    let gate = gate_requested();
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
            // The 1% rows need more samples to tell a real difference from noise.
            let runs =
                if threshold(kind).is_some_and(|t| t <= 1.01) { runs().max(15) } else { runs() };
            let (ac_stats, fc_stats) = compare_ms(
                runs,
                || {
                    let mut sim = ac::Sim::new(columns, lines, HISTORY);
                    sim.feed(&prefill, 64 * 1024);
                    assert_eq!(sim.history_size(), HISTORY);
                    sim
                },
                |mut sim| {
                    sim.feed(&buffer, chunk);
                    sim
                },
                || {
                    let mut sim = fc::Sim::new(columns, lines, HISTORY);
                    sim.feed(&prefill, 64 * 1024);
                    assert_eq!(sim.history_size(), HISTORY);
                    sim
                },
                |mut sim| {
                    sim.feed(&buffer, chunk);
                    sim
                },
            );
            if !table_row(&label, ac_stats, fc_stats, threshold(kind), None) {
                failed += 1;
            }
        }
    }
    if gate && failed > 0 {
        eprintln!("{failed} ingest row(s) missed their threshold");
        std::process::exit(1);
    }
}
