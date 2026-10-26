// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Applies steps to one crate's `Term` and projects it. Compiled twice, as `fc::imp` (with
//! `k` = feathercritty) and `ac::imp` (with `k` = `alacritty_terminal` 0.26.0); both crates
//! expose the same API here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{ORACLE, cell_at, k, new_term, row_at, set_cell};
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
    CellP, CursorP, LinkP, Panicked, QAns, QueryPlan, SearchQ, Snapshot, VI_MOTIONS, Walk, WalkEnd,
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
    /// The matches 0.26.0's iterator yielded in each probed `search` since the last `queries`
    /// (`search_terminates`), `None` where it was not walked; see `check_search_walked`.
    walks: RefCell<Vec<Option<Walk>>>,
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
/// whether `max_lines` bounds it. Copied from upstream `search_next` and `next_match_left/right`,
/// with feathercritty's origin for a leading spacer on the bottommost line (below); re-check
/// against the source when the oracle moves to a new release.
fn search_bounds(
    term: &Term<VoidListener>,
    origin: Point,
    direction: Direction,
    max_lines: Option<usize>,
) -> (Point, Point, Point, bool) {
    // Where `expand_wide` leaves the grid (`origin_leaves_grid`), feathercritty uses the cell its
    // search lands on after reading the spacer, two cells on, clamped to the grid's last cell
    // (fork change 5, `search_next`).
    let origin = if origin_leaves_grid(term, origin, direction) {
        let last = Point::new(term.bottommost_line(), term.last_column());
        let step = |p: Point| if p == last { p } else { p.add(term, Boundary::Grid, 1) };
        step(step(origin))
    } else {
        term.expand_wide(origin, direction)
    };
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

/// Whether `search_next`'s origin is a leading spacer on the bottommost line, going right. 0.26.0's
/// `expand_wide` then moves it to column 1 of the line below the grid, and 0.26.0 panics (debug
/// builds, and release builds when its ring has no row there) or answers from whichever row its
/// ring holds at that index. feathercritty keeps the origin on the line
/// (`search_origin_on_bottom_line_stale_spacer`), so these searches are not compared.
fn origin_leaves_grid(term: &Term<VoidListener>, origin: Point, direction: Direction) -> bool {
    direction == Direction::Right
        && origin.line == term.bottommost_line()
        && cell_at(term.grid(), origin).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
}

/// Whether `Term::search_next` returns, never returns or panics for these arguments (`Probe`).
///
/// Workaround for a 0.26.0 bug, still needed on the oracle side: 0.26.0's match iterator
/// (`RegexIter`) can cycle between two matches forever (next to a `LEADING_WIDE_CHAR_SPACER`,
/// wrapped or stale). This walks the same iterator `search_next` walks, with the same start, end
/// and stop condition (`search_bounds`), but gives up after more matches than the grid has cells.
/// feathercritty ends such iterations (fork change 5), so on its side this answers `Probe::Ends`
/// without walking; keep it while the oracle runs 0.26.0 and remove it once the oracle release
/// fixes the cycle too.
///
/// A single search inside the iterator can also run forever in 0.26.0, and that cannot be bounded
/// from outside. On the oracle side this therefore also answers `Probe::Never`, without searching,
/// when the iterator's searches can jump over its end (`first_search_misses_end`), even though
/// 0.26.0 still terminates there when it finds its match first. feathercritty's answer is then
/// checked on its own (fork change 5). `project::search_window` moves the windows of leftward
/// searches with `max_lines` off such ends instead; this catches the remaining ones (unbounded
/// leftward searches and rightward searches).
fn search_terminates(
    term: &Term<VoidListener>,
    regex: &mut RegexSearch,
    origin: Point,
    direction: Direction,
    side: KSide,
    max_lines: Option<usize>,
) -> (Probe, Option<Walk>) {
    // feathercritty's searches always return (fork change 5: its iteration ends where 0.26.0's
    // would repeat forever), and `check_search` checks its answers, so its side does not walk the
    // iterator here. Its `RegexIter` yields 0.26.0's matches, repeats included, until its cycle
    // check notices the repeat, which can take more than the limit below.
    if !ORACLE {
        return (Probe::Ends, None);
    }

    // Workaround for a 0.26.0 bug: going left from a wide char spacer at column 0 (a stale one),
    // `expand_wide` subtracts from column 0. 0.26.0 panics there in debug builds and goes on from
    // a wrapped-around column in release builds; both builds are answered "panics", here for the
    // origin of `search_next` (no match produced) and in the walk below after a match
    // (differential soak seed 30001812). feathercritty answers there (fork change 5). Remove this
    // once upstream's `expand_wide` handles column 0.
    let underflows = |p: Point| {
        direction == Direction::Left
            && p.column == Column(0)
            && cell_at(term.grid(), p).flags.contains(Flags::WIDE_CHAR_SPACER)
    };
    if underflows(origin) {
        return (Probe::Panics, Some(Walk { matches: Vec::new(), end: WalkEnd::Panics }));
    }

    // On the oracle side, searches 0.26.0 cannot run sensibly are answered "does not terminate":
    // - an origin that leaves the grid (`origin_leaves_grid`): 0.26.0's own search reads outside
    //   the grid. Its iterator is still walked, from feathercritty's in-grid origin
    //   (`search_bounds`), so feathercritty's answer can be checked against the matches 0.26.0's
    //   code yields from there (`search_grid_regressions`);
    // - ranges whose end the searches can step over (`first_search_misses_end`), without running
    //   them: a single 0.26.0 search there can run forever.
    let leaves_grid = origin_leaves_grid(term, origin, direction);
    let (origin, start, end, _) = search_bounds(term, origin, direction, max_lines);
    if first_search_misses_end(term, end, direction) {
        return (Probe::Never, None);
    }
    let ends = if leaves_grid { Probe::Never } else { Probe::Ends };

    // The walk also records every match 0.26.0 yields: where its answer is off, feathercritty
    // answers from them (fork change 5, `check_search_walked`, `Walk::answer`). Each step depends
    // only on its origin, and the next origin only on the match, so a repeated match means the
    // iteration cycles forever, unless it ends right there (the repeat came from a search started
    // at the range's end, which sets `done`): the walk stops at the first repeat, with every match
    // before it recorded. The limit covers every origin once. Where 0.26.0 panics moving its
    // origin past a match (`underflows`), the walk ends with the matches yielded before the panic.
    let limit = term.total_lines() * term.columns() + 2;
    let mut walked = Vec::new();
    let mut seen = HashSet::new();
    let mut matches = RegexIter::new(start, end, direction, term, regex);
    let walk = |matches, end| Some(Walk { matches, end });
    for _ in 0..limit {
        let found = match catch_classified(|| matches.next()) {
            Ok(Some(found)) => found,
            Ok(None) => return (ends, walk(walked, WalkEnd::Ended)),
            Err(Probe::Panics) => return (Probe::Panics, walk(walked, WalkEnd::Panics)),
            Err(probe) => return (probe, walk(walked, WalkEnd::Hangs)),
        };
        // A debug build panics inside `next` before yielding this match (caught above).
        if *found.end() != end && underflows(*found.end()) {
            return (Probe::Panics, walk(walked, WalkEnd::Panics));
        }
        let found_pt = (pt(*found.start()), pt(*found.end()));
        if !seen.insert(found_pt) {
            // A repeated match is never one `search_next` stops at (the earlier one would have
            // been), so the iteration either ends now or cycles.
            return match catch_classified(|| matches.next()) {
                Ok(None) => {
                    walked.push(found_pt);
                    (ends, walk(walked, WalkEnd::Ended))
                },
                _ => (Probe::Never, walk(walked, WalkEnd::Repeats)),
            };
        }
        walked.push(found_pt);
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
            return (ends, walk(walked, WalkEnd::Found));
        }
    }
    (Probe::Never, walk(walked, WalkEnd::Repeats))
}

