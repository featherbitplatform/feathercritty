# Inline images, emulation side (feathercritty) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. The implementer step runs through the `local-implementer` skill.

**Goal:** feathercritty parses sixel, the kitty graphics protocol (incl. Unicode placeholders) and OSC 1337 inline images safely, keeps image placements in step with the grid, answers detection queries, and hands image payloads to the embedder (fbterm) for decoding and drawing.

**Architecture:** the `vte 0.15.0` parser is copied into the crate as `src/vte/` (re-exported as `feathercritty::vte`, replacing today's `pub use vte;`, so fbterm's imports keep working) and extended with streaming APC, DCS sixel and OSC 1337 callbacks and byte caps on every string sequence. `src/graphics/` holds the three protocol state machines (sixel decodes to RGBA here; kitty and OSC 1337 read only the image header for the pixel size) and the placement side table keyed by stable line. Payloads leave through the existing event channel; the embedder reports decode results back.

**Tech Stack:** Rust (crate edition as today), `imagesize` (header dimensions only, pure Rust), `base64`, `cargo-fuzz` (nightly) for fuzz targets.

**Spec:** `../terminal/docs/superpowers/specs/2026-11-16-inline-images-design.md` (the binding design; this repo implements its emulation half). The fbterm half is `../terminal/docs/superpowers/plans/2026-11-16-inline-images.md`.

## Global Constraints

- Fork rules (README): every file changed or added carries the notice line `// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.` (new files: `// Added by the featherbit authors for feathercritty.`) and each change is listed in the README's change list so a future upstream merge knows what is ours; the vte copy keeps vte's licence text (`src/vte/LICENSE-APACHE`, `src/vte/LICENSE-MIT`) and a header naming vte 0.15.0 as its origin.
- Never open issues or PRs on the upstream emulation project or on vte; this is fork-only work.
- Conventional Commits; no attribution of any kind; Gitflow is not used here: `main` plus feature branches, PR into `main`, merge on green CI (as the repo does today).
- Remote input never panics, hangs or allocates without bound: no `unwrap`/indexing on sequence data; every buffer has a named cap.
- Limits (spec section 6, exact values): per image at most 10 000 × 10 000 pixels and 64 MiB decoded RGBA; a chunked kitty transmission or an unterminated string sequence is abandoned after 16 MiB of payload or 10 s without progress; non-image OSC sequences keep a 64 KiB cap (longer ones are discarded).
- Display-only: the kitty transmission media `t=f`, `t=t`, `t=s` are refused with `EBADF` (no file is ever opened); nothing here touches disk, clipboard or processes.
- When images are disabled (`Term::set_images_enabled(false)`), DA1 does not advertise sixel, `a=q` answers `ENOTSUPPORTED`, and image sequences are consumed and discarded.
- No terminal application names in code, docs or commits (protocol names are fine).

## Review Focus

1. A sequence split across reads at every byte boundary (the PTY delivers arbitrary chunks): parses identically to the whole sequence. Test in Task 2 (`split_at_every_byte_is_identical`).
2. A kitty transmission that never sends `m=0`, or a sixel with no ST: memory stays at the cap and the next sequence parses normally. Test in Task 3 (`endless_transmission_is_dropped_at_cap`) and Task 4 (`unterminated_sixel_is_bounded`).
3. A clear (`ED 2`, `ED 3`, `RIS`), alternate-screen switch or resize while images are on screen: placements follow or vanish exactly as the spec says, and every dropped image is reported to the embedder once. Test in Task 6 (`clear_resize_and_screen_switch`).
4. A tiny image (1×1) and an image larger than the screen: cell span at least 1×1, clipped at the bottom edge with the excess scrolling as text would. Test in Task 6 (`tiny_and_oversized_spans`).
5. A sixel or kitty sequence while images are disabled: consumed with no output on screen and no reply other than the protocol's "not supported". Test in Task 7 (`disabled_images_are_inert`).

---

### Task 1: Copy vte into the crate, cap string sequences

