// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Shared harness of the benchmarks (`benches/ingest.rs`, `benches/reads.rs`) and of
//! `examples/memory.rs`: pre-generated workloads, a driver that runs feathercritty and unmodified
//! `alacritty_terminal` 0.26.0 through the same code, and timing and table helpers.
//!
//! # How both crates are driven
//!
//! [`driver!`] expands once per crate into a module with a [`Sim`](fc::Sim) (a `Term` plus its
//! `Processor`) and the handful of operations the benchmarks need. Both crates expose the same API
//! for all of it, so the expansion is literally the same code (`fc::Sim` is feathercritty,
//! `ac::Sim` is 0.26.0). Both parse with the same `vte` crate.
//!
//! # Design gates
//!
//! Thresholds live next to the benchmark that checks them (`ingest.rs`: spec 6.3, `reads.rs`: spec
//! 6.4, `memory.rs`: spec 6.5). Each binary takes `--gate` and exits non-zero on a missed
//! threshold.
//!
//! # Moving the comparison to a new upstream release
//!
//! 1. Bump the dev-dependency `alacritty_terminal = "=X.Y.Z"` in `Cargo.toml`.
//! 2. [`driver!`] uses `Term::new`, `Config::scrolling_history`, `term::test::TermSize`,
//!    `Term::resize`, `Term::grid`, `Processor::advance`, `Selection::new/update`,
//!    `Term::selection` (public field), `Term::selection_to_string`, `RegexSearch::new` and
//!    `Term::search_next`. If upstream changes one of these, change the macro (one place).
//! 3. Re-check the workloads against any new escape-sequence behavior (the scroll-region and
//!    alternate-screen workloads rely on DECSTBM, `CSI ? 1049 h` and `CSI 2 K` with a background).
//! 4. Re-run all three binaries and refresh `benches/BASELINE.md`.

// Each benchmark binary uses a different part of this module.
#![allow(dead_code)]

use std::time::Instant;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Default history size of the benchmarks (the fbterm default).
pub const HISTORY: usize = 10_000;

/// Terminal sizes of the benchmarks (columns, lines).
pub const SIZES: [(usize, usize); 2] = [(126, 37), (200, 60)];

/// Size of every ingest buffer (spec 6.3: at least 50 MB).
pub const BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// Kinds of generated input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    /// 0-20 characters per line, one or two color runs, a newline each.
    ShortLines,
    /// Plain lines exactly as wide as the terminal.
    FullWidthPlain,
    /// Background-colored erase of whole lines, so every row is one uniform run.
    UniformRows,
    /// A different color on every cell, full width, scrolling.
    DenseCells,
    /// Scrolling inside a region with fixed top and bottom lines (primary screen).
    RegionScroll,
    /// Full-screen redraws on the alternate screen.
    AltScreenRedraw,
    /// Single typed characters and a colored prompt line per Enter.
    TypingEcho,
    /// Log lines of about 95 characters with about 6 color runs.
    LogLines95,
    /// Lines of double-width (CJK) characters, full width.
    WideChars,
    /// Plain lines whose cells carry an underline color, which allocates a `CellExtra` each.
    UnderlineColor,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Workload::ShortLines => "short lines",
            Workload::FullWidthPlain => "full-width plain",
            Workload::UniformRows => "uniform rows",
            Workload::DenseCells => "dense cells",
            Workload::RegionScroll => "region scroll",
            Workload::AltScreenRedraw => "alt-screen redraw",
            Workload::TypingEcho => "typing echo",
            Workload::LogLines95 => "log lines 95",
            Workload::WideChars => "wide chars",
            Workload::UnderlineColor => "underline color",
        }
    }

    /// How many bytes one `Processor::advance` call gets: a typed character at a time for typing
    /// echo, a PTY-read-sized slice otherwise.
    pub fn chunk(self) -> usize {
        match self {
            Workload::TypingEcho => 1,
            _ => 64 * 1024,
        }
    }
}

/// How much to generate.
#[derive(Clone, Copy)]
enum Amount {
    Bytes(usize),
    Lines(usize),
}

/// A buffer of `kind` of at least `bytes` bytes for a 126x37 terminal.
pub fn workload(kind: Workload, bytes: usize, seed: u64) -> Vec<u8> {
    workload_for(kind, bytes, seed, 126, 37)
}

