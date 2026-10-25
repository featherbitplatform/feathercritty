<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# Benchmark results: feathercritty with compact history

The measured results of the compact scrollback history (README change 7) against
`alacritty_terminal` 0.26.0, for the design gates (spec sections 6.3, 6.4, 6.5) and for large
histories (6.8). This file replaces `benches/BASELINE.md` (the numbers measured on identical code
before the compact store; summarized at the end). Regenerate each section with the command named
in it.

- Date: 2026-10-25
- Machine: Apple M5 Max, macOS (Darwin 27.0.0), rustc 1.99.1 (58cf8f9c3 2026-10-13), release profile
- Library code: as of `5e53307` (the release commit after it changes only docs, tests, the bench
  harness and one `debug_assert!`, so the release build measured is the released code)
- Comparison crate: the differential test's oracle, the published 0.26.0 source vendored at
  `tests/oracle/` with the two patches of `tests/oracle/PATCHES.md` (neither touches a timed path)
- Nothing else ran on the machine during the gate runs.

Commands:

```
cargo bench --bench ingest -- --gate
cargo bench --bench reads -- --gate
cargo run --release --example memory -- --gate
cargo test --release --test alloc
cargo test --release --test large_history -- --ignored --nocapture
```

`--gate` refuses `BENCH_ONLY`, a filter argument and `BENCH_RUNS` below 5 (error, exit status 2),
so a gate run always measures every row with at least 5 runs.

## Ingest (spec 6.3), `benches/ingest.rs`

`Processor::advance` over at least 50 MB of seeded input on a terminal whose 10,000-row history is
full. Each cell is the fork/0.26.0 ratio of medians of one `--gate` session: 7 interleaved runs per
row, and at least 31 rounds for the 1% rows (alt-screen redraw, typing echo), which also time
0.26.0 against itself (A/A). Three sessions, one after the other; spreads ((max - min) / median)
were 0.4% to 10.8%.

| Workload | Gate | 126x37: sessions 1 / 2 / 3 | 200x60: sessions 1 / 2 / 3 | Verdict |
|---|---|---|---|---|
| short lines | 1.10 | 1.030 / 1.038 / 1.027 | 1.173 / 1.176 / 1.174 | pass at 126x37; **accepted miss** at 200x60 |
| full-width plain | 1.05 | 1.302 / 1.307 / 1.297 | 1.317 / 1.298 / 1.300 | **accepted miss** |
| uniform rows | 1.05 | 0.964 / 0.966 / 0.963 | 0.979 / 0.979 / 0.982 | pass |
| dense cells | 1.05 | 1.060 / 1.092 / 1.073 | 1.085 / 1.097 / 1.066 | **accepted miss** |
| region scroll | 1.05 | 1.004 / 1.004 / 1.001 | 1.004 / 0.988 / 0.993 | pass |
| alt-screen redraw | 1.01 or A/A | 1.009 / 1.031 / 0.988 | 1.021 / 1.019 / 1.006 | see "Alt-screen redraw" |
| typing echo | 1.01 or A/A | 1.115 / 1.075 / 1.105 | 1.100 / 1.066 / 1.099 | **accepted miss** |
| log lines 95 (not gated) | - | 1.173 / 1.182 / 1.149 | 1.175 / 1.210 / 1.186 | - |
| wide chars (not gated) | - | 1.747 / 1.783 / 1.756 | 1.834 / 1.898 / 1.899 | - |

A/A spreads of the 1% rows were 0.01% to 0.72%, so their allowance stayed at 1%. Session 1 in
full (0.26.0 and fork medians in ms):