/// Run one step of 0.26.0's iterator in `search_terminates`, classifying a panic by its message:
/// - `attempt to subtract with overflow`: `expand_wide` going left from a wide char spacer at
///   column 0 (`underflows`; the only subtraction on the search paths that can overflow there),
///   [`Probe::Panics`];
/// - `attempt to add with overflow`: `regex_search_internal`'s `consumed_bytes` (an `i32`)
///   overflowing in a debug build, after about 2^31 bytes of a search that never ends (a release
///   build reads on forever), [`Probe::Never`];
/// - anything else is a 0.26.0 panic this harness does not know: it is raised again, so the query
///   answers `Panicked` without a walk and is compared for equality (`oracle_search_is_off`), which
///   fails and shows it.
fn catch_classified<R>(f: impl FnOnce() -> R) -> Result<R, Probe> {
    catch(f).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|m| m.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if message.contains("attempt to subtract with overflow") {
            Probe::Panics
        } else if message.contains("attempt to add with overflow") {
            Probe::Never
        } else {
            std::panic::resume_unwind(payload)
        }
    })
}

/// What `search_terminates` found out about 0.26.0's `search_next`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// It returns; run it.
    Ends,
    /// It would never return, or its origin leaves the grid.
    Never,
    /// It panics (in debug builds; see `search_terminates`).
    Panics,
}

