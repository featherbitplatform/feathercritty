// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The `alacritty_terminal` 0.26.0 side of the differential tests (the oracle): the published
//! source vendored at `tests/oracle/` with the two patches `tests/oracle/PATCHES.md` lists.

pub use alacritty_terminal as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;

/// Write one screen cell (`term_impl::Side::from_rows`).
pub fn set_cell(
    term: &mut k::term::Term<k::event::VoidListener>,
    point: k::index::Point,
    cell: k::term::cell::Cell,
) {
    term.grid_mut()[point] = cell;
}

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

/// The configuration of a new terminal and of `Step::SetHistory` (`DIFF_COLD` concerns
/// feathercritty only).
pub fn config(scrolling_history: usize, kitty_keyboard: bool) -> k::term::Config {
    k::term::Config { scrolling_history, kitty_keyboard, ..k::term::Config::default() }
}

/// 0.26.0 has no history jobs: `fc::Jobs`'s counterpart does nothing. Not a unit struct, so
/// `term_impl.rs` builds both with `Jobs::default()`.
#[derive(Debug, Default)]
pub struct Jobs(());

impl Jobs {
    pub fn new(_start: &super::Start) -> Self {
        Self::default()
    }

    pub fn after_step(&mut self, _term: &mut k::term::Term<k::event::VoidListener>) {}

    pub fn counts(&self) -> (usize, usize, usize) {
        (0, 0, 0)
    }
}

/// See `cell_at`.
pub fn row_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    line: k::index::Line,
) -> k::grid::Row<k::term::cell::Cell> {
    grid[line].clone()
}
