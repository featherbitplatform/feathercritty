// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

use std::cmp::max;
use std::error::Error;
use std::mem;
use std::ops::RangeInclusive;

use log::{debug, warn};
pub use regex_automata::hybrid::BuildError;
// feathercritty: for `LoopState` (the loop check in `regex_search_internal`).
use regex_automata::hybrid::LazyStateID;
use regex_automata::hybrid::dfa::{Builder, Cache, Config, DFA};
use regex_automata::nfa::thompson::Config as ThompsonConfig;
use regex_automata::util::syntax::Config as SyntaxConfig;
use regex_automata::{Anchored, Input, MatchKind};

use crate::grid::{BidirectionalIterator, Dimensions, GridIterator};
use crate::index::{Boundary, Column, Direction, Point, Side};
use crate::term::Term;
use crate::term::cell::{Cell, Flags};

/// What the regex search reads from a cell (feathercritty: upstream kept a `&Cell` borrowed from
/// the grid; the grid iterator yields owned cells now, and copying these two fields is cheaper
/// than cloning the cell).
#[derive(Clone, Copy)]
struct SearchCell {
    c: char,
    flags: Flags,
}

impl From<&Cell> for SearchCell {
    #[inline]
    fn from(cell: &Cell) -> Self {
        Self { c: cell.c, flags: cell.flags }
    }
}

/// Used to match equal brackets, when performing a bracket-pair selection.
const BRACKET_PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

pub type Match = RangeInclusive<Point>;

/// Terminal regex search state.
#[derive(Clone, Debug)]
pub struct RegexSearch {
    left_fdfa: LazyDfa,
    left_rdfa: LazyDfa,
    right_rdfa: LazyDfa,
    right_fdfa: LazyDfa,
}

impl RegexSearch {
    /// Build the forward and backward search DFAs.
    pub fn new(search: &str) -> Result<RegexSearch, Box<BuildError>> {
        // Setup configs for both DFA directions.
        //
        // Bounds are based on Regex's meta engine:
        // https://github.com/rust-lang/regex/blob/061ee815ef2c44101dba7b0b124600fcb03c1912/regex-automata/src/meta/wrappers.rs#L581-L599
        let has_uppercase = search.chars().any(|c| c.is_uppercase());
        let syntax_config = SyntaxConfig::new().case_insensitive(!has_uppercase);
        let config =
            Config::new().minimum_cache_clear_count(Some(3)).minimum_bytes_per_state(Some(10));
        let max_size = config.get_cache_capacity();
        let thompson_config = ThompsonConfig::new().nfa_size_limit(Some(max_size));

        // Create DFAs to find start/end in right-to-left search.
        let left_rdfa = LazyDfa::new(
            search,
            config.clone(),
            syntax_config,
            thompson_config.clone(),
            Direction::Right,
            true,
        )?;
        let has_empty = left_rdfa.dfa.get_nfa().has_empty();
        let left_fdfa = LazyDfa::new(
            search,
            config.clone(),
            syntax_config,
            thompson_config.clone(),
            Direction::Left,
            has_empty,
        )?;

        // Create DFAs to find start/end in left-to-right search.
        let right_fdfa = LazyDfa::new(
            search,
            config.clone(),
            syntax_config,
            thompson_config.clone(),
            Direction::Right,
            has_empty,
        )?;
        let right_rdfa =
            LazyDfa::new(search, config, syntax_config, thompson_config, Direction::Left, true)?;

        Ok(RegexSearch { left_fdfa, left_rdfa, right_fdfa, right_rdfa })
    }

    /// Whether a search stopped where upstream's would never finish since the last call, and
    /// clear it (feathercritty; see `regex_search_internal` and `RegexIter`).
    fn take_bound_fired(&mut self) -> bool {
        let dfas =
            [&mut self.left_fdfa, &mut self.left_rdfa, &mut self.right_fdfa, &mut self.right_rdfa];
        let mut fired = false;
        for dfa in dfas {
            fired |= mem::take(&mut dfa.bound_fired);
        }
        fired
    }
}

/// Runtime-evaluated DFA.
#[derive(Clone, Debug)]
struct LazyDfa {
    dfa: DFA,
    cache: Cache,
    direction: Direction,
    match_all: bool,
    // feathercritty: set when `regex_search_internal` stopped a search upstream would never
    // finish; read and cleared through `RegexSearch::take_bound_fired`.
    bound_fired: bool,
}

impl LazyDfa {
    fn new(
        search: &str,
        mut config: Config,
        syntax: SyntaxConfig,
        mut thompson: ThompsonConfig,
        direction: Direction,
        match_all: bool,
    ) -> Result<Self, Box<BuildError>> {
        thompson = match direction {
            Direction::Left => thompson.reverse(true),
            Direction::Right => thompson.reverse(false),
        };
        config = if match_all {
            config.match_kind(MatchKind::All)
        } else {
            config.match_kind(MatchKind::LeftmostFirst)
        };

        // Create the DFA.
        let dfa =
            Builder::new().configure(config).syntax(syntax).thompson(thompson).build(search)?;

        let cache = dfa.create_cache();

        // feathercritty: `bound_fired`, see the field.
        Ok(Self { direction, cache, dfa, match_all, bound_fired: false })
    }
}

impl<T> Term<T> {
    /// Get next search match in the specified direction.
    ///
    /// `origin` must lie in the grid (`topmost_line()..=bottommost_line()`, columns below
    /// `columns()`). feathercritty: a line outside the grid reads as "no row" (default cells), so
    /// the search returns instead of panicking, but its answer for such an origin is unspecified.
    ///
    /// feathercritty: the search runs as upstream's does (fork change 5, README). Where upstream
    /// would never return (its iteration loops), panics (going left from a stale wide char spacer
    /// at column 0, `expand_wide_underflows`) or would answer with a match outside the searched
    /// range, this answers with the last match inside the range that the iteration produced, or
    /// `None`; otherwise it answers exactly as upstream.
    pub fn search_next(
        &self,
        regex: &mut RegexSearch,
        mut origin: Point,
        direction: Direction,
        side: Side,
        mut max_lines: Option<usize>,
    ) -> Option<Match> {
        // feathercritty: upstream uses `expand_wide` alone. Going right from a leading spacer it
        // moves to column 1 of the next line, and on the bottommost line (a stale spacer, left in
        // the middle of the line by a resize or an erase) there is no next line: upstream then
        // panics reading below the grid, and with this fork's "no row" reads the search started
        // there and returned the first match of the buffer instead of the next one. We use the
        // cell the search lands on after reading the spacer (`glyph_end`) instead, which stays on
        // the line. Upstream has no answer there, so no result it gives changes.
        // When merging upstream: if `expand_wide` stays inside the grid, drop this; re-run
        // `search_origin_on_bottom_line_stale_spacer`.
        //
        // feathercritty: going left from a wide char spacer at column 0, `expand_wide` subtracts
        // from column 0 (`expand_wide_underflows`): upstream panics here in debug builds, before
        // its search produced any match, so we answer `None` (the answer of rule 3 of fork change
        // 5 with no match). When merging upstream: drop this with `expand_wide_underflows`; re-run
        // `stale_spacer_column_zero_search_next`.
        if self.expand_wide_underflows(origin, direction) {
            return None;
        }
        let expanded = self.expand_wide(origin, direction);
        origin =
            if expanded.line > self.bottommost_line() { self.glyph_end(origin) } else { expanded };

        max_lines = max_lines.filter(|max_lines| max_lines + 1 < self.total_lines());

        match direction {
            Direction::Right => self.next_match_right(regex, origin, side, max_lines),
            Direction::Left => self.next_match_left(regex, origin, side, max_lines),
        }
    }

    /// Find the next match to the right of the origin.
    fn next_match_right(
        &self,
        regex: &mut RegexSearch,
        origin: Point,
        side: Side,
        max_lines: Option<usize>,
    ) -> Option<Match> {
        let start = self.line_search_left(origin);
        let mut end = start;

        // Limit maximum number of lines searched.
        end = match max_lines {
            Some(max_lines) => {
                let line = (start.line + max_lines).grid_clamp(self, Boundary::None);
                Point::new(line, self.last_column())
            },
            _ => end.sub(self, Boundary::None, 1),
        };

        // feathercritty: upstream peeked at the first match and then used `Iterator::find`,
        // falling back to the first match; `final_match` does the same and only changes the
        // answer where upstream had none or answered outside the range.
        let regex_iter = RegexIter::new(start, end, Direction::Right, self, regex);
        let bounded = max_lines.is_some();
        self.final_match(regex_iter, start, end, Direction::Right, bounded, |regex_match| {
            let match_point = Self::match_side(regex_match, side);

            // If the match's point is beyond the origin, we're done.
            match_point.line < start.line
                || match_point.line > origin.line
                || (match_point.line == origin.line && match_point.column >= origin.column)
        })
    }

    /// Find the next match to the left of the origin.
    fn next_match_left(
        &self,
        regex: &mut RegexSearch,
        origin: Point,
        side: Side,
        max_lines: Option<usize>,
    ) -> Option<Match> {
        let start = self.line_search_right(origin);
        let mut end = start;

        // Limit maximum number of lines searched.
        end = match max_lines {
            Some(max_lines) => {
                let line = (start.line - max_lines).grid_clamp(self, Boundary::None);
                Point::new(line, Column(0))
            },
            _ => end.add(self, Boundary::None, 1),
        };

        // feathercritty: see `next_match_right`.
        let regex_iter = RegexIter::new(start, end, Direction::Left, self, regex);
        let bounded = max_lines.is_some();
        self.final_match(regex_iter, start, end, Direction::Left, bounded, |regex_match| {
            let match_point = Self::match_side(regex_match, side);

            // If the match's point is beyond the origin, we're done.
            match_point.line > start.line
                || match_point.line < origin.line
                || (match_point.line == origin.line && match_point.column <= origin.column)
        })
    }

    /// The answer of `search_next`: upstream's (`find` with `done`, falling back to the first
    /// match), unless the iteration hit the bound (`RegexIter::bound_fired`: upstream loops or
    /// panics) or that answer lies outside the searched range (`match_in_range`); then the last
    /// match inside the range that the iteration produced, or `None`. An iteration that upstream
    /// repeats forever counts up to its first repeated match (`last_in_range_before_repeat`), so
    /// the answer does not depend on where the cycle check happened to notice the repeat.
    ///
    /// feathercritty: not in upstream; fork change 5. Upstream wrote
    /// `regex_iter.peekable()`, `peek()?` for the first match and
    /// `find(done).unwrap_or(first_match)`. The matches are upstream's either way; only the
    /// final choice changes, and only where upstream had no answer or an out-of-range one.
    /// When merging upstream: if `next_match_left`/`right` change their choice, change it here.
    fn final_match(
        &self,
        mut regex_iter: RegexIter<'_, T>,
        start: Point,
        end: Point,
        direction: Direction,
        bounded: bool,
        done: impl Fn(&Match) -> bool,
    ) -> Option<Match> {
        let in_range = |regex_match: &Match| {
            !bounded || self.match_in_range(regex_match, start, end, direction)
        };

        let mut first_match = None;
        let mut last_in_range = None;
        let mut found = None;
        for regex_match in regex_iter.by_ref() {
            if in_range(&regex_match) {
                last_in_range = Some(regex_match.clone());
            }
            if done(&regex_match) {
                found = Some(regex_match);
                break;
            }
            if first_match.is_none() {
                first_match = Some(regex_match);
            }
        }

        // Upstream never returns here, or panics (`RegexIter::next`).
        if regex_iter.bound_fired() {
            if let Some(cycle) = regex_iter.cycle {
                // The rerun runs on a copy of the regex, whose cache may give up where this pass
                // did not (it gives up on its fourth clear); then this pass's answer stands.
                let regex = regex_iter.regex.clone();
                return self
                    .last_in_range_before_repeat(regex, start, end, direction, cycle, in_range)
                    .unwrap_or(last_in_range);
            }
            return last_in_range;
        }

        // Check if there's any match at all.
        let regex_match = found.or(first_match)?;
        if in_range(&regex_match) { Some(regex_match) } else { last_in_range }
    }

