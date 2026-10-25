// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Tests for the Grid.
//!
//! feathercritty: upstream indexed `grid[line]`; screen lines are reached through
//! `Grid::screen`/`Grid::screen_mut` and history lines through `row` (a `RowReader`). The tests
//! after `accurate_size_hint` cover the screen/history split.

use super::*;

use std::collections::VecDeque;

use crate::grid::compact::{self, RowHeader};
use crate::term::cell::Cell;

/// Line `line` of `grid`, screen or history, as an owned row.
fn row<T: HistoryCell>(grid: &Grid<T>, line: i32) -> Row<T> {
    RowReader::new().row(grid, Line(line)).clone()
}

impl GridCell for usize {
    fn is_empty(&self) -> bool {
        *self == 0
    }

    fn reset(&mut self, template: &Self) {
        *self = *template;
    }

    fn flags(&self) -> &Flags {
        unimplemented!();
    }

    fn flags_mut(&mut self) -> &mut Flags {
        unimplemented!();
    }
}

/// feathercritty: test-only record format for `usize` cells: the shared header (`P = W`, no runs,
/// marks or extras), then each cell as a varint.
impl HistoryCell for usize {
    type Extra = ();

    fn encode(row: &Row<Self>, out: &mut Vec<u8>, _extras: &mut VecDeque<()>) -> RowHeader {
        let header = RowHeader {
            width: row.len() as u32,
            wraps: false,
            clear: row.is_clear(),
            extras: 0,
            len: row.len() as u32,
        };
        compact::write_header(out, &header, 0, 0);
        for &value in row {
            compact::write_varint(out, value);
        }
        header
    }

    fn decode(bytes: &[u8], _extras: &[()], into: &mut Row<Self>) -> usize {
        let mut reader = compact::RecordReader::new(bytes);
        let width = reader.layout().header.width as usize;
        for cell in compact::prepare_row(into, width) {
            *cell = reader.varint();
        }
        reader.position()
    }
}

#[test]
fn usize_history_cell_roundtrip() {
    let mut row = Row::new(5);
    for (col, value) in [0, 1, 127, 128, usize::MAX].into_iter().enumerate() {
        row[Column(col)] = value;
    }
    let mut out = vec![9];
    let header = usize::encode(&row, &mut out, &mut VecDeque::new());
    assert_eq!(compact::read_header(&out[1..]), header);
    assert_eq!((header.width, header.clear, header.extras), (5, false, 0));

    let mut into = Row::new(2);
    usize::decode(&out[1..], &[], &mut into);
    assert_eq!(into, row);
    assert_eq!(into.occ, 5);
    assert!(usize::encode(&Row::new(3), &mut out, &mut VecDeque::new()).clear);
}

