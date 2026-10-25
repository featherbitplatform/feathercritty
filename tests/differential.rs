// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Differential test (design spec 6.1): identical inputs go to feathercritty and to unmodified
//! `alacritty_terminal` 0.26.0, and their states and query answers must match after every step
//! and after a final synthetic exit from the alternate screen. What is compared, how both crates
//! are driven, and how to move the oracle to a newer upstream release are documented in
//! `tests/support/mod.rs`.
//!
//! Knobs: `DIFF_SEEDS` (seeds per pass, default 300), `DIFF_STEPS` (steps per random seed,
//! default 400), `DIFF_SEED` (replay one seed). A failure prints the seed, the `DIFF_STEPS` value
//! the pass used and a minimized step list. Each pass prints coverage statistics (see them with
//! `--nocapture`).
//!
//! The only intended behavioral difference, fork change 1 (kitty keyboard stack eviction), needs
//! more than 4,096 pushes; the generator stays below 4,000, and
//! `oracle_sees_fork_change_1` checks that the oracle does notice it.
//!
//! Known gaps, all from 0.26.0 bugs the harness steers around (see the comments at each
//! workaround; remove them when upstream fixes the bug):
//! - The primary screen's history is compared only while it is shown. Alternate screen periods are
//!   kept short and every run ends by leaving the alternate screen, but a difference that appears
//!   in the hidden history and is undone before the exit goes unseen.
//! - Leftward searches whose window would end on a wrapped wide character get a window one or more
//!   lines larger, so that exact end is never searched; searches whose match iterator cycles are
//!   answered "does not terminate" without calling `search_next`.
//! - Wide characters never meet a one-column grid.
//! - Queries that panic in 0.26.0 are compared as "panicked", so whatever those functions would
//!   have returned there is not checked.

mod support;

use support::r#gen::{self, Realistic};
use support::{Case, DEFAULT_SEEDS, DEFAULT_STEPS, Start, Step};

/// The weighted random generator, history limits 0 to 10,000.
#[test]
fn random_streams() {
    let n = support::step_count(DEFAULT_STEPS);
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS));
    support::run_pass("random_streams", &seeds, Some(n), |seed| r#gen::random_case(seed, n));
}

/// The separate pass at a 100,000-line history limit, with floods of short lines so history
/// grows past 10,000. Each step compares the whole history, so this pass runs a tenth of the
/// seeds and a quarter of the steps.
#[test]
fn large_history_limit() {
    let knob = support::step_count(DEFAULT_STEPS);
    let n = knob.div_ceil(4);
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS).div_ceil(10));
    support::run_pass("large_history_limit", &seeds, Some(knob), |seed| r#gen::large_case(seed, n));
}

/// A full history: limits from 500 to 10,000, floods of long wrapping lines (CJK included) past
/// the limit, then resizes (reflow of thousands of wrapped rows) and limit changes while the
/// history is full. A tenth of the seeds and a quarter of the steps, like the large pass.
#[test]
fn full_history() {
    let knob = support::step_count(DEFAULT_STEPS);
    let n = knob.div_ceil(4);
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS).div_ceil(10));
    let stats =
        support::run_pass("full_history", &seeds, Some(knob), |seed| r#gen::full_case(seed, n));
    // The pass exists to run at the limit: most cases must get there.
    assert!(
        seeds.len() < 5 || stats.reached_limit * 2 >= stats.cases,
        "too few cases reached their history limit: {stats}"
    );
}

fn realistic(test: &str, kind: Realistic) {
    // A fifth of the seeds per kind.
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS).div_ceil(5));
    support::run_pass(test, &seeds, None, |seed| r#gen::realistic_case(kind, seed));
}

/// Colored `ls -l` style listings.
#[test]
fn realistic_ls() {
    realistic("realistic_ls", Realistic::Ls);
}

/// `git log --graph` style output.
#[test]
fn realistic_git_graph() {
    realistic("realistic_git_graph", Realistic::GitGraph);
}

