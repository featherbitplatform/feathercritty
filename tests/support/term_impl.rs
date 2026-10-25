// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Applies steps to one crate's `Term` and projects it. Compiled twice, as `fc::imp` (with
//! `k` = feathercritty) and `ac::imp` (with `k` = `alacritty_terminal` 0.26.0); both crates
//! expose the same API here.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::k;
use k::event::VoidListener;
use k::grid::{Dimensions, Scroll};
use k::index::{Boundary, Column, Direction, Line, Point, Side as KSide};
use k::selection::{Selection, SelectionType};
use k::term::cell::{Cell, Flags};
use k::term::search::{RegexIter, RegexSearch};
use k::term::test::TermSize;
use k::term::{Config, Term};
use k::vi_mode::{ViModeCursor, ViMotion};
use k::vte::ansi::Processor;

use crate::support::project::{
    CellP, CursorP, LinkP, Panicked, QAns, QueryPlan, SearchQ, Snapshot, VI_MOTIONS,
};
use crate::support::{Pt, SelTy, Start, Step, catch};

/// Every `ViMotion`; `project::VI_MOTIONS` is its length.
const MOTIONS: [ViMotion; VI_MOTIONS] = [
    ViMotion::Up,
    ViMotion::Down,
    ViMotion::Left,
    ViMotion::Right,
    ViMotion::First,
    ViMotion::Last,
    ViMotion::FirstOccupied,
    ViMotion::High,
    ViMotion::Middle,
    ViMotion::Low,
    ViMotion::SemanticLeft,
    ViMotion::SemanticRight,
    ViMotion::SemanticLeftEnd,
    ViMotion::SemanticRightEnd,
    ViMotion::WordLeft,
    ViMotion::WordRight,
    ViMotion::WordLeftEnd,
    ViMotion::WordRightEnd,
    ViMotion::Bracket,
    ViMotion::ParagraphUp,
    ViMotion::ParagraphDown,
];

/// One terminal and its parser.
pub struct Side {
    pub term: Term<VoidListener>,
    parser: Processor,
    kitty: bool,
}

fn sel_ty(ty: SelTy) -> SelectionType {
    match ty {
        SelTy::Simple => SelectionType::Simple,
        SelTy::Block => SelectionType::Block,
        SelTy::Semantic => SelectionType::Semantic,
        SelTy::Lines => SelectionType::Lines,
    }
}

fn side(right: bool) -> KSide {
    if right { KSide::Right } else { KSide::Left }
}

fn pt(point: Point) -> Pt {
    Pt::new(point.line.0, point.column.0)
}

/// Generated (not explicit) hyperlink IDs look like `<counter>_alacritty`.
fn explicit_id(id: &str) -> Option<String> {
    let generated = id
        .strip_suffix("_alacritty")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    (!generated).then(|| id.to_owned())
}

/// Project a cell. `index` is its row-major position, or `None` for a cursor template.
fn cell_p(cell: &Cell, ids: &mut HashMap<String, usize>, index: Option<usize>) -> CellP {
    let link = cell.hyperlink().map(|link| {
        let first = match (ids.get(link.id()), index) {
            (Some(&first), _) => Some(first),
            (None, Some(index)) => {
                ids.insert(link.id().to_owned(), index);
                Some(index)
            },
            (None, None) => None,
        };
        Box::new(LinkP { uri: link.uri().to_owned(), first, explicit: explicit_id(link.id()) })
    });

    CellP {
        c: cell.c,
        zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags.bits(),
        underline_color: cell.underline_color(),
        link,
    }
}