// Scroll up moves lines upward.
#[test]
fn scroll_up() {
    let mut grid = Grid::<usize>::new(10, 1, 0);
    for i in 0..10 {
        grid.screen_mut()[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_up::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid.screen()[Line(0)][Column(0)], 2);
    assert_eq!(grid.screen()[Line(0)].occ, 1);
    assert_eq!(grid.screen()[Line(1)][Column(0)], 3);
    assert_eq!(grid.screen()[Line(1)].occ, 1);
    assert_eq!(grid.screen()[Line(2)][Column(0)], 4);
    assert_eq!(grid.screen()[Line(2)].occ, 1);
    assert_eq!(grid.screen()[Line(3)][Column(0)], 5);
    assert_eq!(grid.screen()[Line(3)].occ, 1);
    assert_eq!(grid.screen()[Line(4)][Column(0)], 6);
    assert_eq!(grid.screen()[Line(4)].occ, 1);
    assert_eq!(grid.screen()[Line(5)][Column(0)], 7);
    assert_eq!(grid.screen()[Line(5)].occ, 1);
    assert_eq!(grid.screen()[Line(6)][Column(0)], 8);
    assert_eq!(grid.screen()[Line(6)].occ, 1);
    assert_eq!(grid.screen()[Line(7)][Column(0)], 9);
    assert_eq!(grid.screen()[Line(7)].occ, 1);
    assert_eq!(grid.screen()[Line(8)][Column(0)], 0); // was 0.
    assert_eq!(grid.screen()[Line(8)].occ, 0);
    assert_eq!(grid.screen()[Line(9)][Column(0)], 0); // was 1.
    assert_eq!(grid.screen()[Line(9)].occ, 0);
}

// Scroll down moves lines downward.
#[test]
fn scroll_down() {
    let mut grid = Grid::<usize>::new(10, 1, 0);
    for i in 0..10 {
        grid.screen_mut()[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_down::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid.screen()[Line(0)][Column(0)], 0); // was 8.
    assert_eq!(grid.screen()[Line(0)].occ, 0);
    assert_eq!(grid.screen()[Line(1)][Column(0)], 0); // was 9.
    assert_eq!(grid.screen()[Line(1)].occ, 0);
    assert_eq!(grid.screen()[Line(2)][Column(0)], 0);
    assert_eq!(grid.screen()[Line(2)].occ, 1);
    assert_eq!(grid.screen()[Line(3)][Column(0)], 1);
    assert_eq!(grid.screen()[Line(3)].occ, 1);
    assert_eq!(grid.screen()[Line(4)][Column(0)], 2);
    assert_eq!(grid.screen()[Line(4)].occ, 1);
    assert_eq!(grid.screen()[Line(5)][Column(0)], 3);
    assert_eq!(grid.screen()[Line(5)].occ, 1);
    assert_eq!(grid.screen()[Line(6)][Column(0)], 4);
    assert_eq!(grid.screen()[Line(6)].occ, 1);
    assert_eq!(grid.screen()[Line(7)][Column(0)], 5);
    assert_eq!(grid.screen()[Line(7)].occ, 1);
    assert_eq!(grid.screen()[Line(8)][Column(0)], 6);
    assert_eq!(grid.screen()[Line(8)].occ, 1);
    assert_eq!(grid.screen()[Line(9)][Column(0)], 7);
    assert_eq!(grid.screen()[Line(9)].occ, 1);
}

#[test]
fn scroll_down_with_history() {
    let mut grid = Grid::<usize>::new(10, 1, 1);
    // feathercritty: upstream `grid.increase_scroll_limit(1)` (removed): one blank history row.
    grid.history.push_newest(&Row::new(1));
    for i in 0..10 {
        grid.screen_mut()[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_down::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid.screen()[Line(0)][Column(0)], 0); // was 8.
    assert_eq!(grid.screen()[Line(0)].occ, 0);
    assert_eq!(grid.screen()[Line(1)][Column(0)], 0); // was 9.
    assert_eq!(grid.screen()[Line(1)].occ, 0);
    assert_eq!(grid.screen()[Line(2)][Column(0)], 0);
    assert_eq!(grid.screen()[Line(2)].occ, 1);
    assert_eq!(grid.screen()[Line(3)][Column(0)], 1);
    assert_eq!(grid.screen()[Line(3)].occ, 1);
    assert_eq!(grid.screen()[Line(4)][Column(0)], 2);
    assert_eq!(grid.screen()[Line(4)].occ, 1);
    assert_eq!(grid.screen()[Line(5)][Column(0)], 3);
    assert_eq!(grid.screen()[Line(5)].occ, 1);
    assert_eq!(grid.screen()[Line(6)][Column(0)], 4);
    assert_eq!(grid.screen()[Line(6)].occ, 1);
    assert_eq!(grid.screen()[Line(7)][Column(0)], 5);
    assert_eq!(grid.screen()[Line(7)].occ, 1);
    assert_eq!(grid.screen()[Line(8)][Column(0)], 6);
    assert_eq!(grid.screen()[Line(8)].occ, 1);
    assert_eq!(grid.screen()[Line(9)][Column(0)], 7);
    assert_eq!(grid.screen()[Line(9)].occ, 1);
}

// Test that GridIterator works.
#[test]
fn test_iter() {
    // feathercritty: the iterator yields owned cells (upstream `Indexed<&usize>`).
    let assert_indexed = |value: usize, indexed: Option<Indexed<usize>>| {
        assert_eq!(Some(value), indexed.map(|indexed| indexed.cell));
    };

    let mut grid = Grid::<usize>::new(5, 5, 0);
    for i in 0..5 {
        for j in 0..5 {
            grid.screen_mut()[Line(i)][Column(j)] = i as usize * 5 + j;
        }
    }

    let mut iter = grid.iter_from(Point::new(Line(0), Column(0)));

    assert_eq!(None, iter.prev());
    assert_indexed(1, iter.next());
    assert_eq!(Column(1), iter.point().column);
    assert_eq!(0, iter.point().line);

    assert_indexed(2, iter.next());
    assert_indexed(3, iter.next());
    assert_indexed(4, iter.next());

    // Test line-wrapping.
    assert_indexed(5, iter.next());
    assert_eq!(Column(0), iter.point().column);
    assert_eq!(1, iter.point().line);

    assert_indexed(4, iter.prev());
    assert_eq!(Column(4), iter.point().column);
    assert_eq!(0, iter.point().line);

    // Make sure iter.cell() returns the current iterator position.
    assert_eq!(&4, iter.cell());

    // Test that iter ends at end of grid.
    let mut final_iter = grid.iter_from(Point { line: Line(4), column: Column(4) });
    assert_eq!(None, final_iter.next());
    assert_indexed(23, final_iter.prev());
}

#[test]
fn shrink_reflow() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = cell('2');
    grid.screen_mut()[Line(0)][Column(2)] = cell('3');
    grid.screen_mut()[Line(0)][Column(3)] = cell('4');
    grid.screen_mut()[Line(0)][Column(4)] = cell('5');

    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(row(&grid, -2).len(), 2);
    assert_eq!(row(&grid, -2)[Column(0)], cell('1'));
    assert_eq!(row(&grid, -2)[Column(1)], wrap_cell('2'));

    assert_eq!(row(&grid, -1).len(), 2);
    assert_eq!(row(&grid, -1)[Column(0)], cell('3'));
    assert_eq!(row(&grid, -1)[Column(1)], wrap_cell('4'));

    assert_eq!(grid.screen()[Line(0)].len(), 2);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('5'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], Cell::default());
}

#[test]
fn shrink_reflow_twice() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = cell('2');
    grid.screen_mut()[Line(0)][Column(2)] = cell('3');
    grid.screen_mut()[Line(0)][Column(3)] = cell('4');
    grid.screen_mut()[Line(0)][Column(4)] = cell('5');

    grid.resize(true, 1, 4);
    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(row(&grid, -2).len(), 2);
    assert_eq!(row(&grid, -2)[Column(0)], cell('1'));
    assert_eq!(row(&grid, -2)[Column(1)], wrap_cell('2'));

    assert_eq!(row(&grid, -1).len(), 2);
    assert_eq!(row(&grid, -1)[Column(0)], cell('3'));
    assert_eq!(row(&grid, -1)[Column(1)], wrap_cell('4'));

    assert_eq!(grid.screen()[Line(0)].len(), 2);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('5'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], Cell::default());
}