/// A buffer of `kind` of at least `bytes` bytes for a `columns` x `lines` terminal.
pub fn workload_for(
    kind: Workload,
    bytes: usize,
    seed: u64,
    columns: usize,
    lines: usize,
) -> Vec<u8> {
    generate(kind, Amount::Bytes(bytes), seed, columns, lines)
}

/// A buffer of `kind` with exactly `count` newline-terminated lines (the alternate-screen and
/// typing workloads count their own units: one redraw or one Enter per "line").
pub fn workload_lines(
    kind: Workload,
    count: usize,
    seed: u64,
    columns: usize,
    lines: usize,
) -> Vec<u8> {
    generate(kind, Amount::Lines(count), seed, columns, lines)
}

const TEXT: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,-_/:=[]";

fn text(rng: &mut ChaCha8Rng, out: &mut Vec<u8>, len: usize) {
    for _ in 0..len {
        out.push(TEXT[rng.gen_range(0..TEXT.len())]);
    }
}

fn sgr_fg(out: &mut Vec<u8>, color: u32) {
    out.extend_from_slice(format!("\x1b[38;5;{color}m").as_bytes());
}

const RESET: &[u8] = b"\x1b[0m";
const NEWLINE: &[u8] = b"\r\n";

fn generate(kind: Workload, amount: Amount, seed: u64, columns: usize, lines: usize) -> Vec<u8> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut count = 0usize;
    let done = |out: &Vec<u8>, count: usize| match amount {
        Amount::Bytes(n) => out.len() >= n,
        Amount::Lines(n) => count >= n,
    };

    match kind {
        Workload::RegionScroll => {
            // Fixed first and last line, the middle scrolls; park the cursor on the region's last
            // row so that every newline scrolls.
            out.extend_from_slice(format!("\x1b[2;{}r\x1b[{};1H", lines - 1, lines - 1).as_bytes());
        },
        Workload::AltScreenRedraw => out.extend_from_slice(b"\x1b[?1049h"),
        _ => (),
    }

    while !done(&out, count) {
        match kind {
            Workload::ShortLines => {
                let len = rng.gen_range(0..=20);
                if len > 0 && rng.gen_range(0..3) > 0 {
                    sgr_fg(&mut out, rng.gen_range(1..8));
                }
                text(&mut rng, &mut out, len / 2);
                if rng.gen_range(0..3) == 0 {
                    sgr_fg(&mut out, rng.gen_range(1..8));
                }
                text(&mut rng, &mut out, len - len / 2);
                out.extend_from_slice(RESET);
                out.extend_from_slice(NEWLINE);
            },
            Workload::FullWidthPlain => {
                text(&mut rng, &mut out, columns);
                out.extend_from_slice(NEWLINE);
            },
            Workload::UniformRows => {
                out.extend_from_slice(format!("\x1b[4{}m\x1b[2K", rng.gen_range(1..8)).as_bytes());
                out.extend_from_slice(NEWLINE);
            },
            Workload::DenseCells => {
                // Consecutive cells never share a color.
                let mut color = rng.gen_range(16..232u32);
                for _ in 0..columns {
                    color = 16 + (color - 16 + 1 + rng.gen_range(0..5)) % 216;
                    sgr_fg(&mut out, color);
                    out.push(TEXT[rng.gen_range(0..TEXT.len())]);
                }
                out.extend_from_slice(RESET);
                out.extend_from_slice(NEWLINE);
            },
            Workload::RegionScroll => {
                let len = rng.gen_range(10..columns.min(100));
                text(&mut rng, &mut out, len);
                out.extend_from_slice(NEWLINE);
            },
            Workload::AltScreenRedraw => {
                out.extend_from_slice(b"\x1b[H");
                for row in 0..lines {
                    if row % 4 == 0 {
                        sgr_fg(&mut out, rng.gen_range(1..8));
                    }
                    text(&mut rng, &mut out, columns);
                    if row + 1 < lines {
                        out.extend_from_slice(NEWLINE);
                    }
                }
                out.extend_from_slice(RESET);
            },
            Workload::TypingEcho => {
                let typed = rng.gen_range(5..60);
                text(&mut rng, &mut out, typed);
                out.extend_from_slice(NEWLINE);
                out.extend_from_slice(b"\x1b[32muser@host\x1b[0m:\x1b[34m~/src\x1b[0m$ ");
            },
            Workload::LogLines95 => {
                // About 95 characters in 6 runs: a timestamp, level, module, three message parts.
                let parts: [(u32, usize); 6] =
                    [(244, 19), (rng.gen_range(1..7), 5), (36, 12), (7, 25), (3, 18), (7, 16)];
                for (color, len) in parts {
                    sgr_fg(&mut out, color);
                    text(&mut rng, &mut out, len);
                }
                out.extend_from_slice(RESET);
                out.extend_from_slice(NEWLINE);
            },
            Workload::WideChars => {
                for _ in 0..columns / 2 {
                    let c = char::from_u32(0x4e00 + rng.gen_range(0..0x500)).unwrap();
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                out.extend_from_slice(NEWLINE);
            },
            Workload::UnderlineColor => {
                for i in 0..columns {
                    if i % 8 == 0 {
                        out.extend_from_slice(
                            format!("\x1b[4m\x1b[58;5;{}m", rng.gen_range(1..200)).as_bytes(),
                        );
                    }
                    out.push(TEXT[rng.gen_range(0..TEXT.len())]);
                }
                out.extend_from_slice(RESET);
                out.extend_from_slice(NEWLINE);
            },
        }
        count += 1;
    }
    out
}