    /// The last match inside the range among those an iteration yields before its first repeated
    /// match, given the length of the cycle it then repeats.
    ///
    /// feathercritty: not in upstream; see `final_match`. The second half of Brent's cycle
    /// search: one iteration runs `cycle` steps ahead of another, and the first origin they share
    /// is where the repeating origins start. Each origin follows from the match before it (`skip`
    /// from its end), so the matches repeat from the same point or one step earlier: when the
    /// matches just before that shared origin are equal too, the last of the `cycle` matches
    /// after it is already a repeat and is not counted. The answer is then the last in-range match
    /// among the distinct matches the iteration yields, the same cut-off as the differential
    /// test's walk of 0.26.0's iteration (first repeated match). `None` if a search of the rerun
    /// ended early (its cache gave up); `Some(None)` if no match before the repeat lies in the
    /// range.
    fn last_in_range_before_repeat(
        &self,
        mut regex: RegexSearch,
        start: Point,
        end: Point,
        direction: Direction,
        cycle: usize,
        in_range: impl Fn(&Match) -> bool,
    ) -> Option<Option<Match>> {
        let mut ahead_regex = regex.clone();
        let mut iter = RegexIter::new(start, end, direction, self, &mut regex);
        let mut ahead = RegexIter::new(start, end, direction, self, &mut ahead_regex);
        for _ in 0..cycle {
            ahead.next()?;
        }

        let mut last_in_range = None;
        let mut keep = |regex_match: &Match| {
            if in_range(regex_match) {
                last_in_range = Some(regex_match.clone());
            }
        };
        // Whether the matches just before the first shared origin are equal.
        let mut repeated = false;
        while iter.point != ahead.point {
            let regex_match = iter.next()?;
            keep(&regex_match);
            repeated = ahead.next()? == regex_match;
        }
        for _ in 0..cycle - usize::from(repeated) {
            keep(&iter.next()?);
        }
        Some(last_in_range)
    }

    /// Get the side of a match.
    fn match_side(regex_match: &Match, side: Side) -> Point {
        match side {
            Side::Right => *regex_match.end(),
            Side::Left => *regex_match.start(),
        }
    }

    /// Find the next regex match to the left of the origin point.
    ///
    /// The origin is always included in the regex.
    ///
    /// `start` and `end` must lie in the grid (`topmost_line()..=bottommost_line()`, columns below
    /// `columns()`). feathercritty: a line outside the grid reads as
    /// "no row" (default cells) instead of the arbitrary row upstream's release builds read, so
    /// the search returns instead of panicking, but its answer for such points is unspecified.
    /// Where upstream's search would never return, this returns `None`
    /// (`regex_search_internal`); otherwise it returns exactly upstream's match, which can lie
    /// outside `start..=end`.
    pub fn regex_search_left(
        &self,
        regex: &mut RegexSearch,
        start: Point,
        end: Point,
    ) -> Option<Match> {
        // Find start and end of match.
        let match_start = self.regex_search(start, end, &mut regex.left_fdfa)?;
        let match_end = self.regex_search(match_start, start, &mut regex.left_rdfa)?;

        Some(match_start..=match_end)
    }

    /// Find the next regex match to the right of the origin point.
    ///
    /// The origin is always included in the regex.
    ///
    /// `start` and `end` must lie in the grid (`topmost_line()..=bottommost_line()`, columns below
    /// `columns()`). feathercritty: a line outside the grid reads as
    /// "no row" (default cells) instead of the arbitrary row upstream's release builds read, so
    /// the search returns instead of panicking, but its answer for such points is unspecified.
    /// Where upstream's search would never return, this returns `None`
    /// (`regex_search_internal`); otherwise it returns exactly upstream's match, which can lie
    /// outside `start..=end`.
    pub fn regex_search_right(
        &self,
        regex: &mut RegexSearch,
        start: Point,
        end: Point,
    ) -> Option<Match> {
        // Find start and end of match.
        let match_end = self.regex_search(start, end, &mut regex.right_fdfa)?;
        let match_start = self.regex_search(match_end, start, &mut regex.right_rdfa)?;

        Some(match_start..=match_end)
    }

    /// Find the next regex match.
    ///
    /// This will always return the side of the first match which is farthest from the start point.
    fn regex_search(&self, start: Point, end: Point, regex: &mut LazyDfa) -> Option<Point> {
        match self.regex_search_internal(start, end, regex) {
            Ok(regex_match) => regex_match,
            Err(err) => {
                warn!("Regex exceeded complexity limit");
                debug!("    {err}");
                None
            },
        }
    }

    /// Find the next regex match.
    ///
    /// To automatically log regex complexity errors, use [`Self::regex_search`] instead.
    fn regex_search_internal(
        &self,
        start: Point,
        end: Point,
        regex: &mut LazyDfa,
    ) -> Result<Option<Point>, Box<dyn Error>> {
        let topmost_line = self.topmost_line();
        let screen_lines = self.screen_lines() as i32;
        let last_column = self.last_column();

        // Advance the iterator.
        let next = match regex.direction {
            // feathercritty: upstream used `GridIterator::next`/`prev`, which yield a cell; the
            // iterator now yields owned cells, so the loop steps without one and reads what it
            // needs through `iter.cell()` (`SearchCell`).
            Direction::Right => GridIterator::advance,
            Direction::Left => GridIterator::retreat,
        };

        // Get start state for the DFA.
        let regex_anchored = if regex.match_all { Anchored::Yes } else { Anchored::No };
        let input = Input::new(&[]).anchored(regex_anchored);
        let mut state = regex.dfa.start_state_forward(&mut regex.cache, &input).unwrap();

        let mut iter = self.grid.iter_from(start);
        let mut regex_match = None;
        let mut done = false;

        // feathercritty: upstream borrowed `cell` from the grid (`&Cell`); `iter.cell()` borrows
        // from the iterator now, so `cell` keeps the two fields the search reads (`SearchCell`).
        let mut cell = SearchCell::from(iter.cell());
        self.skip_fullwidth(&mut iter, &mut cell, regex.direction);
        let mut c = cell.c;
        let mut last_wrapped = iter.cell().flags.contains(Flags::WRAPLINE);

        let mut point = iter.point();
        let mut last_point = point;
        let mut consumed_bytes = 0;

        // feathercritty: upstream has no bound. It stops only on its end, on a dead state after
        // a match, or at a line break after a match; a fullwidth skip can step over the end
        // without standing on it (a leading spacer, wrapped or stale, next to the end), and the
        // search then reads around the buffer for as long as the text gives it no match, which
        // can be forever. We stop exactly where upstream would loop: every time the search wraps
        // around the buffer it records everything its further course depends on (`LoopState`),
        // and when that repeats, upstream would repeat the same laps forever. Brent's cycle
        // check keeps one saved state. Where upstream stops, this never fires, so its results are
        // unchanged. When it fires, the search answers `None` and records it
        // (`LazyDfa::bound_fired`), so `search_next` can treat the iteration as one upstream never
        // finishes.
        //
        // Why it always fires (termination): the state recorded at a wrap is finite as long as
        // the DFA cache's clear count is bounded, and it is: this loop never reports progress to
        // the cache (`Cache::search_start`/`search_update`), so regex-automata (0.4.11) counts
        // every search as inefficient and gives up on a cache's fourth clear
        // (`minimum_cache_clear_count(3)` with `minimum_bytes_per_state(10)` in
        // `RegexSearch::new`): the search then errors and answers no match, as upstream does.
        // `cache_give_up_ends_a_search` pins this.
        //
        // Cost: it fires only after the recorded state has gone round once, about twice the
        // state's period in laps, and that period follows the pattern. A counted pattern over
        // long wrapped lines keeps the DFA alive with a counter (up to about 1,000 states before
        // the cache gives up): `(?:[ab中]{997})*b` over a 20,000-line buffer of wrapped 100-column
        // lines ran 37 s before the check fired, about 2,000 laps. Upstream never finishes there.
        //
        // When merging upstream: keep the check at the wrap below and `LoopState` in step with
        // what the loop reads; drop it only if upstream bounds this loop itself. Re-check the
        // termination argument (run `cache_give_up_ends_a_search`) if upstream starts reporting
        // progress to the cache, if `RegexSearch::new` changes the cache config, or on a
        // regex-automata update that changes when the cache gives up.
        let mut saved: Option<LoopState> = None;
        let (mut power, mut laps) = (1usize, 0usize);

        // Reset the regex state to restart the search.
        macro_rules! reset_state {
            () => {{
                state = regex.dfa.start_state_forward(&mut regex.cache, &input)?;
                consumed_bytes = 0;
                regex_match = None;
            }};
        }

        'outer: loop {
            // Convert char to array of bytes.
            let mut buf = [0; 4];
            let utf8_len = c.encode_utf8(&mut buf).len();

            // Pass char to DFA as individual bytes.
            for i in 0..utf8_len {
                // Inverse byte order when going left.
                let byte = match regex.direction {
                    Direction::Right => buf[i],
                    Direction::Left => buf[utf8_len - i - 1],
                };

                state = regex.dfa.next_state(&mut regex.cache, state, byte)?;
                consumed_bytes += 1;

                if i == 0 && state.is_match() {
                    // Matches require one additional BYTE of lookahead, so we check the match state
                    // for the first byte of every new character to determine if the last character
                    // was a match.
                    regex_match = Some(last_point);
                } else if state.is_dead() {
                    if consumed_bytes == 2 {
                        // Reset search if we found an empty match.
                        //
                        // With an unanchored search, a dead state only occurs after the end of a
                        // match has been found. While we want to abort after the first match has
                        // ended, we don't want empty matches since we cannot highlight them.
                        //
                        // So once we encounter an empty match, we reset our parser state and clear
                        // the match, effectively starting a new search one character farther than
                        // before.
                        //
                        // An empty match requires consuming `2` bytes, since the first byte will
                        // report the match for the empty string, while the second byte then
                        // reports the dead state indicating the first character isn't part of the
                        // match.
                        reset_state!();

                        // Retry this character if first byte caused failure.
                        //
                        // After finding an empty match, we want to advance the search start by one
                        // character. So if the first character has multiple bytes and the dead
                        // state isn't reached at `i == 0`, then we continue with the rest of the
                        // loop to advance the parser by one character.
                        if i == 0 {
                            continue 'outer;
                        }
                    } else {
                        // Abort on dead state.
                        break 'outer;
                    }
                }
            }

            // Stop once we've reached the target point.
            if point == end || done {
                // When reaching the end-of-input, we need to notify the parser that no look-ahead
                // is possible and check for state changes.
                state = regex.dfa.next_eoi_state(&mut regex.cache, state)?;
                if state.is_match() {
                    regex_match = Some(point);
                } else if state.is_dead() && consumed_bytes == 1 {
                    // Ignore empty matches.
                    regex_match = None;
                }

                break;
            }

            // Advance grid cell iterator.
            // feathercritty: `next` steps without producing a cell (see above); upstream took the
            // cell from its result or, after wrapping, from `iter.cell()`. Both read it below.
            if !next(&mut iter) {
                // feathercritty: the loop check described at `saved` above.
                let now = LoopState {
                    point,
                    last_wrapped,
                    state,
                    consumed_bytes: consumed_bytes.min(3),
                    regex_match,
                    cache_clears: regex.cache.clear_count(),
                };
                if saved == Some(now) {
                    regex.bound_fired = true;
                    return Ok(None);
                }
                laps += 1;
                if laps == power {
                    saved = Some(now);
                    power *= 2;
                    laps = 0;
                }

                // Wrap around to other end of the scrollback buffer.
                let line = topmost_line - point.line + screen_lines - 1;
                let start = Point::new(line, last_column - point.column);
                iter = self.grid.iter_from(start);
            }
            let mut cell = SearchCell::from(iter.cell());

            // Check for completion before potentially skipping over fullwidth characters.
            done = iter.point() == end;

            self.skip_fullwidth(&mut iter, &mut cell, regex.direction);

            c = cell.c;
            let wrapped = iter.cell().flags.contains(Flags::WRAPLINE);

            last_point = mem::replace(&mut point, iter.point());

            // Handle linebreaks.
            if (last_point.column == last_column && point.column == Column(0) && !last_wrapped)
                || (last_point.column == Column(0) && point.column == last_column && !wrapped)
            {
                // When reaching the end-of-input, we need to notify the parser that no
                // look-ahead is possible and check if the current state is still a match.
                state = regex.dfa.next_eoi_state(&mut regex.cache, state)?;
                if state.is_match() {
                    regex_match = Some(last_point);
                }

                match regex_match {
                    // Stop if we found a non-empty match before the linebreak.
                    Some(_) if (!state.is_dead() || consumed_bytes > 1) && consumed_bytes != 0 => {
                        break;
                    },
                    _ => reset_state!(),
                }
            }

            last_wrapped = wrapped;
        }