/// Compiler diagnostics with colors, carets and file links.
#[test]
fn realistic_diagnostics() {
    realistic("realistic_diagnostics", Realistic::Diagnostics);
}

/// Mixed CJK and ASCII prose.
#[test]
fn realistic_cjk() {
    realistic("realistic_cjk", Realistic::Cjk);
}

/// A full-screen program redrawing the primary screen.
#[test]
fn realistic_full_screen() {
    realistic("realistic_full_screen", Realistic::FullScreen);
}

/// Every ref test recording replayed whole: once as recorded (its own size and history limit),
/// once with a 10,000-line history and interleaved resizes, view scrolls and selections.
#[test]
fn ref_recordings() {
    let names = r#gen::recordings();
    let count = names.len() as u64 * 2;
    let seeds = support::seeds(count);
    support::run_pass("ref_recordings", &seeds, None, |seed| {
        let name = &names[(seed % names.len() as u64) as usize];
        r#gen::recording_case(name, seed, seed >= names.len() as u64)
    });
}

/// The oracle notices fork change 1: with the kitty protocol enabled, the 4,097th keyboard mode
/// push panics in 0.26.0 (it evicts from the empty title stack) and works in feathercritty.
#[test]
fn oracle_sees_fork_change_1() {
    let start = Start { lines: 5, columns: 10, history: 10, kitty: true, query_seed: 0 };
    let below = Step::Bytes(b"\x1b[>1u".repeat(4_096));
    assert!(support::run(&start, std::slice::from_ref(&below)).is_ok());

    let divergence = support::run(&start, &[below, Step::Bytes(b"\x1b[>1u".to_vec())])
        .expect_err("0.26.0 should panic on the 4,097th push");
    assert!(divergence.diff.contains("0.26.0 panicked"), "{divergence}");
}

/// The oracle compares cells in history, not only the screen.
#[test]
fn oracle_sees_history_difference() {
    let start = Start { lines: 2, columns: 4, history: 10, kitty: false, query_seed: 0 };
    let steps = [Step::Bytes(b"ab\r\ncd\r\nef".to_vec())];
    let mut a = support::fc::Side::new(&start);
    let mut b = support::ac::Side::new(&start);
    a.apply(&steps[0]);
    b.apply(&steps[0]);
    assert_eq!(a.snapshot(), b.snapshot());

    // Change one history cell on the feathercritty side only.
    a.term.grid_mut()[feathercritty::index::Line(-1)][feathercritty::index::Column(0)].c = 'X';
    let (sa, sb) = (a.snapshot(), b.snapshot());
    assert_ne!(sa, sb);
    assert!(support::project::diff(&sa, &sb).contains("row -1"));
}

/// The minimizer keeps only the steps a failure needs.
#[test]
fn minimizer_keeps_needed_steps() {
    let steps: Vec<Step> = (0..50).map(Step::SetHistory).collect();
    let fails = |steps: &[Step]| {
        steps.contains(&Step::SetHistory(7)) && steps.contains(&Step::SetHistory(31))
    };
    let minimized = support::minimize::minimize(&steps, fails);
    assert_eq!(minimized, vec![Step::SetHistory(7), Step::SetHistory(31)]);

    let bytes = vec![Step::Bytes(b"0123456789abcdef".to_vec())];
    let minimized = support::minimize::minimize(&bytes, |steps| match steps {
        [Step::Bytes(b)] => b.contains(&b'a') && b.contains(&b'3'),
        _ => false,
    });
    assert_eq!(minimized, vec![Step::Bytes(b"3a".to_vec())]);
}

/// Generation is deterministic and prefix-stable, so `DIFF_SEED` with a smaller `DIFF_STEPS`
/// replays the start of a failing run.
#[test]
fn generator_is_deterministic() {
    let a: Case = r#gen::random_case(7, 200);
    let b: Case = r#gen::random_case(7, 100);
    assert_eq!(a.start, b.start);
    assert_eq!(a.steps[..100], b.steps[..]);
}