/// Origin (after `expand_wide`), start and end of the range `Term::search_next` searches, and
/// whether `max_lines` bounds it. Copied from upstream `search_next` and `next_match_left/right`;
/// re-check against the source when the oracle moves to a new release.
fn search_bounds(
    term: &Term<VoidListener>,
    origin: Point,
    direction: Direction,
    max_lines: Option<usize>,
) -> (Point, Point, Point, bool) {
    let origin = term.expand_wide(origin, direction);
    let max_lines = max_lines.filter(|max_lines| max_lines + 1 < term.total_lines());
    let (start, end) = match direction {
        Direction::Right => {
            let start = term.line_search_left(origin);
            let end = match max_lines {
                Some(max_lines) => Point::new(
                    (start.line + max_lines).grid_clamp(term, Boundary::None),
                    term.last_column(),
                ),
                None => start.sub(term, Boundary::None, 1),
            };
            (start, end)
        },
        Direction::Left => {
            let start = term.line_search_right(origin);
            let end = match max_lines {
                Some(max_lines) => {
                    Point::new((start.line - max_lines).grid_clamp(term, Boundary::None), Column(0))
                },
                None => start.add(term, Boundary::None, 1),
            };
            (start, end)
        },
    };
    (origin, start, end, max_lines.is_some())
}

/// Whether `Term::search_next` returns for these arguments.
///
/// Workaround for a 0.26.0 bug, still needed on the oracle side: 0.26.0's match iterator
/// (`RegexIter`) can cycle between two matches forever (next to a `LEADING_WIDE_CHAR_SPACER`,
/// wrapped or stale). This walks the same iterator `search_next` walks, with the same start, end
/// and stop condition (`search_bounds`), but gives up after more matches than the grid has cells.
/// feathercritty fixed the cycle (fork change 5, `+fbterm.3`), so on its side this returns `true`
/// (anything else fails the run through `check_search`); keep it while the oracle runs 0.26.0
/// and remove it once the oracle release fixes the cycle too.
fn search_terminates(
    term: &Term<VoidListener>,
    regex: &mut RegexSearch,
    origin: Point,
    direction: Direction,
    side: KSide,
    max_lines: Option<usize>,
) -> bool {
    let (origin, start, end, _) = search_bounds(term, origin, direction, max_lines);

    let limit = term.total_lines() * term.columns() + 2;
    let mut matches = RegexIter::new(start, end, direction, term, regex);
    for _ in 0..limit {
        let Some(found) = matches.next() else { return true };
        let point = match side {
            KSide::Right => *found.end(),
            KSide::Left => *found.start(),
        };
        let stop = match direction {
            Direction::Right => {
                point.line < start.line
                    || point.line > origin.line
                    || (point.line == origin.line && point.column >= origin.column)
            },
            Direction::Left => {
                point.line > start.line
                    || point.line < origin.line
                    || (point.line == origin.line && point.column <= origin.column)
            },
        };
        if stop {
            return true;
        }
    }
    false
}

/// Run one query, answering [`QAns::Panicked`] if it panics.
///
/// Workaround: several 0.26.0 query functions panic on some grid states (vi `Last` and
/// `FirstOccupied` follow wrapped lines past the bottom of the screen, vi `Right` from a stray
/// `LEADING_WIDE_CHAR_SPACER` on the bottom line, block selections as above). Both crates run
/// the same code there, so the panic is compared like any other answer. Once upstream fixes
/// those, this can stay; it then only reports real differences.
fn guarded(query: impl FnOnce() -> QAns) -> QAns {
    catch(query).unwrap_or(QAns::Panicked)
}