#[test]
fn shrink_reflow_empty_cell_inside_line() {
    let mut grid = Grid::<Cell>::new(1, 5, 3);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = Cell::default();
    grid.screen_mut()[Line(0)][Column(2)] = cell('3');
    grid.screen_mut()[Line(0)][Column(3)] = cell('4');
    grid.screen_mut()[Line(0)][Column(4)] = Cell::default();

    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(row(&grid, -1).len(), 2);
    assert_eq!(row(&grid, -1)[Column(0)], cell('1'));
    assert_eq!(row(&grid, -1)[Column(1)], wrap_cell(' '));

    assert_eq!(grid.screen()[Line(0)].len(), 2);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('3'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], cell('4'));

    grid.resize(true, 1, 1);

    assert_eq!(grid.total_lines(), 4);

    assert_eq!(row(&grid, -3).len(), 1);
    assert_eq!(row(&grid, -3)[Column(0)], wrap_cell('1'));

    assert_eq!(row(&grid, -2).len(), 1);
    assert_eq!(row(&grid, -2)[Column(0)], wrap_cell(' '));

    assert_eq!(row(&grid, -1).len(), 1);
    assert_eq!(row(&grid, -1)[Column(0)], wrap_cell('3'));

    assert_eq!(grid.screen()[Line(0)].len(), 1);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('4'));
}

