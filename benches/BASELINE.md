<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# Benchmark baseline: feathercritty against alacritty_terminal 0.26.0

> **Comparison crate since `75324a8`.** The numbers below were measured against the unpatched
> crates.io `alacritty_terminal` 0.26.0. Since the grid split, the benchmarks compare against the
> differential test's oracle instead: the same source vendored at `tests/oracle/` with two patches
> (`tests/oracle/PATCHES.md`): the `Row` `occ` invariant and "no row" reads outside the grid. The
> fork needed both, and the oracle carries them so the differential test can still require
> equality. Neither touches a timed path measurably. The lines-only resize row is no longer
> gated as a ratio (see `benches/reads.rs`). Regenerate this file when the next phase is
> measured.

The numbers later phases of the compact scrollback history work are gated against (design spec
sections 6.3, 6.4, 6.5). Both columns run in the same binary; today the fork has no compact
history, so ratios are noise around 1.0 and the memory gates (expected) fail.

- Date: 2026-10-25
- Machine: Apple M5 Max, macOS (Darwin 27.0.0), rustc 1.99.1 (58cf8f9c3 2026-10-13), release profile
- Commit measured: 25f82f3 (`feature/compact-history`, before the benchmark files were added; the
  library code is identical to the commit that adds them)
- Runs: median of 15 interleaved runs for the 1% rows (alt-screen, typing), 7 for the rest;
  spread is (max - min) / median of the noisier crate. Inputs are seeded (ChaCha8), at least
  50 MB per ingest buffer, history full (10,000 rows) before timing.

Commands:

```
cargo bench --bench ingest -- --gate
cargo bench --bench reads -- --gate
cargo run --release --example memory            # report only
cargo run --release --example memory -- --gate  # pass/fail per row
```

## Ingest (spec 6.3), `benches/ingest.rs`

```
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
----------------------------------------------------------------------------------------------
short lines 126x37                 |      280.674 |    280.798 |  1.000 |     1.5% | pass (<= 1.10x)
full-width plain 126x37            |      181.246 |    179.088 |  0.988 |     2.6% | pass (<= 1.05x)
uniform rows 126x37                |      853.283 |    856.588 |  1.004 |     0.8% | pass (<= 1.05x)
dense cells 126x37                 |      145.220 |    151.444 |  1.043 |     2.3% | pass (<= 1.05x)
region scroll 126x37               |      227.393 |    225.179 |  0.990 |     1.8% | pass (<= 1.05x)
alt-screen redraw 126x37           |      151.169 |    152.591 |  1.009 |     3.6% | pass (<= 1.01x)
typing echo 126x37                 |      388.345 |    386.049 |  0.994 |     2.2% | pass (<= 1.01x)
log lines 95 126x37                |      195.956 |    195.394 |  0.997 |     2.8% | -
wide chars 126x37                  |      139.699 |    138.988 |  0.995 |     2.4% | -
short lines 200x60                 |      244.787 |    243.408 |  0.994 |     1.8% | pass (<= 1.10x)
full-width plain 200x60            |      184.736 |    177.622 |  0.961 |     1.9% | pass (<= 1.05x)
uniform rows 200x60                |     1231.519 |   1234.104 |  1.002 |     0.4% | pass (<= 1.05x)
dense cells 200x60                 |      146.239 |    151.406 |  1.035 |     3.6% | pass (<= 1.05x)
region scroll 200x60               |      272.219 |    266.890 |  0.980 |     0.8% | pass (<= 1.05x)
alt-screen redraw 200x60           |      152.590 |    155.182 |  1.017 |     4.3% | FAIL (<= 1.01x)
typing echo 200x60                 |      407.405 |    399.974 |  0.982 |     3.8% | pass (<= 1.01x)
log lines 95 200x60                |      199.547 |    194.878 |  0.977 |     4.5% | -
wide chars 200x60                  |      133.971 |    133.923 |  1.000 |     1.9% | -
```

Result of `--gate` on identical code: 1 of 14 gated rows missed (alt-screen redraw 200x60,
1.017 against 1.01). See "Noise" below.

## Reads (spec 6.4, library rows), `benches/reads.rs`

