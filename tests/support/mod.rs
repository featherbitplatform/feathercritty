// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Shared support for the differential tests: one step list drives feathercritty and
//! `alacritty_terminal` 0.26.0 (the oracle) side by side, and their states are compared through a
//! crate-neutral projection after every step.
//!
//! # The oracle
//!
//! The oracle is the published `alacritty_terminal` 0.26.0 source vendored at `tests/oracle/`
//! (a path dev-dependency) with exactly the patches `tests/oracle/PATCHES.md` lists, both of them
//! fork behavior changes the oracle could not otherwise model:
//! 1. the `Row` `occ` invariant (`Row::grow`, `Row::append`): without it, what a reset row looks
//!    like depends on which row is recycled, and upstream recycled rows through hidden cache rows
//!    the fork's grid no longer has;
//! 2. reads of lines outside the grid are "no row": they read as a default cell (vi motions), add
//!    nothing (`line_to_string`'s line above the topmost line), and vi motions keep the cursor in
//!    the grid. 0.26.0 read whatever row its ring held there.
//!
//! `tests/oracle_pristine.rs` checks that the vendored source is the published one except for
//! those hunks. Fork changes 1 and 5 stay unpatched in the oracle and are modeled by the harness
//! (below).
//!
//! # What is compared
//!
//! After every [`Step`] (and after a final synthetic [`EXIT_ALT_SCREEN`]), [`run`] compares a
//! [`project::Snapshot`] of each terminal: every cell of every history and screen row of the
//! active grid (character, zero-width characters, fg, bg, flag bits, underline color, hyperlink
//! URI and ID pattern), the cursor and saved cursor (point, `input_needs_wrap`, template cell),
//! display offset, history size, columns, lines, mode bits and the persistent selection's range
//! and text. Then both answer the same seeded [`project::QueryPlan`]: `selection_to_string` for
//! each selection type, `bounds_to_string`, `search_next` both ways, `bracket_search`,
//! `semantic_search_left/right`, `line_search_left/right`, `expand_wide` and three chained vi
//! motions. Queries leave the selection and display offset as they found them.
//!
//! # How both crates are driven
//!
//! `term_impl.rs` holds all code that touches a `Term`. It is compiled twice, as `fc::imp` and
//! `ac::imp`, each with `k` aliased to one crate (`fc.rs`: feathercritty, `ac.rs`:
//! `alacritty_terminal`), so both sides run literally the same harness code. Both crates parse
//! with the same `vte` crate. Everything outside `term_impl.rs` is crate neutral: [`Step`],
//! [`Start`], the generators in [`gen`], the snapshot, plan and diffs in [`project`], the
//! runner and [`minimize`].
//!
//! # Moving the oracle to a newer upstream release
//!
//! When feathercritty merges a new upstream `alacritty_terminal` release X.Y.Z:
//! 1. Re-vendor the oracle (`tests/oracle/PATCHES.md`, "Moving to a new upstream release"): replace
//!    `tests/oracle/src`, `Cargo.toml` and `LICENSE-APACHE` with the published X.Y.Z crate's,
//!    re-apply `PATCHES.diff` (drop a patch if the release fixes it the same way), run
//!    `tests/oracle/refresh.py` and update `Cargo.lock`.
//! 2. `term_impl.rs` must compile against both crates. It uses this upstream API: `Term::new`,
//!    `Config` (`scrolling_history`, `kitty_keyboard`), `term::test::TermSize`, `Term::resize`,
//!    `scroll_display(Scroll::Delta)`, `set_options`, the public `Term::selection` field,
//!    `Selection::new/update/to_range`, `grid()` with `Grid` indexing by `Line`/`Column` on the
//!    oracle side (`ac::cell_at`/`row_at`; feathercritty reads through `Grid::cell` and
//!    `RowReader`, `fc::cell_at`/`row_at`),
//!    `topmost_line`/`bottommost_line`/`history_size`/`display_offset`, the public
//!    `Grid::cursor`/`saved_cursor` fields, `Cell` (`c`, `fg`, `bg`, `flags`, `zerowidth()`,
//!    `underline_color()`, `hyperlink()`), `TermMode::bits`, `RegexSearch`, `RegexIter`,
//!    `search_next`, the other search functions, `ViModeCursor::motion` and the `ViMotion` list
//!    (`MOTIONS`). If upstream changes one of these, change `term_impl.rs` for both sides.
//! 3. `project.rs` hard-codes flag bit values (`WRAPLINE`, `WIDE_CHAR_SPACER`,
//!    `LEADING_WIDE_CHAR_SPACER`) and `term_impl.rs`/`mod.rs` the `ALT_SCREEN` mode bit and the
//!    generated hyperlink ID suffix `_alacritty`; check them against the new release.
//!    `term_impl::search_bounds` (used by `search_terminates`, `match_in_range` and, through the
//!    same formulas, `project::search_window`) replicates the start and end computation of upstream
//!    `search_next` (`next_match_left/right`); re-check it against the new source.
//! 4. Re-check every workaround for a 0.26.0 bug (each is commented with the upstream behavior it
//!    avoids; search for "0.26.0"). Remove the ones the new release fixes, so the oracle covers
//!    those inputs again, and update the "Known gaps" list in `tests/differential.rs`.
//! 5. Re-check the intended differences (the README's change list) and how the oracle models them:
//!    - Fork change 1 (kitty keyboard stack eviction): the generator stays below the stack depth
//!      (`gen::MAX_KITTY_PUSHES`); `oracle_sees_fork_change_1` shows the oracle notices it.
//!    - Fork change 5 (regex search over leading spacers, complete in `0.26.0+fbterm.3`: every
//!      search terminates, and matches stay in their range up to the glyph completion at its ends;
//!      where 0.26.0 terminates in range the answers are 0.26.0's): where 0.26.0's `search_next`
//!      does not terminate or answers from outside the searched range
//!      (`Side::oracle_search_is_off`), [`run`] does not require equality but checks
//!      feathercritty's answer on its own, even when both answers are equal (`Side::check_search`:
//!      it terminated, and a match lies in the range, allowing the glyph completion at its ends
//!      (`Side::match_in_range`), and spells a match of the pattern as the search reads cells,
//!      `Side::text_right`/`text_left`, which follow `skip_fullwidth`); windows that
//!      `project::search_window` moves for 0.26.0 are also searched as drawn on feathercritty alone
//!      and checked the same way. Both are counted in [`Stats`]; `oracle_sees_fork_change_5` shows
//!      both paths are taken. If the new oracle release carries the same fix, drop that modeling
//!      and require equality again; if feathercritty's change was dropped, drop it too.
//!
//! [`gen`]: mod@r#gen