#[test]
fn grow_reflow() {
    let mut grid = Grid::<Cell>::new(2, 2, 0);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = wrap_cell('2');
    grid.screen_mut()[Line(1)][Column(0)] = cell('3');
    grid.screen_mut()[Line(1)][Column(1)] = Cell::default();

    grid.resize(true, 2, 3);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(grid.screen()[Line(0)].len(), 3);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], cell('2'));
    assert_eq!(grid.screen()[Line(0)][Column(2)], cell('3'));

    // Make sure rest of grid is empty.
    assert_eq!(grid.screen()[Line(1)].len(), 3);
    assert_eq!(grid.screen()[Line(1)][Column(0)], Cell::default());
    assert_eq!(grid.screen()[Line(1)][Column(1)], Cell::default());
    assert_eq!(grid.screen()[Line(1)][Column(2)], Cell::default());
}

#[test]
fn grow_reflow_multiline() {
    let mut grid = Grid::<Cell>::new(3, 2, 0);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = wrap_cell('2');
    grid.screen_mut()[Line(1)][Column(0)] = cell('3');
    grid.screen_mut()[Line(1)][Column(1)] = wrap_cell('4');
    grid.screen_mut()[Line(2)][Column(0)] = cell('5');
    grid.screen_mut()[Line(2)][Column(1)] = cell('6');

    grid.resize(true, 3, 6);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(grid.screen()[Line(0)].len(), 6);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], cell('2'));
    assert_eq!(grid.screen()[Line(0)][Column(2)], cell('3'));
    assert_eq!(grid.screen()[Line(0)][Column(3)], cell('4'));
    assert_eq!(grid.screen()[Line(0)][Column(4)], cell('5'));
    assert_eq!(grid.screen()[Line(0)][Column(5)], cell('6'));

    // Make sure rest of grid is empty.
    for r in (1..3).map(Line::from) {
        assert_eq!(grid.screen()[r].len(), 6);
        for c in 0..6 {
            assert_eq!(grid.screen()[r][Column(c)], Cell::default());
        }
    }
}

#[test]
fn grow_reflow_disabled() {
    let mut grid = Grid::<Cell>::new(2, 2, 0);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = wrap_cell('2');
    grid.screen_mut()[Line(1)][Column(0)] = cell('3');
    grid.screen_mut()[Line(1)][Column(1)] = Cell::default();

    grid.resize(false, 2, 3);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(grid.screen()[Line(0)].len(), 3);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], wrap_cell('2'));
    assert_eq!(grid.screen()[Line(0)][Column(2)], Cell::default());

    assert_eq!(grid.screen()[Line(1)].len(), 3);
    assert_eq!(grid.screen()[Line(1)][Column(0)], cell('3'));
    assert_eq!(grid.screen()[Line(1)][Column(1)], Cell::default());
    assert_eq!(grid.screen()[Line(1)][Column(2)], Cell::default());
}

#[test]
fn shrink_reflow_disabled() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid.screen_mut()[Line(0)][Column(0)] = cell('1');
    grid.screen_mut()[Line(0)][Column(1)] = cell('2');
    grid.screen_mut()[Line(0)][Column(2)] = cell('3');
    grid.screen_mut()[Line(0)][Column(3)] = cell('4');
    grid.screen_mut()[Line(0)][Column(4)] = cell('5');

    grid.resize(false, 1, 2);

    assert_eq!(grid.total_lines(), 1);

    assert_eq!(grid.screen()[Line(0)].len(), 2);
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid.screen()[Line(0)][Column(1)], cell('2'));
}