```
workload                           |    0.26.0 ms |    fork ms |  ratio |   spread | gate
----------------------------------------------------------------------------------------------
regex search, no match 126x37      |        6.002 |      6.072 |  1.012 |     5.4% | pass (<= 1.50x)
selection_to_string 1000 rows 126x37 |        0.282 |      0.286 |  1.017 |    14.2% | pass (<= 1.50x)
resize reflow 126->100->126        |        1.601 |      1.606 |  1.003 |    31.5% | pass (<= 2.00x)
resize reflow 200->80              |        3.592 |      3.465 |  0.965 |    35.9% | pass (<= 2.00x)
resize lines only 37->30->37 x2000 |        0.131 |      0.133 |  1.013 |     2.7% | pass (<= 1.20x)
```

The lines-only row repeats the 37->30->37 pair 2,000 times per run (one pair takes about 70 ns,
so a single pair is below timer resolution). The reflow rows are noisy (spread above 30%): the
first resize of a run allocates its new storage, and at 1-4 ms the OS scheduler shows. Their
gate is "2x or 10 ms, whichever is larger", so they pass with a wide margin.

## Memory (spec 6.5, 6.4 peak, 6.3 allocations), `examples/memory.rs`

```
== live bytes after 12,000 lines, 126x37, history 10000 (spec 6.5) ==
content                      |  0.26.0 MB |    fork MB |  ac B/row | fork B/row |  ratio | gate
short lines                  |      33.09 |      33.09 |      3309 |      3309 |  1.000 | FAIL (<= 1.5 MB); spec 0.26.0 ~32 MB, measured +3%
log lines 95                 |      33.09 |      33.09 |      3309 |      3309 |  1.000 | FAIL (<= 3.0 MB); spec 0.26.0 ~33 MB, measured +0%
wide chars                   |      33.09 |      33.09 |      3309 |      3309 |  1.000 | FAIL (<= 0.5x 0.26.0); spec 0.26.0 ~33 MB, measured +0%
dense cells                  |      33.09 |      33.09 |      3309 |      3309 |  1.000 | pass (<= 1.0x 0.26.0); spec 0.26.0 ~33 MB, measured +0%

== peak live bytes during resizes, history full (spec 6.4, 6.5) ==
resize                       |  0.26.0 MB |    fork MB |  ratio | gate
126->100->126                |      33.74 |      33.74 |  1.000 | FAIL (< 0.26.0)
200->80                      |      81.48 |      81.48 |  1.000 | FAIL (< 0.26.0)

== allocation calls per scrolled row, history full (spec 6.3) ==
content                      |     0.26.0 |       fork | gate
full-width plain             |      0.000 |      0.000 | pass (== 0)
short lines                  |      0.000 |      0.000 | pass (== 0)
log lines 95                 |      0.000 |      0.000 | pass (== 0)
dense cells                  |      0.000 |      0.000 | pass (== 0)
underline color              |     31.000 |     31.000 | - (parser CellExtra allocations, reported separately)
```

0.26.0 matches the spec's measured column (about 32 and 33 MB) within 3% (10% allowed). Every
row is 3,309 bytes per history row: 126 columns of 24-byte cells plus the row header, whatever the
content.

`--gate` today: 5 rows fail (short lines, log lines, wide chars, both resize peaks), dense cells
and the allocation rows pass. Exit status 1. That is the expected state before the compact store.

Allocation calls per scrolled row are already zero for 0.26.0 once the history is full (the ring
buffer recycles the evicted row), so the "zero" gate is an invariant the compact store must keep,
not an improvement to make. The underline-color row (31 calls per row) is the parser allocating a
`CellExtra` per styled run; the spec excludes it from the gate.

## Noise

Identical code in the two columns differs by up to about 4% from run to run (dense cells measured
0.966 once and 1.043 another time; the fork is often 1 to 3% "faster" on full-width plain). The
1% gates of spec 6.3 (alt-screen redraw, typing echo) are below that noise floor for a build that
carries two copies of the same code, and a clean pass should be judged by repeated runs, not one.
Do not run other work while benchmarking.

## Moving the comparison to a new upstream release

See the module docs of `benches/common/mod.rs`: bump the `alacritty_terminal = "=X.Y.Z"`
dev-dependency, check the API list given there, re-run all three binaries and replace this file.
