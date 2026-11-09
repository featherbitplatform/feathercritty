// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The compressor comparison behind the history compression design (spec 5.3 and 6.3). `#[ignore]`d
//! and meant for release builds:
//!
//! ```text
//! cargo test --release --test compress_choice -- --ignored --nocapture
//! ```
//!
//! Each corpus (`tests/corpora/`) is fed to a 126-column `Term` with a 1,000,000-line history;
//! the history's 64 KiB blocks are taken through the `test-hooks` accessor and each candidate
//! compresses and decompresses every block. Only the zstd levels remain: the other candidates were
//! dropped after the choice (their tables stay in benches/RESULTS.md). For each candidate the table
//! gives the ratio (compressed bytes over block bytes), compression and decompression speed (MB/s
//! of block bytes, block by block), and the projected size of the whole history against compression
//! off, both counted as block bytes plus 16 bytes of row index per row. The selection rule is spec
//! 5.3: at most 40% of off on every realistic corpus, then the fastest decompression, then pure
//! Rust when within 20% on both ratio and speed.

mod corpora;

use std::time::{Duration, Instant};

use feathercritty::event::VoidListener;
use feathercritty::grid::Dimensions;
use feathercritty::term::test::TermSize;
use feathercritty::term::{Config, Term};
use feathercritty::vte::ansi::Processor;

const COLUMNS: usize = 126;
const LINES: usize = 37;
const INDEX_BYTES_PER_ROW: usize = 16;

/// Rows per generated corpus (about 50 full blocks).
const ROWS: usize = 300_000;

struct History {
    blocks: Vec<Vec<u8>>,
    rows: usize,
}

fn fill(bytes: &[u8]) -> History {
    let config = Config { scrolling_history: 1_000_000, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), VoidListener);
    let mut parser: Processor = Processor::new();
    for chunk in bytes.chunks(4096) {
        parser.advance(&mut term, chunk);
    }
    let grid = term.grid();
    History {
        blocks: grid.history_blocks_for_test().into_iter().map(<[u8]>::to_vec).collect(),
        rows: grid.history_size(),
    }
}

struct Candidate {
    name: &'static str,
    pure_rust: bool,
    compress: fn(&[u8]) -> Vec<u8>,
    decompress: fn(&[u8], usize) -> Vec<u8>,
}

fn zstd1_compress(b: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(b, 1).unwrap()
}
fn zstd3_compress(b: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(b, 3).unwrap()
}
fn zstd_decompress(b: &[u8], size: usize) -> Vec<u8> {
    zstd::bulk::decompress(b, size).unwrap()
}
const CANDIDATES: &[Candidate] = &[
    Candidate {
        name: "zstd level 1",
        pure_rust: false,
        compress: zstd1_compress,
        decompress: zstd_decompress,
    },
    Candidate {
        name: "zstd level 3",
        pure_rust: false,
        compress: zstd3_compress,
        decompress: zstd_decompress,
    },
];

struct Measure {
    ratio: f64,
    compress_mbs: f64,
    decompress_mbs: f64,
    projected: f64,
}

fn mbs(bytes: usize, time: Duration) -> f64 {
    bytes as f64 / 1e6 / time.as_secs_f64()
}

fn measure(candidate: &Candidate, history: &History) -> Measure {
    // Best of 3 passes over all blocks, for each direction.
    let mut compressed: Vec<Vec<u8>> = Vec::new();
    let (mut best_c, mut best_d) = (Duration::MAX, Duration::MAX);
    for _ in 0..3 {
        let start = Instant::now();
        let out: Vec<Vec<u8>> = history.blocks.iter().map(|b| (candidate.compress)(b)).collect();
        best_c = best_c.min(start.elapsed());
        compressed = out;
    }
    for _ in 0..3 {
        let start = Instant::now();
        for (c, b) in compressed.iter().zip(&history.blocks) {
            let out = (candidate.decompress)(c, b.len());
            std::hint::black_box(&out);
        }
        best_d = best_d.min(start.elapsed());
    }
    for (c, b) in compressed.iter().zip(&history.blocks) {
        assert_eq!(&(candidate.decompress)(c, b.len()), b, "{} round trip", candidate.name);
    }
    let raw: usize = history.blocks.iter().map(Vec::len).sum();
    let packed: usize = compressed.iter().map(Vec::len).sum();
    let index = history.rows * INDEX_BYTES_PER_ROW;
    Measure {
        ratio: packed as f64 / raw as f64,
        compress_mbs: mbs(raw, best_c),
        decompress_mbs: mbs(raw, best_d),
        projected: (packed + index) as f64 / (raw + index) as f64,
    }
}

#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn compressor_choice() {
    let corpora: Vec<(&str, Vec<u8>)> = vec![
        ("build log", corpora::build_log(1, ROWS)),
        ("service log", corpora::service_log(2, ROWS)),
        ("shell session", corpora::shell_session(3, ROWS)),
        ("recorded cargo build", corpora::recorded("cargo-build")),
        ("recorded git graph", corpora::recorded("git-graph")),
    ];
    for (name, bytes) in &corpora {
        let history = fill(bytes);
        let raw: usize = history.blocks.iter().map(Vec::len).sum();
        println!(
            "\n### {name}: {} input bytes, {} history rows, {} blocks, {raw} block bytes ({:.1} \
             bytes per row)\n",
            bytes.len(),
            history.rows,
            history.blocks.len(),
            raw as f64 / history.rows.max(1) as f64,
        );
        println!(
            "| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off \
             |"
        );
        println!("|---|---|---|---|---|---|");
        for candidate in CANDIDATES {
            let m = measure(candidate, &history);
            println!(
                "| {} | {} | {:.3} | {:.0} | {:.0} | {:.1}% |",
                candidate.name,
                if candidate.pure_rust { "yes" } else { "no" },
                m.ratio,
                m.compress_mbs,
                m.decompress_mbs,
                m.projected * 100.0,
            );
        }
    }
}
