// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Step generators: the weighted random generator, deterministic realistic streams, and the ref
//! test recordings.

use std::io::Write as _;
use std::path::Path;

use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::{Case, Pt, SelTy, Start, Step};

/// History limits used by the main pass stay at or below this.
pub const MAX_HISTORY: usize = 10_000;

/// History limit of the separate large pass.
pub const LARGE_HISTORY: usize = 100_000;

/// Kitty keyboard pushes per run stay below this (fork change 1 makes the 4097th push an
/// intended difference: 0.26.0 evicts from the wrong stack and panics).
pub const MAX_KITTY_PUSHES: usize = 4_000;

/// Generator configuration.
#[derive(Debug, Clone, Copy)]
pub struct GenCfg {
    pub start: Start,
    /// Upper bound for history limits chosen by `SetHistory` steps.
    pub max_history: usize,
    /// Add occasional floods of short lines (the large-history pass), so history grows far
    /// beyond what 400 ordinary steps produce.
    pub floods: bool,
    /// Emit wide characters. 0.26.0 cannot hold a wide character in a one-column grid (writing
    /// one indexes past the row and panics; resizing a grid that holds one to one column never
    /// returns), so runs with wide characters keep at least two columns and runs without them
    /// may go down to one.
    pub wide: bool,
}

fn rng(seed: u64, stream: u64) -> ChaCha8Rng {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    rng.set_stream(stream);
    rng
}

fn random_lines<R: Rng>(rng: &mut R) -> usize {
    match rng.gen_range(0..100) {
        0..=4 => 1,
        5..=9 => 2,
        10..=79 => rng.gen_range(3..=30),
        _ => rng.gen_range(31..=60),
    }
}

fn random_columns<R: Rng>(rng: &mut R, min: usize) -> usize {
    let columns = match rng.gen_range(0..100) {
        0..=7 => 1,
        8..=15 => 2,
        16..=29 => rng.gen_range(3..=12),
        30..=89 => rng.gen_range(13..=100),
        _ => rng.gen_range(101..=200),
    };
    columns.max(min)
}

/// Fewest columns a run may use.
fn min_columns(wide: bool) -> usize {
    if wide { 2 } else { 1 }
}

fn random_limit<R: Rng>(rng: &mut R, max: usize) -> usize {
    let limit = match rng.gen_range(0..100) {
        0..=14 => 0,
        15..=24 => 1,
        25..=54 => rng.gen_range(2..=64),
        55..=84 => rng.gen_range(65..=2_000),
        _ => MAX_HISTORY,
    };
    limit.min(max)
}

impl GenCfg {
    /// The main pass: history limits from 0 to 10,000.
    pub fn random(seed: u64) -> Self {
        let mut rng = rng(seed, 0);
        let wide = rng.gen_bool(0.75);
        let start = Start {
            lines: random_lines(&mut rng),
            columns: random_columns(&mut rng, min_columns(wide)),
            history: random_limit(&mut rng, MAX_HISTORY),
            kitty: rng.gen_bool(0.5),
            query_seed: rng.r#gen(),
        };
        Self { start, max_history: MAX_HISTORY, floods: false, wide }
    }

    /// The separate pass at a 100,000-line history limit.
    pub fn large(seed: u64) -> Self {
        let mut rng = rng(seed, 0);
        let start = Start {
            lines: rng.gen_range(2..=30),
            columns: rng.gen_range(2..=24),
            history: LARGE_HISTORY,
            kitty: false,
            query_seed: rng.r#gen(),
        };
        Self { start, max_history: LARGE_HISTORY, floods: true, wide: true }
    }
}

/// A random case for the main pass.
pub fn random_case(seed: u64, n: usize) -> Case {
    let cfg = GenCfg::random(seed);
    Case { start: cfg.start, steps: steps(seed, n, &cfg) }
}

/// A random case for the 100,000-line pass.
pub fn large_case(seed: u64, n: usize) -> Case {
    let cfg = GenCfg::large(seed);
    Case { start: cfg.start, steps: steps(seed, n, &cfg) }
}

