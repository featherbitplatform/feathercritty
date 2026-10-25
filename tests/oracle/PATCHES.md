<!-- Modified by the featherbit authors for feathercritty; see README.md for the list of changes. -->

# The differential test's oracle

This directory holds the published `alacritty_terminal` 0.26.0 crate source: `src/`, `Cargo.toml`
(the normalized manifest from crates.io) and `LICENSE-APACHE`. It is used by
`tests/differential.rs` as a path dev-dependency. Its tests and ref-test data are not vendored.

It carries exactly the two patches below and nothing else. Both are fork behavior changes that the
differential harness cannot model from outside: with them the oracle answers as feathercritty
should, so the two crates are still compared for strict equality. Fork changes 1 and 5 (README)
are *not* patched in; the harness models them (`tests/support/mod.rs`).

- `PATCHES.diff` is the unified diff from the published files to the ones here.
- `MANIFEST` records the SHA-256 of every published file (line endings normalized to LF).
- `tests/oracle_pristine.rs` applies `PATCHES.diff` in reverse to the vendored files and checks
  the hashes and the file list against `MANIFEST`. It runs offline with `cargo test`.

Each patched file starts with a line saying it was patched. Each hunk is marked
`// PATCH 1 (occ)` or `// PATCH 2 (no row)`. Line numbers below are the published file's.

## Patch 1: the `Row` `occ` invariant

Upstream documents that "all cells after `occ` are equal". `Row::reset` relies on it: when the
template's discriminant equals the last cell's, it resets only `0..occ`. Two mutators break the
invariant, and feathercritty fixes both (`src/grid/row.rs`, README):

- **`src/grid/row.rs:65`, `Row::grow`.**
  - Before: pads with default cells and leaves `occ` alone.
  - After: first sets `self.occ = max(self.occ, self.inner.len())`.
- **`src/grid/row.rs:141`, `Row::append`.**
  - Before: `self.occ += vec.len();`.
  - After: when `vec` is not empty, `self.occ = self.inner.len() + vec.len();`, so every appended
    cell is inside `occ`.

Why the oracle needs it: with the invariant broken, a reset row keeps stale cells, so what a row
looks like after `scroll_up` recycles it depends on which physical row was recycled. 0.26.0
recycles rows through hidden cache rows beyond its ring's `len` (a queue of evicted rows, refilled
1000 at a time). feathercritty's history has no such rows and recycles the row that just left the
screen. With the invariant kept, every reset row is fully reset and the recycled row's identity is
invisible.

## Patch 2: lines outside the grid are "no row"

0.26.0 reads lines just outside `topmost_line()..=bottommost_line()` in a few places. Its
`Storage` indexing then failed a `debug_assert!` in debug builds and read whichever row the ring
held at that index in release builds: a cache row, or a wrapped-around screen row. feathercritty
defines such reads (`Grid::cell`, `Grid::wraps`, `Grid::is_clear`), and the oracle gets the same
definition:

- **`src/term/mod.rs:628`, `Term::line_to_string`.**
  - Before: when a selected line ends in a leading wide-char spacer, it appends
    `self.grid[line - 1][Column(0)].c`, even on the topmost line.
  - After: the condition also requires `line > self.grid.topmost_line()`, so the topmost line
    adds nothing.
- **`src/term/mod.rs:902`, `Term::expand_wide`.**
  - Before: `let flags = self.grid[point.line][point.column].flags;`.
  - After: a point on a line outside the grid has empty flags (a default cell).
- **`src/vi_mode.rs:183`, end of `ViModeCursor::motion`.**
  - Before: the cursor could leave the grid (`Middle` on a one-line screen without history, or
    `Right`/`Last` across a wrap or leading spacer on the bottommost line).
  - After: `self.point = self.point.grid_clamp(term, Boundary::Grid);` before
    `scroll_to_point`.
- **`src/vi_mode.rs:283` (`semantic`'s `expand_semantic`), `src/vi_mode.rs:394` (`is_space`) and
  `src/vi_mode.rs:401` (`is_wrap`).**
  - Before: `term.grid()[point]`.
  - After: a new helper `cell_at`, which returns `Cell::default()` for a line outside the grid.

## Changing a patch

1. Edit the files here.
2. Run `python3 tests/oracle/refresh.py <published crate dir>`, where the published crate dir is
   for example `~/.cargo/registry/src/index.crates.io-*/alacritty_terminal-0.26.0` (Cargo
   unpacks it there for any project that depends on `alacritty_terminal = "=0.26.0"`; or
   download the `.crate` from crates.io and untar it).
3. Run `cargo test --test oracle_pristine`.

## Moving to a new upstream release

When feathercritty merges upstream X.Y.Z:
1. Delete `src/`, `Cargo.toml` and `LICENSE-APACHE` here and copy them from the published X.Y.Z
   crate.
2. Re-apply the patches: `git apply --directory=tests/oracle tests/oracle/PATCHES.diff` from the
   repository root. Resolve rejected hunks by hand, keeping each one minimal. Drop a patch if the
   release fixes the same behavior the same way, and drop its feathercritty counterpart if the
   fork no longer differs.
3. Run `refresh.py` against the published X.Y.Z directory, update this file's line numbers and
   `Cargo.lock`, and follow the rest of "Moving the oracle to a newer upstream release" in
   `tests/support/mod.rs`.
