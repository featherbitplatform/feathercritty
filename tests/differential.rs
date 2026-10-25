// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Differential test (design spec 6.1): identical inputs go to feathercritty and to unmodified
//! `alacritty_terminal` 0.26.0, and their states and query answers must match after every step.
//!
//! Knobs: `DIFF_SEEDS` (seeds per pass, default 300), `DIFF_STEPS` (steps per random seed,
//! default 400), `DIFF_SEED` (replay one seed). A failure prints the seed and a minimized step
//! list.
//!
//! The only intended behavioral difference, fork change 1 (kitty keyboard stack eviction), needs
//! more than 4,096 pushes; the generator stays below 4,000, and
//! `oracle_sees_fork_change_1` checks that the oracle does notice it.

mod support;

use support::r#gen::{self, Realistic};
use support::{Case, DEFAULT_SEEDS, DEFAULT_STEPS, Start, Step};

/// The weighted random generator, history limits 0 to 10,000.
#[test]
fn random_streams() {
    let n = support::step_count(DEFAULT_STEPS);
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS));
    support::run_pass("random_streams", &seeds, |seed| r#gen::random_case(seed, n));
}

/// The separate pass at a 100,000-line history limit, with floods of short lines so history
/// grows past 10,000. Each step compares the whole history, so this pass runs a tenth of the
/// seeds and a quarter of the steps.
#[test]
fn large_history_limit() {
    let n = support::step_count(DEFAULT_STEPS).div_ceil(4);
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS).div_ceil(10));
    support::run_pass("large_history_limit", &seeds, |seed| r#gen::large_case(seed, n));
}

/// Deterministic realistic streams: `ls` listings, git graphs, compiler diagnostics, CJK text
/// and a full-screen program on the primary screen.
#[test]
fn realistic_streams() {
    let kinds = Realistic::ALL.len() as u64;
    // Every seed runs one kind, so each kind gets a fifth of the seeds.
    let seeds = support::seeds(support::seed_count(DEFAULT_SEEDS).div_ceil(5) * kinds);
    support::run_pass("realistic_streams", &seeds, |seed| {
        r#gen::realistic_case(Realistic::ALL[(seed % kinds) as usize], seed)
    });
}

/// Every ref test recording replayed whole: once as recorded (its own size and history limit),
/// once with a 10,000-line history and interleaved resizes, view scrolls and selections.
#[test]
fn ref_recordings() {
    let names = r#gen::recordings();
    let count = names.len() as u64 * 2;
    let seeds = support::seeds(count);
    support::run_pass("ref_recordings", &seeds, |seed| {
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
