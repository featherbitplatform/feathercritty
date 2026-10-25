// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The feathercritty side of the differential tests.

pub use feathercritty as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;

/// This side is feathercritty, whose searches always terminate (fork change 5).
pub const ORACLE: bool = false;

/// Grid reads written differently per crate (feathercritty does not index `Grid` (screen rows are
/// behind `Grid::screen`, history rows are read through `RowReader` and `Grid::cell`));
/// `term_impl.rs` reads cells and rows through these.
pub fn cell_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    point: k::index::Point,
) -> k::term::cell::Cell {
    grid.cell(point)
}

/// A new terminal; under `DIFF_TINY` its grid's history uses tiny blocks and its column reflow
/// keeps a window of one dense row (see `support::tiny_blocks`). The grid is replaced right after
/// `Term::new`, before anything is written, by one built with the same size and history limit.
pub fn new_term(
    config: k::term::Config,
    size: &k::term::test::TermSize,
) -> k::term::Term<k::event::VoidListener> {
    use k::grid::Dimensions;

    let history = config.scrolling_history;
    let mut term = k::term::Term::new(config, size, k::event::VoidListener);
    if super::tiny_blocks() {
        *term.grid_mut() = k::grid::Grid::new_with_block_size(
            size.screen_lines(),
            size.columns(),
            history,
            super::TINY_BLOCK_BYTES,
        );
        term.grid_mut().set_reflow_window(1);
    }
    term
}

/// See `cell_at`.
pub fn row_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    line: k::index::Line,
) -> k::grid::Row<k::term::cell::Cell> {
    k::grid::RowReader::new().row(grid, line).clone()
}
