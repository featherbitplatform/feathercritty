// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

use std::cmp::max;
use std::error::Error;
use std::mem;
use std::ops::RangeInclusive;

use log::{debug, warn};
pub use regex_automata::hybrid::BuildError;
use regex_automata::hybrid::dfa::{Builder, Cache, Config, DFA};
use regex_automata::nfa::thompson::Config as ThompsonConfig;
use regex_automata::util::syntax::Config as SyntaxConfig;
use regex_automata::{Anchored, Input, MatchKind};

use crate::grid::{BidirectionalIterator, Dimensions, GridIterator, Indexed};
use crate::index::{Boundary, Column, Direction, Point, Side};
use crate::term::Term;
use crate::term::cell::{Cell, Flags};

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
}

/// Runtime-evaluated DFA.
#[derive(Clone, Debug)]
struct LazyDfa {
    dfa: DFA,
    cache: Cache,
    direction: Direction,
    match_all: bool,
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

        Ok(Self { direction, cache, dfa, match_all })
    }
}

impl<T> Term<T> {
    /// Get next search match in the specified direction.
    pub fn search_next(
        &self,
        regex: &mut RegexSearch,
        mut origin: Point,
        direction: Direction,
        side: Side,
        mut max_lines: Option<usize>,
    ) -> Option<Match> {
        origin = self.expand_wide(origin, direction);

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

        let mut regex_iter = RegexIter::new(start, end, Direction::Right, self, regex).peekable();

        // Check if there's any match at all.
        let first_match = regex_iter.peek()?.clone();

        let regex_match = regex_iter
            .find(|regex_match| {
                let match_point = Self::match_side(regex_match, side);

                // If the match's point is beyond the origin, we're done.
                match_point.line < start.line
                    || match_point.line > origin.line
                    || (match_point.line == origin.line && match_point.column >= origin.column)
            })
            .unwrap_or(first_match);

        Some(regex_match)
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

        let mut regex_iter = RegexIter::new(start, end, Direction::Left, self, regex).peekable();

        // Check if there's any match at all.
        let first_match = regex_iter.peek()?.clone();

        let regex_match = regex_iter
            .find(|regex_match| {
                let match_point = Self::match_side(regex_match, side);

                // If the match's point is beyond the origin, we're done.
                match_point.line > start.line
                    || match_point.line < origin.line
                    || (match_point.line == origin.line && match_point.column <= origin.column)
            })
            .unwrap_or(first_match);

        Some(regex_match)
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
    pub fn regex_search_left(
        &self,
        regex: &mut RegexSearch,
        start: Point,
        end: Point,
    ) -> Option<Match> {
        // Find start and end of match.
        let match_start = self.regex_search(start, end, &mut regex.left_fdfa)?;
        let match_end = self.regex_search(match_start, start, &mut regex.left_rdfa)?;

        // feathercritty: as in `regex_search_right`, a search from `end` to itself whose first
        // fullwidth skip leaves `end` (a wide char's spacer) reads on around the buffer, and
        // upstream returned whatever it found there. We keep the glyph completion at `end` (a
        // wrapped wide char is reported at its leading spacer, `expand_wide`) and return none
        // for a match starting further than that. Both searches run as upstream runs them.
        // When merging upstream: if upstream bounds its matches by `end` itself, drop this.
        let to_match = self.search_distance(start, match_start, Direction::Left);
        if to_match > self.search_distance(start, end, Direction::Left) {
            let glyph_end = self.expand_wide(end, Direction::Left);
            if to_match > self.search_distance(start, glyph_end, Direction::Left) {
                return None;
            }
        }

        Some(match_start..=match_end)
    }

    /// Find the next regex match to the right of the origin point.
    ///
    /// The origin is always included in the regex.
    pub fn regex_search_right(
        &self,
        regex: &mut RegexSearch,
        start: Point,
        end: Point,
    ) -> Option<Match> {
        // Find start and end of match.
        let match_end = self.regex_search(start, end, &mut regex.right_fdfa)?;
        let match_start = self.regex_search(match_end, start, &mut regex.right_rdfa)?;

        // feathercritty: upstream returns whatever the search found, even past `end`. A search
        // from `end` to itself (the last search of every rightward `RegexIter`) on a leading
        // spacer skips onto the next line first and reads on around the buffer, so a rightward
        // `search_next` limited to a line ending in a leading spacer returned a match from the
        // next line. We keep upstream's glyph completion: the character at `end` is read whole,
        // so a match may end on the spacer of a wide char that wrapped after `end`
        // (`expand_wide`). A match ending further than that is outside the range, and we
        // return none. When `end` is a stale leading spacer (its next line does not start with a
        // wide char), that completion lands on an unrelated cell of the next line, so a match
        // ending there is reported at `end` instead. Both searches run as upstream runs them,
        // so whether there is a match stays upstream's; only its position changes.
        // When merging upstream: if upstream bounds its matches by `end` itself, drop this.
        let to_end = self.search_distance(start, end, Direction::Right);
        let to_match = self.search_distance(start, match_end, Direction::Right);
        if to_match > to_end {
            let glyph_end = self.glyph_end(end);
            if to_match > self.search_distance(start, glyph_end, Direction::Right)
                || self.search_distance(start, match_start, Direction::Right) > to_end
            {
                return None;
            }
            if self.stale_spacer_end(end) {
                return Some(match_start..=end);
            }
        }

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
            Direction::Right => GridIterator::next,
            Direction::Left => GridIterator::prev,
        };

        // Get start state for the DFA.
        let regex_anchored = if regex.match_all { Anchored::Yes } else { Anchored::No };
        let input = Input::new(&[]).anchored(regex_anchored);
        let mut state = regex.dfa.start_state_forward(&mut regex.cache, &input).unwrap();

        let mut iter = self.grid.iter_from(start);
        let mut regex_match = None;

        // feathercritty: upstream borrowed `cell` from the grid (`&Cell`); the iterator now
        // yields owned cells and `iter.cell()` borrows from the iterator, so `cell` is a copy.
        let mut cell = iter.cell().clone();
        self.skip_fullwidth(&mut iter, &mut cell, regex.direction);
        let mut c = cell.c;

        // feathercritty: upstream starts with `done = false`, so a search whose first fullwidth
        // skip jumps over `end` (start on a wrapped wide char's spacer or leading spacer, end on
        // the wide char) never stops. We stop once the skipped cells contain `end`, as in the loop
        // below.
        let mut done = self.skips_over(start, iter.point(), end, regex.direction);

        // feathercritty: upstream has no bound on the number of cells it visits, so any way of
        // missing `end` (see the fullwidth skip below; a stale leading spacer, one whose next
        // line does not start with a wide char, skips a real cell) loops forever. Every step
        // below advances at least one cell, so after as many steps as there are cells from
        // `start` to `end` the search is on or past `end` and stops there. When `start == end`
        // and the first fullwidth skip leaves it, upstream reads around the whole buffer back to
        // `end`, so that case gets one full lap. Where upstream stops without passing `end`, it
        // stops before this bound, so results there are unchanged. Only cell advances count:
        // the `continue 'outer` retry after an empty match feeds the same character again
        // without advancing, at most once per character, so it does not decrement.
        // When merging upstream: keep this bound unless upstream bounds the loop itself.
        let mut steps_left = match self.search_distance(start, end, regex.direction) {
            0 => self.total_lines() * self.columns(),
            distance => distance,
        };
        let mut last_wrapped = iter.cell().flags.contains(Flags::WRAPLINE);

        let mut point = iter.point();
        let mut last_point = point;
        let mut consumed_bytes = 0;

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
            let mut cell = match next(&mut iter) {
                Some(Indexed { cell, .. }) => cell,
                None => {
                    // Wrap around to other end of the scrollback buffer.
                    let line = topmost_line - point.line + screen_lines - 1;
                    let start = Point::new(line, last_column - point.column);
                    iter = self.grid.iter_from(start);
                    // feathercritty: a copy, as for the first cell above.
                    iter.cell().clone()
                },
            };

            // Check for completion before potentially skipping over fullwidth characters.
            done = iter.point() == end;

            // feathercritty: the step bound, see `steps_left` above.
            steps_left = steps_left.saturating_sub(1);
            done |= steps_left == 0;

            // feathercritty: upstream only checks the cell before the fullwidth skip (above) and
            // the one after it (`point == end` at the top of the loop). Across a wrapped wide
            // char the skip moves two cells: left from its spacer to the leading spacer, right
            // from the leading spacer to its spacer, so it never stands on the wide char itself.
            // An `end` on that cell (a leftward `search_next` with `max_lines` puts `end` at
            // column 0 of any line) was missed and the search wrapped around the buffer forever,
            // or returned a match from beyond `end`. We also stop when the skip jumped over `end`.
            // When merging upstream: if `skip_fullwidth` or this completion check changed, re-run
            // the `wrapped_wide_*` tests; if they pass without this, drop it.
            let skip_from = iter.point();
            self.skip_fullwidth(&mut iter, &mut cell, regex.direction);

            c = cell.c;
            done |= self.skips_over(skip_from, iter.point(), end, regex.direction);
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

    /// Whether `end` lies strictly between `from` and `to`, walking from `from` in `direction`.
    ///
    /// feathercritty: not in upstream. Used where a fullwidth skip moves a search point over more
    /// than one cell, so `end` cannot be jumped over. These skips move at most three cells, which
    /// bounds the walk.
    fn skips_over(&self, from: Point, to: Point, end: Point, direction: Direction) -> bool {
        let step = |point: Point| match direction {
            Direction::Right => point.add(self, Boundary::None, 1),
            Direction::Left => point.sub(self, Boundary::None, 1),
        };

        let mut point = from;
        for _ in 0..3 {
            if point == to {
                return false;
            }
            point = step(point);
            if point == end && point != to {
                return true;
            }
        }

        false
    }

    /// Number of cells from `from` to `to`, walking in `direction` and wrapping around the
    /// buffer like the search does; 0 when they are equal.
    ///
    /// feathercritty: not in upstream. Bounds the regex search loop and `RegexIter`.
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

    /// The cell where a rightward search lands after reading the character at `point`: the
    /// spacer of a wide char, the second cell after a leading spacer (stale or not, as
    /// `skip_fullwidth` steps), and `point` itself otherwise. This is `expand_wide` for
    /// characters that wrapped as they should; unlike `expand_wide` it follows a stale leading
    /// spacer in the middle of a line, and it stays inside the grid on the bottommost line.
    ///
    /// feathercritty: not in upstream; see `regex_search_right` and `RegexIter::next`.
    fn glyph_end(&self, point: Point) -> Point {
        // feathercritty (compact history): `point` can be in history, read through `Grid::cell`.
        let flags = self.grid.cell(point).flags;
        let last = Point::new(self.bottommost_line(), self.last_column());
        let step =
            |point: Point| if point == last { point } else { point.add(self, Boundary::None, 1) };
        if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            step(step(point))
        } else if flags.contains(Flags::WIDE_CHAR) && point.column < self.last_column() {
            step(point)
        } else {
            point
        }
    }