#[test]
fn accurate_size_hint() {
    let grid = Grid::<Cell>::new(5, 5, 2);

    size_hint_matches_count(grid.iter_from(Point::new(Line(0), Column(0))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(2), Column(3))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(4), Column(4))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(4), Column(2))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(10), Column(10))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(2), Column(10))));

    let mut iterator = grid.iter_from(Point::new(Line(3), Column(1)));
    iterator.next();
    iterator.next();
    size_hint_matches_count(iterator);

    size_hint_matches_count(grid.display_iter());
}

fn size_hint_matches_count<T>(iter: impl Iterator<Item = T>) {
    let iterator = iter.into_iter();
    let (lower, upper) = iterator.size_hint();
    let count = iterator.count();
    assert_eq!(lower, count);
    assert_eq!(upper, Some(count));
}

// https://github.com/rust-lang/rust-clippy/pull/6375
#[allow(clippy::all)]
fn cell(c: char) -> Cell {
    let mut cell = Cell::default();
    cell.c = c;
    cell
}

fn wrap_cell(c: char) -> Cell {
    let mut cell = cell(c);
    cell.flags.insert(Flags::WRAPLINE);
    cell
}

// feathercritty: tests of the screen/history split.

/// A one-column grid whose lines hold their own number, top to bottom (`base`, `base + 1`, ...).
fn numbered(lines: usize, history: usize, base: usize) -> Grid<usize> {
    let mut grid = Grid::<usize>::new(lines, 1, history);
    for i in 0..lines {
        grid.screen_mut()[Line(i as i32)][Column(0)] = base + i;
    }
    grid
}

/// Every line of `grid`, topmost first, as the value of its first cell.
fn contents(grid: &Grid<usize>) -> Vec<usize> {
    (grid.topmost_line().0..=grid.bottommost_line().0).map(|l| row(grid, l)[Column(0)]).collect()
}

#[test]
fn scroll_up_moves_rows_into_history() {
    let mut grid = numbered(3, 2, 1);

    grid.scroll_up::<usize>(&(Line(0)..Line(3)), 1);
    assert_eq!(contents(&grid), vec![1, 2, 3, 0]);
    assert_eq!(grid.scrolled_lines(), 1);

    // At the limit the oldest row is evicted.
    grid.screen_mut()[Line(2)][Column(0)] = 4;
    grid.scroll_up::<usize>(&(Line(0)..Line(3)), 2);
    assert_eq!(contents(&grid), vec![2, 3, 4, 0, 0]);
    assert_eq!(grid.history_size(), 2);
    assert_eq!(grid.scrolled_lines(), 3);
}

#[test]
fn scroll_up_with_fixed_bottom_lines_keeps_them() {
    let mut grid = numbered(4, 5, 1);

    grid.scroll_up::<usize>(&(Line(0)..Line(3)), 1);

    assert_eq!(contents(&grid), vec![1, 2, 3, 0, 4]);
}

/// A count above the region's height: upstream rotated history rows through the region and reset
/// them, which is the same as scrolling one line at a time.
#[test]
fn scroll_up_above_region_height_pushes_reset_rows() {
    let mut once = numbered(3, 10, 1);
    once.scroll_up::<usize>(&(Line(0)..Line(1)), 3);

    let mut stepwise = numbered(3, 10, 1);
    for _ in 0..3 {
        stepwise.scroll_up::<usize>(&(Line(0)..Line(1)), 1);
    }

    assert_eq!(contents(&once), vec![1, 0, 0, 0, 2, 3]);
    assert_eq!(contents(&once), contents(&stepwise));
    assert_eq!(once.scrolled_lines(), 3);
}

#[test]
fn scroll_up_without_history_stores_nothing() {
    let mut grid = numbered(3, 0, 1);
    grid.scroll_up::<usize>(&(Line(0)..Line(3)), 2);
    assert_eq!(contents(&grid), vec![3, 0, 0]);
    assert_eq!(grid.history_size(), 0);
}

