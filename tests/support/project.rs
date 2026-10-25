// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The crate-neutral projection of a terminal's state, the random query plan, and readable diffs.

use std::fmt::Write as _;

use feathercritty::vte::ansi::Color;
use rand::Rng;
use rand::seq::SliceRandom;

use super::{Pt, SelTy};

/// A hyperlink, comparable across crates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkP {
    pub uri: String,
    /// Index (row-major from the topmost history row) of the first cell whose hyperlink has the
    /// same ID. IDs generated without an explicit ID come from a per-crate counter, so only this
    /// pattern is comparable. `None` for a cursor template whose ID is not in the grid.
    pub first: Option<usize>,
    /// The ID itself when it was given explicitly (not generated).
    pub explicit: Option<String>,
}

/// One cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellP {
    pub c: char,
    pub zerowidth: Vec<char>,
    pub fg: Color,
    pub bg: Color,
    /// `Flags::bits()`.
    pub flags: u16,
    pub underline_color: Option<Color>,
    pub link: Option<Box<LinkP>>,
}

/// A cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorP {
    pub line: i32,
    pub col: usize,
    pub input_needs_wrap: bool,
    pub template: CellP,
}

/// The persistent selection's range (start, end, is_block) and text.
pub type SelectionP = (Option<(Pt, Pt, bool)>, Option<String>);

/// A query or projection that panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panicked;

/// Everything compared after each step (spec 6.1, "State compared").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub columns: usize,
    pub lines: usize,
    pub history_size: usize,
    pub display_offset: usize,
    /// `TermMode::bits()`.
    pub mode: u32,
    pub cursor: CursorP,
    pub saved_cursor: CursorP,
    /// Every row from the topmost history line down to the bottom of the screen.
    pub rows: Vec<Vec<CellP>>,
    /// The persistent selection: its range (start, end, is_block) and its text, or `Err` when
    /// computing them panics (0.26.0 does for some selections, e.g. an integer underflow in
    /// debug builds for some `Lines` selections reaching above the history).
    pub selection: Result<SelectionP, Panicked>,
}

impl Snapshot {
    /// Line of `rows[0]`.
    pub fn topmost_line(&self) -> i32 {
        -(self.history_size as i32)
    }

    fn row_text(&self, index: usize) -> String {
        self.rows[index].iter().map(|cell| cell.c).collect()
    }
}

/// Queries run against both terminals after each step, drawn from one seeded RNG.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    /// One selection of each type, reaching anywhere in history.
    pub selections: Vec<(SelTy, Pt, Pt)>,
    /// `bounds_to_string(start, end)`, `start <= end`.
    pub bounds: (Pt, Pt),
    /// `(pattern, origin, right, side_right, max_lines)` for `search_next`.
    pub searches: Vec<(String, Pt, bool, bool, Option<usize>)>,
    /// Points for `bracket_search`, `semantic_search_*`, `line_search_*` and `expand_wide`.
    pub points: Vec<Pt>,
    /// Start point and three vi motions (indices into the motion table of each side).
    pub vi: (Pt, [usize; 3]),
}

/// Number of `ViMotion` variants; both sides list them in the same order.
pub const VI_MOTIONS: usize = 21;

/// One query answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QAns {
    Text(Option<String>),
    Point(Pt),
    OptPoint(Option<Pt>),
    Match(Option<(Pt, Pt)>),
}

/// `WIDE_CHAR_SPACER | LEADING_WIDE_CHAR_SPACER`.
const SPACERS: u16 = 0b0000_0100_0100_0000;

const BRACKETS: &[char] = &['(', ')', '[', ']', '{', '}', '<', '>'];