        Ok(regex_match)
    }

    /// Number of cells from `from` to `to`, walking in `direction` and wrapping around the
    /// buffer like the search does; 0 when they are equal.
    ///
    /// feathercritty: not in upstream; see `match_in_range`.
    fn search_distance(&self, from: Point, to: Point, direction: Direction) -> usize {
        let columns = self.columns() as i64;
        let total = self.total_lines() as i64 * columns;
        let topmost_line = self.topmost_line().0 as i64;
        let index =
            |point: Point| (point.line.0 as i64 - topmost_line) * columns + point.column.0 as i64;

        let distance = match direction {
            Direction::Right => index(to) - index(from),
            Direction::Left => index(from) - index(to),
        };

        distance.rem_euclid(total.max(1)) as usize
    }

    /// Whether `regex_match` lies in the range `search_next` searched, from `start` to `end` in
    /// `direction` (wrapping around the buffer as the search does):
    /// - its far end (its end going right, its start going left) lies in the range or in the cells
    ///   the search reads together with the character at `end` (`glyph_cells`: going right, the
    ///   cells after a leading spacer or a wide char; going left, the wide char of a spacer and a
    ///   leading spacer before a wrapped wide char);
    /// - its near end lies in the range or in the cells read with the character at `start` in the
    ///   other direction (the second search, back to the start, reads it whole);
    /// - its near end comes no later than its far end along the range. A match with both ends in
    ///   the range in the wrong order ran around the whole buffer.
    ///
    /// The differential test classifies 0.26.0's answers with the same rules
    /// (`tests/support/term_impl.rs`, `Side::match_in_range`); keep the two in step.
    ///
    /// feathercritty: not in upstream; see `final_match`.
    fn match_in_range(
        &self,
        regex_match: &Match,
        start: Point,
        end: Point,
        direction: Direction,
    ) -> bool {
        let end_distance = self.search_distance(start, end, direction) as isize;
        let reverse = match direction {
            Direction::Right => Direction::Left,
            Direction::Left => Direction::Right,
        };
        let position = |point: Point, completion: Point, past: isize, completion_direction| {
            let distance = self.search_distance(start, point, direction) as isize;
            if distance <= end_distance {
                Some(distance)
            } else {
                self.glyph_cells(completion, completion_direction).contains(&point).then_some(past)
            }
        };

        let (near, far) = match direction {
            Direction::Right => (*regex_match.start(), *regex_match.end()),
            Direction::Left => (*regex_match.end(), *regex_match.start()),
        };
        let near = position(near, start, -1, reverse);
        let far = position(far, end, end_distance + 1, direction);
        matches!((near, far), (Some(near), Some(far)) if near <= far)
    }

    /// The cells the search reads or steps over together with the character at `point`, going
    /// in `direction` (at most two):
    /// - right: the two cells after a leading spacer (a wrapped wide char and its spacer; for a
    ///   stale spacer, the next cell, read in its place, and the one after it), or the spacer after
    ///   a wide char;
    /// - left: the wide char before a spacer, and a leading spacer before a wrapped wide char.
    ///
    /// Clamped to the grid (skips do not wrap around the buffer).
    ///
    /// feathercritty: not in upstream; see `match_in_range`.
    fn glyph_cells(&self, point: Point, direction: Direction) -> Vec<Point> {
        // feathercritty (compact history): points can be in history, read through `Grid::cell`.
        let flags = |point: Point| self.grid.cell(point).flags;
        let first = Point::new(self.topmost_line(), Column(0));
        let last = Point::new(self.bottommost_line(), self.last_column());
        let mut cells = Vec::new();
        match direction {
            Direction::Right => {
                let count = if flags(point).contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    2
                } else if flags(point).contains(Flags::WIDE_CHAR)
                    && point.column < self.last_column()
                {
                    1
                } else {
                    0
                };
                let mut point = point;
                for _ in 0..count {
                    if point == last {
                        break;
                    }
                    point = point.add(self, Boundary::Grid, 1);
                    cells.push(point);
                }
            },
            Direction::Left => {
                let mut wide = point;
                if flags(point).contains(Flags::WIDE_CHAR_SPACER) {
                    if point == first {
                        return cells;
                    }
                    wide = point.sub(self, Boundary::Grid, 1);
                    cells.push(wide);
                }
                if flags(wide).contains(Flags::WIDE_CHAR) && wide > first {
                    let before = wide.sub(self, Boundary::Grid, 1);
                    if flags(before).contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                        cells.push(before);
                    }
                }
            },
        }
        cells
    }

    /// The cell where a rightward search lands after reading the character at `point`: the
    /// spacer of a wide char, the second cell after a leading spacer (stale or not, as
    /// `skip_fullwidth` steps), and `point` itself otherwise, clamped to the grid's last cell.
    ///
    /// feathercritty: not in upstream; see `search_next`.
    fn glyph_end(&self, point: Point) -> Point {
        self.glyph_cells(point, Direction::Right).last().copied().unwrap_or(point)
    }

    /// Whether `expand_wide` at `point` going `direction` subtracts from column 0: going left from
    /// a wide char spacer at column 0 (a stale one, left behind by a resize or an erase), it moves
    /// one column back to the spacer's wide char. 0.26.0 panics there in debug builds (`Column`
    /// overflow) and goes on from a wrapped-around column in release builds.
    ///
    /// feathercritty: not in upstream; see `search_next` and `RegexIter::next`. When merging
    /// upstream: if `expand_wide` handles column 0 itself, drop this and its two uses.
    fn expand_wide_underflows(&self, point: Point, direction: Direction) -> bool {
        direction == Direction::Left
            && point.column == Column(0)
            && self.grid.cell(point).flags.contains(Flags::WIDE_CHAR_SPACER)
    }

    /// Advance a grid iterator over fullwidth characters.
    ///
    /// feathercritty: upstream took `cell: &mut &'a Cell` borrowed from the grid; `cell` is a
    /// `SearchCell` now, replaced the same way, and the iterator steps without producing cells.
    fn skip_fullwidth(
        &self,
        iter: &mut GridIterator<'_, Cell>,
        cell: &mut SearchCell,
        direction: Direction,
    ) {
        match direction {
            // In the alternate screen buffer there might not be a wide char spacer after a wide
            // char, so we only advance the iterator when the wide char is not in the last column.
            Direction::Right
                if cell.flags.contains(Flags::WIDE_CHAR)
                    && iter.point().column < self.last_column() =>
            {
                iter.advance();
            },
            Direction::Right if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                if iter.advance() {
                    *cell = SearchCell::from(iter.cell());
                }
                iter.advance();
            },
            Direction::Left if cell.flags.contains(Flags::WIDE_CHAR_SPACER) => {
                if iter.retreat() {
                    *cell = SearchCell::from(iter.cell());
                }

                // feathercritty: upstream `self.grid[prev]`; `prev` can be in history.
                let prev = iter.point().sub(self, Boundary::Grid, 1);
                if self.grid.cell(prev).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    iter.prev();
                }
            },
            _ => (),
        }
    }

    /// Find next matching bracket.
    pub fn bracket_search(&self, point: Point) -> Option<Point> {
        // feathercritty: upstream `self.grid[point]`; `point` can be in history. The iterator
        // below yields owned cells.
        let start_char = self.grid.cell(point).c;

        // Find the matching bracket we're looking for
        let (forward, end_char) = BRACKET_PAIRS.iter().find_map(|(open, close)| {
            if open == &start_char {
                Some((true, *close))
            } else if close == &start_char {
                Some((false, *open))
            } else {
                None
            }
        })?;

        let mut iter = self.grid.iter_from(point);

        // For every character match that equals the starting bracket, we
        // ignore one bracket of the opposite type.
        let mut skip_pairs = 0;

        loop {
            // Check the next cell
            let cell = if forward { iter.next() } else { iter.prev() };

            // Break if there are no more cells
            let cell = match cell {
                Some(cell) => cell,
                None => break,
            };

            // Check if the bracket matches
            if cell.c == end_char && skip_pairs == 0 {
                return Some(cell.point);
            } else if cell.c == start_char {
                skip_pairs += 1;
            } else if cell.c == end_char {
                skip_pairs -= 1;
            }
        }

        None
    }

    /// Find left end of semantic block.
    #[must_use]
    pub fn semantic_search_left(&self, point: Point) -> Point {
        match self.inline_search_left(point, self.semantic_escape_chars()) {
            // If we found a match, reverse for at least one cell, skipping over wide cell spacers.
            Ok(point) => {
                let wide_spacer = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
                self.grid
                    .iter_from(point)
                    .find(|cell| !cell.flags.intersects(wide_spacer))
                    .map_or(point, |cell| cell.point)
            },
            Err(point) => point,
        }
    }

    /// Find right end of semantic block.
    #[must_use]
    pub fn semantic_search_right(&self, point: Point) -> Point {
        match self.inline_search_right(point, self.semantic_escape_chars()) {
            Ok(point) => self.grid.iter_from(point).prev().map_or(point, |cell| cell.point),
            Err(point) => point,
        }
    }

    /// Searching to the left, find the next character contained in `needles`.
    pub fn inline_search_left(&self, mut point: Point, needles: &str) -> Result<Point, Point> {
        // Limit the starting point to the last line in the history
        point.line = max(point.line, self.topmost_line());

        let mut iter = self.grid.iter_from(point);
        let last_column = self.columns() - 1;

        let wide_spacer = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        while let Some(cell) = iter.prev() {
            if cell.point.column == last_column && !cell.flags.contains(Flags::WRAPLINE) {
                break;
            }

            point = cell.point;

            if !cell.flags.intersects(wide_spacer) && needles.contains(cell.c) {
                return Ok(point);
            }
        }

        Err(point)
    }

    /// Searching to the right, find the next character contained in `needles`.
    pub fn inline_search_right(&self, mut point: Point, needles: &str) -> Result<Point, Point> {
        // Limit the starting point to the last line in the history
        point.line = max(point.line, self.topmost_line());

        let wide_spacer = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        let last_column = self.columns() - 1;

        // Immediately stop if start point in on line break.
        //
        // feathercritty: upstream `self.grid[point]`; `point` can be in history.
        if point.column == last_column && !self.grid.cell(point).flags.contains(Flags::WRAPLINE) {
            return Err(point);
        }

        for cell in self.grid.iter_from(point) {
            point = cell.point;

            if !cell.flags.intersects(wide_spacer) && needles.contains(cell.c) {
                return Ok(point);
            }

            if point.column == last_column && !cell.flags.contains(Flags::WRAPLINE) {
                break;
            }
        }

        Err(point)
    }

    /// Find the beginning of the current line across linewraps.
    pub fn line_search_left(&self, mut point: Point) -> Point {
        // feathercritty: upstream read `self.grid[line][self.last_column()].flags` for
        // `WRAPLINE`; lines can be in history, read through `Grid::wraps`.
        while point.line > self.topmost_line() && self.grid.wraps(point.line - 1i32) {
            point.line -= 1;
        }

        point.column = Column(0);

        point
    }

    /// Find the end of the current line across linewraps.
    pub fn line_search_right(&self, mut point: Point) -> Point {
        // feathercritty: upstream read `self.grid[line][self.last_column()].flags` for
        // `WRAPLINE`; lines can be in history, read through `Grid::wraps`.
        while point.line + 1 < self.screen_lines() && self.grid.wraps(point.line) {
            point.line += 1;
        }

        point.column = self.last_column();

        point
    }
}

/// What `regex_search_internal`'s further course depends on when it wraps around the buffer.
///
/// feathercritty: not in upstream; the loop check in `regex_search_internal`. `consumed_bytes`
/// is only compared with 0, 1 and 2, so it is kept up to 3. The cache's clear count is part of it
/// because the DFA's state ids are only stable between two cache clears. The check depends on
/// that count staying small: the search never reports progress to the cache, so regex-automata
/// gives up on the fourth clear of a `LazyDfa`'s cache (see `regex_search_internal`); with
/// progress reporting or another give-up rule the count could grow without bound, the state would
/// never repeat, and a search upstream never finishes would never finish here either.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LoopState {
    point: Point,
    last_wrapped: bool,
    state: LazyStateID,
    consumed_bytes: usize,
    regex_match: Option<Point>,
    cache_clears: usize,
}

/// Iterator over regex matches.
///
/// feathercritty: it yields exactly upstream's matches, in upstream's order, out-of-range ones
/// included. Where upstream's iteration would never end (it returns to an origin it already
/// searched from, or one of its searches would never return), or where upstream panics moving
/// its origin past a match (`Term::expand_wide_underflows`), it ends instead and `bound_fired`
/// says so.
pub struct RegexIter<'a, T> {
    point: Point,
    end: Point,
    direction: Direction,
    regex: &'a mut RegexSearch,
    term: &'a Term<T>,
    done: bool,
    // feathercritty: the cycle check in `next` (Brent: one saved origin, compared with each new
    // one, replaced after 1, 2, 4, ... searches), whether it, a search's own check or the stop
    // where upstream panics fired, and the length of the cycle it found.
    saved: Option<Point>,
    power: usize,
    steps: usize,
    bound_fired: bool,
    cycle: Option<usize>,
}

impl<'a, T> RegexIter<'a, T> {
    pub fn new(
        start: Point,
        end: Point,
        direction: Direction,
        term: &'a Term<T>,
        regex: &'a mut RegexSearch,
    ) -> Self {
        // feathercritty: clear a bound recorded by searches outside this iteration, and start
        // the cycle check (`next`). Upstream only stored the fields it uses.
        regex.take_bound_fired();
        Self {
            point: start,
            done: false,
            end,
            direction,
            term,
            regex,
            saved: None,
            power: 1,
            steps: 0,
            bound_fired: false,
            cycle: None,
        }
    }

    /// Whether the iteration ended where upstream's would never end, or where upstream panics
    /// (feathercritty; `Term::expand_wide_underflows`).
    pub fn bound_fired(&self) -> bool {
        self.bound_fired
    }