#[test]
fn grow_lines_pulls_rows_from_history() {
    let mut grid = Grid::<Cell>::new(2, 1, 5);
    for c in ['a', 'b', 'c', 'd'] {
        grid.scroll_up(&(Line(0)..Line(2)), 1);
        grid.screen_mut()[Line(1)][Column(0)] = cell(c);
    }
    grid.cursor.point.line = Line(1);
    // History: blank, blank, a, b; screen: c, d.
    assert_eq!(grid.history_size(), 4);

    grid.resize(true, 4, 1);

    assert_eq!(grid.history_size(), 2);
    let text: String = (0..4).map(|l| grid.screen()[Line(l)][Column(0)].c).collect();
    assert_eq!(text, "abcd");
    assert_eq!(grid.cursor.point.line, Line(3));
}

#[test]
fn grow_lines_beyond_history_adds_blank_rows_below() {
    let mut grid = Grid::<Cell>::new(2, 1, 5);
    grid.screen_mut()[Line(0)][Column(0)] = cell('a');
    grid.screen_mut()[Line(1)][Column(0)] = cell('b');

    grid.resize(true, 4, 1);

    assert_eq!(grid.history_size(), 0);
    let text: String = (0..4).map(|l| grid.screen()[Line(l)][Column(0)].c).collect();
    assert_eq!(text, "ab  ");
}

#[test]
fn shrink_lines_pushes_rows_above_the_cursor_into_history() {
    let mut grid = Grid::<Cell>::new(4, 1, 5);
    for (l, c) in ['a', 'b', 'c', 'd'].into_iter().enumerate() {
        grid.screen_mut()[Line(l as i32)][Column(0)] = cell(c);
    }
    grid.cursor.point.line = Line(2);

    grid.resize(true, 2, 1);

    assert_eq!(grid.history_size(), 1);
    assert_eq!(row(&grid, -1)[Column(0)], cell('a'));
    assert_eq!(grid.screen()[Line(0)][Column(0)], cell('b'));
    assert_eq!(grid.screen()[Line(1)][Column(0)], cell('c'));
    assert_eq!(grid.cursor.point.line, Line(1));
}

#[test]
fn row_reader_and_cell_read_screen_and_history() {
    let mut grid = numbered(2, 3, 1);
    grid.scroll_up::<usize>(&(Line(0)..Line(2)), 1);

    let mut reader = RowReader::new();
    assert_eq!(reader.row(&grid, Line(-1))[Column(0)], 1);
    assert_eq!(reader.row(&grid, Line(0))[Column(0)], 2);
    assert_eq!(grid.cell(Point::new(Line(-1), Column(0))), 1);
    assert_eq!(grid.cell(Point::new(Line(0), Column(0))), 2);
}

#[test]
fn reads_outside_the_grid_are_no_row() {
    let mut grid = Grid::<Cell>::new(2, 2, 3);
    grid.screen_mut()[Line(1)][Column(1)].flags.insert(Flags::WRAPLINE);
    grid.screen_mut()[Line(1)][Column(0)] = cell('x');

    assert_eq!(grid.cell(Point::new(Line(-1), Column(0))), Cell::default());
    assert_eq!(grid.cell(Point::new(Line(2), Column(0))), Cell::default());
    assert!(!grid.wraps(Line(2)));
    assert!(grid.is_clear(Line(-1)));
    assert!(grid.wraps(Line(1)));
    assert!(!grid.is_clear(Line(1)));
}

