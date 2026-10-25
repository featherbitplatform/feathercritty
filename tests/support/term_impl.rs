// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Applies steps to one crate's `Term` and projects it. Compiled twice, as `fc::imp` (with
//! `k` = feathercritty) and `ac::imp` (with `k` = `alacritty_terminal` 0.26.0); both crates
//! expose the same API here.

use std::collections::HashMap;

use super::k;
use k::event::VoidListener;
use k::grid::{Dimensions, Scroll};
use k::index::{Boundary, Column, Direction, Line, Point, Side as KSide};
use k::selection::{Selection, SelectionType};
use k::term::cell::Cell;
use k::term::search::{RegexIter, RegexSearch};
use k::term::test::TermSize;
use k::term::{Config, Term};
use k::vi_mode::{ViModeCursor, ViMotion};
use k::vte::ansi::Processor;

use crate::support::project::{
    CellP, CursorP, LinkP, Panicked, QAns, QueryPlan, Snapshot, VI_MOTIONS,
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

/// Whether `Term::search_next` returns for these arguments.
///
/// Workaround for a 0.26.0 bug: its match iterator (`RegexIter`) can cycle between two matches
/// forever (seen next to a `LEADING_WIDE_CHAR_SPACER` at the end of a line). This walks the same
/// iterator `search_next` walks, with the same start, end and stop condition copied from
/// upstream `next_match_left/right` (re-check them when the oracle moves to a new release), but
/// gives up after more matches than the grid has cells. Remove it once upstream fixes the
/// cycle.
fn search_terminates(
    term: &Term<VoidListener>,
    regex: &mut RegexSearch,
    origin: Point,
    direction: Direction,
    side: KSide,
    max_lines: Option<usize>,
) -> bool {
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

        for (pattern, origin, right, side_right, max_lines) in &plan.searches {
            let direction = if *right { Direction::Right } else { Direction::Left };
            let (origin, side) = (self.point(*origin), side(*side_right));
            let answer = guarded(|| match RegexSearch::new(pattern) {
                Ok(mut regex) => {
                    if search_terminates(term, &mut regex, origin, direction, side, *max_lines) {
                        let found =
                            term.search_next(&mut regex, origin, direction, side, *max_lines);
                        QAns::Match(found.map(|m| (pt(*m.start()), pt(*m.end()))))
                    } else {
                        QAns::NoTermination
                    }
                },
                Err(_) => QAns::Text(None),
            });
            out.push(("search_next", answer));
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