impl Side {
    pub fn new(start: &Start) -> Self {
        let config = Config {
            scrolling_history: start.history,
            kitty_keyboard: start.kitty,
            ..Config::default()
        };
        let size = TermSize::new(start.columns, start.lines);
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            kitty: start.kitty,
        }
    }

    /// Clamp a step or query point into the grid.
    fn point(&self, p: Pt) -> Point {
        let line = p.line.clamp(self.term.topmost_line().0, self.term.bottommost_line().0);
        Point::new(Line(line), Column(p.col.min(self.term.last_column().0)))
    }

    /// A selection from `start` to `end`, clamped into the grid.
    fn selection(&self, ty: SelTy, start: Pt, end: Pt) -> Selection {
        let (a, b) = (self.point(start), self.point(end));
        let (mut start_side, mut end_side) = (side(start.right), side(end.right));
        // Workaround: 0.26.0 panics in `selection_to_string` for a block selection whose upper
        // anchor is the right half of the last column and whose lower anchor is the left half
        // of the same column (`range_block` starts the range one column past the grid), so use
        // the left half for the upper anchor in that shape. Queries would only record the panic,
        // but the persistent selection's text would be lost for the rest of the run. Remove
        // this once upstream fixes it.
        let last = self.term.last_column();
        if ty == SelTy::Block && a.column == last && b.column == last && a.line != b.line {
            let (upper, lower) = if a.line < b.line {
                (&mut start_side, end_side)
            } else {
                (&mut end_side, start_side)
            };
            if *upper == KSide::Right && lower == KSide::Left {
                *upper = KSide::Left;
            }
        }
        let mut selection = Selection::new(sel_ty(ty), a, start_side);
        selection.update(b, end_side);
        selection
    }

    pub fn apply(&mut self, step: &Step) {
        match step {
            Step::Bytes(bytes) => self.parser.advance(&mut self.term, bytes),
            Step::Resize { lines, columns } => self.term.resize(TermSize::new(*columns, *lines)),
            Step::ScrollView(delta) => self.term.scroll_display(Scroll::Delta(*delta)),
            Step::SetHistory(limit) => self.term.set_options(Config {
                scrolling_history: *limit,
                kitty_keyboard: self.kitty,
                ..Config::default()
            }),
            Step::Select { ty: None, .. } => self.term.selection = None,
            Step::Select { ty: Some(ty), start, end } => {
                self.term.selection = Some(self.selection(*ty, *start, *end));
            },
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let term = &self.term;
        let grid = term.grid();
        let columns = grid.columns();
        let mut ids = HashMap::new();

        let mut rows = Vec::with_capacity(grid.total_lines());
        let mut index = 0;
        for line in grid.topmost_line().0..=grid.bottommost_line().0 {
            let row = &grid[Line(line)];
            let mut out = Vec::with_capacity(columns);
            for column in 0..columns {
                out.push(cell_p(&row[Column(column)], &mut ids, Some(index)));
                index += 1;
            }
            rows.push(out);
        }

        let mut cursor = |c: &k::grid::Cursor<Cell>| CursorP {
            line: c.point.line.0,
            col: c.point.column.0,
            input_needs_wrap: c.input_needs_wrap,
            template: cell_p(&c.template, &mut ids, None),
        };
        let cursor_p = cursor(&grid.cursor);
        let saved_cursor = cursor(&grid.saved_cursor);

        let selection = catch(|| {
            let range = term.selection.as_ref().and_then(|s| s.to_range(term));
            (range.map(|r| (pt(r.start), pt(r.end), r.is_block)), term.selection_to_string())
        })
        .map_err(|_| Panicked);

        Snapshot {
            columns,
            lines: grid.screen_lines(),
            history_size: grid.history_size(),
            display_offset: grid.display_offset(),
            mode: term.mode().bits(),
            cursor: cursor_p,
            saved_cursor,
            rows,
            selection,
        }
    }

    /// Run `search` with `max_lines`. With `probe`, first check with `search_terminates` that
    /// it returns (the 0.26.0 workaround); without, call `search_next` directly, which only the
    /// feathercritty side may do for windows 0.26.0 cannot search.
    pub fn search(&self, search: &SearchQ, max_lines: Option<usize>, probe: bool) -> QAns {
        let term = &self.term;
        let direction = if search.right { Direction::Right } else { Direction::Left };
        let (origin, side) = (self.point(search.origin), side(search.side_right));
        guarded(|| match RegexSearch::new(&search.pattern) {
            Ok(mut regex) => {
                if probe && !search_terminates(term, &mut regex, origin, direction, side, max_lines)
                {
                    return QAns::NoTermination;
                }
                let found = term.search_next(&mut regex, origin, direction, side, max_lines);
                QAns::Match(found.map(|m| (pt(*m.start()), pt(*m.end()))))
            },
            Err(_) => QAns::Text(None),
        })
    }

    /// Run `search` with `max_lines` on a copy of this terminal's grid on another thread, and
    /// answer `None` if it has not returned after `timeout`. Used for searches 0.26.0 cannot run
    /// (fork change 5), so a feathercritty search that does not terminate fails the test with
    /// its seed instead of hanging it. A timed-out thread cannot be stopped: it keeps running
    /// (and a core busy) until the process exits. Search reads only the grid, so a fresh `Term`
    /// holding a clone of it searches the same.
    pub fn search_detached(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        timeout: Duration,
    ) -> Option<QAns> {
        let grid = self.term.grid().clone();
        let size = TermSize::new(grid.columns(), grid.screen_lines());
        let mut term = Term::new(Config::default(), &size, VoidListener);
        *term.grid_mut() = grid;
        let side = Side { term, parser: Processor::new(), kitty: false };
        let search = search.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(side.search(&search, max_lines, false));
        });
        rx.recv_timeout(timeout).ok()
    }

    /// The cells after `p` that a rightward search reads or steps over together with it, as
    /// `skip_fullwidth` in feathercritty's `src/term/search.rs` does (`Term::glyph_end` there):
    /// the two cells after a `LEADING_WIDE_CHAR_SPACER` (wrapped: the wide character and its
    /// spacer; stale, even mid-line: the next cell, read in the spacer's place, and the one
    /// after it), or the spacer after a wide character. Clamped to the grid's last cell.
    fn completion_right(&self, p: Point) -> Vec<Point> {
        let grid = self.term.grid();
        let last = Point::new(grid.bottommost_line(), grid.last_column());
        let flags = grid[p].flags;
        let count = if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            2
        } else if flags.contains(Flags::WIDE_CHAR) && p.column < grid.last_column() {
            1
        } else {
            0
        };
        let mut cells = Vec::new();
        let mut point = p;
        for _ in 0..count {
            if point == last {
                break;
            }
            point = point.add(grid, Boundary::Grid, 1);
            cells.push(point);
        }
        cells
    }

    /// The cells before `p` that a leftward search reads or steps over together with it: the
    /// wide character before its spacer, and a leading spacer before a wrapped wide character
    /// (left onto a wrapped wide character, the search reports it at its leading spacer). A
    /// stale leading spacer reads as a blank going left, so it adds nothing.
    fn completion_left(&self, p: Point) -> Vec<Point> {
        let grid = self.term.grid();
        let first = Point::new(grid.topmost_line(), Column(0));
        let before = |p: Point| (p > first).then(|| p.sub(grid, Boundary::Grid, 1));
        let mut cells = Vec::new();
        let mut wide = p;
        if grid[p].flags.contains(Flags::WIDE_CHAR_SPACER) {
            let Some(w) = before(p) else { return cells };
            cells.push(w);
            wide = w;
        }
        if grid[wide].flags.contains(Flags::WIDE_CHAR) {
            if let Some(s) = before(wide) {
                if grid[s].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    cells.push(s);
                }
            }
        }
        cells
    }

    /// Whether the match `start..=end` (grid order) lies in the range `search_next` searches for
    /// `search` with `max_lines`, as fork change 5 (`+fbterm.3`) bounds it:
    /// - the range runs from the search's start to its end in search direction, wrapping around the
    ///   buffer like the search (an unbounded search covers the whole buffer);
    /// - the far end of the match (its end going right, its start going left) may also lie in the
    ///   completion of the character at the range's end (`completion_right` going right,
    ///   `completion_left` going left);
    /// - the near end may also lie in the completion of the character at the range's start in the
    ///   other direction (the second search, back to the start, completes it, as 0.26.0 does).
    ///
    /// If upstream's or feathercritty's range handling in `search.rs` changes, update this.
    fn match_in_range(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        start: Pt,
        end: Pt,
    ) -> bool {
        let term = &self.term;
        let direction = if search.right { Direction::Right } else { Direction::Left };
        let (_, range_start, range_end, bounded) =
            search_bounds(term, self.point(search.origin), direction, max_lines);
        if !bounded {
            return true;
        }

        let mut range = Vec::new();
        let mut point = range_start;
        for _ in 0..=term.total_lines() * term.columns() {
            range.push(point);
            if point == range_end {
                break;
            }
            point = match direction {
                Direction::Right => point.add(term, Boundary::None, 1),
                Direction::Left => point.sub(term, Boundary::None, 1),
            };
        }

        let (start, end) = (self.point(start), self.point(end));
        let (near, far, near_completion, far_completion) = match direction {
            Direction::Right => {
                (start, end, self.completion_left(range_start), self.completion_right(range_end))
            },
            Direction::Left => {
                (end, start, self.completion_right(range_start), self.completion_left(range_end))
            },
        };
        (range.contains(&near) || near_completion.contains(&near))
            && (range.contains(&far) || far_completion.contains(&far))
    }

    /// The text `regex_search_internal` reads from `start` to `end` going right, following
    /// `skip_fullwidth` in feathercritty's `src/term/search.rs`: a wide character is read and the
    /// cell after it stepped over; at a `LEADING_WIDE_CHAR_SPACER` the next cell is read in its
    /// place and the cell after that stepped over (for a stale spacer that next cell is an
    /// ordinary one); every other cell, a stray spacer included, is read as its character.
    fn text_right(&self, start: Point, end: Point) -> String {
        let grid = self.term.grid();
        let last = Point::new(grid.bottommost_line(), grid.last_column());
        let mut text = String::new();
        let mut point = start;
        loop {
            let cell = &grid[point];
            let skip = if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && point < last {
                text.push(grid[point.add(grid, Boundary::Grid, 1)].c);
                3
            } else {
                text.push(cell.c);
                if cell.flags.contains(Flags::WIDE_CHAR) && point.column < grid.last_column() {
                    2
                } else {
                    1
                }
            };
            for _ in 0..skip {
                if point >= end || point == last {
                    return text;
                }
                point = point.add(grid, Boundary::Grid, 1);
            }
            if point > end {
                return text;
            }
        }
    }

    /// The text read from `end` back to `start` going left, as `skip_fullwidth` does: at a
    /// wide character's spacer the wide character is read (and a leading spacer just before it
    /// stepped over); every other cell, a stale leading spacer included, is read as its
    /// character (a stale spacer reads as a blank).
    fn text_left(&self, start: Point, end: Point) -> String {
        let grid = self.term.grid();
        let first = Point::new(grid.topmost_line(), Column(0));
        let mut text = Vec::new();
        let mut point = end;
        loop {
            let cell = &grid[point];
            let mut skip = 1;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && point > first {
                let wide = point.sub(grid, Boundary::Grid, 1);
                text.push(grid[wide].c);
                skip = 2;
                if wide > first
                    && grid[wide.sub(grid, Boundary::Grid, 1)]
                        .flags
                        .contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    skip = 3;
                }
            } else {
                text.push(cell.c);
            }
            for _ in 0..skip {
                if point <= start || point == first {
                    return text.into_iter().rev().collect();
                }
                point = point.sub(grid, Boundary::Grid, 1);
            }
            if point < start {
                return text.into_iter().rev().collect();
            }
        }
    }

    /// Whether the cells from `start` to `end` (grid order) spell a match of `pattern` the way
    /// the search reads them (`text_right` or `text_left`; a rightward search finds a match's
    /// end reading right and its start reading back left, so either reading may be the one that
    /// matched), only across wrapped line ends, with smart case (case insensitive unless the
    /// pattern has an uppercase letter), as `RegexSearch` does.
    fn is_real_match(&self, pattern: &str, start: Pt, end: Pt) -> Result<(), String> {
        let grid = self.term.grid();
        let (start, end) = (self.point(start), self.point(end));
        if start > end {
            return Err(format!("match start {start:?} after its end {end:?}"));
        }
        for line in start.line.0..end.line.0 {
            if !grid[Line(line)][grid.last_column()].flags.contains(Flags::WRAPLINE) {
                return Err(format!("match crosses the unwrapped end of line {line}"));
            }
        }

        let case_insensitive = !pattern.chars().any(char::is_uppercase);
        let regex = regex_automata::meta::Regex::builder()
            .syntax(regex_automata::util::syntax::Config::new().case_insensitive(case_insensitive))
            .build(&format!(r"\A(?:{pattern})\z"))
            .map_err(|err| err.to_string())?;
        let (right, left) = (self.text_right(start, end), self.text_left(start, end));
        if regex.is_match(&right) || regex.is_match(&left) {
            Ok(())
        } else {
            Err(format!("neither {right:?} nor {left:?} matches {pattern:?}"))
        }
    }

    /// Check a `search_next` answer without an oracle (used on the feathercritty side where
    /// 0.26.0 cannot answer, fork change 5): it must have terminated, and a match must lie in
    /// the searched range (`match_in_range`) and be a real match of the pattern. `Match(None)` is
    /// accepted (absence is not checked).
    pub fn check_search(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        answer: &QAns,
    ) -> Result<(), String> {
        match answer {
            QAns::Match(None) => Ok(()),
            QAns::Match(Some((start, end))) => {
                if !self.match_in_range(search, max_lines, *start, *end) {
                    return Err("the match lies outside the searched range".to_owned());
                }
                self.is_real_match(&search.pattern, *start, *end)
            },
            other => Err(format!("expected a search result, got {other:?}")),
        }
    }

    /// Whether a 0.26.0 `search_next` answer is one fork change 5 may change: the search did
    /// not terminate, or its match lies (at least partly) outside the searched range because
    /// 0.26.0 missed the range's end and wrapped around.
    pub fn oracle_search_is_off(&self, search: &SearchQ, answer: &QAns) -> bool {
        match answer {
            QAns::NoTermination => true,
            QAns::Match(Some((start, end))) => {
                !self.match_in_range(search, search.max_lines, *start, *end)
            },
            _ => false,
        }
    }

    pub fn queries(&mut self, plan: &QueryPlan) -> Vec<(&'static str, QAns)> {
        let mut out = Vec::new();

        let saved = self.term.selection.take();
        for &(ty, start, end) in &plan.selections {
            self.term.selection = Some(self.selection(ty, start, end));
            let term = &self.term;
            out.push(("selection_to_string", guarded(|| QAns::Text(term.selection_to_string()))));
        }
        self.term.selection = saved;

        let (start, end) = (self.point(plan.bounds.0), self.point(plan.bounds.1));
        let term = &self.term;
        out.push((
            "bounds_to_string",
            guarded(|| QAns::Text(Some(term.bounds_to_string(start, end)))),
        ));

        for search in &plan.searches {
            out.push(("search_next", self.search(search, search.max_lines, true)));
        }

        for &p in &plan.points {
            let p = self.point(p);
            out.push((
                "bracket_search",
                guarded(|| QAns::OptPoint(term.bracket_search(p).map(pt))),
            ));
            out.push((
                "semantic_search_left",
                guarded(|| QAns::Point(pt(term.semantic_search_left(p)))),
            ));
            out.push((
                "semantic_search_right",
                guarded(|| QAns::Point(pt(term.semantic_search_right(p)))),
            ));
            out.push(("line_search_left", guarded(|| QAns::Point(pt(term.line_search_left(p))))));
            out.push(("line_search_right", guarded(|| QAns::Point(pt(term.line_search_right(p))))));
            out.push((
                "expand_wide left",
                guarded(|| QAns::Point(pt(term.expand_wide(p, Direction::Left)))),
            ));
            out.push((
                "expand_wide right",
                guarded(|| QAns::Point(pt(term.expand_wide(p, Direction::Right)))),
            ));
        }

        // Vi motions may scroll the view; put it back afterwards.
        let offset = self.term.grid().display_offset();
        let (start, motions) = plan.vi;
        let mut cursor = ViModeCursor::new(self.point(start));
        for motion in motions {
            let term = &mut self.term;
            let moved = catch(|| cursor.motion(term, MOTIONS[motion]));
            let Ok(moved) = moved else {
                out.push(("vi_motion", QAns::Panicked));
                break;
            };
            cursor = moved;
            out.push(("vi_motion", QAns::Point(pt(cursor.point))));
            // Workaround: some 0.26.0 motions can leave the grid (`Middle` on a one-line screen
            // without history, `Right` from a stray `LEADING_WIDE_CHAR_SPACER` on the bottom
            // line), and motions from there read rows outside it, so the chain stops. Remove
            // this once upstream keeps motions inside the grid.
            let line = cursor.point.line;
            if line < self.term.topmost_line() || line > self.term.bottommost_line() {
                break;
            }
        }
        let moved = self.term.grid().display_offset();
        if moved != offset {
            self.term.scroll_display(Scroll::Delta(offset as i32 - moved as i32));
        }

        out
    }
}
