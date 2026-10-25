// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Step-list minimizer: drop halves, then smaller chunks down to single steps, then shrink the
//! bytes of the remaining byte steps, keeping every change that still fails.

use std::time::{Duration, Instant};

use super::Step;

/// Wall-clock budget; the best list found so far is returned when it runs out.
const BUDGET: Duration = Duration::from_secs(180);

/// Byte steps longer than this are only shrunk by halves and quarters.
const FINE_BYTES: usize = 512;

/// Shrink `steps` while `fails` keeps returning `true`. `fails(steps)` must hold on entry.
pub fn minimize<F>(steps: &[Step], mut fails: F) -> Vec<Step>
where
    F: FnMut(&[Step]) -> bool,
{
    let deadline = Instant::now() + BUDGET;
    let mut steps = steps.to_vec();

    // Drop chunks of steps: halves first, then down to single steps.
    let mut chunk = steps.len().div_ceil(2).max(1);
    loop {
        let mut i = 0;
        while i < steps.len() && Instant::now() < deadline {
            let end = (i + chunk).min(steps.len());
            let candidate: Vec<Step> = steps[..i].iter().chain(&steps[end..]).cloned().collect();
            if fails(&candidate) {
                steps = candidate;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 || Instant::now() >= deadline {
            break;
        }
        chunk = chunk.div_ceil(2);
    }

    // Shrink the bytes of each remaining byte step the same way.
    for index in 0..steps.len() {
        let Step::Bytes(bytes) = &steps[index] else { continue };
        let mut bytes = bytes.clone();
        let mut chunk = bytes.len().div_ceil(2).max(1);
        let floor = if bytes.len() > FINE_BYTES { bytes.len().div_ceil(4) } else { 1 };
        loop {
            let mut i = 0;
            while i < bytes.len() && Instant::now() < deadline {
                let end = (i + chunk).min(bytes.len());
                let shrunk: Vec<u8> = bytes[..i].iter().chain(&bytes[end..]).copied().collect();
                let mut candidate = steps.clone();
                candidate[index] = Step::Bytes(shrunk.clone());
                if fails(&candidate) {
                    bytes = shrunk;
                    steps = candidate;
                } else {
                    i += chunk;
                }
            }
            if chunk <= floor || Instant::now() >= deadline {
                break;
            }
            chunk = chunk.div_ceil(2);
        }
    }

    steps
}