const WORDS: &[&str] = &[
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "src/main.rs",
    "cargo",
    "build",
    "--release",
    "0x1f",
    "42",
    "foo_bar",
    "a.b.c",
    "x=1;",
    "fn(",
    ")",
    "[ok]",
    "{key: v}",
    "<tag>",
    "\"quoted\"",
    "'single'",
    "a,b",
    "|pipe|",
    "path/to/file",
    "C:\\dir",
];
const LATIN: &[&str] = &["à", "é", "î", "õ", "ü", "ß", "ñ", "ç", "Æ", "Ø", "ł", "Ω", "Ж"];
const CJK: &[&str] =
    &["中", "文", "字", "符", "日", "本", "語", "한", "국", "어", "テ", "ス", "ト", "｡"];
const EMOJI: &[&str] = &["😀", "🎉", "🚀", "👍🏽", "❤️", "👨\u{200d}👩\u{200d}👧", "🇮🇹", "✅", "⚡"];
const COMBINING: &[char] = &['\u{301}', '\u{300}', '\u{308}', '\u{323}', '\u{20dd}', '\u{200b}'];
const PUNCT: &[&str] =
    &[" ", " ", " ", "(", ")", "[", "]", "{", "}", "<", ">", ",", "│", "`", "\t"];

fn push_text<R: Rng>(rng: &mut R, out: &mut Vec<u8>, wide: bool) {
    let n = if rng.gen_bool(0.1) { rng.gen_range(40..=300) } else { rng.gen_range(1..=40) };
    for _ in 0..n {
        match rng.gen_range(0..100) {
            0..=39 => {
                let c = rng.gen_range(b' '..=b'~');
                out.push(c);
            },
            40..=54 => out.extend_from_slice(WORDS.choose(rng).unwrap().as_bytes()),
            55..=62 => out.extend_from_slice(LATIN.choose(rng).unwrap().as_bytes()),
            63..=77 if wide => out.extend_from_slice(CJK.choose(rng).unwrap().as_bytes()),
            78..=84 if wide => out.extend_from_slice(EMOJI.choose(rng).unwrap().as_bytes()),
            63..=84 => out.extend_from_slice(LATIN.choose(rng).unwrap().as_bytes()),
            85..=91 => {
                let base = if !wide || rng.gen_bool(0.7) { "e" } else { CJK.choose(rng).unwrap() };
                out.extend_from_slice(base.as_bytes());
                let mark = *COMBINING.choose(rng).unwrap();
                let _ = write!(out, "{mark}");
            },
            _ => out.extend_from_slice(PUNCT.choose(rng).unwrap().as_bytes()),
        }
    }
}

fn push_cup<R: Rng>(rng: &mut R, out: &mut Vec<u8>, lines: usize, columns: usize) {
    let line = rng.gen_range(0..=lines + 2);
    let column = rng.gen_range(0..=columns + 2);
    let _ = write!(out, "\x1b[{line};{column}H");
}