// Each test binary uses a different part of this module.
#![allow(dead_code)]

pub mod ac;
pub mod fc;
pub mod r#gen;
pub mod minimize;
pub mod project;

use std::cell::Cell;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, mpsc};

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// Default number of seeds per pass (`DIFF_SEEDS`).
pub const DEFAULT_SEEDS: u64 = 300;

/// Default number of steps per seed (`DIFF_STEPS`).
pub const DEFAULT_STEPS: usize = 400;

/// Selection kinds, mirroring `selection::SelectionType` of both crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelTy {
    Simple,
    Block,
    Semantic,
    Lines,
}

impl SelTy {
    pub const ALL: [SelTy; 4] = [SelTy::Simple, SelTy::Block, SelTy::Semantic, SelTy::Lines];
}

/// A grid point plus the side of the cell, crate neutral. Lines are `Line` values (negative in
/// history); the runner clamps them to the grid before use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pt {
    pub line: i32,
    pub col: usize,
    pub right: bool,
}

impl Pt {
    pub fn new(line: i32, col: usize) -> Self {
        Self { line, col, right: false }
    }
}

/// One input to both terminals.
#[derive(Clone, PartialEq, Eq)]
pub enum Step {
    /// Bytes fed to the parser.
    Bytes(Vec<u8>),
    /// `Term::resize`.
    Resize { lines: usize, columns: usize },
    /// `Term::scroll_display(Scroll::Delta(n))`.
    ScrollView(i32),
    /// `Term::set_options` with a new history limit.
    SetHistory(usize),
    /// Set (`Some`) or clear (`None`) the persistent selection.
    Select { ty: Option<SelTy>, start: Pt, end: Pt },
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Bytes(bytes) => write!(f, "Bytes(b\"{}\")", bytes.escape_ascii()),
            Step::Resize { lines, columns } => {
                write!(f, "Resize {{ lines: {lines}, columns: {columns} }}")
            },
            Step::ScrollView(delta) => write!(f, "ScrollView({delta})"),
            Step::SetHistory(limit) => write!(f, "SetHistory({limit})"),
            Step::Select { ty, start, end } => {
                write!(f, "Select {{ ty: {ty:?}, start: {start:?}, end: {end:?} }}")
            },
        }
    }
}

