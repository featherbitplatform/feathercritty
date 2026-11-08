// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The feathercritty side of the differential tests.

pub use feathercritty as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Write one screen cell (`term_impl::Side::from_rows`).
pub fn set_cell(
    term: &mut k::term::Term<k::event::VoidListener>,
    point: k::index::Point,
    cell: k::term::cell::Cell,
) {
    term.grid_mut().screen_mut()[point] = cell;
}

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

/// The configuration of a new terminal and of `Step::SetHistory`: under `DIFF_COLD=<n>`,
/// `history_compression: Some(n)` (see `support::cold_history`), so limit changes keep it.
pub fn config(scrolling_history: usize, kitty_keyboard: bool) -> k::term::Config {
    k::term::Config {
        scrolling_history,
        kitty_keyboard,
        history_compression: super::cold_history(),
        ..k::term::Config::default()
    }
}

/// History jobs between steps under `DIFF_COLD` (inactive otherwise). After every step it draws
/// one of three actions from its own RNG, so the generator's and the oracle's streams stay as they
/// are without the knob:
/// - run and install every job (`Term::run_history_jobs`);
/// - take up to eight jobs one at a time, run each, and install about half of the results, dropping
///   the others (the next call offers a dropped block again);
/// - take one job and hold it: it is run and installed after the next step has been applied, so
///   that step's resize, limit change, `CSI 3 J`, RIS or eviction lands between taking and
///   installing, and the install may be refused.
#[derive(Debug, Default)]
pub struct Jobs {
    /// `None` without `DIFF_COLD`.
    rng: Option<ChaCha8Rng>,
    held: Option<k::term::HistoryJob>,
    installed: usize,
    refused: usize,
    dropped: usize,
}

impl Jobs {
    /// Seeded from the case: its `query_seed`, which the generator draws from the case seed, mixed
    /// with a constant so this RNG's draws differ from the queries'.
    pub fn new(start: &super::Start) -> Self {
        let rng = super::cold_history()
            .map(|_| ChaCha8Rng::seed_from_u64(start.query_seed ^ 0xc01d_b10c_4a0b_5eed));
        Self { rng, ..Self::default() }
    }

    fn install(
        &mut self,
        term: &mut k::term::Term<k::event::VoidListener>,
        job: k::term::HistoryJob,
    ) {
        if term.install_history_job(job.run()) {
            self.installed += 1;
        } else {
            self.refused += 1;
        }
    }

    /// Install the job held since the previous step, then draw this step's action.
    pub fn after_step(&mut self, term: &mut k::term::Term<k::event::VoidListener>) {
        let Some(rng) = self.rng.as_mut() else { return };
        let action = rng.gen_range(0..3);
        let count = rng.gen_range(1..=8);
        // Bit `i` set: install the `i`th job's result instead of dropping it.
        let keep: u8 = rng.r#gen();
        if let Some(job) = self.held.take() {
            self.install(term, job);
        }
        match action {
            0 => term.run_history_jobs(),
            1 => {
                for i in 0..count {
                    let Some(job) = term.next_history_job() else { break };
                    if keep & (1 << i) != 0 {
                        self.install(term, job);
                    } else {
                        drop(job.run());
                        self.dropped += 1;
                    }
                }
            },
            _ => self.held = term.next_history_job(),
        }
    }

    /// Results installed, refused and dropped so far.
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.installed, self.refused, self.dropped)
    }
}

/// See `cell_at`.
pub fn row_at(
    grid: &k::grid::Grid<k::term::cell::Cell>,
    line: k::index::Line,
) -> k::grid::Row<k::term::cell::Cell> {
    k::grid::RowReader::new().row(grid, line).clone()
}