    /// Whether two matches share a cell (in grid order, wrapping around the buffer).
    ///
    /// feathercritty: not in upstream; see `RegexIter::next`.
    fn matches_overlap(&self, a: &Match, b: &Match) -> bool {
        let within = |point: Point, m: &Match| {
            self.search_distance(*m.start(), point, Direction::Right)
                <= self.search_distance(*m.start(), *m.end(), Direction::Right)
        };
        within(*a.start(), b) || within(*b.start(), a)
    }

    /// Whether `end` is a stale leading spacer: its next line does not start with a wide char
    /// (a resize or an erase left it behind).
    ///
    /// feathercritty: not in upstream; see `regex_search_right`.
    fn stale_spacer_end(&self, end: Point) -> bool {
        // feathercritty (compact history): both points can be in history, read through
        // `Grid::cell`.
        self.grid.cell(end).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && (end.line >= self.bottommost_line()
                || !self
                    .grid
                    .cell(Point::new(end.line + 1, Column(0)))
                    .flags
                    .contains(Flags::WIDE_CHAR))
    }

    /// Advance a grid iterator over fullwidth characters.
    ///
    /// feathercritty: upstream took `cell: &mut &'a Cell` borrowed from the grid; the iterator
    /// yields owned cells now, so `cell` is an owned copy, replaced the same way.
    fn skip_fullwidth(
        &self,
        iter: &mut GridIterator<'_, Cell>,
        cell: &mut Cell,
        direction: Direction,
    ) {
        match direction {
            // In the alternate screen buffer there might not be a wide char spacer after a wide
            // char, so we only advance the iterator when the wide char is not in the last column.
            Direction::Right
                if cell.flags.contains(Flags::WIDE_CHAR)
                    && iter.point().column < self.last_column() =>
            {
                iter.next();
            },
            Direction::Right if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                if let Some(Indexed { cell: new_cell, .. }) = iter.next() {
                    *cell = new_cell;
                }
                iter.next();
            },
            Direction::Left if cell.flags.contains(Flags::WIDE_CHAR_SPACER) => {
                if let Some(Indexed { cell: new_cell, .. }) = iter.prev() {
                    *cell = new_cell;
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

/// Iterator over regex matches.
pub struct RegexIter<'a, T> {
    point: Point,
    end: Point,
    direction: Direction,
    regex: &'a mut RegexSearch,
    term: &'a Term<T>,
    done: bool,
    // feathercritty: the start of the range, and the distance from it to the current origin
    // and to `end` (`Term::search_distance`), for the progress bound in `next`.
    start: Point,
    travelled: usize,
    end_distance: usize,
    // feathercritty: the first and the last match returned, and whether the origin of the
    // next search was moved forward by the origin rule, for the checks in `next`.
    first: Option<Match>,
    last: Option<Match>,
    forced: bool,
}

impl<'a, T> RegexIter<'a, T> {
    pub fn new(
        start: Point,
        end: Point,
        direction: Direction,
        term: &'a Term<T>,
        regex: &'a mut RegexSearch,
    ) -> Self {
        let end_distance = term.search_distance(start, end, direction);
        Self {
            point: start,
            done: false,
            end,
            direction,
            term,
            regex,
            start,
            travelled: 0,
            end_distance,
            first: None,
            last: None,
            forced: false,
        }
    }

    /// Skip one cell, advancing the origin point to the next one.
    fn skip(&mut self) {
        let from = self.point;

        self.point = self.term.expand_wide(self.point, self.direction);

        self.point = match self.direction {
            Direction::Right => self.point.add(self.term, Boundary::None, 1),
            Direction::Left => self.point.sub(self.term, Boundary::None, 1),
        };

        // feathercritty: upstream only stops when a match ends exactly on `end`. Going left from a
        // match that ends on a wrapped wide char's spacer, `expand_wide` moves to the leading
        // spacer on the previous line, so the new origin can land past an `end` on the wide char
        // or its leading spacer. The next search then wrapped around the buffer and the iterator
        // cycled through its matches forever. The match already covers `end`, so we stop.
        // When merging upstream: if `RegexIter` changed, re-run `wrapped_wide_regex_iter` and
        // `wrapped_wide_fuzz`; if they pass without this, drop it.
        if self.term.skips_over(from, self.point, self.end, self.direction) {
            self.done = true;
        }
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
        // feathercritty: upstream returns every match its searches find. In a range that wraps
        // around the whole buffer the last search can find the first match again, and after the
        // origin rule in `next_unfiltered` moved an origin forward (next to a stale spacer) the
        // search can find a match overlapping the last one. We skip a match equal to the first
        // or the last one returned, and one overlapping the last one when its search started
        // from a moved origin. Other overlaps are upstream's: going left, each search starts
        // inside the previous match (one cell left of its right end), so consecutive leftward
        // matches can overlap, and those stay. Each search still moves the origin forward, so
        // this stays bounded.
        // When merging upstream: keep this with the origin rule in `next_unfiltered`.
        loop {
            let forced = self.forced;
            let regex_match = self.next_unfiltered()?;
            let same = |other: &Option<Match>| other.as_ref() == Some(&regex_match);
            let overlaps_last = forced
                && self
                    .last
                    .as_ref()
                    .is_some_and(|last| self.term.matches_overlap(last, &regex_match));
            if same(&self.first) || same(&self.last) || overlaps_last {
                continue;
            }

            if self.first.is_none() {
                self.first = Some(regex_match.clone());
            }
            self.last = Some(regex_match.clone());
            return Some(regex_match);
        }
    }
}

impl<T> RegexIter<'_, T> {
    /// The next match, before the overlap check in `next`; upstream's `next`.
    fn next_unfiltered(&mut self) -> Option<Match> {
        if self.done {
            return None;
        }

        // Since the end itself might be a single cell match, we search one more time.
        if self.point == self.end {
            self.done = true;
        }

        let origin = self.point;
        let mut regex_match = self.next_match()?;

        // feathercritty: going left, the match's right end comes from a second search back to
        // the origin. When it starts on a stale leading spacer (one whose next line does not
        // start with a wide char), upstream reads the next cell in its place and steps over the
        // cell after it. The match then ends past the origin, possibly beyond the start of this
        // iterator's range. Upstream returned that. We cut such a match at the range's start.
        // Ranges that cover the whole buffer (unbounded searches) are never cut.
        // When merging upstream: if upstream checks for stale leading spacers in
        // `skip_fullwidth`, drop this.
        // If the match starts after the range's start in grid order (a range that wraps around
        // the buffer's top), cutting it would turn it around; we stop instead.
        if self.direction == Direction::Left
            && self.term.search_distance(self.start, *regex_match.end(), Direction::Left)
                > self.end_distance
        {
            if *regex_match.start() > self.start {
                self.done = true;
                return None;
            }
            regex_match = *regex_match.start()..=self.start;
        }

        self.point = *regex_match.end();

        // feathercritty: upstream only stops when the match ends right on `end`. Going right, a
        // match can cover `end` and end one or two cells past it: when `end` is a wide char, the
        // match ends on its spacer; when `end` is a leading spacer, the match ends on the
        // wrapped wide char's spacer on the next line. Upstream then moved the origin past `end`,
        // the next search wrapped around the buffer and the iterator cycled through its matches
        // forever. We also stop when the match ends on the last cell of the glyph at `end`
        // (`expand_wide`), unless this search started inside that glyph after `end` (the range
        // then wraps around the whole buffer, so `end` is still ahead). Going left, the match's
        // end is on the far side from `end`; `skip` handles that direction.
        // When merging upstream: if `RegexIter` changed, re-run `wrapped_wide_regex_iter` and
        // `wrapped_wide_fuzz`; if they pass without this, drop it.
        let past_end = match self.direction {
            Direction::Right => {
                let glyph_end = self.term.glyph_end(self.end);
                let origin_in_glyph = origin == glyph_end
                    || self.term.skips_over(self.end, glyph_end, origin, Direction::Right);
                self.point == glyph_end && !origin_in_glyph
            },
            Direction::Left => false,
        };

        if self.point == self.end || past_end {
            // Stop when the match terminates right on the end limit.
            self.done = true;
        } else {
            // Move the new search origin past the match.
            self.skip();
        }

        // feathercritty: upstream does not check that the origin moves forward, so any way of
        // moving it past `end` (see above; a stale leading spacer, one whose next line does not
        // start with a wide char, makes a match end behind its origin) cycles forever. Every new
        // origin must lie further from the start than the last one and not past `end`;
        // otherwise we stop. Each search then moves at least one cell, which bounds the iterator
        // by the size of its range. Where upstream's origins stay inside the range they always
        // move forward, so results there are unchanged.
        // When merging upstream: keep this bound unless upstream bounds `RegexIter` itself.
        //
        // A new origin that is not further than the last one (a match next to a stale spacer
        // that ends past its origin, see `stale_spacer_match_past_origin`) does not stop the
        // iterator, which would drop the rest of the range: the next search starts one cell past
        // the farther of the last origin and this match's far end instead. Origins still move
        // forward by at least one cell per match.
        self.forced = false;
        if !self.done {
            let distance = |point| self.term.search_distance(self.start, point, self.direction);
            let mut travelled = distance(self.point);
            if travelled <= self.travelled {
                self.forced = true;
                let far_end = match self.direction {
                    Direction::Right => *regex_match.end(),
                    Direction::Left => *regex_match.start(),
                };
                let (from, from_travelled) = if distance(far_end) > self.travelled
                    && distance(far_end) <= self.end_distance
                {
                    (far_end, distance(far_end))
                } else {
                    (origin, self.travelled)
                };
                travelled = from_travelled + 1;
                self.point = match self.direction {
                    Direction::Right => from.add(self.term, Boundary::None, 1),
                    Direction::Left => from.sub(self.term, Boundary::None, 1),
                };
            }

            if travelled > self.end_distance {
                self.done = true;
            } else {
                self.travelled = travelled;
            }
        }

        Some(regex_match)
    }
}

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
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => panic!("{what} did not terminate"),
            Err(RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
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
        // A match covering the iterator's end without ending on it used to move the iterator's
        // origin past its end, so it cycled through the buffer's matches forever.
        let matches = terminates("RegexIter over a wrapped wide char", || {
            let term = wrapped_wide_term();
            let mut regex = RegexSearch::new("字").unwrap();
            let mut iter = |start, end, direction| {
                RegexIter::new(start, end, direction, &term, &mut regex)
                    .take(10)
                    .collect::<Vec<_>>()
            };

            // Left, ending on the wide character or its leading spacer.
            let left_wide = iter(
                Point::new(Line(3), Column(2)),
                Point::new(Line(2), Column(0)),
                Direction::Left,
            );
            let left_leading = iter(
                Point::new(Line(3), Column(2)),
                Point::new(Line(1), Column(2)),
                Direction::Left,
            );

            // Right, ending on the leading spacer or the wide character.
            let right_leading = iter(
                Point::new(Line(0), Column(0)),
                Point::new(Line(1), Column(2)),
                Direction::Right,
            );
            let right_wide = iter(
                Point::new(Line(0), Column(0)),
                Point::new(Line(2), Column(0)),
                Direction::Right,
            );

            [left_wide, left_leading, right_leading, right_wide]
        });

        let wide = Point::new(Line(1), Column(2))..=Point::new(Line(2), Column(1));
        assert_eq!(matches, [vec![wide.clone()], vec![wide.clone()], vec![wide.clone()], vec![
            wide
        ]]);
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

    /// Whether both ends of `m` lie in the range a search from `start` to `end` in `direction`
    /// covers, allowing only the documented glyph completion at each end: the character at
    /// `end` is read whole (going right up to `glyph_end`, going left up to `expand_wide`), and
    /// the second search, back to `start`, completes the character at `start` the same way in
    /// the other direction.
    fn in_search_range(
        term: &Term<VoidListener>,
        start: Point,
        end: Point,
        direction: Direction,
        m: &Match,
    ) -> bool {
        let back = match direction {
            Direction::Right => Direction::Left,
            Direction::Left => Direction::Right,
        };
        let completion = |point: Point, direction: Direction| match direction {
            Direction::Right => term.glyph_end(point),
            Direction::Left => term.expand_wide(point, Direction::Left),
        };

        let ahead = term.search_distance(start, end, direction).max(term.search_distance(
            start,
            completion(end, direction),
            direction,
        ));
        let behind = term.search_distance(start, completion(start, back), back);
        [*m.start(), *m.end()].into_iter().all(|point| {
            term.search_distance(start, point, direction) <= ahead
                || term.search_distance(start, point, back) <= behind
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
        let cap = points.len() + 2;

        let grid = format!(
            "{columns}x{lines} history {history} boundaries {boundaries:?} plain {plain_wide}"
        );
        for pattern in ["字", "a", "字a", "b字", " ", "x", "", "a*"] {
            let mut regex = RegexSearch::new(pattern).unwrap();
            for &start in &points {
                for &end in &points {
                    let case = format!("{grid} pattern {pattern:?} {start:?} -> {end:?}");
                    let _ = tx.send(case.clone());

                    let left = term.regex_search_left(&mut regex, start, end);
                    let right = term.regex_search_right(&mut regex, start, end);
                    for (direction, found) in [(Direction::Left, left), (Direction::Right, right)] {
                        if let Some(m) = found {
                            assert!(
                                in_search_range(&term, start, end, direction, &m),
                                "match outside the range: {case} {direction:?} {m:?}"
                            );
                        }
                    }

                    for direction in [Direction::Left, Direction::Right] {
                        let matches = RegexIter::new(start, end, direction, &term, &mut regex)
                            .take(cap)
                            .collect::<Vec<_>>();
                        assert!(matches.len() < cap, "RegexIter cycled: {case} {direction:?}");
                        for (i, m) in matches.iter().enumerate() {
                            assert!(
                                in_search_range(&term, start, end, direction, m),
                                "RegexIter match outside the range: {case} {direction:?} {m:?}"
                            );
                            assert!(
                                !matches[..i].contains(m),
                                "RegexIter repeated a match: {case} {direction:?} {matches:?}"
                            );
                            // Going left, each search starts inside the previous match (as
                            // upstream does), so leftward matches may overlap.
                            if i > 0 && direction == Direction::Right {
                                assert!(
                                    !term.matches_overlap(&matches[i - 1], m),
                                    "RegexIter matches overlap: {case} {direction:?} {matches:?}"
                                );
                            }
                        }
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
                            term.search_next(&mut regex, start, direction, side, max_lines);
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
        // origin and `RegexIter` cycled between (16,0) and (15,10) forever.
        let result = terminates("search_next over a stale leading spacer", || {
            let term = stale_spacer_soak_term();
            let mut regex = RegexSearch::new(" ").unwrap();
            let origin = Point::new(Line(15), Column(8));
            term.search_next(&mut regex, origin, Direction::Left, Side::Right, Some(5))
        });

        let point = Point::new(Line(20), Column(10));
        assert_eq!(result, Some(point..=point));
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
        // starts and ends on that spacer; the first fullwidth skip moved past it and the search
        // read on into line 2, returning the 'c' there, outside the range.
        let term = wrapped_wide_term();
        let mut regex = RegexSearch::new("[a-z]+").unwrap();
        let origin = Point::new(Line(1), Column(1));
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Left, Some(0));

        let match_start = Point::new(Line(1), Column(0));
        let match_end = Point::new(Line(1), Column(1));
        assert_eq!(result, Some(match_start..=match_end));

        // The search from the leading spacer to itself reads the wrapped wide char only.
        let mut regex = RegexSearch::new("字").unwrap();
        let spacer = Point::new(Line(1), Column(2));
        let wide = term.regex_search_right(&mut regex, spacer, spacer);
        assert_eq!(wide, Some(spacer..=Point::new(Line(2), Column(1))));
        let mut regex = RegexSearch::new("[a-z]").unwrap();
        assert_eq!(term.regex_search_right(&mut regex, spacer, spacer), None);
    }

    #[test]
    fn range_ending_on_stale_leading_spacer() {
        // Differential soak seed 543: a rightward search limited to line 1, which ends in a stale
        // leading spacer. Reading on as if a wide char wrapped there returned a match ending on
        // line 2's second cell. The search still reads the cell after the spacer (line 2's first
        // cell) in its place as upstream does, but a match ending there is reported at the
        // spacer, and one that lies entirely on line 2 is not returned.
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
        let match_end = Point::new(Line(1), Column(2));
        assert_eq!(result, Some(match_start..=match_end));

        let mut regex = RegexSearch::new(" ").unwrap();
        let result = term.search_next(&mut regex, origin, Direction::Right, Side::Left, Some(0));
        assert_eq!(result, None);
    }

    #[test]
    fn stale_spacer_at_range_start() {
        // Differential soak seed 791: a leftward search whose range starts next to a stale
        // leading spacer in the middle of a line. The second search, back from the match start
        // to the origin, reads past the spacer onto the next line, and the match ended outside
        // the range.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("abcd", "...."),
            ("ab  ", "..L."),
            ("字 cd", "Ws.."),
        ]);
        let mut regex = RegexSearch::new(" ").unwrap();
        let start = Point::new(Line(1), Column(3));
        let end = Point::new(Line(1), Column(0));
        let matches = RegexIter::new(start, end, Direction::Left, &term, &mut regex)
            .take(10)
            .collect::<Vec<_>>();
        assert!(!matches.is_empty());
        for m in &matches {
            assert_eq!((m.start().line, m.end().line), (Line(1), Line(1)), "{matches:?}");
        }

        let result = term.search_next(&mut regex, start, Direction::Left, Side::Right, Some(0));
        let result = result.unwrap();
        assert_eq!((result.start().line, result.end().line), (Line(1), Line(1)));
    }

    #[test]
    fn stale_spacer_match_past_origin() {
        // Going left from (3,2), the search from the stale leading spacer at (2,2) reads (3,0) in
        // its place, so that match ends at (3,1), past its origin but inside the range. The next
        // origin then lay behind the previous one; 0.26.0 cycled there, and the first version of
        // the progress check stopped and dropped the blank at (0,1).
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("x x", "..."),
            ("abc", "..."),
            ("ab ", "..L"),
            (" cd", "..."),
            ("abc", "..."),
        ]);
        let mut regex = RegexSearch::new(" ").unwrap();
        let start = Point::new(Line(3), Column(2));
        let end = Point::new(Line(0), Column(0));
        let matches = RegexIter::new(start, end, Direction::Left, &term, &mut regex)
            .take(20)
            .collect::<Vec<_>>();

        let blank = Point::new(Line(0), Column(1));
        assert!(matches.len() < 20, "RegexIter cycled: {matches:?}");
        assert!(matches.contains(&(blank..=blank)), "{matches:?}");
        assert_eq!(
            matches.first(),
            Some(&(Point::new(Line(3), Column(0))..=Point::new(Line(3), Column(0))))
        );
    }

    #[test]
    fn regex_iter_skips_repeated_matches() {
        // In a range that wraps around the whole buffer and starts inside a wrapped wide char,
        // the last search found the first match again.
        #[rustfmt::skip]
        let term = term_from_rows(0, &[
            ("a ", ".L"),
            ("字 ", "Ws"),
        ]);
        let mut regex = RegexSearch::new("字").unwrap();
        let start = Point::new(Line(1), Column(0));
        let end = Point::new(Line(0), Column(1));
        let matches = RegexIter::new(start, end, Direction::Right, &term, &mut regex)
            .take(10)
            .collect::<Vec<_>>();
        assert_eq!(matches, [Point::new(Line(0), Column(1))..=Point::new(Line(1), Column(1))]);
    }
}