/// How both terminals are created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    pub lines: usize,
    pub columns: usize,
    /// `Config::scrolling_history`.
    pub history: usize,
    /// `Config::kitty_keyboard`.
    pub kitty: bool,
    /// Seed of the query RNG (query points, patterns and motions).
    pub query_seed: u64,
}

/// A complete run.
#[derive(Debug, Clone)]
pub struct Case {
    pub start: Start,
    pub steps: Vec<Step>,
}

/// The first difference between the two terminals.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// Index of the step after which the states differ.
    pub step: usize,
    pub diff: String,
    /// feathercritty did not return from a query within its timeout. The query's thread is
    /// still running (it cannot be stopped and keeps a core busy until the process exits), so
    /// the runner reports this at once instead of minimizing, which would start more of them.
    pub hang: bool,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "after step {}: {}", self.step, self.diff)
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// `catch_unwind` without the default hook's message. Several 0.26.0 functions panic on some
/// grid states, identically in both crates; the oracle records those panics as outcomes, and
/// printing each one would bury a real failure report.
pub fn catch<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !QUIET.with(Cell::get) {
                default(info);
            }
        }));
    });

    let was_quiet = QUIET.with(|quiet| quiet.replace(true));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    QUIET.with(|quiet| quiet.set(was_quiet));
    result
}

/// Run `f` on both sides, turning panics into a divergence.
fn both<A, B, RA, RB>(step: usize, what: &str, a: A, b: B) -> Result<(RA, RB), Divergence>
where
    A: FnOnce() -> RA,
    B: FnOnce() -> RB,
{
    let ra = catch(a);
    let rb = catch(b);
    combine(step, what, ra, rb)
}

/// Turn the outcomes of one call on both sides into a divergence if either panicked.
fn combine<RA, RB>(
    step: usize,
    what: &str,
    ra: std::thread::Result<RA>,
    rb: std::thread::Result<RB>,
) -> Result<(RA, RB), Divergence> {
    let diff = match (ra, rb) {
        (Ok(ra), Ok(rb)) => return Ok((ra, rb)),
        (Err(e), Ok(_)) => {
            format!("{what}: feathercritty panicked ({}), 0.26.0 did not", panic_message(&*e))
        },
        (Ok(_), Err(e)) => {
            format!("{what}: 0.26.0 panicked ({}), feathercritty did not", panic_message(&*e))
        },
        (Err(ea), Err(eb)) => format!(
            "{what}: both panicked (feathercritty: {}; 0.26.0: {})",
            panic_message(&*ea),
            panic_message(&*eb)
        ),
    };
    Err(Divergence { step, diff, hang: false })
}

