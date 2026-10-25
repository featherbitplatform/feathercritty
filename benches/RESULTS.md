<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# Benchmark results: feathercritty with compact history

Measured, not gated. Regenerate each section with the command named in it.

## Large histories

Design spec 6.8. Produced by `cargo test --release --test large_history -- --ignored --nocapture`
(`large_history_cost_table`; the same run checks correctness: `large_history_matches_oracle_at_100k`
against the differential test's oracle and `large_history_invariants_at_1m`). These numbers set the
Settings warning threshold of spec 5.11 (default 100,000 lines, raised only if 100,000 lines rewrap
in under about 50 ms).

- Date: 2026-10-25
- Machine: Apple M5 Max, macOS (Darwin 27.0.0), rustc 1.99.1 (58cf8f9c3 2026-10-13), release profile
- Commit: 50c48aa plus the test commit; terminal 126 columns by 37 lines
- Method: each row is a fresh child process. Live bytes come from a counting `#[global_allocator]`
  (calling thread only); RSS is the process's resident set from `ps`, whose empty baseline was
  3.0 MB. Times are medians of 5 with the range in parentheses (the resizes alternate
  126->100->126 columns five times, then 37->30->37 lines five times; the trim runs on five fresh
  terminals; fill is a single run). MB is 10^6 bytes. The content kinds are 0-20 character lines
  with some color (short), 95 character lines with four colored runs (log), and lines exactly as
  wide as the terminal (full: 126 characters, so they wrap at 100 columns).
- The history is exactly full at the given size; "dense" is what 0.26.0's rows would take
  (3,024 bytes a row at 126 columns). Each terminal also carries about 3 MB of fixed overhead
  (it is why 100,000 lines cost more per row than 1,000,000).

### Memory

| content | history lines | live MB | bytes per row | dense MB | RSS MB after fill | RSS MB after 10 resizes | live MB after trim to 10,000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| short lines (0-20 chars) | 100000 | 8.4 | 84 | 302.4 | 11.1 | 18.4 | 3.1 |
| log lines (95 chars, 4 colors) | 100000 | 20.6 | 205 | 302.4 | 22.0 | 27.8 | 4.3 |
| full-width lines (126 chars) | 100000 | 20.0 | 199 | 302.4 | 21.1 | 27.3 | 4.3 |
| short lines (0-20 chars) | 1000000 | 58.6 | 58 | 3024.0 | 60.2 | 223.4 | 3.1 |
| log lines (95 chars, 4 colors) | 1000000 | 180.3 | 180 | 3024.0 | 181.9 | 345.3 | 4.5 |
| full-width lines (126 chars) | 1000000 | 174.6 | 174 | 3024.0 | 175.9 | 315.7 | 4.3 |

The resident set after the resizes is the allocator keeping freed blocks (live bytes stay within 6%
of their value after the fill), not memory the terminal still uses; fbterm's release call
after a resize or trim (spec 5.9) is what returns it.

### Time (ms)

| content | history lines | fill | 126->100 columns | 100->126 columns | 37->30 lines | 30->37 lines | trim to 10,000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| short lines (0-20 chars) | 100000 | 21 | 51.746 (51.557-52.171) | 50.560 (50.441-50.785) | 0.002 (0.002-0.008) | 0.001 (0.001-0.006) | 0.263 (0.262-0.269) |
| log lines (95 chars, 4 colors) | 100000 | 102 | 68.595 (68.296-68.789) | 68.189 (68.065-68.775) | 0.002 (0.002-0.004) | 0.002 (0.002-0.005) | 0.286 (0.277-0.488) |
| full-width lines (126 chars) | 100000 | 81 | 43.850 (43.541-88.719) | 51.769 (51.640-52.294) | 0.002 (0.002-0.002) | 0.002 (0.002-0.006) | 0.281 (0.128-0.282) |
| short lines (0-20 chars) | 1000000 | 183 | 516.682 (515.762-520.571) | 505.811 (504.227-506.254) | 0.002 (0.002-0.002) | 0.001 (0.001-0.002) | 2.906 (2.864-2.939) |
| log lines (95 chars, 4 colors) | 1000000 | 994 | 679.286 (677.539-681.112) | 679.147 (678.907-681.683) | 0.002 (0.002-0.002) | 0.002 (0.002-0.003) | 3.083 (3.038-3.185) |
| full-width lines (126 chars) | 1000000 | 794 | 437.340 (436.838-879.477) | 514.834 (513.542-517.065) | 0.002 (0.002-0.003) | 0.002 (0.002-0.005) | 4.660 (2.255-4.826) |

Reading: a 100,000-line rewrap takes 44 to 69 ms, above the 50 ms bar of spec 5.11, so the warning
threshold stays at 100,000 lines. Resizing 1,000,000 lines takes about half a second. Lines-only
resizes and trims are negligible. In the same differential run, 0.26.0's dense store rewrapped
100,000 lines in about 25 ms (columns, 126->100) and about 18 ms (100->126), against the fork's
71 and 62 ms (mixed content with wide characters, hyperlinks and underline colors; one run each).
The first 126->100 pass of the full-width rows is the slow outlier in the range (every row splits).