fn color_params<R: Rng>(rng: &mut R, base: u8) -> String {
    match rng.gen_range(0..5) {
        0 => format!("{}", base + rng.gen_range(0..8)),
        1 => format!("{}", base + 60 + rng.gen_range(0..8)),
        2 => format!("{};5;{}", base + 8, rng.r#gen::<u8>()),
        3 => format!(
            "{};2;{};{};{}",
            base + 8,
            rng.r#gen::<u8>(),
            rng.r#gen::<u8>(),
            rng.r#gen::<u8>()
        ),
        _ => format!("{}", base + 9),
    }
}

fn sgr<R: Rng>(rng: &mut R) -> Vec<u8> {
    let count = rng.gen_range(1..=4);
    let mut params = Vec::new();
    for _ in 0..count {
        let param = match rng.gen_range(0..14) {
            0 => "0".to_owned(),
            1 => ["1", "2", "3", "5", "7", "8", "9", "53"].choose(rng).unwrap().to_string(),
            2 => ["21", "22", "23", "24", "25", "27", "28", "29", "55"]
                .choose(rng)
                .unwrap()
                .to_string(),
            3 => format!("4:{}", rng.gen_range(0..=5)),
            4 => "4".to_owned(),
            5..=7 => color_params(rng, 30),
            8..=10 => color_params(rng, 40),
            11 => format!("58;5;{}", rng.r#gen::<u8>()),
            12 => {
                format!("58:2::{}:{}:{}", rng.r#gen::<u8>(), rng.r#gen::<u8>(), rng.r#gen::<u8>())
            },
            _ => "59".to_owned(),
        };
        params.push(param);
    }
    format!("\x1b[{}m", params.join(";")).into_bytes()
}

fn osc8<R: Rng>(rng: &mut R, out: &mut Vec<u8>, wide: bool) {
    let st = if rng.gen_bool(0.5) { "\x1b\\" } else { "\x07" };
    let uri = ["https://example.com/", "file:///tmp/a b/", "mailto:x@y.z?", "http://h/中"]
        .choose(rng)
        .unwrap();
    let id =
        if rng.gen_bool(0.5) { format!("id=link{}", rng.gen_range(0..4)) } else { String::new() };
    let _ = write!(out, "\x1b]8;{id};{uri}{}{st}", rng.gen_range(0..4));
    push_text(rng, out, wide);
    if rng.gen_bool(0.7) {
        let _ = write!(out, "\x1b]8;;{st}");
    }
}

/// State the generator tracks to aim its sequences.
struct GenState {
    lines: usize,
    columns: usize,
    limit: usize,
    kitty_pushes: usize,
}

/// The weighted step kinds. The weights follow the compact-history plan; `Modes` and
/// `Selection` are additions.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    Newline,
    Sgr,
    Osc8,
    Erase,
    InsertDelete,
    Region,
    ScrollUpDown,
    TabAlign,
    AltScreen,
    Ris,
    ClearHistory,
    ViewScroll,
    Resize,
    History,
    /// Cursor movement, saved cursor, modes (origin, wrap, insert, LNM), charsets, REP and kitty
    /// keyboard push/pop. Not in the plan's table, which has no cursor positioning.
    Modes,
    /// Set or clear the persistent selection. Not in the plan's table.
    Selection,
    /// Floods of short lines (large pass only).
    Flood,
}

const WEIGHTS: &[(Kind, u32)] = &[
    (Kind::Text, 30),
    (Kind::Newline, 15),
    (Kind::Sgr, 10),
    (Kind::Osc8, 5),
    (Kind::Erase, 6),
    (Kind::InsertDelete, 5),
    (Kind::Region, 5),
    (Kind::ScrollUpDown, 3),
    (Kind::TabAlign, 2),
    (Kind::AltScreen, 2),
    (Kind::Ris, 1),
    (Kind::ClearHistory, 1),
    (Kind::ViewScroll, 5),
    (Kind::Resize, 6),
    (Kind::History, 3),
    (Kind::Modes, 4),
    (Kind::Selection, 2),
];

fn pick_kind<R: Rng>(rng: &mut R, floods: bool) -> Kind {
    if floods && rng.gen_bool(0.05) {
        return Kind::Flood;
    }
    let total: u32 = WEIGHTS.iter().map(|(_, w)| w).sum();
    let mut pick = rng.gen_range(0..total);
    for &(kind, weight) in WEIGHTS {
        if pick < weight {
            return kind;
        }
        pick -= weight;
    }
    unreachable!()
}

fn newline_bytes<R: Rng>(rng: &mut R, out: &mut Vec<u8>) {
    for _ in 0..rng.gen_range(1..=5) {
        let s: &[u8] = match rng.gen_range(0..12) {
            0..=4 => b"\n",
            5..=7 => b"\r\n",
            8 => b"\r",
            9 => b"\x1bD",
            10 => b"\x1bE",
            _ => [b"\x0b".as_slice(), b"\x0c", b"\x1bM"].choose(rng).unwrap(),
        };
        out.extend_from_slice(s);
    }
}

/// `gen::steps`: `n` steps from the weighted generator.
pub fn steps(seed: u64, n: usize, cfg: &GenCfg) -> Vec<Step> {
    let mut rng = rng(seed, 1);
    let mut st = GenState {
        lines: cfg.start.lines,
        columns: cfg.start.columns,
        limit: cfg.start.history,
        kitty_pushes: 0,
    };
    let mut steps = Vec::with_capacity(n);

    while steps.len() < n {
        let mut out = Vec::new();
        let (lines, columns) = (st.lines, st.columns);
        match pick_kind(&mut rng, cfg.floods) {
            Kind::Text => {
                if rng.gen_bool(0.2) {
                    push_cup(&mut rng, &mut out, lines, columns);
                }
                push_text(&mut rng, &mut out, cfg.wide);
            },
            Kind::Newline => newline_bytes(&mut rng, &mut out),
            Kind::Sgr => out = sgr(&mut rng),
            Kind::Osc8 => osc8(&mut rng, &mut out, cfg.wide),
            Kind::Erase => {
                let _ = write!(out, "\x1b[{}m", color_params(&mut rng, 40));
                if rng.gen_bool(0.7) {
                    push_cup(&mut rng, &mut out, lines, columns);
                }
                let mode = ["", "0", "1", "2"].choose(&mut rng).unwrap();
                let _ = match rng.gen_range(0..6) {
                    0 | 1 => write!(out, "\x1b[{mode}K"),
                    2 | 3 => write!(out, "\x1b[{mode}J"),
                    4 => write!(out, "\x1b[{}X", rng.gen_range(0..=columns + 2)),
                    _ => write!(out, "\x1b[?{mode}{}", ["K", "J"].choose(&mut rng).unwrap()),
                };
            },
            Kind::InsertDelete => {
                if rng.gen_bool(0.7) {
                    push_cup(&mut rng, &mut out, lines, columns);
                }
                let (count, op) = match rng.gen_range(0..4) {
                    0 => (rng.gen_range(0..=lines + 3), 'L'),
                    1 => (rng.gen_range(0..=lines + 3), 'M'),
                    2 => (rng.gen_range(0..=columns + 3), '@'),
                    _ => (rng.gen_range(0..=columns + 3), 'P'),
                };
                let _ = write!(out, "\x1b[{count}{op}");
            },
            Kind::Region => {
                if rng.gen_bool(0.15) {
                    out.extend_from_slice(b"\x1b[r");
                } else {
                    let top = rng.gen_range(1..=lines);
                    let bottom = rng.gen_range(top..=lines);
                    let _ = write!(out, "\x1b[{top};{bottom}r");
                    let _ = write!(out, "\x1b[{};1H", rng.gen_range(top..=bottom));
                    for _ in 0..rng.gen_range(1..=2 * lines + 2) {
                        match rng.gen_range(0..5) {
                            0 => out.extend_from_slice(b"\x1bM"),
                            1 => out.extend_from_slice(b"\x1bD"),
                            2 => {
                                push_text(&mut rng, &mut out, cfg.wide);
                                out.extend_from_slice(b"\r\n");
                            },
                            _ => out.push(b'\n'),
                        }
                    }
                    if rng.gen_bool(0.5) {
                        out.extend_from_slice(b"\x1b[r");
                    }
                }
            },
            Kind::ScrollUpDown => {
                if rng.gen_bool(0.3) {
                    let top = rng.gen_range(1..=lines);
                    let bottom = rng.gen_range(top..=lines);
                    let _ = write!(out, "\x1b[{top};{bottom}r");
                }
                let count = rng.gen_range(0..=2 * lines + 1);
                let op = if rng.gen_bool(0.5) { 'S' } else { 'T' };
                let _ = write!(out, "\x1b[{count}{op}");
            },
            Kind::TabAlign => {
                let s = match rng.gen_range(0..7) {
                    0 => "\t\t".to_owned(),
                    1 => format!("\x1b[{}I", rng.gen_range(0..4)),
                    2 => format!("\x1b[{}Z", rng.gen_range(0..4)),
                    3 => "\x1bH".to_owned(),
                    4 => ["\x1b[g", "\x1b[0g", "\x1b[3g"].choose(&mut rng).unwrap().to_string(),
                    _ => "\x1b#8".to_owned(),
                };
                out.extend_from_slice(s.as_bytes());
            },
            Kind::AltScreen => {
                let mode = ["1049", "47", "1047"].choose(&mut rng).unwrap();
                let set = if rng.gen_bool(0.5) { 'h' } else { 'l' };
                let _ = write!(out, "\x1b[?{mode}{set}");
            },
            Kind::Ris => out.extend_from_slice(b"\x1bc"),
            Kind::ClearHistory => out.extend_from_slice(b"\x1b[3J"),
            Kind::ViewScroll => {
                let span = (lines * 3) as i32;
                let delta = if rng.gen_bool(0.1) {
                    if rng.gen_bool(0.5) { 1_000_000 } else { -1_000_000 }
                } else {
                    rng.gen_range(-span..=span)
                };
                steps.push(Step::ScrollView(delta));
                // Output keeps arriving while the view is scrolled.
                if rng.gen_bool(0.5) {
                    push_text(&mut rng, &mut out, cfg.wide);
                }
                newline_bytes(&mut rng, &mut out);
            },
            Kind::Resize => {
                // Keep some resizes to one dimension.
                let (l, c) = match rng.gen_range(0..4) {
                    0 => (lines, random_columns(&mut rng, min_columns(cfg.wide))),
                    1 => (random_lines(&mut rng), columns),
                    _ => (random_lines(&mut rng), random_columns(&mut rng, min_columns(cfg.wide))),
                };
                st.lines = l;
                st.columns = c;
                steps.push(Step::Resize { lines: l, columns: c });
                continue;
            },
            Kind::History => {
                let limit = match rng.gen_range(0..6) {
                    0 => 0,
                    1 => 1,
                    2 => rng.gen_range(2..=64),
                    3 | 4 => st.limit + rng.gen_range(1..=2_000),
                    _ => rng.gen_range(0..=st.limit),
                };
                st.limit = limit.min(cfg.max_history);
                steps.push(Step::SetHistory(st.limit));
                continue;
            },
            Kind::Modes => {
                let s = match rng.gen_range(0..14) {
                    0 => {
                        let mut v = Vec::new();
                        push_cup(&mut rng, &mut v, lines, columns);
                        String::from_utf8(v).unwrap()
                    },
                    1 => {
                        let op =
                            ["A", "B", "C", "D", "E", "F", "G", "d", "`"].choose(&mut rng).unwrap();
                        format!("\x1b[{}{op}", rng.gen_range(0..=lines.max(columns) + 2))
                    },
                    2 => {
                        ["\x1b7", "\x1b8", "\x1b[s", "\x1b[u"].choose(&mut rng).unwrap().to_string()
                    },
                    3 => format!("\x1b[?6{}", ["h", "l"].choose(&mut rng).unwrap()),
                    4 => format!("\x1b[?7{}", ["h", "l"].choose(&mut rng).unwrap()),
                    5 => format!("\x1b[4{}", ["h", "l"].choose(&mut rng).unwrap()),
                    6 => format!("\x1b[20{}", ["h", "l"].choose(&mut rng).unwrap()),
                    7 => {
                        let mode = ["1", "25", "1000", "1002", "1006", "2004", "1004", "1007"]
                            .choose(&mut rng)
                            .unwrap();
                        format!("\x1b[?{mode}{}", ["h", "l"].choose(&mut rng).unwrap())
                    },
                    8 => {
                        let mut s = "\x1b(0".to_owned();
                        for _ in 0..rng.gen_range(1..10) {
                            s.push(rng.gen_range(b'_'..=b'~') as char);
                        }
                        s + "\x1b(B"
                    },
                    9 => format!("x\x1b[{}b", rng.gen_range(0..=columns * 2)),
                    10 if st.kitty_pushes < MAX_KITTY_PUSHES => {
                        st.kitty_pushes += 1;
                        format!("\x1b[>{}u", rng.gen_range(0..32))
                    },
                    11 => format!("\x1b[<{}u", rng.gen_range(0..3)),
                    12 => format!("\x1b[={};{}u", rng.gen_range(0..32), rng.gen_range(1..=3)),
                    _ => ["\x1b[22t", "\x1b[23t", "\x1b[6n", "\x1b[5 q", "\x1b[c"]
                        .choose(&mut rng)
                        .unwrap()
                        .to_string(),
                };
                out.extend_from_slice(s.as_bytes());
            },
            Kind::Selection => {
                let ty = if rng.gen_bool(0.2) {
                    None
                } else {
                    Some(*SelTy::ALL.choose(&mut rng).unwrap())
                };
                let point = |rng: &mut ChaCha8Rng| Pt {
                    line: rng.gen_range(-(st.limit.min(500) as i32)..lines as i32),
                    col: rng.gen_range(0..columns),
                    right: rng.r#gen(),
                };
                let (start, end) = (point(&mut rng), point(&mut rng));
                steps.push(Step::Select { ty, start, end });
                continue;
            },
            Kind::Flood => {
                for i in 0..rng.gen_range(500..=6_000) {
                    let _ = write!(out, "{i} x\r\n");
                }
            },
        }
        steps.push(Step::Bytes(out));
    }

    steps.truncate(n);
    steps
}

/// Split `stream` into byte steps of random sizes, sometimes interleaving view scrolls, resizes,
/// history changes and selections.
fn chunk_stream<R: Rng>(rng: &mut R, stream: &[u8], start: &Start, interleave: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut rest = stream;
    let (mut lines, mut columns) = (start.lines, start.columns);
    while !rest.is_empty() {
        let size = rng.gen_range(1..=2048).min(rest.len());
        steps.push(Step::Bytes(rest[..size].to_vec()));
        rest = &rest[size..];

        if interleave {
            match rng.gen_range(0..40) {
                0 => {
                    if rng.gen_bool(0.5) {
                        // Streams may hold wide characters: keep two columns.
                        columns = random_columns(rng, 2);
                    } else {
                        lines = random_lines(rng);
                    }
                    steps.push(Step::Resize { lines, columns });
                },
                1 => steps.push(Step::ScrollView(rng.gen_range(-50..=50))),
                2 => steps.push(Step::SetHistory(random_limit(rng, MAX_HISTORY))),
                3 => {
                    let ty = Some(*SelTy::ALL.choose(rng).unwrap());
                    let start =
                        Pt::new(rng.gen_range(-100..lines as i32), rng.gen_range(0..columns));
                    let end = Pt::new(rng.gen_range(-100..lines as i32), rng.gen_range(0..columns));
                    steps.push(Step::Select { ty, start, end });
                },
                _ => (),
            }
        }
    }
    steps
}

/// Realistic stream kinds.
#[derive(Debug, Clone, Copy)]
pub enum Realistic {
    /// Colored `ls -l` style listings.
    Ls,
    /// `git log --graph` style output.
    GitGraph,
    /// Compiler diagnostics with colors, carets and file links.
    Diagnostics,
    /// Mixed CJK and ASCII prose.
    Cjk,
    /// A `top`-like full-screen program redrawing the primary screen (no alternate screen).
    FullScreen,
}

impl Realistic {
    pub const ALL: [Realistic; 5] = [
        Realistic::Ls,
        Realistic::GitGraph,
        Realistic::Diagnostics,
        Realistic::Cjk,
        Realistic::FullScreen,
    ];
}

fn ls_stream<R: Rng>(rng: &mut R, out: &mut Vec<u8>) {
    let names = [
        "src",
        "target",
        "Cargo.toml",
        "README.md",
        "build.sh",
        "日本語.txt",
        "img.png",
        ".git",
        "a b",
    ];
    for _ in 0..rng.gen_range(5..40) {
        let _ = write!(out, "$ ls -la --color\r\ntotal {}\r\n", rng.gen_range(0..999));
        for _ in 0..rng.gen_range(3..60) {
            let name = names.choose(rng).unwrap();
            let (perm, color) = match rng.gen_range(0..4) {
                0 => ("drwxr-xr-x", "\x1b[1;34m"),
                1 => ("-rwxr-xr-x", "\x1b[1;32m"),
                2 => ("lrwxr-xr-x", "\x1b[1;36m"),
                _ => ("-rw-r--r--", "\x1b[0m"),
            };
            let _ = write!(
                out,
                "{perm}  {:>2} user  staff  {:>8} Oct {:>2} {:02}:{:02} {color}{name}{}\x1b[0m\r\n",
                rng.gen_range(1..30),
                rng.gen_range(0..10_000_000),
                rng.gen_range(1..32),
                rng.gen_range(0..24),
                rng.gen_range(0..60),
                rng.gen_range(0..100),
            );
        }
    }
}

fn git_graph_stream<R: Rng>(rng: &mut R, out: &mut Vec<u8>) {
    let graph = ["* ", "| * ", "|\\  ", "| |/  ", "|/  ", "* | ", "| | * "];
    let subjects =
        ["feat: add history", "fix(grid): wrap 中文", "Merge branch 'develop'", "docs: README"];
    for _ in 0..rng.gen_range(50..400) {
        let g = graph.choose(rng).unwrap();
        let _ = write!(out, "\x1b[31m{g}\x1b[m");
        if g.contains('*') {
            let _ = write!(out, "\x1b[33m{:07x}\x1b[m ", rng.gen_range(0..0xfff_ffff));
            if rng.gen_bool(0.2) {
                out.extend_from_slice(
                    b"(\x1b[1;36mHEAD -> \x1b[1;32mdevelop\x1b[m, \x1b[1;31morigin/develop\x1b[m) ",
                );
            }
            let _ = write!(
                out,
                "{} \x1b[32m({} days ago)\x1b[m \x1b[1;34m<author>\x1b[m",
                subjects.choose(rng).unwrap(),
                rng.gen_range(1..400)
            );
        }
        out.extend_from_slice(b"\r\n");
    }
}

fn diagnostics_stream<R: Rng>(rng: &mut R, out: &mut Vec<u8>) {
    for _ in 0..rng.gen_range(5..40) {
        let line = rng.gen_range(1..999);
        let col = rng.gen_range(1..80);
        let (level, color) =
            if rng.gen_bool(0.6) { ("error[E0308]", "9") } else { ("warning", "11") };
        let _ = write!(
            out,
            "\x1b[0m\x1b[1m\x1b[38;5;{color}m{level}\x1b[0m\x1b[0m\x1b[1m: mismatched types in a rather \
             long message that keeps going past the edge of a narrow terminal\x1b[0m\r\n\x1b[0m   \
             \x1b[0m\x1b[0m\x1b[1m\x1b[38;5;12m--> \x1b[0m\x1b[0m\x1b]8;;file:///src/lib.rs\x1b\\src/lib.rs:{line}:{col}\x1b]8;;\x1b\\\r\n"
        );
        let _ = write!(
            out,
            "\x1b[1m\x1b[38;5;12m{line:>4} |\x1b[0m     let x: u32 = \"text\"; // 中文\r\n"
        );
        let _ = write!(
            out,
            "     \x1b[1m\x1b[38;5;12m|\x1b[0m{}\x1b[1m\x1b[38;5;{color}m{} expected \
             `u32`\x1b[0m\r\n\r\n",
            " ".repeat(rng.gen_range(1..30)),
            "^".repeat(rng.gen_range(1..20)),
        );
    }
}

fn cjk_stream<R: Rng>(rng: &mut R, out: &mut Vec<u8>) {
    let words = [
        "日本語の",
        "文章",
        "と",
        "English",
        "words",
        "混在",
        "한국어",
        "テキスト",
        "。",
        "、",
        " ",
    ];
    for _ in 0..rng.gen_range(20..200) {
        for _ in 0..rng.gen_range(1..60) {
            out.extend_from_slice(words.choose(rng).unwrap().as_bytes());
        }
        out.extend_from_slice(if rng.gen_bool(0.8) { b"\r\n" } else { b"\r\n\r\n" });
    }
}

fn full_screen_stream<R: Rng>(rng: &mut R, out: &mut Vec<u8>, lines: usize, columns: usize) {
    out.extend_from_slice(b"\x1b[?25l\x1b[H\x1b[2J");
    for frame in 0..rng.gen_range(5..40) {
        let _ = write!(
            out,
            "\x1b[H\x1b[7m top - {frame:02}:00 up 3 days, load average: 0.{}\x1b[K\x1b[m",
            rng.gen_range(0..99)
        );
        let _ = write!(out, "\x1b[3;{}r", lines.max(3));
        for row in 2..=lines {
            let _ = write!(
                out,
                "\x1b[{row};1H\x1b[{}m{:>6} user {:>5.1} {:>5.1} {}\x1b[m\x1b[K",
                if row % 3 == 0 { 1 } else { 0 },
                rng.gen_range(1..99_999),
                rng.gen_range(0.0..100.0),
                rng.gen_range(0.0..100.0),
                ["cargo", "rustc", "fbterm", "中文进程", "sshd"].choose(rng).unwrap(),
            );
        }
        if rng.gen_bool(0.3) {
            let _ = write!(out, "\x1b[{}S", rng.gen_range(1..4));
        }
        let _ = write!(out, "\x1b[r\x1b[{lines};{}H", columns.min(10));
    }
    out.extend_from_slice(b"\x1b[?25h\x1b[H\x1b[2J");
}

/// A realistic case: a deterministic stream of `kind`, chunked, with occasional interleaved
/// view scrolls, resizes, history changes and selections.
pub fn realistic_case(kind: Realistic, seed: u64) -> Case {
    let mut rng = rng(seed, 2);
    let start = Start {
        lines: rng.gen_range(5..=50),
        columns: rng.gen_range(20..=160),
        history: random_limit(&mut rng, MAX_HISTORY),
        kitty: false,
        query_seed: rng.r#gen(),
    };
    let mut stream = Vec::new();
    match kind {
        Realistic::Ls => ls_stream(&mut rng, &mut stream),
        Realistic::GitGraph => git_graph_stream(&mut rng, &mut stream),
        Realistic::Diagnostics => diagnostics_stream(&mut rng, &mut stream),
        Realistic::Cjk => cjk_stream(&mut rng, &mut stream),
        Realistic::FullScreen => {
            full_screen_stream(&mut rng, &mut stream, start.lines, start.columns)
        },
    }
    let steps = chunk_stream(&mut rng, &stream, &start, true);
    Case { start, steps }
}

/// Names of the ref test recordings in `tests/ref`.
pub fn recordings() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ref");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A ref test recording replayed whole as one run. With `extended`, the history limit is 10,000
/// and view scrolls, resizes, history changes and selections are interleaved; otherwise the
/// recording's own size and history limit are used and nothing is interleaved.
pub fn recording_case(name: &str, seed: u64, extended: bool) -> Case {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ref").join(name);
    let stream = std::fs::read(dir.join("alacritty.recording")).unwrap();
    let json = |file: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
    };
    let size = json("size.json");
    let config = json("config.json");

    let mut rng = rng(seed, 3);
    let start = Start {
        lines: size["screen_lines"].as_u64().unwrap() as usize,
        columns: size["columns"].as_u64().unwrap() as usize,
        history: if extended {
            MAX_HISTORY
        } else {
            config["history_size"].as_u64().unwrap() as usize
        },
        kitty: false,
        query_seed: rng.r#gen(),
    };
    let steps = chunk_stream(&mut rng, &stream, &start, extended);
    Case { start, steps }
}