/// Labeled query answers, as `Side::queries` returns them.
type Answers = Vec<(&'static str, project::QAns)>;

/// Run the feathercritty side's queries on another thread, so a search that never returns
/// fails the run instead of hanging it. The side moves to the thread and back (no copy). After
/// `FORK_TIMEOUT` the run fails with a `hang` divergence; the thread, and the side with it, is
/// left running until the process exits.
fn fork_queries(
    side: fc::Side,
    plan: &project::QueryPlan,
    step: usize,
) -> Result<(fc::Side, std::thread::Result<Answers>), Divergence> {
    let (tx, rx) = mpsc::channel();
    let thread_plan = plan.clone();
    std::thread::spawn(move || {
        let mut side = side;
        let answers = catch(|| side.queries(&thread_plan));
        let _ = tx.send((side, answers));
    });
    rx.recv_timeout(FORK_TIMEOUT).map_err(|_| Divergence {
        step,
        diff: format!(
            "feathercritty queries did not return within {} s (a search that never \
             terminates?)\nsearches: {:?}",
            FORK_TIMEOUT.as_secs(),
            plan.searches
        ),
        hang: true,
    })
}

/// Bytes that leave the alternate screen whichever way it was entered. `run` feeds them after
/// the last step: 0.26.0 has no getter for the inactive grid (feathercritty's
/// `Term::inactive_grid` is fork change 3), so this is how history that was hidden behind the
/// alternate screen at the end of a run still gets compared. If the oracle release gains an
/// inactive-grid getter, compare the inactive grid directly instead.
pub const EXIT_ALT_SCREEN: &[u8] = b"\x1b[?1049l\x1b[?47l\x1b[?1047l";

/// What a run covered.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub cases: usize,
    pub steps: usize,
    /// Steps after which the alternate screen was up (its primary history is not compared).
    pub alt_steps: usize,
    /// Largest history seen.
    pub max_history: usize,
    /// Cases whose history reached the current limit (limit above 0) at least once.
    pub reached_limit: usize,
    /// `search_next` queries run.
    pub searches: usize,
    /// Of those, searches whose window end was moved off a wrapped wide character.
    pub moved_windows: usize,
    /// Of those, searches answered "does not terminate" (0.26.0's match iterator cycles).
    pub no_termination: usize,
    /// Searches where 0.26.0 did not terminate or answered from outside the searched range, so
    /// feathercritty's answer was checked on its own instead (fork change 5).
    pub change5_answers: usize,
    /// Moved windows that feathercritty also searched as drawn, checked on its own (fork
    /// change 5; 0.26.0 would hang there).
    pub change5_windows: usize,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.cases += other.cases;
        self.steps += other.steps;
        self.alt_steps += other.alt_steps;
        self.max_history = self.max_history.max(other.max_history);
        self.reached_limit += other.reached_limit;
        self.searches += other.searches;
        self.moved_windows += other.moved_windows;
        self.no_termination += other.no_termination;
        self.change5_answers += other.change5_answers;
        self.change5_windows += other.change5_windows;
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |n: usize, of: usize| if of == 0 { 0.0 } else { 100.0 * n as f64 / of as f64 };
        write!(
            f,
            "{} cases, {} steps ({:.1}% with the alternate screen up), max history {}, {} cases \
             ({:.1}%) reached their history limit, {} searches: {:.1}% windows moved, {} not \
             terminating in 0.26.0; fork change 5 checked alone: {} answers, {} windows",
            self.cases,
            self.steps,
            pct(self.alt_steps, self.steps),
            self.max_history,
            self.reached_limit,
            pct(self.reached_limit, self.cases),
            self.searches,
            pct(self.moved_windows, self.searches),
            self.no_termination,
            self.change5_answers,
            self.change5_windows,
        )
    }
}

