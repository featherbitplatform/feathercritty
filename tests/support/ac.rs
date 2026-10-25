// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The `alacritty_terminal` 0.26.0 side of the differential tests (the oracle): the published
//! source vendored at `tests/oracle/` with the two patches `tests/oracle/PATCHES.md` lists.

pub use alacritty_terminal as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;

/// This side is 0.26.0, whose searches `term_impl::search_terminates` must steer around.
pub const ORACLE: bool = true;

/// Grid reads written differently per crate (0.26.0 indexes `Grid` directly); `term_impl.rs` reads
/// cells and rows through these.
pub fn cell_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    point: k::index::Point,
) -> k::term::cell::Cell {
    grid[point].clone()
}

/// A new terminal (`DIFF_TINY` concerns feathercritty's history blocks only).
pub fn new_term(
    config: k::term::Config,
    size: &k::term::test::TermSize,
) -> k::term::Term<k::event::VoidListener> {
    k::term::Term::new(config, size, k::event::VoidListener)
}

/// See `cell_at`.
pub fn row_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    line: k::index::Line,
) -> k::grid::Row<k::term::cell::Cell> {
    grid[line].clone()
}