/// Whether 0.26.0's searches towards `end` (the first search of every `RegexIter` step) can step
/// over it and never stop there: `regex_search_internal` checks for `end` only on the cells it
/// stands on before and after `skip_fullwidth`, and a skip moves past `end` without standing on
/// it when
/// - going right, the cell before `end` is a `LEADING_WIDE_CHAR_SPACER` (wrapped or stale, also in
///   the middle of a line): the skip reads `end` in its place and lands on the cell after it;
/// - going left, `end` is a wide character between a leading spacer and its spacer: the skip goes
///   from the spacer to the leading spacer.
///
/// The search then reads around the buffer and past `end` again on every lap, so it stops only if
/// it finds a match first (differential soak seed `random_streams` 2001598: a rightward search
/// on a 2-column grid whose `end` follows a stale spacer in column 0 ran forever). Skips do not
/// cross the grid's edges, so `end` next to them is never stepped over.
fn first_search_misses_end(term: &Term<VoidListener>, end: Point, direction: Direction) -> bool {
    let grid = term.grid();
    let first = Point::new(grid.topmost_line(), Column(0));
    let last = Point::new(grid.bottommost_line(), grid.last_column());
    let flags = |p: Point| cell_at(grid, p).flags;
    match direction {
        Direction::Right => {
            end > first
                && end < last
                && flags(end.sub(grid, Boundary::Grid, 1)).contains(Flags::LEADING_WIDE_CHAR_SPACER)
        },
        Direction::Left => {
            end > first
                && end < last
                && flags(end).contains(Flags::WIDE_CHAR)
                && flags(end.add(grid, Boundary::Grid, 1)).contains(Flags::WIDE_CHAR_SPACER)
                && flags(end.sub(grid, Boundary::Grid, 1)).contains(Flags::LEADING_WIDE_CHAR_SPACER)
        },
    }
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
    /// A terminal with no history whose screen holds `rows`, each a line of characters and a
    /// line of flag codes, one per cell: `.` none, `r` `WRAPLINE`, `W` `WIDE_CHAR`, `s`
    /// `WIDE_CHAR_SPACER`, `S` `WIDE_CHAR_SPACER | WRAPLINE`, `L`
    /// `LEADING_WIDE_CHAR_SPACER | WRAPLINE`, `l`
    /// `LEADING_WIDE_CHAR_SPACER`. Used by `search_grid_probe` to write the same grid into both
    /// crates without a byte stream.
    pub fn from_rows(rows: &[(String, String)]) -> Self {
        let columns = rows[0].1.chars().count();
        let start = Start { lines: rows.len(), columns, history: 0, kitty: false, query_seed: 0 };
        let mut side = Self::new(&start);
        for (line, (text, flags)) in rows.iter().enumerate() {
            for (column, (c, code)) in text.chars().zip(flags.chars()).enumerate() {
                let flags = match code {
                    '.' => Flags::empty(),
                    'r' => Flags::WRAPLINE,
                    'W' => Flags::WIDE_CHAR,
                    's' => Flags::WIDE_CHAR_SPACER,
                    'L' => Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE,
                    'l' => Flags::LEADING_WIDE_CHAR_SPACER,
                    'S' => Flags::WIDE_CHAR_SPACER | Flags::WRAPLINE,
                    other => panic!("unknown flag code {other:?}"),
                };
                let cell = Cell { c, flags, ..Cell::default() };
                set_cell(&mut side.term, Point::new(Line(line as i32), Column(column)), cell);
            }
        }
        side
    }

    pub fn new(start: &Start) -> Self {
        let config = Config {
            scrolling_history: start.history,
            kitty_keyboard: start.kitty,
            ..Config::default()
        };
        let size = TermSize::new(start.columns, start.lines);
        Self {
            term: new_term(config, &size),
            parser: Processor::new(),
            kitty: start.kitty,
            walks: RefCell::default(),
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
            let row = row_at(grid, Line(line));
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
        // One entry per probed search, even if the search panics, so `walk(k)` stays aligned.
        if probe {
            self.walks.borrow_mut().push(None);
        }
        guarded(|| match RegexSearch::new(&search.pattern) {
            Ok(mut regex) => {
                if probe {
                    let (probe, walked) =
                        search_terminates(term, &mut regex, origin, direction, side, max_lines);
                    *self.walks.borrow_mut().last_mut().unwrap() = walked;
                    match probe {
                        Probe::Ends => (),
                        Probe::Never => return QAns::NoTermination,
                        Probe::Panics => return QAns::Panicked,
                    }
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
        self.search_timed(search, max_lines, false, timeout).map(|(answer, _)| answer)
    }

    /// `search` with `probe` on another thread, as `search_detached` runs it: `None` if it has not
    /// returned after `timeout`. `search_grid_probe` runs the oracle side this way, so a 0.26.0
    /// search that never returns (one the probe does not foresee) is classified instead of
    /// hanging the test.
    pub fn search_timed(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        probe: bool,
        timeout: Duration,
    ) -> Option<(QAns, Option<Walk>)> {
        let grid = self.term.grid().clone();
        let size = TermSize::new(grid.columns(), grid.screen_lines());
        let mut term = Term::new(Config::default(), &size, VoidListener);
        *term.grid_mut() = grid;
        let side = Side { term, parser: Processor::new(), kitty: false, walks: RefCell::default() };
        let search = search.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let answer = side.search(&search, max_lines, probe);
            let walked = side.walks.borrow_mut().pop().flatten();
            let _ = tx.send((answer, walked));
        });
        rx.recv_timeout(timeout).ok()
    }

    /// The matches 0.26.0's iterator yielded in the `k`th probed search of the last `queries`.
    pub fn walk(&self, k: usize) -> Option<Walk> {
        self.walks.borrow().get(k).cloned().flatten()
    }

    /// The walk of the last probed search (see `walk`).
    pub fn last_walk(&self) -> Option<Walk> {
        self.walks.borrow().last().cloned().flatten()
    }

    /// The cells after `p` that a rightward search reads or steps over together with it, as
    /// `skip_fullwidth` in feathercritty's `src/term/search.rs` does (`Term::glyph_end` there):
    /// the two cells after a `LEADING_WIDE_CHAR_SPACER` (wrapped: the wide character and its
    /// spacer; stale, even mid-line: the next cell, read in the spacer's place, and the one
    /// after it), or the spacer after a wide character. Clamped to the grid's last cell.
    fn completion_right(&self, p: Point) -> Vec<Point> {
        let grid = self.term.grid();
        let last = Point::new(grid.bottommost_line(), grid.last_column());
        let flags = cell_at(grid, p).flags;
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
        if cell_at(grid, p).flags.contains(Flags::WIDE_CHAR_SPACER) {
            let Some(w) = before(p) else { return cells };
            cells.push(w);
            wide = w;
        }
        if cell_at(grid, wide).flags.contains(Flags::WIDE_CHAR) {
            if let Some(s) = before(wide) {
                if cell_at(grid, s).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
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
    ///   other direction (the second search, back to the start, completes it, as 0.26.0 does);
    /// - the near end comes no later than the far end along the range. A match whose ends both lie
    ///   in the range but in the wrong order ran around the whole buffer: 0.26.0's last search of a
    ///   range, from its end to itself, starting on a leading spacer reads past the end, around the
    ///   buffer and back (differential soak seed `random_streams` 2013725: a rightward search
    ///   answered with a match from (8,1) to (8,0) on a 2-column grid). A match that runs around
    ///   the buffer's end inside the range, as a search that wraps around it reads, keeps its ends
    ///   in order along the range.
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
        // Position along the range: the near completion lies before it, the far one after it.
        let near = match range.iter().position(|&p| p == near) {
            Some(index) => index as isize,
            None if near_completion.contains(&near) => -1,
            None => return false,
        };
        let far = match range.iter().position(|&p| p == far) {
            Some(index) => index as isize,
            None if far_completion.contains(&far) => range.len() as isize,
            None => return false,
        };
        near <= far
    }

    /// What `regex_search_internal` reads at `point` going right and where it lands, following
    /// `skip_fullwidth` in feathercritty's `src/term/search.rs`: a wide character is read and the
    /// search lands on the cell after it (its spacer); at a `LEADING_WIDE_CHAR_SPACER` the next
    /// cell is read in its place and the search lands on the cell after that (for a stale spacer
    /// that next cell is an ordinary one); every other cell, a stray spacer included, is read as
    /// its character and landed on. The skip stops at the grid's last cell, as the grid iterator
    /// does.
    fn read_right(&self, point: Point) -> (char, Point) {
        let grid = self.term.grid();
        let last = Point::new(grid.bottommost_line(), grid.last_column());
        let cell = cell_at(grid, point);
        if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && point < last {
            let next = point.add(grid, Boundary::Grid, 1);
            let landing = if next < last { next.add(grid, Boundary::Grid, 1) } else { next };
            (cell_at(grid, next).c, landing)
        } else if cell.flags.contains(Flags::WIDE_CHAR) && point.column < grid.last_column() {
            (cell.c, point.add(grid, Boundary::Grid, 1))
        } else {
            (cell.c, point)
        }
    }

    /// What the search reads at `point` going left and where it lands, as `skip_fullwidth` does:
    /// at a wide character's spacer the wide character is read and the search lands on it, or on
    /// the `LEADING_WIDE_CHAR_SPACER` just before it; every other cell, a stale leading spacer
    /// included, is read as its character (a stale spacer reads as a blank) and landed on.
    fn read_left(&self, point: Point) -> (char, Point) {
        let grid = self.term.grid();
        let first = Point::new(grid.topmost_line(), Column(0));
        let cell = cell_at(grid, point);
        if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) || point == first {
            return (cell.c, point);
        }
        let wide = point.sub(grid, Boundary::Grid, 1);
        let before = (wide > first).then(|| wide.sub(grid, Boundary::Grid, 1));
        let landing = match before {
            Some(s) if cell_at(grid, s).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => s,
            _ => wide,
        };
        (cell_at(grid, wide).c, landing)
    }

    /// The text `regex_search_internal` reads walking from `from` to `to` in `direction` (in
    /// reading order), or `None` when the search sees a line break on the way, which ends any
    /// match. Modeled on the search loop:
    /// - each character is read with `read_right`/`read_left`, and the next one starts on the cell
    ///   after the landing cell, wrapping around the buffer (from the bottom line's last cell to
    ///   the top line's first and back), as the search does;
    /// - the search sees a line break between two consecutive landing cells `a` and `b` when `a` is
    ///   in the last column, `b` in column 0 and `a` has no `WRAPLINE`, or `a` is in column 0, `b`
    ///   in the last column and `b` has no `WRAPLINE` (both directions check both). A line whose
    ///   last cell is a wide character's spacer and whose next line starts with a wide character
    ///   therefore reads on without a break even when the line does not wrap. The walk from the
    ///   bottom line to the top line is a break unless the bottom line's last cell has `WRAPLINE`.
    ///
    /// 0.26.0 reads the same way: it returns matches across such hidden breaks and, when the
    /// bottom line wraps, matches that run around the buffer's end (start after end in grid
    /// order). If upstream's or feathercritty's reading in `search.rs` changes, update this.
    fn read(&self, from: Point, to: Point, direction: Direction) -> Option<String> {
        let grid = self.term.grid();
        let last_column = grid.last_column();
        let wrapped = |p: Point| cell_at(grid, p).flags.contains(Flags::WRAPLINE);
        let step = |p: Point| match direction {
            Direction::Right => p.add(grid, Boundary::None, 1),
            Direction::Left => p.sub(grid, Boundary::None, 1),
        };

        let mut chars = Vec::new();
        let mut cell = from;
        let mut last_landing: Option<Point> = None;
        for _ in 0..=grid.total_lines() * grid.columns() {
            let (c, landing) = match direction {
                Direction::Right => self.read_right(cell),
                Direction::Left => self.read_left(cell),
            };
            if let Some(a) = last_landing {
                if (a.column == last_column && landing.column == Column(0) && !wrapped(a))
                    || (a.column == Column(0) && landing.column == last_column && !wrapped(landing))
                {
                    return None;
                }
            }
            chars.push(c);

            // Done once `to` is among the cells this read covered.
            let mut p = cell;
            loop {
                if p == to {
                    if direction == Direction::Left {
                        chars.reverse();
                    }
                    return Some(chars.into_iter().collect());
                }
                if p == landing {
                    break;
                }
                p = step(p);
            }
            last_landing = Some(landing);
            cell = step(landing);
        }
        None
    }

    /// Whether the cells from `start` to `end` spell a match of `pattern` the way the search
    /// reads them (`read`; a rightward search finds a match's end reading right and its start
    /// reading back left, so either reading may be the one that matched), with smart case (case
    /// insensitive unless the pattern has an uppercase letter), as `RegexSearch` does. The match
    /// runs from `start` to `end` in reading order, around the buffer's end when `start` lies
    /// after `end`; a reading that sees a line break (`read` answers `None`) is not a match.
    fn is_real_match(&self, pattern: &str, start: Pt, end: Pt) -> Result<(), String> {
        let (start, end) = (self.point(start), self.point(end));
        let case_insensitive = !pattern.chars().any(char::is_uppercase);
        let regex = regex_automata::meta::Regex::builder()
            .syntax(regex_automata::util::syntax::Config::new().case_insensitive(case_insensitive))
            .build(&format!(r"\A(?:{pattern})\z"))
            .map_err(|err| err.to_string())?;
        let right = self.read(start, end, Direction::Right);
        let left = self.read(end, start, Direction::Left);
        let matches = |text: &Option<String>| text.as_ref().is_some_and(|t| regex.is_match(t));
        if matches(&right) || matches(&left) {
            Ok(())
        } else {
            Err(format!(
                "neither reading matches {pattern:?}: right {right:?}, left {left:?} (None: the \
                 search sees a line break, so no match can span it)"
            ))
        }
    }

    /// Check a `search_next` answer without an oracle (used on the feathercritty side where
    /// 0.26.0 cannot answer, fork change 5): it must have terminated, and a match must lie in
    /// the searched range (`match_in_range`) and be a real match of the pattern. `Match(None)` is
    /// accepted (absence is not checked). `check_search_walked` is stricter where 0.26.0's
    /// iteration could be walked.
    pub fn check_search(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        answer: &QAns,
    ) -> Result<(), String> {
        self.check_search_walked(search, max_lines, answer, None)
    }

    /// `check_search`, given 0.26.0's own iteration for this search (`walked`, from
    /// `search_terminates` on the oracle side). Where 0.26.0 has no in-range answer, fork change 5
    /// answers with upstream's choice if that lies in the range, else with the last in-range match
    /// the iteration produced (up to its first repeated match, or up to where it panics), or
    /// `None`; with `walked` the answer must equal that one (`Walk::answer`). The match is compared
    /// exactly, not by its text: 0.26.0's two searches can read a stale spacer or a wide char
    /// without its spacer differently from either single reading (soak seed 10009868,
    /// `search_grid_probe` grid 133174).
    ///
    /// Without `walked` (0.26.0's iteration was not walked: `first_search_misses_end`, or a window
    /// moved for 0.26.0), it is `check_search`.
    pub fn check_search_walked(
        &self,
        search: &SearchQ,
        max_lines: Option<usize>,
        answer: &QAns,
        walked: Option<&Walk>,
    ) -> Result<(), String> {
        let in_range =
            |&(start, end): &(Pt, Pt)| self.match_in_range(search, max_lines, start, end);
        match (answer, walked) {
            (QAns::Match(answer), Some(walked)) => {
                let expected = walked.answer(in_range);
                if *answer == expected {
                    Ok(())
                } else {
                    Err(format!("expected {expected:?} from 0.26.0's iteration (fork change 5)"))
                }
            },
            (QAns::Match(None), None) => Ok(()),
            (QAns::Match(Some(m)), None) => {
                if !in_range(m) {
                    return Err("the match lies outside the searched range".to_owned());
                }
                self.is_real_match(&search.pattern, m.0, m.1)
            },
            (other, _) => Err(format!("expected a search result, got {other:?}")),
        }
    }

    /// Whether a 0.26.0 `search_next` answer is one fork change 5 may change: the search did
    /// not terminate, or its match lies (at least partly) outside the searched range because
    /// 0.26.0 missed the range's end and wrapped around, or its origin leaves the grid
    /// (`origin_leaves_grid`), whatever 0.26.0 answered, or it panicked where `search_terminates`
    /// predicts it (`walked` then ends in [`WalkEnd::Panics`]). Any other panic is compared like
    /// an answer, so a 0.26.0 panic the harness does not know fails the run.
    pub fn oracle_search_is_off(
        &self,
        search: &SearchQ,
        answer: &QAns,
        walked: Option<&Walk>,
    ) -> bool {
        let direction = if search.right { Direction::Right } else { Direction::Left };
        if origin_leaves_grid(&self.term, self.point(search.origin), direction) {
            return true;
        }
        match answer {
            QAns::NoTermination => true,
            // 0.26.0 panics going left from a stale wide char spacer at column 0 (`expand_wide`;
            // soak seed 30001812); feathercritty answers there (fork change 5).
            QAns::Panicked => walked.is_some_and(|walk| walk.end == WalkEnd::Panics),
            QAns::Match(Some((start, end))) => {
                !self.match_in_range(search, search.max_lines, *start, *end)
            },
            _ => false,
        }
    }

    pub fn queries(&mut self, plan: &QueryPlan) -> Vec<(&'static str, QAns)> {
        let mut out = Vec::new();
        self.walks.borrow_mut().clear();

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
            // 0.26.0's motions could leave the grid and the chain stopped there; both crates
            // now keep the cursor in the grid (the oracle through `tests/oracle/PATCHES.md`
            // patch 2), so every motion of the chain is compared.
        }
        let moved = self.term.grid().display_offset();
        if moved != offset {
            self.term.scroll_display(Scroll::Delta(offset as i32 - moved as i32));
        }

        out
    }
}