/// Drive both terminals through `steps`, then through [`EXIT_ALT_SCREEN`], comparing the
/// projection and a seeded set of queries after every step. A divergence after the synthetic
/// exit is reported at step `steps.len()`.
pub fn run(start: &Start, steps: &[Step]) -> Result<Stats, Divergence> {
    let mut a = fc::Side::new(start);
    let mut b = ac::Side::new(start);
    let mut qrng = ChaCha8Rng::seed_from_u64(start.query_seed);
    let mut stats = Stats { cases: 1, ..Stats::default() };
    let mut limit = start.history;
    let mut reached = false;

    let exit = Step::Bytes(EXIT_ALT_SCREEN.to_vec());
    for (i, step) in steps.iter().chain([&exit]).enumerate() {
        both(i, "step", || a.apply(step), || b.apply(step))?;
        if let Step::SetHistory(new) = step {
            limit = *new;
        }

        let (sa, sb) = both(i, "snapshot", || a.snapshot(), || b.snapshot())?;
        if sa != sb {
            return Err(Divergence { step: i, diff: project::diff(&sa, &sb), hang: false });
        }

        let plan = project::QueryPlan::random(&mut qrng, &sa);
        let (side, ra) = fork_queries(a, &plan, i)?;
        a = side;
        let rb = catch(|| b.queries(&plan));
        let (mut qa, qb) = combine(i, "queries", ra, rb)?;

        // Fork change 5: where 0.26.0's search does not terminate or answers from outside the
        // searched range, feathercritty's answer is checked on its own instead of compared,
        // even when both answers are the same (both could be wrong the same way).
        // The searches follow the selections and `bounds_to_string` in the answers.
        let first = plan.selections.len() + 1;
        for (k, search) in plan.searches.iter().enumerate() {
            assert_eq!((qa[first + k].0, qb[first + k].0), ("search_next", "search_next"));
            let (fork, oracle) = (&qa[first + k].1, &qb[first + k].1);
            if b.oracle_search_is_off(search, oracle) {
                if let Err(why) = a.check_search(search, search.max_lines, fork) {
                    let diff = format!(
                        "search_next where 0.26.0 cannot answer (fork change 5): feathercritty \
                         {fork:?} fails its own check: {why}\n0.26.0 {oracle:?}\nsearch {search:?}"
                    );
                    return Err(Divergence { step: i, diff, hang: false });
                }
                stats.change5_answers += 1;
                qa[first + k].1 = oracle.clone();
            }
            // Windows moved for 0.26.0's sake: feathercritty searches them as drawn.
            if let Some(unmoved) = search.unmoved {
                let Some(fork) = a.search_detached(search, unmoved, FORK_TIMEOUT) else {
                    let diff = format!(
                        "feathercritty search_next did not return within {} s on a window 0.26.0 \
                         cannot search (fork change 5)\nsearch {search:?} with max_lines \
                         {unmoved:?}",
                        FORK_TIMEOUT.as_secs()
                    );
                    return Err(Divergence { step: i, diff, hang: true });
                };
                if let Err(why) = a.check_search(search, unmoved, &fork) {
                    let diff = format!(
                        "search_next on a window 0.26.0 cannot search (fork change 5): \
                         feathercritty {fork:?} fails its own check: {why}\nsearch {search:?} \
                         with max_lines {unmoved:?}"
                    );
                    return Err(Divergence { step: i, diff, hang: false });
                }
                stats.change5_windows += 1;
            }
        }

        if qa != qb {
            return Err(Divergence {
                step: i,
                diff: project::diff_queries(&plan, &qa, &qb),
                hang: false,
            });
        }

        stats.steps += 1;
        stats.alt_steps += usize::from(sa.mode & ALT_SCREEN_MODE != 0);
        stats.max_history = stats.max_history.max(sa.history_size);
        reached |= limit > 0 && sa.history_size == limit;
        stats.searches += plan.searches.len();
        stats.moved_windows += plan.moved_windows;
        stats.no_termination +=
            qb.iter().filter(|(_, q)| *q == project::QAns::NoTermination).count();
    }
    stats.reached_limit = usize::from(reached);

    Ok(stats)
}

/// How long feathercritty's queries (`fork_queries`) or a search 0.26.0 cannot run
/// (`Side::search_detached`) may take before the run fails as a hang. Generous, so debug builds
/// on slow runners searching a 100,000-line history stay well inside it.
const FORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// `TermMode::ALT_SCREEN`.
const ALT_SCREEN_MODE: u32 = 1 << 12;

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(value.trim().parse().unwrap_or_else(|_| panic!("{name} must be an integer: {value:?}")))
}

