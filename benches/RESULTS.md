<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# Benchmark results: feathercritty with compact history

Measured, not gated. Regenerate each section with the command named in it.

## Large histories

Design spec 6.8. Produced by `cargo test --release --test large_history -- --ignored --nocapture`
(`large_history_cost_table`; the same run checks correctness: `large_history_matches_oracle_at_100k`
against the differential test's oracle and `large_history_invariants_at_1m`). These numbers set the
Settings warning threshold of spec 5.11 (default 100,000 lines, raised only if 100,000 lines rewrap
in under about 50 ms).

- Date: 2026-10-25 (re-measured after the phase 4 tuning; the first measurement, at 50c48aa, had
  100,000-line rewraps of 44 to 69 ms and 1,000,000-line rewraps of 437 to 679 ms)
- Machine: Apple M5 Max, macOS (Darwin 27.0.0), rustc 1.99.1 (58cf8f9c3 2026-10-13), release profile
- Commit: d946bd3 (phase 4: reflow byte copy and codec tuning); terminal 126 columns by 37 lines
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
| short lines (0-20 chars) | 100000 | 7.9 | 79 | 302.4 | 10.4 | 12.1 | 3.1 |
| log lines (95 chars, 4 colors) | 100000 | 20.1 | 200 | 302.4 | 22.0 | 24.5 | 4.3 |
| full-width lines (126 chars) | 100000 | 19.5 | 194 | 302.4 | 21.1 | 25.2 | 4.3 |
| short lines (0-20 chars) | 1000000 | 57.8 | 57 | 3024.0 | 60.1 | 222.9 | 3.1 |
| log lines (95 chars, 4 colors) | 1000000 | 179.5 | 179 | 3024.0 | 182.0 | 344.9 | 4.5 |
| full-width lines (126 chars) | 1000000 | 173.8 | 173 | 3024.0 | 175.9 | 312.2 | 4.3 |

The resident set after the resizes is the allocator keeping freed blocks (live bytes stay within 6%
of their value after the fill), not memory the terminal still uses; fbterm's release call
after a resize or trim (spec 5.9) is what returns it.

### Time (ms)

| content | history lines | fill | 126->100 columns | 100->126 columns | 37->30 lines | 30->37 lines | trim to 10,000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| short lines (0-20 chars) | 100000 | 18 | 4.236 (4.163-4.472) | 4.607 (4.526-4.703) | 0.002 (0.002-0.003) | 0.001 (0.001-0.002) | 0.262 (0.260-0.266) |
| log lines (95 chars, 4 colors) | 100000 | 96 | 2.987 (2.953-3.190) | 3.470 (3.452-3.498) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 0.280 (0.274-0.290) |
| full-width lines (126 chars) | 100000 | 71 | 27.742 (27.613-55.785) | 32.795 (32.611-32.993) | 0.002 (0.002-0.002) | 0.001 (0.001-0.004) | 0.276 (0.150-0.480) |
| short lines (0-20 chars) | 1000000 | 164 | 42.852 (42.652-44.478) | 46.418 (46.282-47.485) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 3.979 (3.927-4.271) |
| log lines (95 chars, 4 colors) | 1000000 | 965 | 30.974 (30.820-31.466) | 35.200 (35.048-35.833) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 3.134 (2.902-3.175) |
| full-width lines (126 chars) | 1000000 | 721 | 278.772 (277.781-558.257) | 325.239 (324.356-327.320) | 0.002 (0.002-0.002) | 0.001 (0.001-0.005) | 3.118 (2.168-4.033) |

Reading: lines narrower than both widths (short and log lines) now rewrap by copying their records
with only the width changed (the byte-copy fast path of spec 5.6): 3 to 5 ms for 100,000 lines and
31 to 46 ms for 1,000,000. Full-width lines wrap at 100 columns, so every row is decoded, split and
re-encoded: 28 to 33 ms for 100,000 lines and about 0.3 s for 1,000,000. Every 100,000-line rewrap
measured here is now under the 50 ms bar of spec 5.11; whether to raise the Settings warning
threshold is fbterm's decision (spec 5.11) and is not changed here. Lines-only resizes and trims are
negligible. The first 126->100 pass of the full-width rows is the slow outlier in the range (every
row splits).