```
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
short lines 126x37                 |      282.583 |    291.137 |  1.030 |     1.4% | pass (<= 1.10x)
full-width plain 126x37            |      180.273 |    234.800 |  1.302 |     4.3% | FAIL (<= 1.05x)
uniform rows 126x37                |      862.317 |    831.421 |  0.964 |     1.6% | pass (<= 1.05x)
dense cells 126x37                 |      154.347 |    163.597 |  1.060 |     4.5% | FAIL (<= 1.05x)
region scroll 126x37               |      229.179 |    230.157 |  1.004 |     1.7% | pass (<= 1.05x)
alt-screen redraw 126x37           |      152.130 |    153.473 |  1.009 |     5.0% | pass (<= 1.01x)
  A/A alt-screen redraw 126x37     |      151.745 |            |  0.997 |     3.7% | A/A spread 0.25%
typing echo 126x37                 |      386.406 |    430.781 |  1.115 |     4.7% | FAIL (<= 1.01x)
  A/A typing echo 126x37           |      389.174 |            |  1.007 |     4.9% | A/A spread 0.72%
log lines 95 126x37                |      195.416 |    229.202 |  1.173 |     2.0% | -
wide chars 126x37                  |      141.669 |    247.505 |  1.747 |     3.5% | -
short lines 200x60                 |      248.804 |    291.950 |  1.173 |    10.8% | FAIL (<= 1.10x)
full-width plain 200x60            |      181.432 |    238.922 |  1.317 |     2.4% | FAIL (<= 1.05x)
uniform rows 200x60                |     1240.371 |   1213.830 |  0.979 |     0.8% | pass (<= 1.05x)
dense cells 200x60                 |      149.555 |    162.308 |  1.085 |     3.0% | FAIL (<= 1.05x)
region scroll 200x60               |      268.263 |    269.273 |  1.004 |     1.1% | pass (<= 1.05x)
alt-screen redraw 200x60           |      153.450 |    156.707 |  1.021 |     5.1% | FAIL (<= 1.01x)
  A/A alt-screen redraw 200x60     |      154.052 |            |  1.004 |     5.6% | A/A spread 0.39%
typing echo 200x60                 |      393.670 |    433.057 |  1.100 |     9.0% | FAIL (<= 1.01x)
  A/A typing echo 200x60           |      394.751 |            |  1.003 |    10.5% | A/A spread 0.27%
log lines 95 200x60                |      194.970 |    229.086 |  1.175 |     1.4% | -
wide chars 200x60                  |      134.552 |    246.805 |  1.834 |     2.6% | -
```

`--gate` therefore exits with status 1 on this release: the gates are kept as the spec wrote them,
and the misses below are recorded instead of loosened.

### Accepted misses

By the controller's ruling of 2026-10-25 (reported to the owner), these ingest rows miss their gate and are accepted as measured
costs of the design; the gate values are unchanged:

| Row | Gate | Measured (sessions above) |
|---|---|---|
| full-width plain, both sizes | 1.05 | 1.30 to 1.32 |
| dense cells, both sizes | 1.05 | 1.06 to 1.10 |
| typing echo, both sizes | 1.01 | 1.07 to 1.12 |
| short lines 200x60 | 1.10 | 1.17 to 1.18 |

Why: every row that scrolls off the screen is read and encoded, while 0.26.0 only rotates its
ring. The encoder has a floor of about 1 ns per stored cell for the run scan (four field compares
per cell, two of them on `Color`, an enum the compiler compares with branches) plus about 0.25 ns
per cell for the text. Per row, against the gate's budget (tuning pass, `codec_timing`):

| Row | 0.26.0 per row | Gate's budget | Fork's encode |
|---|---|---|---|
| full-width plain | about 450 ns | 22 ns (5%) | 154 ns, plus about 10 ns store and evict |
| dense | about 4.5 us | 225 ns (5%) | 323 ns |
| typing echo | about 560 ns | 5.6 ns (1%) | about 85 ns |
| short lines 200x60 | about 120 ns | 12 ns (10%) | about 30 ns |

Short lines pass at 126x37 because 0.26.0's own row resets cost more there. Meeting the other rows
would need the write path to record runs as it writes (a cost on every character in
`Term::input`) or `unsafe` SIMD compares of cells; both are outside this change's constraints.

### Alt-screen redraw

