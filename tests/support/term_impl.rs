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
/// (`RegexIter`) can cycle between two matches forever (next to a `LEADING_WIDE_CHAR_SPACER` at
/// the end of a line). This walks the same iterator `search_next` walks, with the same start, end
/// and stop condition (`search_bounds`), but gives up after more matches than the grid has cells.
/// feathercritty fixed the cycle (fork change 5), so on its side this always returns `true`;
/// remove it once the oracle release fixes the cycle too.
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
    /// answer [`QAns::NoTermination`] if it has not returned after `timeout` (the thread is
    /// left running). Used for searches 0.26.0 cannot run (fork change 5), so a feathercritty
    /// search that does not terminate fails the test with its seed instead of hanging it.
    /// Search reads only the grid, so a fresh `Term` holding a clone of it searches the same.
    pub fn search_detached(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        timeout: Duration,
    ) -> QAns {
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
        rx.recv_timeout(timeout).unwrap_or(QAns::NoTermination)
    }

    /// The cells of the character at `p`: a wide character with its spacer, plus the leading
    /// spacer on the previous line when it wrapped.
    fn glyph(&self, p: Point) -> Vec<Point> {
        let grid = self.term.grid();
        let last = grid.last_column();
        let top = grid.topmost_line();
        let bottom = grid.bottommost_line();
        let flags = |p: Point| grid[p].flags;

        let mut wide = p;
        if flags(p).contains(Flags::LEADING_WIDE_CHAR_SPACER) && p.line < bottom {
            wide = Point::new(p.line + 1, Column(0));
        } else if flags(p).contains(Flags::WIDE_CHAR_SPACER) && p.column.0 > 0 {
            wide = Point::new(p.line, p.column - 1);
        }
        let mut cells = vec![p, wide];
        if flags(wide).contains(Flags::WIDE_CHAR) {
            if wide.column < last {
                cells.push(Point::new(wide.line, wide.column + 1));
            }
            let above = Point::new(wide.line - 1, last);
            if wide.column.0 == 0
                && wide.line > top
                && flags(above).contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                cells.push(above);
            }
        }
        cells
    }

    /// Whether the character at `p` overlaps the range `search_next` searches for `search`
    /// with `max_lines` (walked cell by cell from its start to its end, wrapping around the
    /// buffer as the search does).
    fn in_search_range(&self, search: &SearchQ, max_lines: Option<usize>, p: Pt) -> bool {
        let term = &self.term;
        let direction = if search.right { Direction::Right } else { Direction::Left };
        let (_, start, end, bounded) =
            search_bounds(term, self.point(search.origin), direction, max_lines);
        if !bounded {
            return true;
        }
        let glyph = self.glyph(self.point(p));
        let mut point = start;
        for _ in 0..=term.total_lines() * term.columns() {
            if glyph.contains(&point) {
                return true;
            }
            if point == end {
                return false;
            }
            point = match direction {
                Direction::Right => point.add(term, Boundary::None, 1),
                Direction::Left => point.sub(term, Boundary::None, 1),
            };
        }
        false
    }

    /// Whether the cells from `start` to `end` (grid order) spell a match of `pattern` as
    /// `RegexSearch` reads them: the characters of all but spacer cells, only across wrapped
    /// line ends, with smart case (case insensitive unless the pattern has an uppercase letter).
    fn is_real_match(&self, pattern: &str, start: Pt, end: Pt) -> Result<(), String> {
        let grid = self.term.grid();
        let (start, end) = (self.point(start), self.point(end));
        if start > end {
            return Err(format!("match start {start:?} after its end {end:?}"));
        }
        let mut text = String::new();
        let mut point = start;
        let mut skip_next = false;
        loop {
            let cell = &grid[point];
            // `regex_search_internal` reads a wide character and steps over the cell after it
            // (whatever its flags), and steps over leading spacers.
            if skip_next {
                skip_next = false;
            } else if !cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                text.push(cell.c);
                skip_next =
                    cell.flags.contains(Flags::WIDE_CHAR) && point.column < grid.last_column();
            }
            if point == end {
                break;
            }
            if point.column == grid.last_column() && !cell.flags.contains(Flags::WRAPLINE) {
                return Err(format!("match crosses the unwrapped end of line {}", point.line.0));
            }
            point = point.add(grid, Boundary::Grid, 1);
        }

        let case_insensitive = !pattern.chars().any(char::is_uppercase);
        let regex = regex_automata::meta::Regex::builder()
            .syntax(regex_automata::util::syntax::Config::new().case_insensitive(case_insensitive))
            .build(&format!(r"\A(?:{pattern})\z"))
            .map_err(|err| err.to_string())?;
        if regex.is_match(&text) {
            Ok(())
        } else {
            Err(format!("{text:?} does not match {pattern:?}"))
        }
    }

    /// Check a `search_next` answer without an oracle (used on the feathercritty side where
    /// 0.26.0 cannot answer, fork change 5): it must have terminated, and a match must overlap
    /// the searched range at both ends and be a real match of the pattern. `Match(None)` is
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
                for p in [start, end] {
                    if !self.in_search_range(search, max_lines, *p) {
                        return Err(format!("match end {p:?} lies outside the searched range"));
                    }
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
                !self.in_search_range(search, search.max_lines, *start)
                    || !self.in_search_range(search, search.max_lines, *end)
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