**Files:** Create `src/vte/{mod.rs,ansi.rs,params.rs,table.rs...}` (exact copies of vte 0.15.0's `src/` files, module paths adjusted), `src/vte/LICENSE-APACHE`, `src/vte/LICENSE-MIT`; Modify `src/lib.rs` (`pub mod vte;` replaces `pub use vte;`), `Cargo.toml` (drop the `vte` dependency, add the deps vte itself needs, keep the `serde` feature wiring), `README.md` (change list entry).

**Interfaces:** Produces `crate::vte` with the same public API as vte 0.15.0 (`vte::ansi::{Processor, Handler, ...}`, `vte::Parser`, `vte::Perform`) plus the caps below.

- Caps: `pub const MAX_OSC_BYTES: usize = 64 * 1024;` (OSC strings other than 1337 longer than this are discarded, not dispatched); OSC parameters beyond `MAX_OSC_PARAMS` (16) are kept joined in the last parameter instead of dropped (OSC 1337 arguments contain `;`).
- [ ] **Step 1: Copy** the files verbatim from `~/.cargo/registry/src/*/vte-0.15.0/src/` first, commit nothing yet; build and run the existing test suite (`cargo test`) with the module swap: everything passes unchanged.
- [ ] **Step 2: Failing tests** in `src/vte/` (unit): `oversized_osc_is_discarded` (an OSC 52 of 70 000 bytes dispatches nothing and the next `CSI 1 m` still works), `osc_params_past_sixteen_are_joined`.
- [ ] **Step 3: Implement** the caps.
- [ ] **Step 4: Run** `cargo test` and clippy as CI runs them. Expected: PASS.
- [ ] **Step 5: Commit** `feat(vte): own copy of the parser with string caps`.

### Task 2: Streaming APC, DCS sixel and OSC 1337 callbacks

**Files:** Modify `src/vte/mod.rs` (state machine), `src/vte/ansi.rs` (`Handler` methods, `Processor` dispatch); Test: `src/vte/tests_strings.rs`.

**Interfaces:** Produces new `Handler` methods (default no-ops so existing implementors compile):
- `fn apc_start(&mut self) {}`, `fn apc_put(&mut self, _bytes: &[u8]) {}`, `fn apc_end(&mut self) {}` (also on CAN/SUB abort: `fn apc_abort(&mut self) {}`)
- `fn sixel_start(&mut self, _params: &[u16]) {}`, `fn sixel_put(&mut self, _bytes: &[u8]) {}`, `fn sixel_end(&mut self) {}`, `fn sixel_abort(&mut self) {}` (DCS whose final byte is `q` with no intermediates)
- `fn inline_image_start(&mut self, _args: &[u8]) {}`, `fn inline_image_put(&mut self, _bytes: &[u8]) {}`, `fn inline_image_end(&mut self) {}`, `fn inline_image_abort(&mut self) {}` (OSC whose first parameter is `1337` and second starts with `File=`; `args` is the text between `File=` and `:`, capped at 4 KiB)
- `fn graphics_attributes(&mut self, _item: u16, _action: u16, _value: Option<u16>) {}` for `CSI ? Pi ; Pa ; Pv S` (XTSMGRAPHICS)
- `fn cell_size_pixels(&mut self) {}` for `CSI 16 t`
- Streaming means `put` is called with each slice as bytes arrive; nothing is buffered beyond the current read. SOS and PM stay discarded.
- [ ] **Step 1: Failing tests** with a recording handler: `apc_round_trip`, `sixel_round_trip` (params `0;1;0`), `inline_image_round_trip`, `can_aborts_apc`, `split_at_every_byte_is_identical` (each of the three sequences fed in two slices at every possible split point produces the same recorded calls once the puts are concatenated), `xtsmgraphics_dispatch`, `csi_16t_dispatch`, `sos_pm_still_ignored`.
- [ ] **Step 2-4:** run (FAIL), implement, run the full suite (PASS).
- [ ] **Step 5: Commit** `feat(vte): stream apc, sixel and inline image sequences`.

### Task 3: The kitty graphics protocol state machine

**Files:** Create `src/graphics/{mod.rs,kitty.rs,limits.rs}`; Test: `src/graphics/kitty_tests.rs`.

**Interfaces:**
- `limits.rs`: `pub const MAX_IMAGE_SIDE: u32 = 10_000; pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024; pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024; pub const STALL: Duration = Duration::from_secs(10);`
- `pub struct KittyParser` fed by the APC callbacks: `start()`, `put(&[u8])`, `end(now: Instant) -> Option<KittyCommand>`, `abort()`. It parses the control keys before `;` (all single-letter keys of the protocol: `a t f o m i I p q s v x y w h X Y c r C z U d` with their value types), accumulates base64 payload across chunks keyed by `i`/`I` (chunked transmissions continue across separate APC sequences until `m=0`), decodes base64 incrementally, inflates `o=z` with a streaming decoder bounded by `MAX_IMAGE_BYTES`, and refuses `t=f|t|s` with `EBADF`.
- `pub enum KittyCommand { Transmit { id: Option<u32>, number: Option<u32>, format: KittyFormat /* Png | Rgb{w,h} | Rgba{w,h} */, bytes: Vec<u8>, then_place: Option<KittyPlace>, quiet: u8 }, Place(KittyPlace), Delete(KittyDelete), Query { id: u32, quiet: u8 } }`; `KittyPlace { id, placement_id, cols, rows, src: Option<Rect>, x_off, y_off, z, virtual_: bool /* U=1 */, cursor_movement: bool /* C */ }`; `KittyDelete` covering every `d=` selector (a/A, i/I, n/N, c/C, p/P, q/Q, x/X, y/Y, z/Z, r/R).
- Replies: `pub fn reply(id: u32, placement: Option<u32>, result: Result<(), KittyError>, quiet: u8) -> Option<Vec<u8>>` producing `ESC _ G i=<id>[,p=<pid>] ; OK|<ECODE>:<message> ESC \`, suppressed per `q=1` (OK) / `q=2` (all). `KittyError` variants `EBADF, EINVAL, ENOENT, ENOMEM, EBADPNG, ENOTSUPPORTED`.
- Pixel size for PNG comes from the header (`imagesize`), validated against the limits before anything else.
- [ ] **Step 1: Failing tests:** `parses_all_keys`, `chunked_transmission_across_sequences`, `base64_split_mid_quad`, `zlib_payload_inflates_within_cap`, `zip_bomb_is_enomem`, `file_media_is_refused_ebadf` (for `t=f`, `t=t`, `t=s`, and no file access happens: use a path that would exist), `oversized_header_is_enomem`, `endless_transmission_is_dropped_at_cap` (17 MiB of `m=1` chunks: the transmission is dropped with `ENOMEM` once past 16 MiB, memory held never exceeds the cap, the next APC parses), `stalled_transmission_is_dropped` (`end` with `now` 11 s after the last chunk), `reply_quiet_levels`, `garbage_keys_are_einval`.
- [ ] **Step 2-4:** run, implement, run.
- [ ] **Step 5: Commit** `feat(graphics): kitty graphics protocol parser`.

### Task 4: Sixel decoder

**Files:** Create `src/graphics/sixel.rs`; Test: `src/graphics/sixel_tests.rs`.

**Interfaces:** `pub struct SixelDecoder` from `sixel_start(params)`: `new(params: &[u16], default_bg: Rgba) -> SixelDecoder`, `put(&[u8])`, `finish() -> Result<SixelImage, SixelError>`, where `SixelImage { width: u32, height: u32, rgba: Vec<u8> }` (premultiplied no, straight RGBA8). Covers raster attributes (`"` Pan;Pad;Ph;Pv), colour introducer `#Pc` with `;2;R;G;B` (0-100) and `;1;H;L;S`, at least 1024 private registers per image with the VT340 default palette for 0-15, repeat `!Pn`, `$` and `-`, `P2=1` transparent background (pixels never painted are alpha 0), growth of the canvas as sixels extend it, every size bounded by `MAX_IMAGE_SIDE` and `MAX_IMAGE_BYTES` (further data ignored, image kept as far as it fits).
- [ ] **Step 1: Failing tests:** `decodes_reference_images` (three small sixel files under `tests/fixtures/sixel/` produced by a reference encoder, compared to their PNG pixels decoded in the test with a dev-dependency `png`), `hls_colours`, `repeat_and_newline`, `transparent_background`, `register_limit`, `unterminated_sixel_is_bounded` (an endless `!32767~` stream: canvas capped, `finish` returns what fits), `zero_size_is_error`.
- [ ] **Step 2-4:** run, implement, run.
- [ ] **Step 5: Commit** `feat(graphics): sixel decoder`.

### Task 5: OSC 1337 inline images

**Files:** Create `src/graphics/inline.rs`; Test inside.

**Interfaces:** `pub struct InlineImageParser`: `start(args: &[u8])`, `put(&[u8])`, `end() -> Option<InlineImage>`; `InlineImage { bytes: Vec<u8>, pixel_size: (u32, u32), width: Dim, height: Dim, preserve_aspect: bool, inline: bool }`, `pub enum Dim { Auto, Cells(u32), Pixels(u32), Percent(u32) }`; parses `name` (ignored), `size`, `width`, `height`, `preserveAspectRatio`, `inline`; `inline=0` (download) is discarded (display-only); format and size from the header (`imagesize`: PNG, JPEG, GIF), refused beyond the limits.
- [ ] **Step 1: Failing tests:** `parses_args`, `dims_in_all_units`, `download_is_discarded`, `base64_across_puts`, `oversized_is_dropped`, `unknown_format_is_dropped`.
- [ ] **Step 2-5:** run, implement, run, commit `feat(graphics): osc 1337 inline images`.

### Task 6: Placements in the grid

**Files:** Create `src/graphics/placements.rs`; Modify `src/term/mod.rs` (Handler callbacks wired to the parsers; scroll, erase, insert/delete lines, scroll regions, reverse index, alternate screen, resize, `RIS`, `ED 3` updating placements), `src/event.rs` (new events); Test: `src/graphics/placement_tests.rs`.

**Interfaces:**
- `pub struct ImageKey(pub u64)` (monotonic per `Term`).
- New `Event` variants: `ImageReceived { key: ImageKey, data: ImageData }` with `pub enum ImageData { Rgba { width: u32, height: u32, pixels: Vec<u8> } /* sixel, kitty RGB/RGBA */, Encoded { format: ImageFormat /* Png | Jpeg | Gif */, bytes: Vec<u8>, width: u32, height: u32 } }`, and `ImageReleased(ImageKey)` (exactly once per key, when nothing references it any more).
- `Term::image_decoded(&mut self, key: ImageKey, result: Result<(), ImageDecodeError>)`: sends any pending kitty reply (`OK` or `EBADPNG`/`ENOMEM`) through the existing `PtyWrite` path; a failed decode removes its placements.
- `Term::set_cell_pixel_size(&mut self, width: u32, height: u32)`: physical pixels, used to turn pixel sizes into cell spans at placement time.
- Placement: `pub struct Placement { pub key: ImageKey, pub line: i64 /* stable line = scrolled_lines + grid line */, pub col: usize, pub cols: u32, pub rows: u32, pub src: Option<Rect>, pub x_off: u32, pub y_off: u32, pub z: i32, pub screen: Screen /* Primary | Alternate */ }`. Cell span: from `c`/`r`, `width`/`height` (cells, or pixels / percent converted with the cell size), else image pixels divided by the cell size, rounded up, at least 1×1. Sixel places at the cursor and moves the cursor below the image (DECSDM off) as the standard specifies; kitty honours `C=1` (no cursor movement).
- `Term::visible_placements(&self) -> Vec<VisiblePlacement>` with `VisiblePlacement { key, row_in_view: i32 /* may be negative when partly scrolled off */, col, cols, rows, src, x_off, y_off, z }`, for the current view (scrollback offset included).
- `Term::set_scrollback_images(&mut self, keep: bool)`: when false (default), a placement whose rows all left the screen is turned into a `Placeholder { line, col, pixel_size: (u32, u32) }` (kept, a few bytes) and its image released; `Term::visible_placeholders(&self) -> Vec<VisiblePlaceholder { row_in_view, col, text: String /* "[image 640×480]" */ }>`. When true, placements stay and scroll back into view; `Term::evict_oldest_scrollback_image(&mut self) -> Option<ImageKey>` lets the embedder enforce its memory caps (turns the oldest scrollback placement into a placeholder).
- Kitty Unicode placeholders: virtual placements (`U=1`) create no `Placement`; `Term::kitty_virtual_image(&self, image_id: u32, placement_id: Option<u32>) -> Option<(ImageKey, u32 /*cols*/, u32 /*rows*/)>` lets the renderer resolve `U+10EEEE` cells (image id from the cell's foreground colour, row/column from the combining diacritics, per the protocol's diacritic table, included as a constant table in `src/graphics/kitty_diacritics.rs`), and `pub fn decode_placeholder_cell(cell: &Cell) -> Option<(u32 /*image id*/, u32 /*row*/, u32 /*col*/)>`.
- [ ] **Step 1: Failing tests:** `sixel_places_at_cursor_and_moves_cursor`, `kitty_place_with_cells`, `span_from_pixels_rounds_up`, `tiny_and_oversized_spans`, `scroll_moves_placements`, `clear_resize_and_screen_switch` (ED 2 removes, ED 3 removes scrollback ones, RIS removes all, leaving the alternate screen drops its placements, resize keeps anchors and drops those whose anchor reflows away; each removal yields exactly one `ImageReleased`), `scrolled_off_becomes_placeholder`, `scrollback_kept_when_enabled`, `evict_oldest_scrollback_image`, `virtual_placement_resolves_placeholder_cells`, `kitty_delete_selectors`, `decode_failure_sends_ebadpng`.
- [ ] **Step 2-4:** run, implement, run.
- [ ] **Step 5: Commit** `feat(graphics): image placements follow the grid`.

### Task 7: Detection and the master switch

**Files:** Modify `src/term/mod.rs` (`identify_terminal`, `graphics_attributes`, `cell_size_pixels`, `text_area_size_pixels`), Test inside.

**Interfaces:** `Term::set_images_enabled(&mut self, on: bool)` (default true). DA1 replies `\x1b[?62;4;22c` when on (`62` level, `4` sixel, `22` ANSI colour) and today's `\x1b[?6c` when off. XTSMGRAPHICS: item 1 (colour registers) read → `\x1b[?1;0;1024S`; item 2 (sixel geometry) read → `\x1b[?2;0;<w>;<h>S` with the text area in physical pixels capped at `MAX_IMAGE_SIDE`; set and reset requests answer success without changing anything; any request while off → status 1 (error). `CSI 16 t` replies `\x1b[6;<cell_h>;<cell_w>t` and `CSI 14 t` `\x1b[4;<h>;<w>t`, both from `set_cell_pixel_size` (physical pixels). Kitty `a=q` replies `OK` when on, `ENOTSUPPORTED` when off.
- [ ] **Step 1: Failing tests:** `da1_advertises_sixel_when_on`, `xtsmgraphics_replies`, `pixel_size_reports_physical`, `disabled_images_are_inert` (DA1 without `4`, sixel and kitty sequences produce no placement and no event, `a=q` → `ENOTSUPPORTED`).
- [ ] **Step 2-5:** run, implement, run, commit `feat(graphics): advertise image support and report pixel sizes`.

### Task 8: Fuzzing, docs, release

**Files:** Create `fuzz/Cargo.toml`, `fuzz/fuzz_targets/{kitty.rs,sixel.rs,inline.rs,parser_strings.rs}`, seeds under `fuzz/seeds/<target>/`; Modify `.github/workflows/ci.yml` (a nightly job running each target 60 s, like fbt-lib's fuzz workflow), `README.md` (change list entries for every change in this plan, and a "Graphics" section describing the API for embedders), `Cargo.toml` version `0.26.0+fbterm.8`.
- [ ] **Step 1:** write the targets (each feeds arbitrary bytes through the real entry point: the parser with a recording handler for `parser_strings`, the three protocol parsers directly) and run each locally for 60 s: no crash, no timeout, memory under the caps.
- [ ] **Step 2:** docs and version.
- [ ] **Step 3: Commit** `test: fuzz the image parsers` and `docs: graphics api and change list`; PR into `main`, merge on green CI; the fbterm plan then pins the merge commit by `rev`.