The alternate screen has no history, so no row is encoded there; the fork's only work on that path
is upstream's. The noise ruling judges the 1% rows at 31 or more interleaved runs. In the tuning
pass, 31 runs gave 1.007 and 0.995 (126x37 and 200x60), and separate processes holding one crate
each measured fork and 0.26.0 equal (152.4/152.9, 152.9/153.0, 153.4/152.2 ms), while one process
running both interleaved swings 0.93 to 1.03 between runs. In the three sessions above, at 31 runs,
126x37 gave 1.009, 1.031 and 0.988 (pass, FAIL, pass) and 200x60 gave 1.021, 1.019 and 1.006
(FAIL, FAIL, pass), with per-session spreads of 4% to 7%. The 200x60 row sits about 1% to 2% above
0.26.0 in this binary; it is reported here as measured, not as a pass.

## Reads (spec 6.4, library rows), `benches/reads.rs`

```
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
regex search, no match 126x37      |        6.028 |      7.964 |  1.321 |     3.9% | pass (<= 1.50x)
selection_to_string 1000 rows 126x37 |        0.258 |      0.090 |  0.350 |     7.5% | pass (<= 1.50x)
resize reflow 126->100->126        |        1.893 |      0.724 |  0.382 |    21.2% | pass (<= 2.00x)
resize reflow 200->80              |        4.235 |      7.644 |  1.805 |    14.1% | pass (<= 2.00x)
lines only 37->30->37 x2000 (info) |        0.131 |      4.821 | 36.697 |     2.5% | -
lines only step <=10 126x37        |     0.000042 |   0.001771 | 42.167 |   397.6% | pass (fork <= 0.05 ms)
lines only 37->1->37 126x37        |     0.000166 |   0.012125 | 73.042 |    25.3% | pass (fork <= 1 ms)
lines only step <=10 200x60        |     0.000042 |   0.002812 | 66.952 |   118.5% | pass (fork <= 0.05 ms)
lines only 60->1->60 200x60        |     0.000208 |   0.031459 | 151.245 |    20.2% | pass (fork <= 1 ms)
```

`--gate`: every row passes (exit status 0). The reflow gate is "2x or 10 ms, whichever is larger";
both reflows are under 2x in this run (200->80 has measured up to 2.02x, still under 10 ms: its log
lines are wider than 80 columns, so no row qualifies for the byte copy). Lines-only resizes are
gated by absolute bounds (controller ruling, Task 5): upstream does O(1) counter updates there and
the fork moves rows between screen and history, so the ratio rows are informational.

## Memory (spec 6.5, 6.4 peak, 6.3 allocations), `examples/memory.rs`

History memory is a terminal after 12,000 lines minus a fresh terminal of the same size, for both
crates (controller ruling, Task 8); the whole terminal is printed alongside.

```
content (minus a new Term)   |  0.26.0 MB |    fork MB |  ac B/row | fork B/row |  ratio | gate
short lines                  |      30.77 |       0.68 |      3076 |        68 |  0.022 | pass (<= 1.5 MB); whole Term 33.09 / 3.01 MB
log lines 95                 |      30.77 |       2.13 |      3076 |       212 |  0.069 | pass (<= 3.0 MB); whole Term 33.09 / 4.45 MB
wide chars                   |      30.77 |       5.60 |      3076 |       560 |  0.182 | pass (<= 0.5x 0.26.0); whole Term 33.09 / 7.93 MB
dense cells                  |      30.77 |      17.09 |      3076 |      1708 |  0.555 | pass (<= 1.0x 0.26.0); whole Term 33.09 / 19.41 MB

== peak live bytes during resizes, history full (spec 6.4, 6.5) ==
resize                       |  0.26.0 MB |    fork MB |  ratio | gate
126->100->126                |      33.74 |       4.75 |  0.141 | pass (< 0.26.0)
200->80                      |      81.48 |       5.57 |  0.068 | pass (< 0.26.0)

== allocation calls per scrolled row, history full (spec 6.3) ==
content                      |     0.26.0 |       fork | gate
full-width plain             |      0.000 |      0.000 | pass (== 0)
short lines                  |      0.000 |      0.000 | pass (== 0)
log lines 95                 |      0.000 |      0.000 | pass (== 0)
dense cells                  |      0.000 |      0.000 | pass (== 0)
underline color              |     31.000 |     31.000 | - (parser CellExtra allocations, reported separately)
```