/// `DIFF_TINY`: when set (to anything but `0`), every feathercritty grid's history uses
/// [`TINY_BLOCK_BYTES`]-byte blocks, so block boundaries, spares and oversized rows are crossed
/// all the time, and its column reflow keeps a window of one dense row (`Grid::set_reflow_window`),
/// so every row the reflow produces goes through the new history; the primary grid only, the
/// alternate one has no history.
pub fn tiny_blocks() -> bool {
    std::env::var("DIFF_TINY").is_ok_and(|value| value.trim() != "0")
}

/// History block size under `DIFF_TINY`.
pub const TINY_BLOCK_BYTES: usize = 256;

/// `DIFF_SEEDS`, or `default`.
pub fn seed_count(default: u64) -> u64 {
    env_u64("DIFF_SEEDS").unwrap_or(default)
}

/// `DIFF_STEPS`, or `default`.
pub fn step_count(default: usize) -> usize {
    env_u64("DIFF_STEPS").map_or(default, |n| n as usize)
}

/// The seeds to run: `DIFF_SEED` alone when set, otherwise `0..count`.
pub fn seeds(count: u64) -> Vec<u64> {
    match env_u64("DIFF_SEED") {
        Some(seed) => vec![seed],
        None => (0..count).collect(),
    }
}

/// Run `make(seed)` for every seed on all cores and return the combined [`Stats`] (also
/// printed; see them with `--nocapture`). On the first divergence, minimize it and panic with
/// the seed, the minimized steps and the difference. `steps` is the `DIFF_STEPS` value the pass
/// used, for the reproduce command (`None` for passes that ignore it).
pub fn run_pass<F>(test: &str, seeds: &[u64], steps: Option<usize>, make: F) -> Stats
where
    F: Fn(u64) -> Case + Sync,
{
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failure: Mutex<Option<(u64, Case, Divergence)>> = Mutex::new(None);
    let total = Mutex::new(Stats::default());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(seeds.len());

    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&seed) = seeds.get(index) else { break };
                    let case = make(seed);
                    match run(&case.start, &case.steps) {
                        Ok(stats) => total.lock().unwrap().add(&stats),
                        Err(divergence) => {
                            stop.store(true, Ordering::Relaxed);
                            let mut failure = failure.lock().unwrap();
                            if failure.as_ref().is_none_or(|(s, ..)| seed < *s) {
                                *failure = Some((seed, case, divergence));
                            }
                        },
                    }
                }
            });
        }
    });

    let Some((seed, case, divergence)) = failure.into_inner().unwrap() else {
        let total = total.into_inner().unwrap();
        println!("{test}: {total}");
        return total;
    };
    let start = case.start;
    let mut steps = steps.map(|n| format!(" DIFF_STEPS={n}")).unwrap_or_default();
    if tiny_blocks() {
        steps += " DIFF_TINY=1";
    }
    if divergence.hang {
        // Minimizing would rerun the hanging query many times, leaving a spinning thread each
        // time; report the seed and the query instead.
        panic!(
            "differential divergence in `{test}`, seed {seed}, {divergence}\n\nreproduce: \
             DIFF_SEED={seed}{steps} cargo test --test differential {test}\n\nstart: \
             {start:?}\n(not minimized: a feathercritty query did not return, and its thread is \
             still running)\n"
        );
    }
    let minimized = minimize::minimize(&case.steps, |steps| run(&start, steps).is_err());
    let after = run(&start, &minimized).err();

    let mut report = format!(
        "differential divergence in `{test}`, seed {seed}, {divergence}\n\nreproduce: \
         DIFF_SEED={seed}{steps} cargo test --test differential {test}\n\nstart: \
         {start:?}\nminimized steps ({} of {}; a synthetic alternate screen exit follows them):\n",
        minimized.len(),
        case.steps.len(),
    );
    for (i, step) in minimized.iter().enumerate() {
        report += &format!("  {i:4}: {step:?}\n");
    }
    match after {
        Some(after) => report += &format!("\nminimized divergence {after}\n"),
        None => report += "\n(minimized list no longer diverges; showing the original)\n",
    }
    panic!("{report}");
}