    /// Skip one cell, advancing the origin point to the next one.
    fn skip(&mut self) {
        self.point = self.term.expand_wide(self.point, self.direction);

        self.point = match self.direction {
            Direction::Right => self.point.add(self.term, Boundary::None, 1),
            Direction::Left => self.point.sub(self.term, Boundary::None, 1),
        };
    }

    /// Get the next match in the specified direction.
    fn next_match(&mut self) -> Option<Match> {
        match self.direction {
            Direction::Right => self.term.regex_search_right(self.regex, self.point, self.end),
            Direction::Left => self.term.regex_search_left(self.regex, self.point, self.end),
        }
    }
}

impl<T> Iterator for RegexIter<'_, T> {
    type Item = Match;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        // feathercritty: upstream has no bound. Each step depends only on its origin, so an
        // origin it already searched from means it repeats the same matches forever (next to a
        // leading spacer, wrapped or stale, a match can end on or past `end`, or behind its own
        // origin, and upstream never meets its stop condition). We end the iteration there.
        // Brent's cycle check finds the repeat with one saved origin; where upstream ends, it
        // never fires, so upstream's matches are unchanged.
        // When merging upstream: drop this only if upstream bounds `RegexIter` itself.
        if self.saved == Some(self.point) {
            self.done = true;
            self.bound_fired = true;
            // The saved origin is first met again after one full cycle; `steps` counted the
            // searches in between.
            self.cycle = Some(self.steps + 1);
            return None;
        }
        self.steps += 1;
        if self.steps == self.power {
            self.saved = Some(self.point);
            self.power *= 2;
            self.steps = 0;
        }

        // Since the end itself might be a single cell match, we search one more time.
        if self.point == self.end {
            self.done = true;
        }

        // feathercritty: upstream wrote `let regex_match = self.next_match()?;`. A search that
        // upstream would never finish answers `None` and is recorded (`regex_search_internal`);
        // it ends the iteration here too.
        let Some(regex_match) = self.next_match() else {
            if self.regex.take_bound_fired() {
                self.bound_fired = true;
            }
            self.done = true;
            return None;
        };

        self.point = *regex_match.end();
        if self.point == self.end {
            // Stop when the match terminates right on the end limit.
            self.done = true;
        } else if self.term.expand_wide_underflows(self.point, self.direction) {
            // feathercritty: upstream calls `skip` here in every case. A leftward match ending on
            // a stale wide char spacer at column 0 makes `expand_wide` subtract from column 0
            // (`Term::expand_wide_underflows`): upstream panics in debug builds before yielding
            // this match, and goes on from a wrapped-around column in release builds. We end the
            // iteration where it panics, without the match, and record it like the bounds, so
            // `search_next` answers with the last in-range match yielded before (rule 3 of fork
            // change 5). When merging upstream: drop this branch if `expand_wide` handles column
            // 0; re-run `stale_spacer_column_zero_left_iteration`.
            self.done = true;
            self.bound_fired = true;
            return None;
        } else {
            // Move the new search origin past the match.
            self.skip();
        }

        Some(regex_match)
    }
}