`--gate`: every row passes (exit status 0). `tests/alloc.rs` (steady scrolling with rows that share
hyperlinks, a combining mark and an underline color: 0 allocations; reflow peak) passes too.

## Block size

The history block size stays 64 KiB. The tuning pass (Task 16) measured 16 to 256 KiB (ingest
ratios, single sessions):

| Block size | Dense 126x37 / 200x60 | Short 126x37 / 200x60 | Log lines memory |
|---|---|---|---|
| 16 KiB | 1.12 / - | 1.07 / - | |
| 64 KiB | 1.08 / 1.06 | 1.07 / 1.23 | 2.13 MB |
| 128 KiB | 1.12 / 1.12 | 1.03 / 1.20 | 2.26 MB |
| 256 KiB | 1.11 / 1.11 | 1.03 / 1.19 | 2.52 MB |

Larger blocks did not help dense cells, helped short lines only slightly, cost memory, and pushed
the allocation check's 20,000-row window short of the steady state (it reported allocations at
128 and 256 KiB).

## Large histories

Design spec 6.8. Produced by `cargo test --release --test large_history -- --ignored --nocapture`
(`large_history_cost_table`; the same run checks correctness: `large_history_matches_oracle_at_100k`
against the differential test's oracle and `large_history_invariants_at_1m`). These numbers set the
Settings warning threshold of spec 5.11 (default 100,000 lines, raised only if 100,000 lines rewrap
in under about 50 ms).

- Method: each row is a fresh child process. Live bytes come from a counting `#[global_allocator]`
  (calling thread only); RSS is the process's resident set from `ps`, whose empty baseline was
  3.0 MB. Times are medians of 5 with the range in parentheses (the resizes alternate
  126->100->126 columns five times, then 37->30->37 lines five times; the trim runs on five fresh
  terminals; fill is a single run and includes generating the input). MB is 10^6 bytes. The content
  kinds are 0-20 character lines with some color (short), 95 character lines with four colored
  runs (log), and lines exactly as wide as the terminal (full: 126 characters, so they wrap at 100
  columns). Terminal 126 columns by 37 lines.
- The history is exactly full at the given size; "dense" is what 0.26.0's rows would take
  (3,024 bytes a row at 126 columns). Each terminal also carries about 3 MB of fixed overhead
  (it is why 100,000 lines cost more per row than 1,000,000).

### Memory

| content | history lines | live MB | bytes per row | dense MB | RSS MB after fill | RSS MB after 10 resizes | live MB after 10 resizes | live MB after trim to 10,000 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| short lines (0-20 chars) | 100000 | 7.9 | 79 | 302.4 | 11.1 | 14.0 | 8.1 | 3.1 |
| log lines (95 chars, 4 colors) | 100000 | 20.1 | 200 | 302.4 | 22.0 | 24.4 | 20.2 | 4.3 |
| full-width lines (126 chars) | 100000 | 19.5 | 194 | 302.4 | 21.1 | 25.5 | 11.3 | 4.3 |
| short lines (0-20 chars) | 1000000 | 57.8 | 57 | 3024.0 | 60.2 | 222.9 | 57.9 | 3.1 |
| log lines (95 chars, 4 colors) | 1000000 | 179.5 | 179 | 3024.0 | 182.0 | 344.9 | 179.6 | 4.5 |
| full-width lines (126 chars) | 1000000 | 173.8 | 173 | 3024.0 | 175.9 | 312.4 | 88.6 | 4.3 |

After the resizes, live bytes of short and log lines are within 3% of their value after the fill.
Full-width lines lose about half: at 100 columns each one wraps into two rows, the full history
evicts the oldest half, and the round trip back to 126 columns joins the rest again (0.26.0 evicts
the same rows; `large_history_matches_oracle_at_100k` compares every cell). The resident set after
the resizes is the allocator keeping freed blocks, not memory the terminal still uses; fbterm's
release call after a resize or trim (spec 5.9) is what returns it.

### Time (ms)

| content | history lines | fill | 126->100 columns | 100->126 columns | 37->30 lines | 30->37 lines | trim to 10,000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| short lines (0-20 chars) | 100000 | 21 | 4.276 (4.232-4.453) | 4.617 (4.585-4.801) | 0.002 (0.002-0.003) | 0.001 (0.001-0.002) | 0.261 (0.260-0.265) |
| log lines (95 chars, 4 colors) | 100000 | 97 | 3.015 (2.944-3.221) | 3.481 (3.415-3.519) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 0.277 (0.260-0.282) |
| full-width lines (126 chars) | 100000 | 73 | 28.332 (28.125-56.871) | 32.809 (32.727-33.010) | 0.002 (0.002-0.002) | 0.001 (0.001-0.003) | 0.254 (0.129-0.383) |
| short lines (0-20 chars) | 1000000 | 167 | 43.054 (42.622-43.972) | 46.718 (46.314-47.759) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 2.866 (2.661-2.944) |
| log lines (95 chars, 4 colors) | 1000000 | 940 | 31.050 (30.821-31.357) | 35.115 (35.030-35.554) | 0.002 (0.002-0.003) | 0.001 (0.001-0.003) | 2.950 (2.887-3.081) |
| full-width lines (126 chars) | 1000000 | 711 | 280.810 (277.480-559.557) | 328.486 (327.856-329.884) | 0.002 (0.002-0.003) | 0.001 (0.001-0.004) | 3.043 (1.438-3.060) |

Reading: lines narrower than both widths (short and log lines) rewrap by copying their records
with only the width changed (the byte-copy fast path of spec 5.6): 3 to 5 ms for 100,000 lines and
31 to 47 ms for 1,000,000. Full-width lines wrap at 100 columns, so every row is decoded, split and
re-encoded: 28 to 33 ms for 100,000 lines and about 0.3 s for 1,000,000. Every 100,000-line rewrap
measured here is under the 50 ms bar of spec 5.11; whether to raise the Settings warning threshold
is fbterm's decision (spec 5.11) and is not changed here. Lines-only resizes and trims are
negligible. The first 126->100 pass of the full-width rows is the slow outlier in the range (every
row splits).

In the correctness run (`large_history_matches_oracle_at_100k`, rich content: colors, bold, wide
characters, hyperlinks, lines wider than 126 columns) single resizes of the 100,000-line history
took 31.4 ms (126->100) and 37.0 ms (100->126) in the fork against 26.0 and 18.1 ms in 0.26.0, and
the trim to 10,000 lines 0.5 ms against 30.4 ms. The first measurement of this table (Task 10,
before the byte copy) had 100,000-line rewraps of 44 to 69 ms.

## Baseline on identical code (formerly `benches/BASELINE.md`)

Before the compact store (commit `25f82f3`, the fork's library identical to 0.26.0 apart from
changes 1 to 5) the same binaries measured every ingest ratio between 0.96 and 1.05 (one
alt-screen row, at 1.017, missed its 1% gate), every read ratio between 0.96 and 1.02, and 3,309 bytes per history
row for both crates whatever the content (126 columns of 24-byte cells plus the row header). It
established the noise: identical code in the two columns differs by up to about 4% from run to
run, and the 1% gates sit below that floor for a binary that carries two copies of the same code,
which is why the 1% rows are judged at 31 or more interleaved runs with an A/A comparison. Do not
run other work while benchmarking.

## Moving the comparison to a new upstream release

See the module docs of `benches/common/mod.rs`: move the oracle (`tests/oracle/PATCHES.md`), check
the API list given there, re-run all binaries and replace this file.