fn escape_regex(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `Flags::WRAPLINE`.
const WRAPLINE: u16 = 0b0000_0000_0001_0000;

/// `Flags::LEADING_WIDE_CHAR_SPACER`.
const LEADING_SPACER: u16 = 0b0000_0100_0000_0000;

/// The `max_lines` to use for a `search_next` from `origin_line`, or `None` to skip the search.
///
/// 0.26.0's regex search skips two cells at once over a wide character that wrapped (one whose
/// previous line ends in a `LEADING_WIDE_CHAR_SPACER`). When the cell it skips is the end of
/// the searched range, the search wraps around the buffer forever, and its match iterator can
/// also cycle between two matches near such a cell. So searches stay one line clear of every
/// line that ends in a leading spacer: the requested window is kept when it is clear, and is
/// otherwise shrunk to the clear lines next to the start (skipped when there are none). Windows
/// wrap around the buffer like the search does (`Boundary::None`).
fn search_window(
    snap: &Snapshot,
    origin_line: i32,
    right: bool,
    max_lines: Option<usize>,
) -> Option<Option<usize>> {
    let top = snap.topmost_line();
    let bottom = snap.lines as i32 - 1;
    let total = snap.rows.len() as i32;
    let last = snap.columns - 1;
    let flags = |line: i32| snap.rows[(line - top) as usize][last].flags;
    // Leading spacers belong in the last column, but a resize can leave one in another.
    let leading: Vec<bool> = snap
        .rows
        .iter()
        .map(|row| row.iter().any(|cell| cell.flags & LEADING_SPACER != 0))
        .collect();
    let tainted = |index: i32| leading[index.rem_euclid(total) as usize];

    // The line the search starts on (`line_search_left` / `line_search_right`).
    let mut start = origin_line;
    if right {
        while start > top && flags(start - 1) & WRAPLINE != 0 {
            start -= 1;
        }
    } else {
        while start < bottom && flags(start) & WRAPLINE != 0 {
            start += 1;
        }
    }
    let start = start - top;
    let step = if right { 1 } else { -1 };

    // Lines `start ..= start + m` in search direction, plus one line on either side.
    let clear = |m: i32| {
        if m + 3 >= total {
            return (0..total).all(|index| !tainted(index));
        }
        (-1..=m + 1).all(|offset| !tainted(start + step * offset))
    };

    match max_lines.filter(|m| m + 1 < total as usize) {
        None if clear(total) => return Some(max_lines),
        Some(m) if clear(m as i32) => return Some(Some(m)),
        _ => (),
    }

    if !clear(0) {
        return None;
    }
    let mut m = 0;
    while m + 2 < total && clear(m + 1) {
        m += 1;
    }
    Some(Some(m as usize))
}

impl QueryPlan {
    pub fn random<R: Rng>(rng: &mut R, snap: &Snapshot) -> Self {
        let top = snap.topmost_line();
        let bottom = snap.lines as i32 - 1;
        let columns = snap.columns;

        let point = |rng: &mut R| Pt {
            line: rng.gen_range(top..=bottom),
            col: rng.gen_range(0..columns),
            right: rng.gen_bool(0.5),
        };

        let selections = SelTy::ALL.iter().map(|&ty| (ty, point(rng), point(rng))).collect();

        let (a, b) = (point(rng), point(rng));
        let bounds = if (a.line, a.col) <= (b.line, b.col) { (a, b) } else { (b, a) };

        // Patterns are sampled from the text, with a few generic ones mixed in.
        let mut searches = Vec::new();
        for right in [false, true] {
            let pattern = if rng.gen_bool(0.15) {
                ["[a-z]+", r"\d+", r"\w+ \w+", "[^ ]+ ?", "é|中|😀"]
                    .choose(rng)
                    .unwrap()
                    .to_string()
            } else {
                let index = rng.gen_range(0..snap.rows.len());
                let row = &snap.rows[index];
                let start = rng.gen_range(0..row.len());
                let len = rng.gen_range(1..=4);
                let text: String = row[start..]
                    .iter()
                    .filter(|cell| cell.flags & SPACERS == 0)
                    .take(len)
                    .map(|cell| cell.c)
                    .collect();
                // A pattern that matches the empty string makes `search_next` loop forever in
                // 0.26.0 (the UI never searches for one), so never sample an empty pattern.
                if text.is_empty() { "x".to_owned() } else { escape_regex(&text) }
            };
            let max_lines = rng.gen_bool(0.5).then(|| rng.gen_range(0..=snap.rows.len()));
            let origin = point(rng);
            if let Some(max_lines) = search_window(snap, origin.line, right, max_lines) {
                searches.push((pattern, origin, right, rng.gen_bool(0.5), max_lines));
            }
        }

        // Prefer bracket cells for one point so `bracket_search` finds pairs.
        let mut points = vec![point(rng), point(rng)];
        for _ in 0..20 {
            let p = point(rng);
            let cell = &snap.rows[(p.line - top) as usize][p.col];
            if BRACKETS.contains(&cell.c) {
                points.push(p);
                break;
            }
        }

        let vi_start = point(rng);
        let motions = [(); 3].map(|()| rng.gen_range(0..VI_MOTIONS));

        Self { selections, bounds, searches, points, vi: (vi_start, motions) }
    }
}

fn cursor_diff(name: &str, a: &CursorP, b: &CursorP, out: &mut String) {
    if a != b {
        let _ = writeln!(out, "{name}: feathercritty {a:?}\n{name}: 0.26.0       {b:?}");
    }
}

/// A readable description of the first differences between two snapshots (`a` is
/// feathercritty, `b` is 0.26.0).
pub fn diff(a: &Snapshot, b: &Snapshot) -> String {
    let mut out = String::from("state differs\n");
    macro_rules! field {
        ($($f:ident),*) => {$(
            if a.$f != b.$f {
                let _ = writeln!(
                    out,
                    "{}: feathercritty {:?}, 0.26.0 {:?}",
                    stringify!($f),
                    a.$f,
                    b.$f
                );
            }
        )*};
    }
    field!(columns, lines, history_size, display_offset, selection);
    if a.mode != b.mode {
        let _ = writeln!(out, "mode: feathercritty {:#x}, 0.26.0 {:#x}", a.mode, b.mode);
    }
    cursor_diff("cursor", &a.cursor, &b.cursor, &mut out);
    cursor_diff("saved_cursor", &a.saved_cursor, &b.saved_cursor, &mut out);

    if a.rows.len() != b.rows.len() {
        let _ = writeln!(out, "row count: feathercritty {}, 0.26.0 {}", a.rows.len(), b.rows.len());
    }
    let mut shown = 0;
    for (index, (ra, rb)) in a.rows.iter().zip(&b.rows).enumerate() {
        if ra == rb {
            continue;
        }
        let line = a.topmost_line() + index as i32;
        let _ = writeln!(
            out,
            "row {line}:\n  feathercritty {:?}\n  0.26.0        {:?}",
            a.row_text(index),
            b.row_text(index)
        );
        for (col, (ca, cb)) in ra.iter().zip(rb).enumerate() {
            if ca != cb {
                let _ = writeln!(
                    out,
                    "  col {col}:\n    feathercritty {ca:?}\n    0.26.0        {cb:?}"
                );
                break;
            }
        }
        shown += 1;
        if shown == 3 {
            out += "  (further rows omitted)\n";
            break;
        }
    }
    out
}

/// A readable description of differing query answers.
pub fn diff_queries(
    plan: &QueryPlan,
    a: &[(&'static str, QAns)],
    b: &[(&'static str, QAns)],
) -> String {
    let mut out = String::from("query answers differ\n");
    for ((name, qa), (_, qb)) in a.iter().zip(b) {
        if qa != qb {
            let _ = writeln!(out, "{name}:\n  feathercritty {qa:?}\n  0.26.0        {qb:?}");
        }
    }
    let _ = writeln!(out, "plan: {plan:?}");
    out
}