/// Expands into a module `$m` that drives crate `$k` (`feathercritty` or `alacritty_terminal`).
macro_rules! driver {
    ($m:ident, $k:ident) => {
        pub mod $m {
            use $k::event::VoidListener;
            use $k::grid::Dimensions;
            use $k::index::{Column, Direction, Line, Point, Side};
            use $k::selection::{Selection, SelectionType};
            use $k::term::search::RegexSearch;
            use $k::term::test::TermSize;
            use $k::term::{Config, Term};
            use $k::vte::ansi::Processor;

            /// One terminal and its parser.
            pub struct Sim {
                pub term: Term<VoidListener>,
                parser: Processor,
            }

            impl Sim {
                pub fn new(columns: usize, lines: usize, history: usize) -> Self {
                    let config = Config { scrolling_history: history, ..Config::default() };
                    Self {
                        term: Term::new(config, &TermSize::new(columns, lines), VoidListener),
                        parser: Processor::new(),
                    }
                }

                pub fn feed(&mut self, bytes: &[u8], chunk: usize) {
                    for piece in bytes.chunks(chunk) {
                        self.parser.advance(&mut self.term, piece);
                    }
                }

                pub fn resize(&mut self, columns: usize, lines: usize) {
                    self.term.resize(TermSize::new(columns, lines));
                }

                pub fn history_size(&self) -> usize {
                    self.term.history_size()
                }

                /// A regex search over the whole buffer that finds nothing.
                pub fn search_no_match(&self) -> bool {
                    let mut regex = RegexSearch::new("zzqq_no_such_text").unwrap();
                    let origin = Point::new(self.term.topmost_line(), Column(0));
                    self.term
                        .search_next(&mut regex, origin, Direction::Right, Side::Left, None)
                        .is_none()
                }

                /// `selection_to_string` over the last `rows` history rows; returns its length.
                pub fn select_history_rows(&mut self, rows: usize) -> usize {
                    let columns = self.term.columns();
                    let start = Point::new(Line(-(rows as i32)), Column(0));
                    let end = Point::new(Line(-1), Column(columns - 1));
                    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
                    selection.update(end, Side::Right);
                    self.term.selection = Some(selection);
                    let text = self.term.selection_to_string().unwrap_or_default();
                    self.term.selection = None;
                    text.len()
                }
            }
        }
    };
}

driver!(fc, feathercritty);
driver!(ac, alacritty_terminal);

/// Input that fills the history: 12,000 log lines scrolled through a terminal leave its
/// 10,000-row history full, so every following scrolled row evicts one (spec 6.3: "history full
/// before timing").
pub fn prefill_buffer(columns: usize) -> Vec<u8> {
    workload_lines(Workload::LogLines95, 12_000, 1, columns, 37)
}

/// Summary of repeated timings in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