// feathercritty: upstream's tests indexed `Grid` directly; screen writes go through
// `Grid::screen_mut()`/`screen()`, history writes through `Grid::update_line`.
#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    use std::{iter, thread};

    use crate::event::VoidListener;
    use crate::index::{Column, Line};
    use crate::term::Config;
    use crate::term::test::{TermSize, mock_term};

    #[test]
    fn regex_right() {
        #[rustfmt::skip]
        let term = mock_term("\
            testing66\r\n\
            Alacritty\n\
            123\r\n\
            Alacritty\r\n\
            123\
        ");

        // Check regex across wrapped and unwrapped lines.
        let mut regex = RegexSearch::new("Ala.*123").unwrap();
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(4), Column(2));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(2), Column(2));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn regex_left() {
        #[rustfmt::skip]
        let term = mock_term("\
            testing66\r\n\
            Alacritty\n\
            123\r\n\
            Alacritty\r\n\
            123\
        ");

        // Check regex across wrapped and unwrapped lines.
        let mut regex = RegexSearch::new("Ala.*123").unwrap();
        let start = Point::new(Line(4), Column(2));
        let end = Point::new(Line(1), Column(0));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(2), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn nested_regex() {
        #[rustfmt::skip]
        let term = mock_term("\
            Ala -> Alacritty -> critty\r\n\
            critty\
        ");

        // Greedy stopped at linebreak.
        let mut regex = RegexSearch::new("Ala.*critty").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(25));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));

        // Greedy stopped at dead state.
        let mut regex = RegexSearch::new("Ala[^y]*critty").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(15));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));
    }

    #[test]
    fn no_match_right() {
        #[rustfmt::skip]
        let term = mock_term("\
            first line\n\
            broken second\r\n\
            third\
        ");

        let mut regex = RegexSearch::new("nothing").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(2), Column(4));
        assert_eq!(term.regex_search_right(&mut regex, start, end), None);
    }

    #[test]
    fn no_match_left() {
        #[rustfmt::skip]
        let term = mock_term("\
            first line\n\
            broken second\r\n\
            third\
        ");

        let mut regex = RegexSearch::new("nothing").unwrap();
        let start = Point::new(Line(2), Column(4));
        let end = Point::new(Line(0), Column(0));
        assert_eq!(term.regex_search_left(&mut regex, start, end), None);
    }

    #[test]
    fn include_linebreak_left() {
        #[rustfmt::skip]
        let term = mock_term("\
            testing123\r\n\
            xxx\
        ");

        // Make sure the cell containing the linebreak is not skipped.
        let mut regex = RegexSearch::new("te.*123").unwrap();
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(0), Column(0));
        let match_end = Point::new(Line(0), Column(9));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn include_linebreak_right() {
        #[rustfmt::skip]
        let term = mock_term("\
            xxx\r\n\
            testing123\
        ");

        // Make sure the cell containing the linebreak is not skipped.
        let mut regex = RegexSearch::new("te.*123").unwrap();
        let start = Point::new(Line(0), Column(2));
        let end = Point::new(Line(1), Column(9));
        let match_start = Point::new(Line(1), Column(0));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=end));
    }

    #[test]
    fn skip_dead_cell() {
        let term = mock_term("alacritty");

        // Make sure dead state cell is skipped when reversing.
        let mut regex = RegexSearch::new("alacrit").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(6));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));
    }

    #[test]
    fn reverse_search_dead_recovery() {
        let term = mock_term("zooo lense");

        // Make sure the reverse DFA operates the same as a forward DFA.
        let mut regex = RegexSearch::new("zoo").unwrap();
        let start = Point::new(Line(0), Column(9));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(0), Column(0));
        let match_end = Point::new(Line(0), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn multibyte_unicode() {
        let term = mock_term("testвосибing");

        let mut regex = RegexSearch::new("te.*ing").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(11));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));

        let mut regex = RegexSearch::new("te.*ing").unwrap();
        let start = Point::new(Line(0), Column(11));
        let end = Point::new(Line(0), Column(0));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(end..=start));
    }

    #[test]
    fn end_on_multibyte_unicode() {
        let term = mock_term("testвосиб");

        let mut regex = RegexSearch::new("te.*и").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(8));
        let match_end = Point::new(Line(0), Column(7));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=match_end));
    }

    #[test]
    fn fullwidth() {
        let term = mock_term("a🦇x🦇");

        let mut regex = RegexSearch::new("[^ ]*").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(5));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));

        let mut regex = RegexSearch::new("[^ ]*").unwrap();
        let start = Point::new(Line(0), Column(5));
        let end = Point::new(Line(0), Column(0));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(end..=start));
    }

    #[test]
    fn singlecell_fullwidth() {
        let term = mock_term("🦇");

        let mut regex = RegexSearch::new("🦇").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(1));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));

        let mut regex = RegexSearch::new("🦇").unwrap();
        let start = Point::new(Line(0), Column(1));
        let end = Point::new(Line(0), Column(0));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(end..=start));
    }

    #[test]
    fn end_on_fullwidth() {
        let term = mock_term("jarr🦇");

        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(4));

        // Ensure ending without a match doesn't loop indefinitely.
        let mut regex = RegexSearch::new("x").unwrap();
        assert_eq!(term.regex_search_right(&mut regex, start, end), None);

        let mut regex = RegexSearch::new("x").unwrap();
        let match_end = Point::new(Line(0), Column(5));
        assert_eq!(term.regex_search_right(&mut regex, start, match_end), None);

        // Ensure match is captured when only partially inside range.
        let mut regex = RegexSearch::new("jarr🦇").unwrap();
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=match_end));
    }

    #[test]
    fn wrapping() {
        #[rustfmt::skip]
        let term = mock_term("\
            xxx\r\n\
            xxx\
        ");

        let mut regex = RegexSearch::new("xxx").unwrap();
        let start = Point::new(Line(0), Column(2));
        let end = Point::new(Line(1), Column(2));
        let match_start = Point::new(Line(1), Column(0));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=end));

        let mut regex = RegexSearch::new("xxx").unwrap();
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(0), Column(0));
        let match_end = Point::new(Line(0), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(end..=match_end));
    }

    #[test]
    fn wrapping_into_fullwidth() {
        #[rustfmt::skip]
        let term = mock_term("\
            🦇xx\r\n\
            xx🦇\
        ");

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(3));
        let match_start = Point::new(Line(0), Column(0));
        let match_end = Point::new(Line(0), Column(2));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("x🦇").unwrap();
        let start = Point::new(Line(1), Column(2));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(1), Column(1));
        let match_end = Point::new(Line(1), Column(3));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn multiline() {
        #[rustfmt::skip]
        let term = mock_term("\
            test \r\n\
            test\
        ");

        const PATTERN: &str = "[a-z]*";
        let mut regex = RegexSearch::new(PATTERN).unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(3));
        let match_start = Point::new(Line(0), Column(0));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=end));

        let mut regex = RegexSearch::new(PATTERN).unwrap();
        let start = Point::new(Line(0), Column(4));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(1), Column(3));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn empty_match() {
        #[rustfmt::skip]
        let term = mock_term(" abc ");

        const PATTERN: &str = "[a-z]*";
        let mut regex = RegexSearch::new(PATTERN).unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(4));
        let match_start = Point::new(Line(0), Column(1));
        let match_end = Point::new(Line(0), Column(3));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn empty_match_multibyte() {
        #[rustfmt::skip]
        let term = mock_term(" ↑");

        const PATTERN: &str = "[a-z]*";
        let mut regex = RegexSearch::new(PATTERN).unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(1));
        assert_eq!(term.regex_search_right(&mut regex, start, end), None);
    }

    #[test]
    fn empty_match_multiline() {
        #[rustfmt::skip]
        let term = mock_term("abc          \nxxx");

        const PATTERN: &str = "[a-z]*";
        let mut regex = RegexSearch::new(PATTERN).unwrap();
        let start = Point::new(Line(0), Column(3));
        let end = Point::new(Line(1), Column(2));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn leading_spacer() {
        #[rustfmt::skip]
        let mut term = mock_term("\
            xxx \n\
            🦇xx\
        ");
        term.grid.screen_mut()[Line(0)][Column(3)].flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(3));
        let match_start = Point::new(Line(0), Column(3));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(1), Column(3));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(0), Column(3));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("x🦇").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(3));
        let match_start = Point::new(Line(0), Column(2));
        let match_end = Point::new(Line(1), Column(1));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("x🦇").unwrap();
        let start = Point::new(Line(1), Column(3));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(0), Column(2));
        let match_end = Point::new(Line(1), Column(1));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn wide_without_spacer() {
        let size = TermSize::new(2, 2);
        let mut term = Term::new(Config::default(), &size, ());
        term.grid.screen_mut()[Line(0)][Column(0)].c = 'x';
        term.grid.screen_mut()[Line(0)][Column(1)].c = '字';
        term.grid.screen_mut()[Line(0)][Column(1)].flags = Flags::WIDE_CHAR;

        let mut regex = RegexSearch::new("test").unwrap();

        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(1));

        let mut iter = RegexIter::new(start, end, Direction::Right, &term, &mut regex);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn wrap_around_to_another_end() {
        #[rustfmt::skip]
        let term = mock_term("\
            abc\r\n\
            def\
        ");

        // Bottom to top.
        let mut regex = RegexSearch::new("abc").unwrap();
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(0), Column(2));
        let match_start = Point::new(Line(0), Column(0));
        let match_end = Point::new(Line(0), Column(2));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        // Top to bottom.
        let mut regex = RegexSearch::new("def").unwrap();
        let start = Point::new(Line(0), Column(2));
        let end = Point::new(Line(1), Column(0));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn nfa_compile_error() {
        assert!(RegexSearch::new("[0-9A-Za-z]{9999999}").is_err());
    }

    #[test]
    fn runtime_cache_error() {
        let term = mock_term(&str::repeat("i", 9999));

        let mut regex = RegexSearch::new("[0-9A-Za-z]{9999}").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(9999));
        assert_eq!(term.regex_search_right(&mut regex, start, end), None);
    }

    #[test]
    fn greed_is_good() {
        #[rustfmt::skip]
        let term = mock_term("https://github.com");

        // Bottom to top.
        let mut regex = RegexSearch::new("/github.com|https://github.com").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(17));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));
    }

    #[test]
    fn anchored_empty() {
        #[rustfmt::skip]
        let term = mock_term("rust");

        // Bottom to top.
        let mut regex = RegexSearch::new(";*|rust").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(3));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(start..=end));
    }

    #[test]
    fn newline_breaking_semantic() {
        #[rustfmt::skip]
        let term = mock_term("\
            test abc\r\n\
            def test\
        ");

        // Start at last character.
        let start = term.semantic_search_left(Point::new(Line(0), Column(7)));
        let end = term.semantic_search_right(Point::new(Line(0), Column(7)));
        assert_eq!(start, Point::new(Line(0), Column(5)));
        assert_eq!(end, Point::new(Line(0), Column(7)));

        // Start at first character.
        let start = term.semantic_search_left(Point::new(Line(1), Column(0)));
        let end = term.semantic_search_right(Point::new(Line(1), Column(0)));
        assert_eq!(start, Point::new(Line(1), Column(0)));
        assert_eq!(end, Point::new(Line(1), Column(2)));
    }

    #[test]
    fn inline_word_search() {
        #[rustfmt::skip]
        let term = mock_term("\
            word word word word w\n\
            ord word word word\
        ");

        let mut regex = RegexSearch::new("word").unwrap();
        let start = Point::new(Line(1), Column(4));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(0), Column(20));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_start..=match_end));
    }

    #[test]
    fn fullwidth_semantic() {
        #[rustfmt::skip]
        let mut term = mock_term("test－x－test");
        term.config.semantic_escape_chars = "－".into();

        let start = term.semantic_search_left(Point::new(Line(0), Column(6)));
        let end = term.semantic_search_right(Point::new(Line(0), Column(6)));
        assert_eq!(start, Point::new(Line(0), Column(6)));
        assert_eq!(end, Point::new(Line(0), Column(6)));
    }

    #[test]
    fn fullwidth_across_lines() {
        let term = mock_term("a🦇\n🦇b");

        let mut regex = RegexSearch::new("🦇🦇").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(2));
        let match_start = Point::new(Line(0), Column(1));
        let match_end = Point::new(Line(1), Column(1));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("🦇🦇").unwrap();
        let start = Point::new(Line(1), Column(2));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(1), Column(1));
        let match_end = Point::new(Line(0), Column(1));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_end..=match_start));
    }

    #[test]
    fn fullwidth_into_halfwidth_across_lines() {
        let term = mock_term("a🦇\nxab");

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(2));
        let match_start = Point::new(Line(0), Column(1));
        let match_end = Point::new(Line(1), Column(0));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(1), Column(2));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(0), Column(1));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_end..=match_start));
    }

    #[test]
    fn no_spacer_fullwidth_linewrap() {
        let mut term = mock_term("abY\nxab");
        term.grid_mut().screen_mut()[Line(0)][Column(2)].c = '🦇';

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(1), Column(2));
        let match_start = Point::new(Line(0), Column(2));
        let match_end = Point::new(Line(1), Column(0));
        assert_eq!(term.regex_search_right(&mut regex, start, end), Some(match_start..=match_end));

        let mut regex = RegexSearch::new("🦇x").unwrap();
        let start = Point::new(Line(1), Column(2));
        let end = Point::new(Line(0), Column(0));
        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(0), Column(2));
        assert_eq!(term.regex_search_left(&mut regex, start, end), Some(match_end..=match_start));
    }

    /// Run `f` on another thread and fail, instead of hanging the test run, when it does not
    /// finish within a few seconds.
    fn terminates<R: Send + 'static>(what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        // A hang shows up as a timeout, not a stuck run. The bound is generous: on a loaded CI
        // runner a debug build can take several times the ~4 s `cache_give_up_ends_a_search`
        // takes alone, and a hang never finishes, so any bound catches it.
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => panic!("{what} did not terminate"),
            Err(RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
        }
    }

    /// A match from (line, column) pairs.
    fn span(from: (i32, usize), to: (i32, usize)) -> Match {
        Point::new(Line(from.0), Column(from.1))..=Point::new(Line(to.0), Column(to.1))
    }

    /// Run `RegexIter` with a timeout; its matches, and whether it ended on its bound.
    fn iterate(
        term: Term<VoidListener>,
        pattern: &'static str,
        start: Point,
        end: Point,
        direction: Direction,
    ) -> (Vec<Match>, bool) {
        terminates("RegexIter", move || {
            let mut regex = RegexSearch::new(pattern).unwrap();
            let mut iter = RegexIter::new(start, end, direction, &term, &mut regex);
            let matches = iter.by_ref().take(1000).collect::<Vec<_>>();
            (matches, iter.bound_fired())
        })
    }

    /// Assert that `matches` is 0.26.0's endless sequence, recorded from the differential test's
    /// oracle (`cycle` repeated), up to where the cycle check ended it: at least one full cycle,
    /// then the bound.
    fn assert_upstream_cycle((matches, bound_fired): (Vec<Match>, bool), cycle: &[Match]) {
        assert!(bound_fired, "the iteration did not end on its bound: {matches:?}");
        assert!(matches.len() >= cycle.len(), "{matches:?}");
        for (i, m) in matches.iter().enumerate() {
            assert_eq!(m, &cycle[i % cycle.len()], "match {i} of {matches:?}");
        }
    }

    /// A wide character wrapped from line 1 to line 2:
    ///
    /// ```text
    /// 0: abc
    /// 1: ab<leading spacer>
    /// 2: 字<spacer>c
    /// 3: abc
    /// ```
    fn wrapped_wide_term() -> Term<VoidListener> {
        let mut term = mock_term("abc\r\nab \n字c\r\nabc");
        term.grid.screen_mut()[Line(1)][Column(2)].flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
        term
    }

    #[test]
    fn wrapped_wide_end_left() {
        // The search ends on the wrapped wide character itself, which the leftward fullwidth skip
        // used to jump over, wrapping around the buffer forever.
        let result = terminates("leftward search ending on a wrapped wide char", || {
            let term = wrapped_wide_term();

            let mut regex = RegexSearch::new("x").unwrap();
            let start = Point::new(Line(3), Column(2));
            let end = Point::new(Line(2), Column(0));
            let no_match = term.regex_search_left(&mut regex, start, end);

            let mut regex = RegexSearch::new("字").unwrap();
            let wide = term.regex_search_left(&mut regex, start, end);

            (no_match, wide)
        });

        let match_start = Point::new(Line(1), Column(2));
        let match_end = Point::new(Line(2), Column(1));
        assert_eq!(result, (None, Some(match_start..=match_end)));
    }

    #[test]
    fn wrapped_wide_search_next_max_lines() {
        // `max_lines` puts the end of a leftward search at column 0 of an arbitrary line.
        let result = terminates("leftward search_next with max_lines", || {
            let term = wrapped_wide_term();
            let origin = Point::new(Line(3), Column(2));

            let mut regex = RegexSearch::new("x").unwrap();
            let no_match =
                term.search_next(&mut regex, origin, Direction::Left, Side::Left, Some(1));

            let mut regex = RegexSearch::new("字").unwrap();
            let wide = term.search_next(&mut regex, origin, Direction::Left, Side::Left, Some(1));

            (no_match, wide)
        });

        let match_start = Point::new(Line(1), Column(2));
        let match_end = Point::new(Line(2), Column(1));
        assert_eq!(result, (None, Some(match_start..=match_end)));
    }

    #[test]
    fn wrapped_wide_end_right() {
        // Rightward, the fullwidth skip over a leading spacer used to jump over the wrapped wide
        // character too.
        let result = terminates("rightward search ending on a wrapped wide char", || {
            let term = wrapped_wide_term();

            let mut regex = RegexSearch::new("x").unwrap();
            let start = Point::new(Line(0), Column(0));
            let end = Point::new(Line(2), Column(0));
            let no_match = term.regex_search_right(&mut regex, start, end);

            let mut regex = RegexSearch::new("b字").unwrap();
            let wide = term.regex_search_right(&mut regex, start, end);

            (no_match, wide)
        });

        let match_start = Point::new(Line(1), Column(1));
        let match_end = Point::new(Line(2), Column(1));
        assert_eq!(result, (None, Some(match_start..=match_end)));
    }

    #[test]
    fn wrapped_wide_regex_iter() {
        // A match covering the iterator's end without ending on it moves the iterator's origin
        // past its end, and 0.26.0 then yields that match forever. The cycle check ends it; the
        // matches are 0.26.0's (recorded from the oracle).
        let wide = span((1, 2), (2, 1));
        let cases = [
            ((3, 2), (2, 0), Direction::Left),
            ((3, 2), (1, 2), Direction::Left),
            ((0, 0), (1, 2), Direction::Right),
            ((0, 0), (2, 0), Direction::Right),
        ];
        for (start, end, direction) in cases {
            let start = Point::new(Line(start.0), Column(start.1));
            let end = Point::new(Line(end.0), Column(end.1));
            let result = iterate(wrapped_wide_term(), "字", start, end, direction);
            assert_upstream_cycle(result, std::slice::from_ref(&wide));
        }
    }

    #[test]
    fn empty_pattern_terminates() {
        // On a plain grid the empty pattern already terminated; it only hung through the wrapped
        // wide character end-cell skip.
        let result = terminates("empty pattern", || {
            let mut results = Vec::new();
            for term in [mock_term("ab\r\ncd"), wrapped_wide_term()] {
                for direction in [Direction::Left, Direction::Right] {
                    let mut regex = RegexSearch::new("").unwrap();
                    let origin = Point::new(Line(1), Column(0));
                    for max_lines in [None, Some(0), Some(1), Some(2)] {
                        results.push(term.search_next(
                            &mut regex,
                            origin,
                            direction,
                            Side::Left,
                            max_lines,
                        ));
                    }
                }
            }
            results
        });

        assert!(result.iter().all(Option::is_none));
    }

    /// Kind of the boundary between two lines in `wrapped_wide_fuzz`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Boundary2 {
        /// Plain line break.
        None,
        /// A wide char wrapped from the upper line to the lower one.
        Wide,
        /// The upper line ends in a stale leading spacer: the lower line does not start with a
        /// wide char (a resize or an erase left the spacer behind).
        Stale,
    }

    /// Exhaustive search over small grids with wrapped wide characters and stale leading spacers,
    /// with and without scrollback, checking that every search terminates and that `RegexIter`
    /// does not cycle.
    #[test]
    fn wrapped_wide_fuzz() {
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let kinds = [Boundary2::None, Boundary2::Wide, Boundary2::Stale];
            for columns in 2..=4 {
                for lines in 2..=4 {
                    for mask in 0..3usize.pow(lines as u32 - 1) {
                        // Digit `n` (base 3) is the boundary between lines `n` and `n + 1`.
                        let boundaries: Vec<Boundary2> = (0..lines - 1)
                            .map(|n| kinds[mask / 3usize.pow(n as u32) % 3])
                            .collect();

                        // On two columns a wrapped wide char fills its line, so that line cannot
                        // also end in a leading spacer.
                        if columns == 2
                            && boundaries
                                .windows(2)
                                .any(|w| w[0] == Boundary2::Wide && w[1] != Boundary2::None)
                        {
                            continue;
                        }

                        for history in [0, 1] {
                            wrapped_wide_fuzz_grid(
                                &tx,
                                columns,
                                lines,
                                history,
                                &boundaries,
                                false,
                            );

                            // Also with a wide char that is not wrapped at the start of line 0.
                            if columns >= 3 {
                                wrapped_wide_fuzz_grid(
                                    &tx,
                                    columns,
                                    lines,
                                    history,
                                    &boundaries,
                                    true,
                                );
                            }
                        }
                    }
                }
            }
            let _ = tx.send(String::from("done"));
        });

        let mut last = String::from("start");
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(case) if case == "done" => break,
                Ok(case) => last = case,
                Err(RecvTimeoutError::Timeout) => panic!("did not terminate: {last}"),
                Err(RecvTimeoutError::Disconnected) => panic!("panicked: {last}"),
            }
        }
    }

    /// The range `search_next` searches for these arguments, as `search_next` computes it, or
    /// `None` when `max_lines` does not bound it.
    fn search_range(
        term: &Term<VoidListener>,
        origin: Point,
        direction: Direction,
        max_lines: Option<usize>,
    ) -> Option<(Point, Point)> {
        let max_lines = max_lines.filter(|max_lines| max_lines + 1 < term.total_lines())?;
        let expanded = term.expand_wide(origin, direction);
        let origin =
            if expanded.line > term.bottommost_line() { term.glyph_end(origin) } else { expanded };
        Some(match direction {
            Direction::Right => {
                let start = term.line_search_left(origin);
                let line = (start.line + max_lines).grid_clamp(term, Boundary::None);
                (start, Point::new(line, term.last_column()))
            },
            Direction::Left => {
                let start = term.line_search_right(origin);
                let line = (start.line - max_lines).grid_clamp(term, Boundary::None);
                (start, Point::new(line, Column(0)))
            },
        })
    }

    /// One grid of `wrapped_wide_fuzz`: `lines` lines in all, the first `history` of them in the
    /// scrollback.
    fn wrapped_wide_fuzz_grid(
        tx: &mpsc::Sender<String>,
        columns: usize,
        lines: usize,
        history: usize,
        boundaries: &[Boundary2],
        plain_wide: bool,
    ) {
        let screen_lines = lines - history;
        let size = TermSize::new(columns, screen_lines);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        term.grid.scroll_up(&(Line(0)..Line(screen_lines as i32)), history);
        assert_eq!(term.total_lines(), lines);

        // feathercritty (compact history): lines can be in history, written through
        // `Grid::update_line`.
        let line = |n: usize| Line(n as i32 - history as i32);
        for n in 0..lines {
            term.grid.update_line(line(n), |row| {
                for column in 0..columns {
                    let c = if (n + column) % 2 == 0 { 'a' } else { 'b' };
                    row[Column(column)] = Cell::default();
                    row[Column(column)].c = c;
                }
            });
        }
        if plain_wide {
            term.grid.update_line(line(0), |row| {
                row[Column(0)].c = '字';
                row[Column(0)].flags.insert(Flags::WIDE_CHAR);
                row[Column(1)].c = ' ';
                row[Column(1)].flags.insert(Flags::WIDE_CHAR_SPACER);
            });
        }
        for (n, boundary) in boundaries.iter().enumerate() {
            if *boundary == Boundary2::None {
                continue;
            }

            term.grid.update_line(line(n), |row| {
                let cell = &mut row[Column(columns - 1)];
                cell.c = ' ';
                cell.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE);
            });

            if *boundary == Boundary2::Wide {
                term.grid.update_line(line(n + 1), |row| {
                    row[Column(0)].c = '字';
                    row[Column(0)].flags.insert(Flags::WIDE_CHAR);
                    row[Column(1)].c = ' ';
                    row[Column(1)].flags.insert(Flags::WIDE_CHAR_SPACER);
                });
            }
        }

        let points: Vec<Point> = (0..lines)
            .flat_map(|n| (0..columns).map(move |column| Point::new(line(n), Column(column))))
            .collect();
        // Brent's check ends a cycling iteration within about three times its length.
        let cap = 4 * points.len() + 8;

        let grid = format!(
            "{columns}x{lines} history {history} boundaries {boundaries:?} plain {plain_wide}"
        );
        for pattern in ["字", "a", "字a", "b字", " ", "x", "", "a*"] {
            let mut regex = RegexSearch::new(pattern).unwrap();
            for &start in &points {
                for &end in &points {
                    let case = format!("{grid} pattern {pattern:?} {start:?} -> {end:?}");
                    let _ = tx.send(case.clone());

                    // Upstream's searches, which can answer outside the range; they must end.
                    term.regex_search_left(&mut regex, start, end);
                    term.regex_search_right(&mut regex, start, end);

                    // Upstream's iterations end, or the cycle check ends them.
                    for direction in [Direction::Left, Direction::Right] {
                        let count = RegexIter::new(start, end, direction, &term, &mut regex)
                            .take(cap)
                            .count();
                        assert!(count < cap, "RegexIter did not end: {case} {direction:?}");
                    }
                }

                let max_lines = iter::once(None).chain((0..lines).map(Some));
                for max_lines in max_lines {
                    for direction in [Direction::Left, Direction::Right] {
                        let _ = tx.send(format!(
                            "{grid} pattern {pattern:?} search_next {start:?} {direction:?} \
                             max_lines {max_lines:?}"
                        ));
                        for side in [Side::Left, Side::Right] {
                            let found =
                                term.search_next(&mut regex, start, direction, side, max_lines);
                            let range = search_range(&term, start, direction, max_lines);
                            if let (Some(m), Some((from, to))) = (found, range) {
                                assert!(
                                    term.match_in_range(&m, from, to, direction),
                                    "search_next answered outside the range: {grid} pattern \
                                     {pattern:?} {start:?} {direction:?} {side:?} {max_lines:?} \
                                     {m:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Rows 14 to 20 of an 11 x 21 grid from the differential soak (seed 305, step 164), as
    /// (cell chars, flag bits). Line 15 ends in a stale leading spacer: line 16 starts with a
    /// plain space, not a wide char.
    fn stale_spacer_soak_term() -> Term<VoidListener> {
        #[rustfmt::skip]
        let rows: [(&str, [u16; 11]); 7] = [
            ("v!;🎉 字 L국  ", [256, 256, 256, 288, 320, 288, 320, 256, 288, 320, 1296]),
            ("ト lス Ωover ", [288, 320, 256, 288, 320, 256, 256, 256, 256, 256, 1296]),
            ("          🇮", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 272]),
            ("d\"42🚀 JWv국 ", [256, 256, 256, 256, 288, 320, 256, 256, 256, 288, 336]),
            ("pł文 日 ß<국 b", [256, 256, 288, 320, 288, 320, 256, 256, 288, 320, 272]),
            ("ł`<ar😀 carg", [256, 256, 256, 256, 256, 288, 320, 256, 256, 256, 272]),
            ("oõS🇮🇹T     ", [256, 256, 256, 256, 256, 256, 0, 0, 0, 0, 0]),
        ];

        let size = TermSize::new(11, 21);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        for (i, (text, flags)) in rows.iter().enumerate() {
            let line = Line(14 + i as i32);
            for (column, c) in text.chars().enumerate() {
                term.grid.screen_mut()[line][Column(column)].c = c;
                term.grid.screen_mut()[line][Column(column)].flags =
                    Flags::from_bits_truncate(flags[column]);
            }
        }
        term
    }

    #[test]
    fn stale_leading_spacer_soak() {
        // The search window runs from (20,10) left to (15,0). Going right from the stale spacer
        // at (15,10), the fullwidth skip also skips (16,0), so a match there ends behind its
        // origin and 0.26.0's `RegexIter` cycles between (16,0) and (15,10) forever, so its
        // `search_next` never returns. We answer with the last match inside the range that the
        // iteration produced before it started repeating.
        let result = terminates("search_next over a stale leading spacer", || {
            let term = stale_spacer_soak_term();
            let mut regex = RegexSearch::new(" ").unwrap();
            let origin = Point::new(Line(15), Column(8));
            term.search_next(&mut regex, origin, Direction::Left, Side::Right, Some(5))
        });

        assert_eq!(result, Some(span((15, 10), (16, 1))));
    }

    /// Two columns with one line of scrollback; line -1 ends in a stale leading spacer:
    ///
    /// ```text
    /// -1: a<stale leading spacer>
    ///  0:  a
    ///  1: a
    /// ```
    fn stale_spacer_history_term() -> Term<VoidListener> {
        let size = TermSize::new(2, 2);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        term.grid.scroll_up(&(Line(0)..Line(2)), 1);
        // feathercritty (compact history): line -1 is in history, written through
        // `Grid::update_line`.
        for (line, text) in [(-1, "a "), (0, " a"), (1, "a ")] {
            term.grid.update_line(Line(line), |row| {
                for (column, c) in text.chars().enumerate() {
                    row[Column(column)] = Cell::default();
                    row[Column(column)].c = c;
                }
            });
        }
        term.grid.update_line(Line(-1), |row| {
            row[Column(1)].flags.insert(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE);
        });
        term
    }

    #[test]
    fn stale_leading_spacer_history() {
        // The shape of the second soak failure (seed 500): two columns, scrollback, a stale
        // leading spacer, and a leftward `search_next` with `max_lines`.
        let result = terminates("RegexIter over a stale leading spacer", || {
            let term = stale_spacer_history_term();
            let mut regex = RegexSearch::new(" ").unwrap();
            let start = Point::new(Line(0), Column(1));
            let end = Point::new(Line(-1), Column(0));
            let matches = RegexIter::new(start, end, Direction::Left, &term, &mut regex)
                .take(10)
                .collect::<Vec<_>>();

            let origin = Point::new(Line(0), Column(1));
            let next = term.search_next(&mut regex, origin, Direction::Left, Side::Right, Some(1));

            (matches, next)
        });

        assert!(result.0.len() < 10, "RegexIter cycled: {:?}", result.0);
        assert!(result.1.is_some());
    }

    #[test]
    fn stale_leading_spacer_seed_500() {
        // The literal grid of differential soak seed 500, step 170: 2 columns, 66 lines of
        // scrollback, stale leading spacers among wrapped wide chars. `RegexIter` cycled.
        let result = terminates("search_next on the seed 500 grid", || {
            #[rustfmt::skip]
            let term = term_from_rows(66, &[
                ("ec", ".r"),
                ("e ", ".L"),
                ("日 ", "WS"),
                ("$C", ".r"),
                (":\\", ".r"),
                ("di", ".r"),
                ("rz", ".r"),
                ("6Ø", ".r"),
                ("<t", ".r"),
                ("ag", ".r"),
                (">(", ".r"),
                ("ju", ".r"),
                ("mp", ".r"),
                ("s)", ".r"),
                ("(G", ".r"),
                ("wj", ".r"),
                ("ス ", "WS"),
                ("~V", ".r"),
                ("ł ", ".L"),
                ("本 ", "WS"),
                ("q ", ".L"),
                ("本 ", "WS"),
                ("1!", ".r"),
                (") ", ".L"),
                ("👍 ", "WS"),
                ("🏽 ", "WS"),
                ("?{", ".r"),
                ("( ", ".L"),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("d ", ".L"),
                ("字 ", "WS"),
                ("W ", ".L"),
                ("⚡ ", "WS"),
                ("R)", ".r"),
                ("{}", ".r"),
                ("y ", ".L"),
                ("本 ", "WS"),
                ("ΩK", ".r"),
                ("e ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("👨 ", "WS"),
                ("👩 ", "WS"),
                ("👧 ", "WS"),
                ("çs", ".r"),
                ("rc", ".r"),
                ("/m", ".r"),
                ("ai", ".r"),
                ("n.", ".r"),
                ("rs", ".r"),
                ("x=", ".r"),
                ("1;", ".r"),
                ("字 ", "WS"),
                ("< ", ".L"),
                ("日 ", "WS"),
                ("│|", ".r"),
                ("pi", ".r"),
                ("pe", ".r"),
                ("|l", ".r"),
                ("az", ".r"),
                ("y ", ".L"),
                ("語 ", "WS"),
                ("{k", ".r"),
                ("ey", ".r"),
                (": ", ".r"),
                ("v}", ".r"),
                ("Eu", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
                ("  ", ".."),
            ]);
            let mut regex = RegexSearch::new(" ").unwrap();
            let origin = Point::new(Line(-41), Column(1));
            term.search_next(&mut regex, origin, Direction::Left, Side::Right, Some(9))
        });

        assert!(result.is_some());
    }

    /// A grid from rows of (cell chars, cell flags), the first `history` rows in the scrollback.
    /// Flags: `.` none, `r` `WRAPLINE`, `W` `WIDE_CHAR`, `s` `WIDE_CHAR_SPACER`, `S` a spacer
    /// with `WRAPLINE`, `L` a leading spacer with `WRAPLINE`.
    fn term_from_rows(history: usize, rows: &[(&str, &str)]) -> Term<VoidListener> {
        let columns = rows[0].1.len();
        let screen_lines = rows.len() - history;
        let size = TermSize::new(columns, screen_lines);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        term.grid.scroll_up(&(Line(0)..Line(screen_lines as i32)), history);

        for (n, (text, flags)) in rows.iter().enumerate() {
            let line = Line(n as i32 - history as i32);
            assert_eq!(text.chars().count(), columns);
            // feathercritty (compact history): lines can be in history, written through
            // `Grid::update_line`.
            term.grid.update_line(line, |row| {
                for (column, (c, flag)) in text.chars().zip(flags.chars()).enumerate() {
                    let cell = &mut row[Column(column)];
                    *cell = Cell::default();
                    cell.c = c;
                    cell.flags = match flag {
                        '.' => Flags::empty(),
                        'r' => Flags::WRAPLINE,
                        'W' => Flags::WIDE_CHAR,
                        's' => Flags::WIDE_CHAR_SPACER,
                        'S' => Flags::WIDE_CHAR_SPACER | Flags::WRAPLINE,
                        'L' => Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE,
                        _ => unreachable!(),
                    };
                }
            });
        }
        term
    }

    #[test]
    fn stale_leading_spacer_unbounded() {
        // Differential soak seed 6, step 6: 80 columns, two lines of scrollback, line 5 ends in a
        // stale leading spacer. An unbounded leftward search (the find bar's ordinary case) never
        // returned.
        let result = terminates("unbounded leftward search over a stale leading spacer", || {
            #[rustfmt::skip]
            let term = term_from_rows(2, &[
            ("ス 2                                                                             ", "Ws.............................................................................."),
            ("                                                                                ", "................................................................................"),
            ("                                                 )>ト t=--releasex=1;e ßxé\\E⚡ qui", "...................................................Ws......................Ws..r"),
            ("ck$✅ ,👨 👩 👧 kEa,b✅ )Püçrñ42src/main.rs文 語 b0x1fÆ-K| |pipe|Lover{Jçト é{8Rテ 文 !C어 ", "...Ws.WsWsWs.....Ws...................WsWs........................Ws....WsWs..WS"),
            ("S국 |pipe| 🚀 🇮🇹ha.b.cecargoX😀 fn(Æ'single'brown🚀 K ス fxofn(equick本 e🚀 ]}ス P{key: ", ".Ws.......Ws...............Ws.................Ws..Ws............Ws.Ws..Ws......r"),
            ("v}a42テ Ж<jumpswYQ本 ｡]🇮🇹>oversrc/main.rs!)LÆ😀 Sà[Slazy😀 e<tag><-C:\\dir한 ト 🚀 foxbr", ".....Ws..........Ws........................Ws........Ws..............WsWsWs....r"),
            ("ownM'국 D'single'                                                   C{e字 語 C:\\dir", ".....Ws...............................................................WsWs.....r"),
            ("src/main.rs!中 [ok]]ßJe語 L3한 \\🎉 日 X>brown\"N｡--releaseM))日 字 eト L]/4✅ Æ42over>&文  ", "............Ws........Ws..Ws.WsWs......................WsWs.Ws....Ws.........WsL"),
            ("                                                                                ", "................................................................................"),
            ]);
            let mut regex = RegexSearch::new(" ").unwrap();
            let origin = Point::new(Line(2), Column(1));
            term.search_next(&mut regex, origin, Direction::Left, Side::Right, None)
        });

        assert!(result.is_some());
    }

    #[test]
    fn stale_leading_spacer_large_history() {
        // Differential soak `large_history_limit` seed 3, step 19: 9 columns, line 11 ends in a
        // stale leading spacer.
        let result = terminates("leftward searches over leading spacers", || {
            #[rustfmt::skip]
            let term = term_from_rows(0, &[
                ("l8fox❤!y ", "........L"),
                ("日 e7Wト e🇮", "Ws...Ws.r"),
                ("🇹ス QBEa.b", ".Ws.....r"),
                (".cO      ", "........."),
                ("         ", "........."),
                ("g🇮🇹어 👨 👩 ", "...WsWsWS"),
                ("👧 8lHeXu ", "Ws......L"),
                ("👨 👩 👧 中 Z", "WsWsWsWsr"),
                ("BçslC:\\di", "........r"),
                ("r<buildl<", "........r"),
                ("çedogW{ut", "........r"),
                ("rス jumps ", ".Ws.....L"),
                ("    ?|pip", "........r"),
                ("e|e한 f🚀 n", "...Ws.Wsr"),
                ("42ñ<tag>)", "........r"),
                ("文 z ❤a.b.", "Ws......r"),
                ("c#õVî字 │ ", ".....Ws.L"),
                ("한 t~S    ", "Ws......."),
                ("         ", "........."),
                ("         ", "........."),
            ]);
            let mut regex = RegexSearch::new(" ").unwrap();
            let unbounded = term.search_next(
                &mut regex,
                Point::new(Line(6), Column(5)),
                Direction::Left,
                Side::Right,
                None,
            );
            let bounded = term.search_next(
                &mut regex,
                Point::new(Line(12), Column(5)),
                Direction::Left,
                Side::Left,
                Some(1),
            );
            (unbounded, bounded)
        });

        assert!(result.0.is_some());
        assert!(result.1.is_some());
    }

    #[test]
    fn range_ending_on_leading_spacer() {
        // A rightward search limited to line 1, which ends in a leading spacer. Its last search
        // starts and ends on that spacer; the first fullwidth skip moves past it and the search
        // reads on into line 2. 0.26.0's `search_next` answers with the 'c' there, (2,2), outside
        // the range; we answer with the last match inside the range, (1,0)..=(1,1).
        let term = wrapped_wide_term();
        let mut regex = RegexSearch::new("[a-z]+").unwrap();
        let origin = Point::new(Line(1), Column(1));
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Left, Some(0));
        assert_eq!(result, Some(span((1, 0), (1, 1))));

        // The searches themselves are 0.26.0's: from the spacer to itself the first reads the
        // wrapped wide char, and with a pattern it does not match, reads on to the 'c'.
        let mut regex = RegexSearch::new("字").unwrap();
        let spacer = Point::new(Line(1), Column(2));
        let wide = term.regex_search_right(&mut regex, spacer, spacer);
        assert_eq!(wide, Some(span((1, 2), (2, 1))));
        let mut regex = RegexSearch::new("[a-z]").unwrap();
        assert_eq!(term.regex_search_right(&mut regex, spacer, spacer), Some(span((2, 2), (2, 2))));
    }

    #[test]
    fn range_ending_on_stale_leading_spacer() {
        // Differential soak seed 543: a rightward search limited to line 1, which ends in a stale
        // leading spacer. The search reads the cell after the spacer (line 2's first cell) in
        // its place and lands on line 2's second cell, as upstream does, so "b +" ends there
        // (0.26.0 returns the same match). A match that lies entirely on line 2 (0.26.0 returns
        // the blank at (2,1)) is outside the range and not returned.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("abc", "..."),
            ("ab ", "..L"),
            ("  d", "..."),
            ("abc", "..."),
        ]);
        let origin = Point::new(Line(1), Column(0));

        let mut regex = RegexSearch::new("b +").unwrap();
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Left, Some(0));
        let match_start = Point::new(Line(1), Column(1));
        let match_end = Point::new(Line(2), Column(1));
        assert_eq!(result, Some(match_start..=match_end));

        let mut regex = RegexSearch::new(" ").unwrap();
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Left, Some(0));
        assert_eq!(result, None);
    }

    #[test]
    fn stale_spacer_end_keeps_upstream_match() {
        // Differential soak seed 5000017 (`random_streams`, step 172): a rightward search limited
        // to line -1, which ends in a stale leading spacer (line 0 starts with a blank and holds
        // a stray spacer). Reading the spacer, the search reads line 0's first cell in its place
        // and lands on its second cell, so the blank match at the spacer ends on (0,1). 0.26.0
        // returns that match and stops there; reporting it at the spacer instead made
        // `search_next` skip it (its end then lay before the origin) and return the first match
        // of the line.
        #[rustfmt::skip]
        let rows = [
            ("Y foo_barZ ", "..........L"),
            ("           ", ".s........."),
        ];
        let term = term_from_rows(1, &rows);
        let origin = Point::new(Line(0), Column(0));

        let mut regex = RegexSearch::new(" ").unwrap();
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Right, Some(0));
        assert_eq!(result, Some(span((-1, 10), (0, 1))));

        // 0.26.0's iterator over the line goes on past the spacer's completion through line 0
        // and around the buffer, forever (recorded from the oracle); the cycle check ends it.
        let start = Point::new(Line(-1), Column(0));
        let end = Point::new(Line(-1), Column(10));
        let mut cycle = vec![span((-1, 1), (-1, 1)), span((-1, 10), (0, 1))];
        cycle.extend((2..=10).map(|column| span((0, column), (0, column))));
        let result = iterate(term_from_rows(1, &rows), " ", start, end, Direction::Right);
        assert_upstream_cycle(result, &cycle);
    }

    #[test]
    fn stale_spacer_second_search_reads_past_origin() {
        // Differential soak seed 2001369 (`random_streams`, step 26): line 0 ends in a stale
        // leading spacer, line 1 is blank. Going left from (1,0), the first search reads (1,0)
        // and the spacer as two blanks and finds "  " starting on the spacer. The second search,
        // back right to (1,0), reads (1,0) in the spacer's place and lands on (1,1), past its
        // end, so it has read one blank. 0.26.0 reads on to (1,2), completes the match there
        // and returns (0,7)..=(1,2); stopping at the skipped end dropped the match and ended
        // the iteration.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("G⚡ 文 �� ", ".WsWs..L"),
            ("        ", "........"),
            ("        ", "........"),
            ("        ", "........"),
            ("        ", "........"),
            ("        ", "........"),
            ("single'[", ".......r"),
            ("��생 本 th", "..WsWs.r"),
            ("e       ", "........"),
        ]);
        let origin = Point::new(Line(0), Column(7));

        let result = terminates("leftward search_next over a stale spacer", move || {
            let mut regex = RegexSearch::new("  ").unwrap();
            term.search_next(&mut regex, origin, Direction::Left, Side::Left, Some(3))
        });
        let match_start = Point::new(Line(0), Column(7));
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(result, Some(match_start..=match_end));
    }

    #[test]
    fn search_origin_on_bottom_line_stale_spacer() {
        // Differential soak seeds 2008270, 2032022 and five more (`random_streams`): a rightward
        // `search_next` from a stale leading spacer in the middle of the bottommost line.
        // `expand_wide` moves right from a leading spacer to column 1 of the next line, which
        // does not exist there, so the origin left the grid: 0.26.0 panics, and the search
        // started below the grid, found no match after that origin and returned the first match
        // of the buffer, before the origin.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("  a    ", "...L..."),
        ]);
        let origin = Point::new(Line(0), Column(3));

        // The search reads the spacer with the cell after it in its place (as from any other
        // origin), so the next match is the blank pair after that.
        for side in [Side::Left, Side::Right] {
            let mut regex = RegexSearch::new("  ").unwrap();
            let result = term.search_next(&mut regex, origin, Direction::Right, side, None);
            let match_start = Point::new(Line(0), Column(5));
            let match_end = Point::new(Line(0), Column(6));
            assert_eq!(result, Some(match_start..=match_end), "{side:?}");
        }
    }

    /// Lines 0 to 2 are one wrapped line whose line 1 starts with a stale wide char spacer
    /// (`WIDE_CHAR_SPACER` without its wide char, at column 0), as a resize to 2 columns and an
    /// erase leave behind (differential soak seed 30001812, `large_history_limit`, shrunk).
    fn stale_spacer_column_zero_term() -> Term<VoidListener> {
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("ab", ".r"),
            ("  ", "Sr"),
            ("cd", ".."),
        ]);
        term
    }

    #[test]
    fn stale_spacer_column_zero_left_iteration() {
        // A leftward `RegexIter` from (2,1) yields (2,1) and (2,0); its third match ends on the
        // spacer at (1,0), and moving its origin past that match (`expand_wide`, going left from a
        // spacer to its wide char one column back) subtracts from column 0: 0.26.0 panics in debug
        // builds (recorded from the differential test's oracle) and goes on from a wrapped-around
        // column in release builds. We end the iteration there, with the bound, after 0.26.0's
        // matches.
        let start = Point::new(Line(2), Column(1));
        let end = Point::new(Line(0), Column(0));
        let result =
            iterate(stale_spacer_column_zero_term(), "[^ ]+ ?", start, end, Direction::Left);
        assert_eq!(result, (vec![span((2, 1), (2, 1)), span((2, 0), (2, 0))], true));
    }

    #[test]
    fn stale_spacer_column_zero_search_next() {
        // `search_next` runs the iteration of `stale_spacer_column_zero_left_iteration` (its start
        // is the end of the wrapped line, (2,1)); neither match yielded is past the origin, and
        // 0.26.0 panics before it finds one, so the answer is the last match in range.
        for origin in [(0, 0), (0, 1), (1, 1)] {
            let origin = Point::new(Line(origin.0), Column(origin.1));
            let term = stale_spacer_column_zero_term();
            let result = next_match(term, "[^ ]+ ?", origin, Direction::Left, Side::Left, None);
            assert_eq!(result, Some(span((2, 0), (2, 0))), "{origin:?}");
        }

        // From the spacer itself, 0.26.0 already panics moving the origin (`expand_wide`): no
        // match was produced, so there is none.
        let origin = Point::new(Line(1), Column(0));
        for max_lines in [None, Some(1)] {
            let term = stale_spacer_column_zero_term();
            let result =
                next_match(term, "[^ ]+ ?", origin, Direction::Left, Side::Left, max_lines);
            assert_eq!(result, None, "{max_lines:?}");
        }
    }

    #[test]
    fn repeat_from_range_end_ends_iteration() {
        // Search fix 3 re-review 3, probe-style grid 345. 0.26.0's iteration from (8,2) to (7,0)
        // yields (7,2), then (7,0)..=(7,1) from (7,1), then (7,0)..=(7,1) again from (7,0): that
        // origin is the range's end, so the iteration ends there (`done`) although its last match
        // repeats an earlier one. 0.26.0 terminates with its first match, (7,2)..=(7,2), in range,
        // and the answer is 0.26.0's (the differential test's walk used to read the repeat as an
        // endless cycle).
        #[rustfmt::skip]
        let rows = [
            ("aa ", "..r"),
            ("abb", "..."),
            ("b符 ", ".W."),
            ("中 x", "Ws."),
            ("中  ", "WsL"),
            ("符aa", "W.."),
            ("中  ", "WsL"),
            ("符ab", "W.r"),
            ("中  ", "Ws."),
            (" ba", "..."),
        ];
        let start = Point::new(Line(8), Column(2));
        let end = Point::new(Line(7), Column(0));
        let result = iterate(term_from_rows(0, &rows), "b|符", start, end, Direction::Left);
        let matches = vec![span((7, 2), (7, 2)), span((7, 0), (7, 1)), span((7, 0), (7, 1))];
        assert_eq!(result, (matches, false));

        let origin = Point::new(Line(6), Column(2));
        let term = term_from_rows(0, &rows);
        let result = next_match(term, "b|符", origin, Direction::Left, Side::Right, Some(1));
        assert_eq!(result, Some(span((7, 2), (7, 2))));
    }

    #[test]
    fn left_match_past_range_start_wrapping() {
        // Differential soak seed 2003541 (`random_streams`, `DIFF_TINY=1`), shrunk: a leftward
        // range from (1,1) up around the buffer's top to (6,0), so lines 2 to 5 lie outside it
        // (apart from the leading spacer completing the wide char at its end). Line 1 holds a wide
        // char followed by a plain blank instead of its spacer. After the first match the
        // iterator searches from that wide char to itself: the second search (a full lap, as for
        // any search from a cell to itself) reads right through lines 2 to 5 and ends on line 12.
        // 0.26.0 yields that match, (1,0)..=(12,0), and then cycles through 14 matches forever
        // (recorded from the oracle). We yield the same and end there.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("x ", ".L"),
            ("文 ", "W."),
            ("👍 ", "WS"),
            ("x ", ".L"),
            ("👧 ", "WS"),
            ("8 ", ".L"),
            ("本 ", "WS"),
            ("テ ", "WS"),
            ("q ", ".L"),
            ("⚡ ", "WS"),
            ("xj", ".r"),
            ("*)", ".r"),
            ("x5", ".."),
        ]);
        let start = Point::new(Line(1), Column(1));
        let end = Point::new(Line(6), Column(0));
        let mut cycle = vec![span((1, 0), (1, 1)), span((1, 0), (12, 0))];
        for to in [(11, 1), (11, 0), (10, 1), (10, 0), (9, 1), (8, 0), (7, 1), (6, 1), (5, 0)] {
            cycle.push(span((2, 0), to));
        }
        cycle.extend([span((2, 0), (4, 1)), span((2, 0), (3, 0)), span((2, 0), (2, 1))]);
        let result = iterate(term, "[^ ]+ ?", start, end, Direction::Left);
        assert_upstream_cycle(result, &cycle);
    }

    #[test]
    fn left_iteration_stops_where_upstream_stops() {
        // Differential soak seed 3001698 (`random_streams`, step 381): a leftward range from
        // (7,1) up around the buffer's top to (9,0), so line 8 lies outside it. After the match
        // (7,0)..=(7,1), the search from the wide char at (7,0) (followed by a plain blank, not
        // its spacer) to itself reads a full lap and returns (7,0)..=(9,0), through line 8.
        // 0.26.0 stops there, because that match ends on the range's end, so `search_next` finds
        // nothing past the origin and answers with the first match, (7,0)..=(7,1), inside the
        // range. A version of fork change 5 that cut matches inside the iteration went on and
        // answered (0,0)..=(6,0).
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("🏽 ", "WS"),
            ("中 ", "WS"),
            ("e ", ".L"),
            ("어 ", "WS"),
            ("5i", ".r"),
            ("Ж{", ".r"),
            ("{ ", ".L"),
            ("符 ", "W."),
            ("中 ", "WS"),
            ("  ", ".L"),
            ("日 ", "WS"),
            ("│｡", ".."),
            ("  ", ".."),
            ("  ", ".."),
        ]);
        let origin = Point::new(Line(4), Column(1));

        let result = terminates("leftward search_next over a wrapping range", move || {
            let mut regex = RegexSearch::new("[^ ]+ ?").unwrap();
            term.search_next(&mut regex, origin, Direction::Left, Side::Left, Some(12))
        });
        let match_start = Point::new(Line(7), Column(0));
        let match_end = Point::new(Line(7), Column(1));
        assert_eq!(result, Some(match_start..=match_end));
    }

    /// `search_next` with `pattern` on `term`, run with a timeout.
    fn next_match(
        term: Term<VoidListener>,
        pattern: &'static str,
        origin: Point,
        direction: Direction,
        side: Side,
        max_lines: Option<usize>,
    ) -> Option<Match> {
        terminates("search_next", move || {
            let mut regex = RegexSearch::new(pattern).unwrap();
            term.search_next(&mut regex, origin, direction, side, max_lines)
        })
    }

    #[test]
    fn rereview_stale_wide_char_left() {
        // Search fix 3 re-review, grid probe seed 11351: line 2 holds a wide char with a plain cell
        // after it instead of its spacer. 0.26.0 answers (1,0)..=(1,1); the cut that fork change
        // 5 used to make inside the iteration changed that to (1,0)..=(1,2).
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("中 a", "Wsr"),
            ("中 a", "Wsr"),
            (" 符 ", ".Wr"),
        ]);
        let origin = Point::new(Line(1), Column(2));
        let result = next_match(term, "[^ ]+", origin, Direction::Left, Side::Left, Some(1));
        assert_eq!(result, Some(Point::new(Line(1), Column(0))..=Point::new(Line(1), Column(1))));
    }

    #[test]
    fn rereview_right_second_search_over_skipped_origin() {
        // Search fix 3 re-review, Important 2: stale leading spacers at line ends only. `skip`
        // from the stale spacer at (1,1) lands on the wide char at (3,0), and the second search,
        // back to that origin, steps over it; 0.26.0 reads on and answers (2,0)..=(3,1). Fork
        // change 5 used to stop there and drop the `x`.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("a ", ".L"),
            ("  ", ".L"),
            ("x ", ".L"),
            ("中 ", "Ws"),
        ]);
        let origin = Point::new(Line(1), Column(1));
        let result = next_match(term, "[^ ]+ ?", origin, Direction::Right, Side::Right, None);
        assert_eq!(result, Some(Point::new(Line(2), Column(0))..=Point::new(Line(3), Column(1))));
    }

    #[test]
    fn rereview_left_skip_over_end() {
        // Search fix 3 re-review, Important 2 (related grid): 0.26.0 answers (1,0)..=(1,0), in
        // range; fork change 5 used to stop `RegexIter::skip` when its origin jumped over the end
        // and answered (1,0)..=(1,1).
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("  ", "L."),
            ("中 ", "Ws"),
            ("中 ", "WS"),
        ]);
        let origin = Point::new(Line(1), Column(0));
        let result = next_match(term, "[^ ]+ ?", origin, Direction::Left, Side::Right, Some(0));
        let point = Point::new(Line(1), Column(0));
        assert_eq!(result, Some(point..=point));
    }

    /// 100 columns by `lines` lines, every line wrapped (the last one too, so the search reads on
    /// from the bottom line into the top one), all `a` but for a wide char wrapped
    /// from the end of line 0 to the start of line 1 (its leading spacer at (0,99)) and a `b` in
    /// the last cell. A rightward search from (1,0) to the last cell finds the `b`; its second
    /// search, back left to (1,0), steps over that wide char and, like 0.26.0, reads on around
    /// the buffer for as long as the pattern keeps it alive (search fix 3, re-review 2).
    fn counting_term(lines: usize) -> Term<VoidListener> {
        let mut rows = vec![(format!("{} ", "a".repeat(99)), format!("{}L", "r".repeat(99)))];
        for line in 1..lines {
            let mut text = "a".repeat(100);
            let mut flags = format!("{}r", ".".repeat(99));
            if line == 1 {
                text.replace_range(0..2, "中 ");
                flags.replace_range(0..2, "Ws");
            }
            if line + 1 == lines {
                text.replace_range(99..100, "b");
            }
            rows.push((text, flags));
        }
        let rows: Vec<(&str, &str)> = rows.iter().map(|(t, f)| (t.as_str(), f.as_str())).collect();
        term_from_rows(0, &rows)
    }

    #[test]
    fn counted_pattern_hits_the_cycle_check() {
        // The second search counts the cells it reads modulo 97 and never finds the end of a
        // match, so 0.26.0 never returns. Its state at each wrap around the buffer repeats with
        // the counter, and the loop check in `regex_search_internal` ends it: no match.
        let found = terminates("a counted pattern over a stepped-over origin", || {
            let term = counting_term(20);
            let mut regex = RegexSearch::new("(?:[ab中]{97})*b").unwrap();
            let start = Point::new(Line(1), Column(0));
            let end = Point::new(Line(19), Column(99));
            let found = term.regex_search_right(&mut regex, start, end);
            (found, regex.take_bound_fired())
        });
        assert_eq!(found, (None, true));
    }

    #[test]
    fn cache_give_up_ends_a_search() {
        // As above with a counter too large for the lazy DFA's cache: the cache is cleared again
        // and again, so the loop check, whose state includes the clear count, never sees a repeat.
        // The search ends only because regex-automata gives up on the cache's fourth clear: this
        // search never reports progress to the cache (`Cache::search_update`), so it always
        // counts as inefficient (`minimum_cache_clear_count(3)`, `minimum_bytes_per_state(10)` in
        // `RegexSearch::new`), as in 0.26.0, which logs "Regex exceeded complexity limit" and
        // returns no match. If a regex-automata update or an upstream change makes the cache keep
        // clearing, this search runs forever and the test fails on its timeout.
        let found = terminates("a counted pattern too large for the DFA cache", || {
            let term = counting_term(21);
            let mut regex = RegexSearch::new("(?:[ab中]{4999})*b").unwrap();
            let start = Point::new(Line(1), Column(0));
            let end = Point::new(Line(20), Column(99));
            let found = term.regex_search_right(&mut regex, start, end);
            (found, regex.take_bound_fired(), regex.right_rdfa.cache.clear_count())
        });
        // Ended by the cache giving up after its third clear, not by the loop check.
        assert_eq!(found, (None, false, 3));
    }

    #[test]
    fn stale_spacer_at_range_start() {
        // Differential soak seed 791: a leftward search whose range starts next to a stale
        // leading spacer in the middle of a line. The second search, back from the match start
        // to the origin, reads past the spacer onto the next line, and 0.26.0's iterator cycles
        // between two matches forever, one of them reaching line 2 (recorded from the oracle).
        // `search_next` stops at the first one, (1,3), inside the range, as 0.26.0 does.
        #[rustfmt::skip]
        let rows = [
            ("abcd", "...."),
            ("ab  ", "..L."),
            ("字 cd", "Ws.."),
        ];
        let start = Point::new(Line(1), Column(3));
        let end = Point::new(Line(1), Column(0));
        let result = iterate(term_from_rows(0, &rows), " ", start, end, Direction::Left);
        assert_upstream_cycle(result, &[span((1, 3), (1, 3)), span((1, 2), (2, 0))]);

        let term = term_from_rows(0, &rows);
        let mut regex = RegexSearch::new(" ").unwrap();
        let result = term.search_next(&mut regex, start, Direction::Left, Side::Right, Some(0));
        assert_eq!(result, Some(span((1, 3), (1, 3))));
    }

    #[test]
    fn stale_spacer_match_past_origin() {
        // Going left from (3,2), the search from the stale leading spacer at (2,2) reads (3,0) in
        // its place, so that match ends at (3,1), past its origin, and the next origin lies
        // behind the previous one: 0.26.0 cycles between (3,0) and (2,2)..=(3,1) forever
        // (recorded from the oracle). The cycle check ends it; the blank at (0,1), which 0.26.0
        // never reaches, is not returned.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("x x", "..."),
            ("abc", "..."),
            ("ab ", "..L"),
            (" cd", "..."),
            ("abc", "..."),
        ]);
        let start = Point::new(Line(3), Column(2));
        let end = Point::new(Line(0), Column(0));
        let result = iterate(term, " ", start, end, Direction::Left);
        assert_upstream_cycle(result, &[span((3, 0), (3, 0)), span((2, 2), (3, 1))]);
    }

    #[test]
    fn regex_iter_ends_where_upstream_repeats() {
        // In a range that wraps around the whole buffer and starts inside a wrapped wide char,
        // 0.26.0's last search finds the first match again, and again, forever. The cycle check
        // ends it after 0.26.0's matches.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("a ", ".L"),
            ("字 ", "Ws"),
        ]);
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(0), Column(1));
        let result = iterate(term, "字", start, end, Direction::Right);
        assert_upstream_cycle(result, &[span((0, 1), (1, 1))]);
    }
}
