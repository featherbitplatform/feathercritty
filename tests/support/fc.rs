// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The feathercritty side of the differential tests.

pub use feathercritty as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;

/// Grid reads written differently per crate (feathercritty does not index `Grid` (screen rows are
/// behind `Grid::screen`, history rows are read through `RowReader` and `Grid::cell`));
/// `term_impl.rs` reads cells and rows through these.
pub fn cell_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    point: k::index::Point,
) -> k::term::cell::Cell {
    grid.cell(point)
}

/// See `cell_at`.
pub fn row_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    line: k::index::Line,
) -> k::grid::Row<k::term::cell::Cell> {
    k::grid::RowReader::new().row(grid, line).clone()
}
