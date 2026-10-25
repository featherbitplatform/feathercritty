// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Shared support for the differential tests: one step list drives feathercritty and unmodified
//! `alacritty_terminal` 0.26.0 side by side, and their states are compared through a projection
//! after every step.
//!
//! Layout: [`Step`] and [`Start`] describe a run, [`gen`] produces runs, [`fc`] and [`ac`] apply
//! steps to each crate's `Term` and project it, [`project`] holds the crate-neutral snapshot,
//! query plan and diff, [`run`] compares the two, and [`minimize`] shrinks a failing step list.

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
use std::sync::{Mutex, Once};

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
    match (ra, rb) {
        (Ok(ra), Ok(rb)) => Ok((ra, rb)),
        (Err(e), Ok(_)) => Err(Divergence {
            step,
            diff: format!(
                "{what}: feathercritty panicked ({}), 0.26.0 did not",
                panic_message(&*e)
            ),
        }),
        (Ok(_), Err(e)) => Err(Divergence {
            step,
            diff: format!(
                "{what}: 0.26.0 panicked ({}), feathercritty did not",
                panic_message(&*e)
            ),
        }),
        (Err(ea), Err(eb)) => Err(Divergence {
            step,
            diff: format!(
                "{what}: both panicked (feathercritty: {}; 0.26.0: {})",
                panic_message(&*ea),
                panic_message(&*eb)
            ),
        }),
    }
}

/// Drive both terminals through `steps`, comparing the projection and a seeded set of queries
/// after every step.
pub fn run(start: &Start, steps: &[Step]) -> Result<(), Divergence> {
    let mut a = fc::Side::new(start);
    let mut b = ac::Side::new(start);
    let mut qrng = ChaCha8Rng::seed_from_u64(start.query_seed);

    for (i, step) in steps.iter().enumerate() {
        both(i, "step", || a.apply(step), || b.apply(step))?;

        let (sa, sb) = both(i, "snapshot", || a.snapshot(), || b.snapshot())?;
        if sa != sb {
            return Err(Divergence { step: i, diff: project::diff(&sa, &sb) });
        }

        let plan = project::QueryPlan::random(&mut qrng, &sa);
        let (qa, qb) = both(i, "queries", || a.queries(&plan), || b.queries(&plan))?;
        if qa != qb {
            return Err(Divergence { step: i, diff: project::diff_queries(&plan, &qa, &qb) });
        }
    }

    Ok(())
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(value.trim().parse().unwrap_or_else(|_| panic!("{name} must be an integer: {value:?}")))
}

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

/// Run `make(seed)` for every seed on all cores. On the first divergence, minimize it and panic
/// with the seed, the minimized steps and the difference.
pub fn run_pass<F>(test: &str, seeds: &[u64], make: F)
where
    F: Fn(u64) -> Case + Sync,
{
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failure: Mutex<Option<(u64, Case, Divergence)>> = Mutex::new(None);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(seeds.len());

    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&seed) = seeds.get(index) else { break };
                    let case = make(seed);
                    if let Err(divergence) = run(&case.start, &case.steps) {
                        stop.store(true, Ordering::Relaxed);
                        let mut failure = failure.lock().unwrap();
                        if failure.as_ref().is_none_or(|(s, ..)| seed < *s) {
                            *failure = Some((seed, case, divergence));
                        }
                    }
                }
            });
        }
    });

    let Some((seed, case, divergence)) = failure.into_inner().unwrap() else { return };
    let start = case.start;
    let minimized = minimize::minimize(&case.steps, |steps| run(&start, steps).is_err());
    let after = run(&start, &minimized).err();

    let mut report = format!(
        "differential divergence in `{test}`, seed {seed}, {divergence}\n\nreproduce: \
         DIFF_SEED={seed} DIFF_STEPS={} cargo test --test differential {test}\n\nstart: \
         {start:?}\nminimized steps ({} of {}):\n",
        case.steps.len(),
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