#[test]
fn iterator_crosses_from_history_to_screen() {
    let mut grid = Grid::<usize>::new(2, 2, 3);
    grid.screen_mut()[Line(0)][Column(0)] = 1;
    grid.screen_mut()[Line(0)][Column(1)] = 2;
    grid.screen_mut()[Line(1)][Column(0)] = 3;
    grid.scroll_up::<usize>(&(Line(0)..Line(2)), 1);

    let mut iter = grid.iter_from(Point::new(Line(-1), Column(0)));
    assert_eq!(iter.cell(), &1);
    let cells: Vec<usize> = iter.by_ref().map(|indexed| indexed.cell).collect();
    assert_eq!(cells, vec![2, 3, 0, 0, 0]);

    let mut back = grid.iter_from(Point::new(Line(0), Column(0)));
    assert_eq!(
        back.prev().map(|indexed| (indexed.point, indexed.cell)),
        Some((Point::new(Line(-1), Column(1)), 2))
    );
    assert_eq!(back.cell(), &2);
    assert_eq!(back.prev().map(|indexed| indexed.cell), Some(1));
    assert_eq!(back.prev(), None);
}

#[test]
fn update_history_lowers_the_limit_at_once() {
    let mut grid = numbered(1, 5, 1);
    for value in 2..6 {
        grid.scroll_up::<usize>(&(Line(0)..Line(1)), 1);
        grid.screen_mut()[Line(0)][Column(0)] = value;
    }
    grid.scroll_display(Scroll::Top);
    assert_eq!(contents(&grid), vec![1, 2, 3, 4, 5]);

    grid.update_history(2);

    assert_eq!(contents(&grid), vec![3, 4, 5]);
    assert_eq!(grid.display_offset(), 2);

    grid.clear_history();
    assert_eq!(contents(&grid), vec![5]);
    assert_eq!(grid.display_offset(), 0);
}

#[test]
fn initialize_all_fills_history_with_blank_rows_at_the_oldest_end() {
    let mut grid = numbered(1, 3, 7);
    grid.scroll_up::<usize>(&(Line(0)..Line(1)), 1);
    grid.screen_mut()[Line(0)][Column(0)] = 8;

    grid.initialize_all();

    assert_eq!(contents(&grid), vec![0, 0, 7, 8]);
    assert_eq!(grid.total_lines(), 4);
}

#[test]
fn equality_compares_history_content() {
    let mut a = numbered(1, 3, 1);
    a.scroll_up::<usize>(&(Line(0)..Line(1)), 1);
    let mut b = a.clone();
    assert_eq!(a, b);

    b.update_line(Line(-1), |row| row[Column(0)] = 9);
    assert_ne!(a, b);

    a.update_line(Line(-1), |row| row[Column(0)] = 9);
    assert_eq!(a, b);
}

/// The serialized form is upstream's: `raw.inner` from the bottom line up through history,
/// `zero` 0, `len` rows.
#[cfg(feature = "serde")]
#[test]
fn serde_keeps_upstream_format() {
    let mut grid = numbered(2, 3, 1);
    grid.scroll_up::<usize>(&(Line(0)..Line(2)), 1);
    grid.screen_mut()[Line(1)][Column(0)] = 3;

    let json = serde_json::to_value(&grid).unwrap();
    let inner: Vec<usize> = json["raw"]["inner"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["inner"][0].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(inner, vec![3, 2, 1]);
    assert_eq!(json["raw"]["zero"], 0);
    assert_eq!(json["raw"]["len"], 3);
    assert_eq!(json["raw"]["visible_lines"], 2);
    assert_eq!(json["max_scroll_limit"], 3);

    let back: Grid<usize> = serde_json::from_value(json).unwrap();
    assert_eq!(back, grid);
    assert_eq!(contents(&back), vec![1, 2, 3]);
}

#[test]
fn iterator_reads_outside_the_grid_as_no_row() {
    let mut grid = Grid::<Cell>::new(2, 2, 3);
    grid.screen_mut()[Line(0)][Column(0)] = cell('x');

    let mut iter = grid.iter_from(Point::new(Line(5), Column(0)));
    assert_eq!(iter.cell(), &Cell::default());
    assert_eq!(iter.next(), None);

    let mut above = grid.iter_from(Point::new(Line(-1), Column(1)));
    assert_eq!(above.cell(), &Cell::default());
    assert_eq!(above.next().map(|indexed| indexed.cell), Some(cell('x')));
}
