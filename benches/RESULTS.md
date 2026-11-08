<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# Benchmark results: feathercritty with compact history

The measured results of the compact scrollback history (README change 7) against
`alacritty_terminal` 0.26.0, for the design gates (spec sections 6.3, 6.4, 6.5) and for large
histories (6.8). This file replaces `benches/BASELINE.md` (the numbers measured on identical code
before the compact store; summarized at the end). Regenerate each section with the command named
in it.

- Date: 2026-10-25
- Machine: Apple M5 Max, macOS (Darwin 27.0.0), rustc 1.99.1 (58cf8f9c3 2026-10-13), release profile
- Library code: as of `8766119` (the release commit after it changes only docs, tests, the bench
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
| alt-screen redraw | 1.01 or A/A | 1.009 / 1.031 / 0.988 | 1.021 / 1.019 / 1.006 | **accepted miss** (see "Alt-screen redraw") |
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

Five ingest rows miss their gate; they were accepted on 2026-10-25, and the gate
values are unchanged. The first four are measured costs of the design:

| Row | Gate | Measured (sessions above) |
|---|---|---|
| full-width plain, both sizes | 1.05 | 1.30 to 1.32 |
| dense cells, both sizes | 1.05 | 1.06 to 1.10 |
| typing echo, both sizes | 1.01 | 1.07 to 1.12 |
| short lines 200x60 | 1.10 | 1.17 to 1.18 |
| alt-screen redraw, both sizes | 1.01 or A/A | 0.99 to 1.03 (see "Alt-screen redraw") |

Why, for the first four: every row that scrolls off the screen is read and encoded, while 0.26.0 only rotates its
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
0.26.0 in this binary. It is the fifth accepted miss: the path does no history work, and the
separate single-crate processes measured fork and 0.26.0 equal, so the residue is attributed to the
code layout of the combined bench binary, not to the fork.

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
gated by absolute bounds (project ruling on spec 6.4): upstream does O(1) counter updates there and
the fork moves rows between screen and history, so the ratio rows are informational.

## Memory (spec 6.5, 6.4 peak, 6.3 allocations), `examples/memory.rs`

History memory is a terminal after 12,000 lines minus a fresh terminal of the same size, for both
crates (project ruling on spec 6.5); the whole terminal is printed alongside.

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

The history block size stays 64 KiB. The tuning pass measured 16 to 256 KiB (ingest
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
re-encoded: 28 to 33 ms for 100,000 lines and about 0.3 s for 1,000,000. Every median of a
100,000-line rewrap is under the 50 ms bar of spec 5.11; the first 126->100 pass of full-width rows
reached 56.9 ms; whether to raise the Settings warning threshold
is fbterm's decision (spec 5.11) and is not changed here. Lines-only resizes and trims are
negligible. The first 126->100 pass of the full-width rows is the slow outlier in the range (every
row splits).

In the correctness run (`large_history_matches_oracle_at_100k`, rich content: colors, bold, wide
characters, hyperlinks, lines wider than 126 columns) single resizes of the 100,000-line history
took 31.4 ms (126->100) and 37.0 ms (100->126) in the fork against 26.0 and 18.1 ms in 0.26.0, and
the trim to 10,000 lines 0.5 ms against 30.4 ms. The first measurement of this table (before
the byte copy) had 100,000-line rewraps of 44 to 69 ms.

## Compressor choice (history compression, phase 0)

Which compressor and level the cold-block compression of the design (spec 5.3, 6.3) uses.
Command: `cargo test --release --test compress_choice -- --ignored --nocapture` (about 4 s).
Each corpus (`tests/corpora/`) goes through `Processor::advance` into a 126-column terminal with a
1,000,000-line history; every 64 KiB history block (partly filled ones included) is compressed and
decompressed on its own, best of 3 passes, and checked to round trip. The three generated corpora
have 300,000 rows (seeds 1, 2 and 3); the two recorded streams (a `cargo build` log of this
workspace, 417 lines, and `git log --graph` of this repository, 562 lines, redacted to `/work`) fit
in one partly filled block each, so their ratios are for a block of 28 and 55 KB (smaller than
the 64 KiB a full block has, so a little worse than a full block would give). "Projected vs off"
is (compressed block bytes + 16 bytes of row index per row) over (block bytes + the same index);
the rule below allows 40%.

Machine as above (Apple M5 Max), zstd 0.14.3 (C library 1.5.7), lz4_flex 0.14.0 (`safe-encode`,
`safe-decode`, with `std` for the size-prefixed block functions; the 4-byte prefix is counted).

### build log: 14182801 input bytes, 299965 history rows, 313 blocks, 20494665 block bytes (68.3 bytes per row)

| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off |
|---|---|---|---|---|---|
| lz4_flex block | yes | 0.178 | 1944 | 6001 | 33.4% |
| zstd level 1 | no | 0.114 | 1473 | 3675 | 28.2% |
| zstd level 3 | no | 0.111 | 1373 | 3700 | 28.0% |

### service log: 36075261 input bytes, 299964 history rows, 840 blocks, 54968639 block bytes (183.3 bytes per row)

| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off |
|---|---|---|---|---|---|
| lz4_flex block | yes | 0.248 | 1450 | 4552 | 30.8% |
| zstd level 1 | no | 0.146 | 1295 | 2942 | 21.5% |
| zstd level 3 | no | 0.143 | 1156 | 2998 | 21.2% |

### shell session: 18059993 input bytes, 299984 history rows, 400 blocks, 26172485 block bytes (87.2 bytes per row)

| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off |
|---|---|---|---|---|---|
| lz4_flex block | yes | 0.253 | 1384 | 4757 | 36.9% |
| zstd level 1 | no | 0.152 | 1210 | 2709 | 28.3% |
| zstd level 3 | no | 0.152 | 1046 | 2834 | 28.3% |

### recorded cargo build: 19481 input bytes, 385 history rows, 1 blocks, 27967 block bytes (72.6 bytes per row)

| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off |
|---|---|---|---|---|---|
| lz4_flex block | yes | 0.278 | 2245 | 8949 | 40.8% |
| zstd level 1 | no | 0.182 | 1203 | 2881 | 33.0% |
| zstd level 3 | no | 0.178 | 1128 | 2893 | 32.6% |

### recorded git graph: 33811 input bytes, 526 history rows, 1 blocks, 54811 block bytes (104.2 bytes per row)

| candidate | pure Rust | ratio | compress MB/s | decompress MB/s | projected vs off |
|---|---|---|---|---|---|
| lz4_flex block | yes | 0.389 | 1720 | 6294 | 47.1% |
| zstd level 1 | no | 0.266 | 1177 | 2685 | 36.3% |
| zstd level 3 | no | 0.257 | 837 | 2621 | 35.6% |

`ruzstd` 0.8.2, the pure-Rust zstd, was measured in a first run and then dropped from the test:
its encoder only implements the fastest level, `cargo deny` reports RUSTSEC-2026-0344 on it
(fixed in 0.9.1, which needs a newer Rust than this crate's 1.85), and it misses the 40% rule on
the git graph anyway. Its rows (ratio, compress MB/s, decompress MB/s, projected vs off): build log
0.149 / 336 / 1012 / 31.1%, service log 0.206 / 271 / 782 / 26.9%, shell session 0.200 / 269 /
768 / 32.4%, recorded cargo build 0.222 / 287 / 735 / 36.3%, recorded git graph 0.341 / 221 / 541 /
42.9%.

### Decision (spec 5.3 rule)

1. At most 40% of off on every realistic corpus: **lz4_flex fails** (recorded cargo build 40.8%,
   recorded git graph 47.1%); ruzstd fails (git graph 42.9%). **zstd level 1 and level 3 pass**
   (worst case 36.3% and 35.6%, both on the git graph).
2. Fastest decompression among those: level 3 is ahead by 0.4% to 4.6% on four corpora and behind by 2.4% on one (git graph), run to run spreads were about 2.5%, so it is a tie. Level 1
   compresses 7% to 41% faster, and compression runs on the ingest path (spec 6.3), so the tie goes
   to level 1; level 3 buys at most 0.7 points of projected size.
3. Pure Rust within 20%: no pure-Rust candidate passes step 1, so it does not apply.

**Chosen: `zstd`, level 1** (the `zstd` crate, which builds the C library; MIT or Apache-2.0).
`cargo deny check` passes with it under the terminal repository's `deny.toml` (advisories, bans,
licenses and sources ok; this repository has no `deny.toml` of its own, and with cargo-deny's
empty default every license is rejected, with or without the new crates).

Projected compressed history against off (level 1, block bytes plus index): build log 28.2%, service
log 21.5%, shell session 28.3%, recorded cargo build 33.0%, recorded git graph 36.3%. For a
1,000,000-row history at the measured bytes per row (68 to 183 for the generated corpora) that is
68 to 183 MB of blocks plus 16 MB of index off, against 8 to 27 MB of blocks plus the same index
compressed. The index (16 bytes per row) is uncompressed and caps the saving: at 68 bytes per row
it is 19% of off.

## History compression (README change 9)

The gates of the history compression design (its sections 6.2, 6.3 and 6.4, library rows) for
`0.26.0+fbterm.7`. The fbterm rows of 6.4 (draw-list build, fbterm search and export, scenario I)
are measured in the terminal repository.

- Date: 2026-11-08. Machine as above (Apple M5 Max), rustc 1.99.1, release profile.
- Every gate is a median of at least 5 runs (31 interleaved rounds for "off means off"); spreads
  are (max - min) / median.

Commands:

```
cargo bench --bench ingest -- --gate     # last section: compression on, worker, lock holds, flood
cargo bench --bench reads -- --gate      # last section: 1,000,000-line reads, reflow, switching
cargo test --release --test large_history -- --ignored --nocapture --exact large_history_cost_table
cargo run --release --example memory -- --gate
cargo test --release --test alloc
```

plus the harness of "Off means off" below, which is not part of the repository.

### Off means off (design 6.2)

**Method.** Compression off (`history_compression: None`, the default) against the fork's `main`
before this work (`026ea07`), in one binary, the way `benches/ingest.rs` compares with 0.26.0: a
scratch crate depends on this branch and on a worktree of `026ea07` whose package is renamed
(`feathercritty_main`, dependency key `alacritty_terminal`), includes `benches/common/mod.rs`, and
runs every ingest workload at both sizes plus the warm reads (regex search, `selection_to_string`
over 1,000 rows, every history row through `RowReader` ten times, the two reflows). Every row runs
31 interleaved rounds of main, branch, main again, and passes within `max(1%, A/A spread)` (the
noise rule of the 1% rows above, applied to every row). It is built with fbterm's release profile
(`lto = "thin"`, `codegen-units = 1`): with the default profile, codegen-unit partitioning moved
whole functions between runs of unchanged source (`decode_cell_row` took 15% more samples on the
branch with its source identical).

**What was found and fixed** (first measurements, before the fixes):

| Row | Before | Cause | Fix |
|---|---|---|---|
| every history row through `RowReader` | 1.035 to 1.047 | reads returned an enum carrying an `Arc` for cold records; its drop and `Deref` cost every warm read | records reach their reader through a closure (`with_record`); the warm path is a borrow and one `cold` check |
| reflow 126->100->126 (byte copy) | 1.12 to 1.13 | every stored record's index entry parsed the record header again for its two header bits | entries take the bits from the encoder's returned header, or from the entry a record is copied from |
| same, after that | 1.05 | `release` carried the generation and cold-block bookkeeping inline | the rare branches (a truncated last record, an emptied block) moved to `#[cold]` functions |

**Result**, three sessions (ratio branch / main). Session 1: before the `release` split; session
2: the final code except the cold-read and reflow changes of the compression section below (they
do not touch these paths); session 3: the final code.

| Row | Session 1 | Session 2 | Session 3 | Verdict |
|---|---|---|---|---|
| ingest, all 18 rows but those below | 0.981 to 1.010 | 0.915 to 1.009 | 0.983 to 1.008 | pass |
| ingest short lines 126x37 | 1.009 | 0.995 | **1.015** | pass, pass, FAIL |
| ingest dense cells 126x37 | 1.003 | 1.004 | **1.025** | pass, pass, FAIL |
| ingest dense cells 200x60 | **1.030** | **1.016** | **1.012** | FAIL |
| ingest typing echo 200x60 | **1.011** | 0.943 | 1.008 | FAIL, pass, pass |
| regex search, no match | 0.970 | 1.005 | 0.976 | pass |
| `selection_to_string` 1,000 rows | 1.003 | 1.021 | 1.018 | pass (A/A 1.8% to 2.2%) |
| every history row through `RowReader` | 0.984 | 0.991 | 0.986 | pass |
| reflow 126->100->126 (10,000 rows, 0.64 ms) | **1.051** | **1.043** | **1.032** | FAIL |
| reflow 200->80 | **1.013** | 0.980 | 1.004 | FAIL, pass, pass |

Session 3 in full (main, branch and main again, medians in ms):

```
row                                  |    main ms |  branch ms |   main2 ms |  ratio |
ingest short lines 126x37            |    280.070 |    284.340 |    280.532 |  1.015 | A/A  0.16% | spread   2.9% | FAIL (<= 1.010)
ingest full-width plain 126x37       |    242.190 |    242.385 |    242.094 |  1.001 | A/A  0.04% | spread   2.1% | pass (<= 1.010)
ingest uniform rows 126x37           |    785.430 |    789.685 |    785.531 |  1.005 | A/A  0.01% | spread   2.1% | pass (<= 1.010)
ingest dense cells 126x37            |    134.510 |    137.892 |    134.690 |  1.025 | A/A  0.13% | spread   2.5% | FAIL (<= 1.010)
ingest region scroll 126x37          |    228.018 |    227.472 |    228.158 |  0.998 | A/A  0.06% | spread   1.5% | pass (<= 1.010)
ingest alt-screen redraw 126x37      |    157.332 |    156.283 |    156.938 |  0.993 | A/A  0.25% | spread   3.9% | pass (<= 1.010)
ingest typing echo 126x37            |    448.759 |    452.184 |    447.427 |  1.008 | A/A  0.30% | spread   3.6% | pass (<= 1.010)
ingest log lines 95 126x37           |    220.202 |    219.119 |    220.429 |  0.995 | A/A  0.10% | spread   1.4% | pass (<= 1.010)
ingest wide chars 126x37             |    245.742 |    246.887 |    245.903 |  1.005 | A/A  0.07% | spread   4.4% | pass (<= 1.010)
ingest short lines 200x60            |    274.615 |    276.404 |    275.138 |  1.007 | A/A  0.19% | spread  27.5% | pass (<= 1.010)
ingest full-width plain 200x60       |    244.688 |    240.539 |    244.756 |  0.983 | A/A  0.03% | spread   1.3% | pass (<= 1.010)
ingest uniform rows 200x60           |   1135.688 |   1139.447 |   1134.721 |  1.003 | A/A  0.09% | spread   1.6% | pass (<= 1.010)
ingest dense cells 200x60            |    133.978 |    135.596 |    134.001 |  1.012 | A/A  0.02% | spread   6.0% | FAIL (<= 1.010)
ingest region scroll 200x60          |    265.319 |    264.441 |    265.248 |  0.997 | A/A  0.03% | spread   1.6% | pass (<= 1.010)
ingest alt-screen redraw 200x60      |    162.531 |    161.667 |    162.311 |  0.995 | A/A  0.14% | spread   3.0% | pass (<= 1.010)
ingest typing echo 200x60            |    439.193 |    442.919 |    438.812 |  1.008 | A/A  0.09% | spread   2.8% | pass (<= 1.010)
ingest log lines 95 200x60           |    221.781 |    220.113 |    221.876 |  0.992 | A/A  0.04% | spread   3.4% | pass (<= 1.010)
ingest wide chars 200x60             |    246.872 |    246.600 |    246.812 |  0.999 | A/A  0.02% | spread   2.2% | pass (<= 1.010)
read regex search, no match 126x37   |      6.596 |      6.437 |      6.612 |  0.976 | A/A  0.24% | spread   8.8% | pass (<= 1.010)
read selection_to_string 1000 rows   |      0.069 |      0.071 |      0.068 |  1.018 | A/A  1.92% | spread  48.7% | pass (<= 1.019)
resize reflow 126->100->126          |      0.640 |      0.660 |      0.650 |  1.032 | A/A  1.65% | spread  13.8% | FAIL (<= 1.016)
resize reflow 200->80                |      7.116 |      7.143 |      7.105 |  1.004 | A/A  0.15% | spread   5.6% | pass (<= 1.010)
read every history row x10 (RowReader) |     10.135 |      9.995 |     10.168 |  0.986 | A/A  0.33% | spread   4.5% | pass (<= 1.010)
4 row(s) missed
```

**The remaining misses.** Rows move in and out of the 1% band between sessions whose changes do
not touch their path (short lines and dense cells at 126x37 between sessions 2 and 3; typing echo
went from 1.011 to 0.943), and session 2 has rows up to 8.5% *faster* than main for no change of
theirs. A control build of `026ea07` whose only change is one unused `Option<usize>` field in
`Config` measured 0.98 to 1.02 against `026ea07` in separate processes on dense cells 200x60 and
the reflow. With compression off the branch adds, on the ingest path, one `Option` check per
pushed row and one per new block (design 5.2 allows "one branch on an `Option`"), and on reads one
`cold` check. The reflow of a 10,000-row history keeps a residue of 2% to 4% (0.02 ms): profiling it
(`samply`) put the difference in `Cell::reflow_info`, whose machine code is byte-identical in both
builds and equally aligned, and in `RowSource::next_reflow`, which carries the two `Option` checks
per row of `move_oldest_to` and `push_encoded`. The verdict is left to the owner (see the report of
this task); the gates are unchanged.

**Memory and allocations.** Live bytes are not identical: a block's bookkeeping grew from 32 to 48
bytes (warm-row count, generation, cold flag), so a history holds 16 bytes more per slot of its
block deque, measured exactly with a counting allocator: +128 B (short lines, 10,000 rows), +512 B
(log lines), +8,192 B (dense cells), +65,536 B (1,000,000 log lines, 203.6 MB in all; 0.03%).
`examples/memory.rs --gate` passes as before (log lines 2.13 MB, dense cells 17.10 against 17.09 MB
before), and `tests/alloc.rs` passes: zero allocations per scrolled row at a full history, with
compression off and on.

### Memory (design 6.3)

Live bytes of a terminal (126x37) at 1,000,000 history lines with `Some(10_000)` after every job
ran, against the same content with compression off (`large_history_cost_table`, last table; each
cell a fresh child process, jobs run after every 64 KiB of input). The recorded streams have a few
hundred lines, so they are repeated to fill the history; a 64 KiB block holds about one copy, so
the repetition helps little (phase 0 measured the single streams at 33.0% and 36.3% projected).

| Corpus | Off MB | Compressed MB | Compressed / off | Gate |
|---|---:|---:|---:|---|
| build log | 86.8 | 27.0 | 31.1% | pass (<= 40%) |
| service log | 202.1 | 47.0 | 23.3% | pass |
| shell session | 105.8 | 32.6 | 30.8% | pass |
| recorded cargo build (repeated) | 91.8 | 25.8 | 28.1% | pass |
| recorded git graph (repeated) | 122.7 | 44.3 | 36.1% | pass |
| random log lines (`large_history.rs`) | 179.6 | 82.1 | 45.7% | pass (never above off) |

Recorded, not gated: at 100,000 lines the same corpora take 37.5% to 51.1% of off (random 56.4%):
the uncompressed 16-byte index entry per row and the fixed per-terminal overhead weigh more there.
The read cache holds at most 4 decompressed blocks (256 KiB) and is empty after the jobs ran.

Peaks (design 6.4, "peak live bytes never above off" for the reflow, "memory never above the
uncompressed size" when switching on), exact bytes at 1,000,000 lines:

| Corpus | Off peak in 10 resizes | Compressed peak | Switching on: peak over uncompressed |
|---|---:|---:|---:|
| build log | 103,141,157 | 103,017,695 (pass) | +131,222 B |
| service log | 222,712,867 | 200,986,295 (pass) | +131,306 B |
| recorded cargo build (repeated) | 108,072,949 | 108,078,845 (**+5,896 B**) | +131,292 B |
| shell session, git graph, random | | pass (at most equal in MB) | +131,298 to +156,702 B |

A job copies its block under the lock (design 5.2), and the warm buffer an install replaces is
kept as a spare (at most 2), so switching on goes about two blocks (128 KiB, 0.08%) above the
uncompressed size before it comes down (service log: 174.8 MB to 43.1 MB). The compressed reflow's
peak includes the read cache (up to 256 KiB) and comes within 6 KB above off on the most
compressible corpus. Both are reported to the owner as measured; the gates are unchanged.

### Speed with compression on (design 6.4, library rows)

`benches/ingest.rs` and `benches/reads.rs`, last sections: 1,000,000 history lines of log lines at
126x37, `Some(10_000)`. The worker (`benches/common/mod.rs`, `worker`) runs on its own thread and
shares the terminal through a `parking_lot::Mutex` as fbterm does; the parser locks, parses
64 KiB, checks `has_history_jobs`, unlocks and wakes the worker.

```
== history compression (feathercritty only; the 0.26.0 column is compression off) ==
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
----------------------------------------------------------------------------------------------
log lines 95 126x37, 1M history, on+worker vs off |      223.013 |    229.670 |  1.030 |     1.3% | pass (<= 1.03x)
  jobs left when the parser finished: 8 of 8 runs
worker lock holds, 1M-line fill    | p99 0.0070 ms (fills 0.0069-0.0072) | max 0.0107 ms (median of fills) | 1044 holds per fill | pass (p99 <= 0.1 ms)
flood with the worker              | 60 s, 227 MB/s | most warm blocks: first half 38, second half 38, after stop 29 | worker 243716 jobs, 20.9% of one core | pass (bounded)
9 ingest row(s) missed their threshold
```

| Row | Gate | Measured | Verdict |
|---|---|---|---|
| parser-side ingest, on with the worker, against off | at most 1.03x | 1.041 (first session), 1.030 (second) | borderline: FAIL, pass |
| worker lock holds over a 1,000,000-line fill | p99 at most 0.1 ms | p99 0.0070 ms, max 0.0107 ms, about 1,050 holds | pass |
| 60 s flood of log lines (227 MB/s) | backlog bounded | at most 38 warm blocks in both halves (29 is the warm window), 20.9% of one core | pass |
| switching on, 1,000,000 lines | holds as above; time recorded | 137 ms, 2,796 jobs, holds p99 0.0167 ms | pass |
| switching off | holds as above; time recorded | 79 ms, holds p99 0.0055 ms | pass |

**Batches.** With one job per lock hold the worker fell behind a flood: a parser that holds the
lock almost all the time left it about one hold per 64 KiB parsed, and two holds per job (take,
install) were too few. The warm blocks grew by about 24 per 2 MB of input with an ordinary unlock
and by about 5.5 with a fair one. `Term::next_history_jobs(max)` takes a batch in one hold, and the
worker installs a batch and takes the next in the same hold: the backlog stays at the warm window
plus up to 9 blocks for 60 s, and the holds stay below 0.011 ms.

**Where the ingest cost goes** (experiments in the tuning pass, 5 runs each): the parser waited
3.0 ms per 50 MB for the worker's holds (1.4%; each block is copied under the lock, design 5.2); a
thread compressing on a neighbouring core with no lock at all cost the parser 2.0% to 2.9%
(shared caches); compression on with nothing ever eligible (`keep_warm` above the history size) cost 2.3%,
most of it lock traffic: there every evicted block raised a job that turned out empty. Keeping more spare blocks changed
nothing (1.033 with 16 spares). A host that runs the worker on efficiency cores (fbterm's
low-priority worker, design 5.4) would move the cache share away from the parser; the library
cannot set that.

```
== history compression, 1,000,000-line history (feathercritty only; the 0.26.0 column is compression off, the fork column compression on) ==
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
----------------------------------------------------------------------------------------------
1M: regex search, no match         |      650.064 |    729.806 |  1.123 |     1.8% | pass (<= 2.00x)
1M: selection_to_string 1000 rows  |        1.462 |      3.708 |  2.537 |     4.2% | -
1M: export-equivalent read         |      175.441 |    250.668 |  1.429 |    25.4% | pass (<= 2.00x)
1M: reflow 126->100                |      127.490 |    184.064 |  1.444 |     1.9% | FAIL (<= off + decompress 53.135 ms = 180.626 ms, and <= 1000 ms)
1M: reflow 100->126                |      201.710 |    263.057 |  1.304 |     1.3% | FAIL (<= off + decompress 57.188 ms = 258.898 ms, and <= 1000 ms)
1M: switch on                      | total 137.4 ms (137.0-139.0) | 2796 jobs | holds p99 0.0167 ms, max 0.0266 ms | pass (p99 <= 0.1 ms)
1M: switch off                     | total 78.9 ms (78.8-79.2) | 2796 jobs | holds p99 0.0055 ms, max 0.0087 ms | pass (p99 <= 0.1 ms)
decompress one block               | as cold.rs 28.00 us | reused context and buffer 26.94 us | 0.962 | 2796 blocks | -
2 read row(s) missed their threshold
```

| Row | Gate | Measured | Verdict |
|---|---|---|---|
| regex search, no match | at most 2x | 1.12x (730 ms against 650 ms) | pass |
| every history row read as text (export-equivalent) | at most 2x | 1.43x | pass |
| `selection_to_string`, 1,000 rows 500,000 up, read cache cold | none (recorded) | 2.54x (185 us against 73 us per selection) | - |
| reflow 126->100 | off + decompress once, at most 1 s | 184.1 ms against 127.5 + 53.1 = 180.6 ms | **FAIL by 1.9%** (spread 1.9%) |
| reflow 100->126 | same | 263.1 ms against 201.7 + 57.2 = 258.9 ms | **FAIL by 1.6%** (spread 1.3%) |
| decompress one 64 KiB block | recorded | 28.0 us as `cold.rs` does it; 26.9 us with a reused context and no zeroing (-3.8%) | - |

The selection row has no gate in the design; a selection that starts in cold history with an
empty cache pays for about four blocks of decompression, more than reading those rows warm. A
selection made by dragging over the rows has just read them through the cache.

The reflow was first 1.53x and 1.33x (12 ms and 9 ms over the bound): every record read looked
the block up in the cache, moved it to the back and cloned its `Arc`, twice per row, and the new
history kept the warm-row accounting for every pushed row. Two changes took most of it: a read of
the block read last is served in place under a shared borrow of the cache (no reordering, no
reference count), and the reflow builds its new history with compression off and applies the
setting once at the end (one count over the warm rows). What is left (3 to 4 ms over 1,000,000
rows) is within the run's spread, and is reported to the owner as measured.

The per-call zstd context and the zeroed buffer of `cold.rs` cost 3.8% of a block's
decompression. No gate depends on it (the reflow bound counts decompression as `cold.rs` does it,
and the other reads pass with margin), so `cold.rs` is unchanged.

### Large histories with compression off

`large_history_cost_table`, first two tables, as in "Large histories" above (fill once, resizes
median of 5; this test binary uses the default release profile): 1,000,000 log lines take
179.6 MB live (179.5 before; the 64 KiB of block bookkeeping above). Two runs at 1,000,000 lines,
against the table above: log lines 126->100 30.5 and 32.1 ms (31.1 before), 100->126 35.2 and
39.7 ms (35.1); full-width 269.8 and 266.3 ms (280.8), 312.0 and 311.9 ms (328.5); short lines
41.5 and 43.2 ms (43.1), 46.6 and 47.5 ms (46.7). The second run's log-line 100->126 is 13%
slower at both sizes, after changes that do not touch the path with compression off; in fbterm's
release profile (thin LTO, one codegen unit, separate processes) 200 rounds of 126->100->126 on a
10,000-row history measured 0.8% to 1.9% slower than `026ea07`, as in "Off means off".

## Baseline on identical code (formerly `benches/BASELINE.md`)

Before the compact store (commit `05dd594`, the fork's library identical to 0.26.0 apart from
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