impl Stats {
    pub fn from_samples(mut samples: Vec<f64>) -> Stats {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = samples.len();
        let median =
            if n % 2 == 1 { samples[n / 2] } else { (samples[n / 2 - 1] + samples[n / 2]) / 2.0 };
        Stats { median, min: samples[0], max: samples[n - 1] }
    }

    /// (max - min) / median, in percent.
    pub fn spread_pct(&self) -> f64 {
        if self.median == 0.0 { 0.0 } else { (self.max - self.min) / self.median * 100.0 }
    }
}

/// Median of `runs` timings of `f` in milliseconds (after one untimed warm-up run).
pub fn median_ms(runs: usize, mut f: impl FnMut()) -> Stats {
    median_ms_setup(runs, || (), |()| f())
}

/// Like [`median_ms`], but each run gets fresh state from `setup`, which is not timed.
pub fn median_ms_setup<S>(
    runs: usize,
    mut setup: impl FnMut() -> S,
    mut f: impl FnMut(S),
) -> Stats {
    f(setup());
    let samples = (0..runs)
        .map(|_| {
            let state = setup();
            let start = Instant::now();
            f(state);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    Stats::from_samples(samples)
}

/// Median timings of two competitors, run interleaved (each `run_*` hands its state back, see
/// [`median_ms_setup`]) (the order alternates every round) so
/// that thermal drift and background load hit both alike. Returns (first, second).
pub fn compare_ms<A, B>(
    runs: usize,
    mut setup_a: impl FnMut() -> A,
    mut run_a: impl FnMut(A) -> A,
    mut setup_b: impl FnMut() -> B,
    mut run_b: impl FnMut(B) -> B,
) -> (Stats, Stats) {
    drop(run_a(setup_a()));
    drop(run_b(setup_b()));
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    for round in 0..runs {
        for slot in 0..2 {
            if (round + slot) % 2 == 0 {
                let state = setup_a();
                let start = Instant::now();
                let state = run_a(state);
                sa.push(start.elapsed().as_secs_f64() * 1000.0);
                drop(state);
            } else {
                let state = setup_b();
                let start = Instant::now();
                let state = run_b(state);
                sb.push(start.elapsed().as_secs_f64() * 1000.0);
                drop(state);
            }
        }
    }
    (Stats::from_samples(sa), Stats::from_samples(sb))
}

/// Number of benchmark runs per row (spec: at least 5), `BENCH_RUNS` overrides.
pub fn runs() -> usize {
    std::env::var("BENCH_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(7)
}

/// Whether the command line asks for threshold checking.
pub fn gate_requested() -> bool {
    std::env::args().any(|a| a == "--gate")
}

/// Row filter from `BENCH_ONLY` (substring of the row label), for quick runs.
pub fn selected(label: &str) -> bool {
    std::env::var("BENCH_ONLY").map_or(true, |f| label.contains(&f))
}

pub fn table_header() {
    println!(
        "{:<34} | {:>12} | {:>10} | {:>6} | {:>8} | gate",
        "workload", "0.26.0 ms", "fork ms", "ratio", "spread"
    );
    println!("{}", "-".repeat(94));
}

/// Prints one row; returns whether the gate (if any) holds. `max_ratio` is the allowed
/// fork/0.26.0 median ratio; `floor_ms` makes the row pass whenever the fork's median is below it
/// (spec 6.4 resize: "or 10 ms, whichever is larger").
pub fn table_row(
    label: &str,
    ac: Stats,
    fc: Stats,
    max_ratio: Option<f64>,
    floor_ms: Option<f64>,
) -> bool {
    let ratio = fc.median / ac.median;
    let spread = ac.spread_pct().max(fc.spread_pct());
    let (ok, note) = match max_ratio {
        None => (true, "-".to_owned()),
        Some(max) => {
            let ok = ratio <= max || floor_ms.is_some_and(|f| fc.median <= f);
            (ok, format!("{} (<= {max:.2}x)", if ok { "pass" } else { "FAIL" }))
        },
    };
    println!(
        "{:<34} | {:>12.3} | {:>10.3} | {:>6.3} | {:>7.1}% | {}",
        label, ac.median, fc.median, ratio, spread, note
    );
    ok
}
