// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Compact row codec for the scrollback history (feathercritty).
//!
//! Rows that scroll off the screen are stored as byte records instead of dense `Row<Cell>`s
//! (24 bytes per cell). This module owns:
//!
//! - [`HistoryCell`]: what a cell type needs to be stored in the history: the header bits
//!   (`row_wraps`), the type of its shared extra data, and a row encoder and decoder.
//! - [`RowHeader`] and the header layout shared by every record format, so `read_header` answers
//!   `wraps`, `is_clear`, width and extras count without decoding the row.
//! - The record format of `Row<Cell>` (`impl HistoryCell for Cell`), specified below.
//! - Small helpers (`write_varint`, `write_header`, `RecordReader`, `prepare_row`) that the
//!   test-only formats for `usize` and `char` (in `grid/tests.rs` and `grid/storage.rs`) reuse.
//!
//! Nothing in upstream alacritty_terminal corresponds to this file: upstream keeps every
//! history row as a dense `Row<Cell>` in the grid's ring.
//!
//! # Record format
//!
//! A record is self-contained and self-delimiting. Integers are unsigned LEB128 varints (one
//! byte below 128). Fixed-size fields are little-endian. There is no padding, no alignment and
//! no pointer: extras are referred to by position (see "Extras").
//!
//! ```text
//! header  W       varint  width: cells in the row
//!         P       varint  cells stored explicitly (0..P); every cell in P..W equals the tail
//!         R       varint  number of runs
//!         M       varint  number of marks
//!         E       varint  number of extras the row refers to
//!         bits    1 byte  0x01 WRAPS: cell W-1 has WRAPLINE
//!                         0x02 CLEAR: every cell is `GridCell::is_empty`
//! text    P chars UTF-8   `c` of cells 0..P, one char per cell (spacers and '\t' included)
//! runs    R x     varint  cells in the run (the runs add up to P)
//!                 4 bytes fg (color, below)
//!                 4 bytes bg
//!                 2 bytes flags (u16 LE) without the structural bits
//!                 varint  extra: 0 for none, k for the row's k-th extra (1-based)
//! marks   M x     varint  column delta: column of this mark minus that of the previous one
//!                         (the first mark: its column)
//!                 1 byte  structural bits: 0x01 WIDE_CHAR, 0x02 WIDE_CHAR_SPACER,
//!                         0x04 LEADING_WIDE_CHAR_SPACER, 0x08 WRAPLINE
//! tail            UTF-8   `c` of cell W-1
//!                 4 bytes fg
//!                 4 bytes bg
//!                 2 bytes flags (u16 LE), structural bits included
//!                 varint  extra, as in runs
//! ```
//!
//! A row of width 0 (never stored by the grid, which is at least one column wide) is the header
//! alone, with no tail.
//!
//! **Colors** are 4 bytes, a tag and three payload bytes: `00 lo hi 00` for `Named` (the code of
//! `named_code`, u16 LE), `01 i 00 00` for `Indexed(i)`, `02 r g b` for `Spec`. The named codes
//! are vte 0.15's discriminants, written out in an exhaustive `match`, so they stay stable even
//! if vte renumbers its enum.
//!
//! **Structural flags** (`STRUCTURAL`: the wide-character flags and `WRAPLINE`) are kept out of
//! the runs, so a line of wide characters, whose cells alternate `WIDE_CHAR` and
//! `WIDE_CHAR_SPACER`, is one run plus a mark per cell instead of one run per cell. Every flag is
//! stored as it is (nothing is derived from Unicode widths), so decoding is exact even in the
//! states partial overwrites leave behind.
//!
//! **Runs** break where `fg`, `bg`, the non-structural flags, or the extra (by `Arc::ptr_eq`)
//! change. Run lengths are varints, so there is no length limit.
//!
//! **Extras.** A `Cell`'s `Option<Arc<CellExtra>>` is not in the record. The encoder pushes the
//! `Arc`s the row refers to onto the caller's queue, in order, once per run (a run whose extra is
//! the same `Arc` as the one pushed just before reuses that entry, so a hyperlink across a color
//! change is one entry); the record refers to them by 1-based position within the row's own
//! entries, and the header's `E` says how many there are. The decoder receives exactly that
//! row's slice of the queue and clones the `Arc`s, so decoded cells share them with the
//! originals.
//!
//! # Invariants
//!
//! - `P` is one past the last cell before `W-1` that differs from cell `W-1` (full `Cell`
//!   equality), so trailing blanks and background-colored erased tails cost one tail record. The
//!   encoder scans backwards from the row's `occ` only: it relies on the `Row` invariant "all cells
//!   from `occ` on are equal" (kept by every `Row` mutator, see `Row::grow`). Debug builds check
//!   after every encode that cells `P..W` equal the tail.
//! - A decoded row is exactly the encoded one, cell for cell, and its `occ` is `W` (conservative: a
//!   reset of a decoded row clears every cell).
//! - `decode` reads exactly the record's bytes and ignores what follows, so records can sit back to
//!   back in a block.
//! - Records hold no pointer and no process-specific value, so a block of records plus the extras
//!   it refers to can be written out and read back as is (disk spill, design 5.10). Keep it that
//!   way.
//! - Records are produced by this module only. Malformed bytes make the decoder panic (out of
//!   bounds, invalid UTF-8, unknown color); it never reads outside the slice it is given.
//!
//! # Speed (phase 4 tuning)
//!
//! A row is encoded every time it scrolls off the screen, so the encoder is on the ingest path;
//! the decoder is on every history read and reflow. What keeps them fast, and must stay true:
//!
//! - The scan for `P` starts at `occ` (see Invariants). `Term::clear_line` lowers `occ` after an
//!   erase to the end of the line (`Row::set_equal_from`), so a background-colored blank line costs
//!   no scan.
//! - Cells are compared field by field with non-short-circuiting `&` (`cell_eq`, `RunKey`), the
//!   extra by `Arc` identity; a run's style is read once per run.
//! - Text that is all ASCII is written and read as one byte per cell in a single pass
//!   (`write_text`; the decoder writes each run's cells whole), with a per-character fallback.
//! - When `P + 1 < 128`, `R`, `M` and `E` each take one byte, so the header is written first with
//!   placeholders and patched at the end; the body is never moved.
//! - Small fixed-size pieces (header, a run's style) are copied as whole arrays and cut back
//!   (`put_fixed`), which the compiler inlines instead of calling `memcpy`.
//! - The marks pass runs only when a stored cell has a structural flag.
//! - Readers that need only the text of a plain row (`Term::line_to_string`) read it straight from
//!   the record ([`plain_text`]), without decoding cells.
//!
//! `codec_timing` (an ignored test) prints the cost of typical rows for tuning.
//!
//! # When merging upstream
//!
//! Re-check this file whenever upstream changes `Cell`, `Flags`, `CellExtra` or vte's colors:
//!
//! - **`Cell` gains a field**: store it in the runs (if it varies per character run) or the text,
//!   in `encode`, `decode_cell_row`, `same_run` and the tail, and add it to the tests'
//!   `random_cell`. `same_run` compares every field but `c`; a field missing there is silently
//!   dropped from history.
//! - **`Flags` gains a bit**: `flags_codec_knows_every_flag` fails. Add it to `STYLE` (kept in the
//!   runs) or, if it describes a cell's place in a wide character or a line (like `WRAPLINE`), to
//!   `STRUCTURAL` with a new bit in `MARKS`. `Flags` must stay 16 bits wide, or the runs' flags
//!   field grows.
//! - **vte adds a `NamedColor` variant**: `named_code` stops compiling. Give the variant the next
//!   free code (never reuse or renumber one), add it to `named_from_code` and to the tests'
//!   `ALL_NAMED`. A new `Color` variant makes `pack_color` stop compiling: add a tag.
//! - **`CellExtra` changes** (upstream `main` already stores zero-width characters in an
//!   `ArrayVec`): nothing here changes while `Cell::extra` is an `Option<Arc<CellExtra>>`; if it
//!   stops being an `Arc`, revisit `HistoryCell::Extra` and the extras section above.
//! - **`GridCell::is_empty` for `Cell` changes**: the `CLEAR` bit follows it (it is computed with
//!   `is_empty`); stored records keep the old answer until they are evicted.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use crate::grid::{GridCell, Row};
use crate::term::cell::{Cell, CellExtra, Flags};
use crate::vte::ansi::{Color, NamedColor, Rgb};

/// A cell type the history can store: header bits, a row encoder and a row decoder.
///
/// `Cell` implements the real format (this module); the grid's unit tests implement trivial
/// formats for `usize` and `char` that share the header layout (see `write_header`).
pub trait HistoryCell: GridCell + Clone + Default + PartialEq {
    /// Shared data cells point to, kept outside the byte record (for `Cell`, its
    /// `Arc<CellExtra>`). The history keeps the extras of all its rows in one queue; it clones
    /// one only when it hands a row's extras to `decode` as a copy (see `History`).
    type Extra: Clone + fmt::Debug;

    /// Whether `row` continues on the next line: its last cell has `Flags::WRAPLINE`.
    ///
    /// Rows are always as wide as the grid, so this is upstream's
    /// `grid[line][last_column].flags.contains(Flags::WRAPLINE)`.
    #[inline]
    fn row_wraps(row: &Row<Self>) -> bool {
        row.last().is_some_and(|cell| cell.flags().contains(Flags::WRAPLINE))
    }

    /// Append `row`'s record to `out` and the extras it refers to to `extras` (exactly
    /// `header.extras` of them, at the back), and return its header.
    fn encode(row: &Row<Self>, out: &mut Vec<u8>, extras: &mut VecDeque<Self::Extra>) -> RowHeader;

    /// An upper bound on the bytes `encode` appends to `out` for a row of `width` cells,
    /// counting any bytes it writes past the record's end and cuts off again. `History` encodes
    /// straight into a block when the block has that much room left; `None` (the default) means
    /// no bound is known, and every record goes through a scratch buffer first.
    fn max_record_bytes(_width: usize) -> Option<usize> {
        None
    }

    /// What a column reflow needs to know to copy the record at the start of `bytes` with only
    /// its width changed instead of decoding and re-encoding the row (the byte-copy fast path of
    /// design spec 5.6, `RowSource::next_reflow` in `resize.rs`). `None` (the default) when the
    /// format has no such fast path: every row is decoded.
    fn reflow_info(_bytes: &[u8]) -> Option<ReflowInfo> {
        None
    }

    /// Decode the record at the start of `bytes` (bytes after it are ignored) into `into`,
    /// which is resized only when its width differs, and return the record's length in bytes.
    /// `extras` is the row's own slice of the queue `encode` pushed to. The decoded row's `occ`
    /// is its width.
    fn decode(bytes: &[u8], extras: &[Self::Extra], into: &mut Row<Self>) -> usize;

    /// The cell at `column` of the record at the start of `bytes`, without decoding the rest of
    /// the row (`Grid::cell`). Panics when `column` is not inside the row, as indexing a `Row`
    /// does.
    ///
    /// The default decodes the whole row into a new one; `Cell` reads only what it needs.
    fn decode_cell(bytes: &[u8], extras: &[Self::Extra], column: usize) -> Self {
        let mut row = Row::default();
        Self::decode(bytes, extras, &mut row);
        row[..][column].clone()
    }
}

/// What a record's header says about its row, readable without decoding the row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowHeader {
    /// Cells in the row (`W`).
    pub(crate) width: u32,
    /// The last cell has `WRAPLINE` (`Grid::wraps`).
    pub(crate) wraps: bool,
    /// Every cell is `GridCell::is_empty` (`Grid::is_clear`).
    pub(crate) clear: bool,
    /// Extras the row pushed and refers to (`E`).
    pub(crate) extras: u32,
    /// Cells stored explicitly (`P`); the cells from `len` on all equal the last one.
    pub(crate) len: u32,
}

/// What `HistoryCell::reflow_info` reports about a record: enough to decide whether a reflow
/// leaves the row's cells untouched, so that the record can be copied with only its width
/// changed (see `RowSource::next_reflow` in `resize.rs` for the exact conditions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflowInfo {
    /// Cells in the row (`W`).
    pub(crate) width: usize,
    /// Cells stored explicitly (`P`); every cell from `P` on equals the tail.
    pub(crate) stored: usize,
    /// The tail is the default cell, so growing the row (which appends default cells) adds cells
    /// equal to the tail: the grown row's record is this one with `W` changed.
    pub(crate) tail_default: bool,
    /// The tail is empty (`GridCell::is_empty`), so cutting cells off the tail drops nothing a
    /// reflow would carry to the next line. Conservatively `false` when the tail has an extra.
    pub(crate) tail_empty: bool,
}

/// Append the record `bytes` (exactly one record) to `out` with its width (`W`, the header's
/// first varint, shared by every record format) changed to `width`; everything else is copied as
/// is.
pub(crate) fn copy_with_width(bytes: &[u8], width: usize, out: &mut Vec<u8>) {
    let mut reader = RecordReader::new(bytes);
    reader.varint();
    write_varint(out, width);
    out.extend_from_slice(&bytes[reader.position()..]);
}

/// A record's header fields, including the two only the `Cell` format uses.
pub(crate) struct Layout {
    pub(crate) header: RowHeader,
    pub(crate) runs: usize,
    pub(crate) marks: usize,
}

const HEADER_WRAPS: u8 = 0b01;
const HEADER_CLEAR: u8 = 0b10;

/// Flags that place a cell in a wide character or a line. Stored as marks, not in runs.
const STRUCTURAL: Flags = Flags::WIDE_CHAR
    .union(Flags::WIDE_CHAR_SPACER)
    .union(Flags::LEADING_WIDE_CHAR_SPACER)
    .union(Flags::WRAPLINE);

/// Flags that style a cell. Stored in runs (as every flag outside `STRUCTURAL`); listed so that
/// `flags_codec_knows_every_flag` notices a new `Flags` bit.
#[cfg_attr(not(test), expect(dead_code, reason = "only the flag classification test reads it"))]
const STYLE: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::UNDERLINE)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::DOUBLE_UNDERLINE)
    .union(Flags::UNDERCURL)
    .union(Flags::DOTTED_UNDERLINE)
    .union(Flags::DASHED_UNDERLINE);

/// The structural flags and their bit in a mark. Fixed by the record format.
const MARKS: [(Flags, u8); 4] = [
    (Flags::WIDE_CHAR, 0x01),
    (Flags::WIDE_CHAR_SPACER, 0x02),
    (Flags::LEADING_WIDE_CHAR_SPACER, 0x04),
    (Flags::WRAPLINE, 0x08),
];

const COLOR_NAMED: u8 = 0;
const COLOR_INDEXED: u8 = 1;
const COLOR_SPEC: u8 = 2;

/// Append `value` as an unsigned LEB128 varint.
#[inline]
pub(crate) fn write_varint(out: &mut Vec<u8>, value: usize) {
    let mut value = value as u64;
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Append the header: `W`, `P` (`header.len`), `R`, `M`, `E` as varints, then the bits byte.
pub(crate) fn write_header(out: &mut Vec<u8>, header: &RowHeader, runs: usize, marks: usize) {
    let mut buf = [0; MAX_HEADER_BYTES];
    let len = put_header(&mut buf, header, runs, marks);
    put_fixed(out, &buf, len);
}

/// Most bytes a header takes: five varints of `u32` values (at most 5 bytes each) and the bits
/// byte.
const MAX_HEADER_BYTES: usize = 5 * 5 + 1;

/// The header's bits byte.
#[inline]
fn header_bits(header: &RowHeader) -> u8 {
    let mut bits = 0;
    if header.wraps {
        bits |= HEADER_WRAPS;
    }
    if header.clear {
        bits |= HEADER_CLEAR;
    }
    bits
}

/// Append the first `len` bytes of `buf`. The whole array is copied (a fixed-size copy the
/// compiler inlines, where a copy of a run-time length would call `memcpy`) and the rest is cut
/// off again.
#[inline(always)]
fn put_fixed<const N: usize>(out: &mut Vec<u8>, buf: &[u8; N], len: usize) {
    debug_assert!(len <= N);
    let end = out.len() + len;
    out.extend_from_slice(buf);
    out.truncate(end);
}

/// Write the header into `buf` (see `write_header`) and return its length.
#[inline]
fn put_header(
    buf: &mut [u8; MAX_HEADER_BYTES],
    header: &RowHeader,
    runs: usize,
    marks: usize,
) -> usize {
    let mut n = put_varint(buf, 0, header.width as usize);
    n = put_varint(buf, n, header.len as usize);
    n = put_varint(buf, n, runs);
    n = put_varint(buf, n, marks);
    n = put_varint(buf, n, header.extras as usize);
    buf[n] = header_bits(header);
    n + 1
}

/// Read the header of the record at the start of `bytes` without decoding the row.
pub(crate) fn read_header(bytes: &[u8]) -> RowHeader {
    read_layout(bytes).header
}

fn read_layout(bytes: &[u8]) -> Layout {
    RecordReader::new(bytes).layout()
}

/// Reads a record front to back. Records come from this module's encoders; malformed input
/// panics (index out of bounds or invalid UTF-8), it never reads outside `bytes`.
pub(crate) struct RecordReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> RecordReader<'a> {
    #[inline]
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes read so far.
    #[inline]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub(crate) fn byte(&mut self) -> u8 {
        let byte = self.bytes[self.pos];
        self.pos += 1;
        byte
    }

    #[inline]
    fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut array = [0; N];
        array.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        array
    }

    /// An unsigned LEB128 varint.
    #[inline]
    pub(crate) fn varint(&mut self) -> usize {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte();
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return value as usize;
            }
            shift += 7;
        }
    }

    /// One UTF-8 encoded character.
    #[inline]
    pub(crate) fn char(&mut self) -> char {
        let first = self.bytes[self.pos];
        if first < 0x80 {
            self.pos += 1;
            return char::from(first);
        }

        let len = match first {
            0xf0.. => 4,
            0xe0.. => 3,
            _ => 2,
        };
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + len])
            .expect("history record: invalid UTF-8");
        self.pos += len;
        text.chars().next().expect("history record: empty character")
    }

    /// Step over one UTF-8 encoded character.
    #[inline]
    fn skip_char(&mut self) {
        self.pos += match self.bytes[self.pos] {
            0xf0.. => 4,
            0xe0.. => 3,
            0xc0.. => 2,
            _ => 1,
        };
    }

    /// Step over a color, flags and extra reference (see `style`).
    #[inline]
    fn skip_style(&mut self) {
        self.pos += 10;
        self.varint();
    }

    /// The header (`W`, `P`, `R`, `M`, `E`, bits).
    pub(crate) fn layout(&mut self) -> Layout {
        let width = self.varint() as u32;
        let len = self.varint() as u32;
        let runs = self.varint();
        let marks = self.varint();
        let extras = self.varint() as u32;
        let bits = self.byte();
        let header = RowHeader {
            width,
            wraps: bits & HEADER_WRAPS != 0,
            clear: bits & HEADER_CLEAR != 0,
            extras,
            len,
        };
        Layout { header, runs, marks }
    }

    /// A color, flags and extra reference, as stored in a run or the tail.
    #[inline]
    fn style(&mut self, extras: &[Arc<CellExtra>]) -> Style {
        let fg = unpack_color(self.array());
        let bg = unpack_color(self.array());
        let flags = Flags::from_bits_retain(u16::from_le_bytes(self.array()));
        let extra = match self.varint() {
            0 => None,
            k => Some(extras[k - 1].clone()),
        };
        Style { fg, bg, flags, extra }
    }
}

/// The attributes a run or the tail sets on its cells.
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
    extra: Option<Arc<CellExtra>>,
}

impl Style {
    /// A cell with character `c` and this style.
    #[inline(always)]
    fn cell(&self, c: char) -> Cell {
        Cell { c, fg: self.fg, bg: self.bg, flags: self.flags, extra: self.extra.clone() }
    }

    #[inline]
    fn apply(&self, cell: &mut Cell) {
        cell.fg = self.fg;
        cell.bg = self.bg;
        cell.flags = self.flags;
        cell.extra.clone_from(&self.extra);
    }
}

/// Make `into` `width` cells wide (a new row only when the width differs) and return its
/// cells for the decoder to overwrite. The row's `occ` becomes `width`.
pub(crate) fn prepare_row<T: Default>(into: &mut Row<T>, width: usize) -> &mut [T] {
    if into.len() != width {
        into.set_width_for_overwrite(width);
    }
    &mut into[..]
}

/// Pack a color into its 4-byte form (see the module docs).
#[inline]
fn pack_color(color: Color) -> [u8; 4] {
    match color {
        Color::Named(named) => {
            let [lo, hi] = named_code(named).to_le_bytes();
            [COLOR_NAMED, lo, hi, 0]
        },
        Color::Indexed(index) => [COLOR_INDEXED, index, 0, 0],
        Color::Spec(Rgb { r, g, b }) => [COLOR_SPEC, r, g, b],
    }
}

/// Unpack a color packed by [`pack_color`].
#[inline]
fn unpack_color([tag, a, b, c]: [u8; 4]) -> Color {
    match tag {
        COLOR_NAMED => {
            let code = u16::from_le_bytes([a, b]);
            Color::Named(named_from_code(code).expect("history record: unknown named color"))
        },
        COLOR_INDEXED => Color::Indexed(a),
        COLOR_SPEC => Color::Spec(Rgb { r: a, g: b, b: c }),
        _ => panic!("history record: unknown color tag {tag}"),
    }
}

/// The stored code of a named color: vte 0.15's discriminant, written out so that a new
/// variant fails to compile here and existing codes never change (they are part of the format).
#[inline]
fn named_code(named: NamedColor) -> u16 {
    match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Foreground => 256,
        NamedColor::Background => 257,
        NamedColor::Cursor => 258,
        NamedColor::DimBlack => 259,
        NamedColor::DimRed => 260,
        NamedColor::DimGreen => 261,
        NamedColor::DimYellow => 262,
        NamedColor::DimBlue => 263,
        NamedColor::DimMagenta => 264,
        NamedColor::DimCyan => 265,
        NamedColor::DimWhite => 266,
        NamedColor::BrightForeground => 267,
        NamedColor::DimForeground => 268,
    }
}

/// The named color stored as `code` (the inverse of [`named_code`]).
#[inline]
fn named_from_code(code: u16) -> Option<NamedColor> {
    Some(match code {
        0 => NamedColor::Black,
        1 => NamedColor::Red,
        2 => NamedColor::Green,
        3 => NamedColor::Yellow,
        4 => NamedColor::Blue,
        5 => NamedColor::Magenta,
        6 => NamedColor::Cyan,
        7 => NamedColor::White,
        8 => NamedColor::BrightBlack,
        9 => NamedColor::BrightRed,
        10 => NamedColor::BrightGreen,
        11 => NamedColor::BrightYellow,
        12 => NamedColor::BrightBlue,
        13 => NamedColor::BrightMagenta,
        14 => NamedColor::BrightCyan,
        15 => NamedColor::BrightWhite,
        256 => NamedColor::Foreground,
        257 => NamedColor::Background,
        258 => NamedColor::Cursor,
        259 => NamedColor::DimBlack,
        260 => NamedColor::DimRed,
        261 => NamedColor::DimGreen,
        262 => NamedColor::DimYellow,
        263 => NamedColor::DimBlue,
        264 => NamedColor::DimMagenta,
        265 => NamedColor::DimCyan,
        266 => NamedColor::DimWhite,
        267 => NamedColor::BrightForeground,
        268 => NamedColor::DimForeground,
        _ => return None,
    })
}

/// The mark byte for `flags`' structural bits (0 when there are none).
#[inline]
fn mark_bits(flags: Flags) -> u8 {
    if !flags.intersects(STRUCTURAL) {
        return 0;
    }
    MARKS.iter().filter(|(flag, _)| flags.contains(*flag)).fold(0, |bits, (_, bit)| bits | bit)
}

/// The structural flags of a mark byte.
#[inline]
fn mark_flags(bits: u8) -> Flags {
    MARKS
        .iter()
        .filter(|(_, bit)| bits & bit != 0)
        .fold(Flags::empty(), |flags, (flag, _)| flags | *flag)
}

/// The extra of a cell by identity, for comparing runs without touching the reference count.
#[inline(always)]
fn extra_ptr(extra: &Option<Arc<CellExtra>>) -> *const CellExtra {
    extra.as_ref().map_or(std::ptr::null(), Arc::as_ptr)
}

/// What a run shares, read once per run so that the per-cell check compares plain values:
/// colors, the non-structural flags and the extra by identity (see `RunKey::matches`).
#[derive(Clone, Copy)]
struct RunKey {
    fg: Color,
    bg: Color,
    flags: Flags,
    extra: *const CellExtra,
}

impl RunKey {
    #[inline(always)]
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags.difference(STRUCTURAL),
            extra: extra_ptr(&cell.extra),
        }
    }

    /// Whether `cell` belongs to this run: everything but the character and the structural
    /// flags is equal, the extra by identity (`Arc::ptr_eq`). The comparisons do not
    /// short-circuit, so the loop over a run has no data-dependent branch but its end.
    #[inline(always)]
    fn matches(&self, cell: &Cell) -> bool {
        (cell.fg == self.fg)
            & (cell.bg == self.bg)
            & (cell.flags.difference(STRUCTURAL) == self.flags)
            & std::ptr::eq(extra_ptr(&cell.extra), self.extra)
    }
}

/// Full `Cell` equality, cheapest fields first and without short-circuiting them; the extras
/// are compared by identity first and by value only when they are different `Arc`s (as
/// `Arc`'s own `PartialEq` does).
#[inline(always)]
fn cell_eq(a: &Cell, b: &Cell) -> bool {
    ((a.c == b.c) & (a.flags == b.flags) & (a.fg == b.fg) & (a.bg == b.bg))
        && (std::ptr::eq(extra_ptr(&a.extra), extra_ptr(&b.extra)) || a.extra == b.extra)
}

#[inline]
fn write_char(out: &mut Vec<u8>, c: char) {
    if c.is_ascii() {
        out.push(c as u8);
    } else {
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

/// Append the UTF-8 text of `cells`, one character per cell. Text that is all ASCII (the common
/// case) is one byte per cell, written in a single pass; otherwise character by character.
#[inline]
fn write_text(out: &mut Vec<u8>, cells: &[Cell]) {
    let start = out.len();
    let mut high = 0u32;
    out.extend(cells.iter().map(|cell| {
        high |= cell.c as u32;
        cell.c as u8
    }));
    if high >= 0x80 {
        out.truncate(start);
        for cell in cells {
            write_char(out, cell.c);
        }
    }
}

/// Append colors, `flags` and the extra reference of `cell` (prefixed by the run length
/// `len`, when there is one) in one copy, pushing its extra onto `extras` unless it is the entry
/// this row pushed last.
#[inline]
fn write_style(
    out: &mut Vec<u8>,
    len: Option<usize>,
    cell: &Cell,
    flags: Flags,
    extras: &mut VecDeque<Arc<CellExtra>>,
    first_extra: usize,
) {
    // A varint of a `usize` takes at most 10 bytes: length, 10 bytes of style, extra reference.
    let mut buf = [0u8; 30];
    let mut n = 0;
    if let Some(len) = len {
        n = put_varint(&mut buf, n, len);
    }
    buf[n..n + 4].copy_from_slice(&pack_color(cell.fg));
    buf[n + 4..n + 8].copy_from_slice(&pack_color(cell.bg));
    buf[n + 8..n + 10].copy_from_slice(&flags.bits().to_le_bytes());
    n += 10;
    let reference = match &cell.extra {
        None => 0,
        Some(extra) => {
            let shared = extras.len() > first_extra
                && extras.back().is_some_and(|last| Arc::ptr_eq(last, extra));
            if !shared {
                extras.push_back(extra.clone());
            }
            extras.len() - first_extra
        },
    };
    n = put_varint(&mut buf, n, reference);
    put_fixed(out, &buf, n);
}

/// Write `value` as a varint into `buf` at `n`, returning the position after it.
#[inline(always)]
fn put_varint(buf: &mut [u8], mut n: usize, value: usize) -> usize {
    let mut value = value as u64;
    while value >= 0x80 {
        buf[n] = (value as u8 & 0x7f) | 0x80;
        value >>= 7;
        n += 1;
    }
    buf[n] = value as u8;
    n + 1
}

/// Decode a `Cell` record into `into` and return its length in bytes.
fn decode_cell_row(bytes: &[u8], extras: &[Arc<CellExtra>], into: &mut Row<Cell>) -> usize {
    let mut reader = RecordReader::new(bytes);
    let Layout { header, runs, marks } = reader.layout();
    debug_assert_eq!(extras.len(), header.extras as usize, "extras slice does not match row");
    let width = header.width as usize;
    let stored = header.len as usize;
    let cells = prepare_row(into, width);

    // Text that is all ASCII is exactly `stored` bytes, one per cell, so each run writes its
    // cells whole in one pass. Otherwise the characters are decoded first and the runs keep
    // them.
    let text_start = reader.position();
    let ascii = bytes.get(text_start..text_start + stored).filter(|text| text.is_ascii());
    if ascii.is_some() {
        reader.pos += stored;
    } else {
        for cell in &mut cells[..stored] {
            cell.c = reader.char();
        }
    }

    let mut col = 0;
    for _ in 0..runs {
        let len = reader.varint();
        let style = reader.style(extras);
        let run = &mut cells[col..col + len];
        match ascii {
            Some(text) => {
                for (cell, &byte) in run.iter_mut().zip(&text[col..col + len]) {
                    *cell = style.cell(char::from(byte));
                }
            },
            None => run.iter_mut().for_each(|cell| *cell = style.cell(cell.c)),
        }
        col += len;
    }
    debug_assert_eq!(col, stored, "runs do not cover the stored cells");

    let mut col = 0;
    for _ in 0..marks {
        col += reader.varint();
        let bits = reader.byte();
        cells[col].flags.insert(mark_flags(bits));
    }

    if width > 0 {
        let c = reader.char();
        let tail = reader.style(extras).cell(c);
        cells[stored..].fill(tail);
    }

    reader.position()
}

/// A history row that is plain text, read straight from its `Cell` record without decoding
/// cells (the text fast path of design spec 5.6/8, used by `Term::line_to_string` for selection
/// and export). See [`plain_text`] for when a row qualifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlainText<'a> {
    /// The characters of the stored cells (`0..P`), one ASCII byte per cell.
    pub(crate) text: &'a [u8],
    /// The character of every cell from `P` to the end of the row (the tail), ASCII.
    pub(crate) tail: u8,
    /// Cells in the row.
    pub(crate) width: usize,
    /// The tail has `WRAPLINE`, so every cell from `P` on has it (the row wraps).
    pub(crate) wraps: bool,
}

/// The row of the `Cell` record at the start of `bytes` as [`PlainText`], when it is: no marks
/// (no wide-character flags and no `WRAPLINE` before the tail), no extras (no zero-width
/// characters, hyperlinks or underline colors), ASCII text without tabs, and a tail that is ASCII,
/// not a tab and has no wide-character flag. Its colors and style flags are not read. `None`
/// otherwise; callers then decode the row.
pub(crate) fn plain_text(bytes: &[u8]) -> Option<PlainText<'_>> {
    let mut reader = RecordReader::new(bytes);
    let Layout { header, runs, marks } = reader.layout();
    if marks != 0 || header.extras != 0 || header.width == 0 {
        return None;
    }

    let stored = header.len as usize;
    let text = bytes.get(reader.pos..reader.pos + stored)?;
    if !text.is_ascii() || text.contains(&b'\t') {
        return None;
    }
    reader.pos += stored;
    for _ in 0..runs {
        reader.varint();
        reader.skip_style();
    }

    let tail = reader.byte();
    if tail >= 0x80 || tail == b'\t' {
        return None;
    }
    reader.pos += 8;
    let flags = Flags::from_bits_retain(u16::from_le_bytes(reader.array()));
    if flags.intersects(STRUCTURAL.difference(Flags::WRAPLINE)) {
        return None;
    }

    let width = header.width as usize;
    Some(PlainText { text, tail, width, wraps: flags.contains(Flags::WRAPLINE) })
}

/// `HistoryCell::reflow_info` of a `Cell` record: the header, then the tail, found by skipping
/// the text, runs and marks.
fn cell_reflow_info(bytes: &[u8]) -> ReflowInfo {
    let mut reader = RecordReader::new(bytes);
    let Layout { header, runs, marks } = reader.layout();
    let width = header.width as usize;
    let stored = header.len as usize;
    if width == 0 {
        return ReflowInfo { width, stored, tail_default: false, tail_empty: false };
    }

    match bytes.get(reader.pos..reader.pos + stored) {
        Some(text) if text.is_ascii() => reader.pos += stored,
        _ => (0..stored).for_each(|_| reader.skip_char()),
    }
    for _ in 0..runs {
        reader.varint();
        reader.skip_style();
    }
    for _ in 0..marks {
        reader.varint();
        reader.byte();
    }

    let c = reader.char();
    let fg = unpack_color(reader.array());
    let bg = unpack_color(reader.array());
    let flags = Flags::from_bits_retain(u16::from_le_bytes(reader.array()));
    let tail = Cell { c, fg, bg, flags, extra: None };
    let no_extra = reader.varint() == 0;
    ReflowInfo {
        width,
        stored,
        tail_default: no_extra && tail == Cell::default(),
        tail_empty: no_extra && tail.is_empty(),
    }
}

/// The cell at `column` of a `Cell` record, reading only the parts that describe it: the text
/// up to it, then its run, mark and the tail are found by skipping the others.
fn decode_one_cell(bytes: &[u8], extras: &[Arc<CellExtra>], column: usize) -> Cell {
    let mut reader = RecordReader::new(bytes);
    let Layout { header, runs, marks } = reader.layout();
    debug_assert_eq!(extras.len(), header.extras as usize, "extras slice does not match row");
    let width = header.width as usize;
    let stored = header.len as usize;
    assert!(column < width, "column {column} outside the row (width {width})");

    let mut cell = Cell::default();
    for col in 0..stored {
        if col == column {
            cell.c = reader.char();
        } else {
            reader.skip_char();
        }
    }

    let mut col = 0;
    for _ in 0..runs {
        let len = reader.varint();
        if (col..col + len).contains(&column) {
            reader.style(extras).apply(&mut cell);
        } else {
            reader.skip_style();
        }
        col += len;
    }

    let mut col = 0;
    for _ in 0..marks {
        col += reader.varint();
        let bits = reader.byte();
        if col == column {
            cell.flags.insert(mark_flags(bits));
        }
    }

    if column >= stored {
        cell.c = reader.char();
        reader.style(extras).apply(&mut cell);
    }

    cell
}

impl HistoryCell for Cell {
    type Extra = Arc<CellExtra>;

    fn encode(row: &Row<Self>, out: &mut Vec<u8>, extras: &mut VecDeque<Self::Extra>) -> RowHeader {
        let cells = &row[..];
        let width = cells.len();
        let start = out.len();
        let first_extra = extras.len();

        let Some(tail) = cells.last() else {
            let header = RowHeader { clear: true, ..RowHeader::default() };
            write_header(out, &header, 0, 0);
            return header;
        };
        let width32 = u32::try_from(width).expect("row wider than u32::MAX cells");

        // `P`: cells from `occ` on all equal the tail (the `Row` invariant), so the scan for the
        // last differing cell starts there.
        let scan_end = row.occ.min(width - 1);
        let stored =
            cells[..scan_end].iter().rposition(|cell| !cell_eq(cell, tail)).map_or(0, |i| i + 1);
        debug_assert!(
            cells[stored..].iter().all(|cell| cell == tail),
            "Row occ invariant broken: cells from occ on are not all equal"
        );
        let explicit = &cells[..stored];

        // The header's counts `R`, `M` and `E` are known only at the end. Each is at most
        // `P + 1` (a run, a mark or an extra per stored cell, plus the tail's extra), so when
        // that is below 128 each is one byte: the header goes first with placeholders that are
        // patched at the end, and the body is written once, in place. Longer rows write the body
        // first and move it up behind the header.
        let counts_at = (stored < 0x7f).then(|| {
            let mut head = [0; MAX_HEADER_BYTES];
            let n = put_varint(&mut head, 0, width);
            let n = put_varint(&mut head, n, stored);
            put_fixed(out, &head, n + 4);
            start + n
        });

        write_text(out, explicit);

        // Runs, noting on the way whether any cell has a structural flag (most rows have none,
        // and then the marks need no pass of their own).
        let mut runs = 0;
        let mut any_flags = Flags::empty();
        let mut col = 0;
        while col < stored {
            let first = &explicit[col];
            let key = RunKey::of(first);
            let mut end = col + 1;
            while end < stored && key.matches(&explicit[end]) {
                any_flags |= explicit[end].flags;
                end += 1;
            }
            any_flags |= first.flags;
            write_style(out, Some(end - col), first, key.flags, extras, first_extra);
            runs += 1;
            col = end;
        }

        let mut marks = 0;
        if any_flags.intersects(STRUCTURAL) {
            let mut previous = 0;
            for (col, cell) in explicit.iter().enumerate() {
                let bits = mark_bits(cell.flags);
                if bits != 0 {
                    write_varint(out, col - previous);
                    out.push(bits);
                    previous = col;
                    marks += 1;
                }
            }
        }

        write_char(out, tail.c);
        write_style(out, None, tail, tail.flags, extras, first_extra);

        let header = RowHeader {
            width: width32,
            wraps: tail.flags.contains(Flags::WRAPLINE),
            clear: tail.is_empty() && explicit.iter().all(GridCell::is_empty),
            extras: (extras.len() - first_extra) as u32,
            len: stored as u32,
        };
        match counts_at {
            Some(at) => {
                debug_assert!(runs < 0x80 && marks < 0x80 && header.extras < 0x80);
                out[at] = runs as u8;
                out[at + 1] = marks as u8;
                out[at + 2] = header.extras as u8;
                out[at + 3] = header_bits(&header);
            },
            None => {
                // Move the body up by the header's length and write the header in front of it.
                let mut head = [0; MAX_HEADER_BYTES];
                let head_len = put_header(&mut head, &header, runs, marks);
                let body_end = out.len();
                out.extend_from_slice(&head[..head_len]);
                out.copy_within(start..body_end, start + head_len);
                out[start..start + head_len].copy_from_slice(&head[..head_len]);
            },
        }

        header
    }

    /// Every count in a record is at most the width (or one more, for `E`), so each varint takes
    /// at most 5 bytes: the header, at most 4 bytes of text per cell, a run (5 + 10 + 5 bytes) and
    /// a mark (5 + 1) per cell, the tail (4 + 10 + 5), and the 30 bytes `put_fixed` may write past
    /// the end before cutting them off.
    fn max_record_bytes(width: usize) -> Option<usize> {
        Some(MAX_HEADER_BYTES + width * (4 + 20 + 6) + 19 + 30)
    }

    fn reflow_info(bytes: &[u8]) -> Option<ReflowInfo> {
        Some(cell_reflow_info(bytes))
    }

    fn decode(bytes: &[u8], extras: &[Self::Extra], into: &mut Row<Self>) -> usize {
        decode_cell_row(bytes, extras, into)
    }

    fn decode_cell(bytes: &[u8], extras: &[Self::Extra], column: usize) -> Self {
        decode_one_cell(bytes, extras, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    use crate::index::Column;
    use crate::term::cell::Hyperlink;
    use crate::vte::ansi::Rgb;

    /// Every `NamedColor` of vte 0.15. `color_pack_every_variant` checks the codec's table has
    /// exactly these.
    const ALL_NAMED: [NamedColor; 29] = [
        NamedColor::Black,
        NamedColor::Red,
        NamedColor::Green,
        NamedColor::Yellow,
        NamedColor::Blue,
        NamedColor::Magenta,
        NamedColor::Cyan,
        NamedColor::White,
        NamedColor::BrightBlack,
        NamedColor::BrightRed,
        NamedColor::BrightGreen,
        NamedColor::BrightYellow,
        NamedColor::BrightBlue,
        NamedColor::BrightMagenta,
        NamedColor::BrightCyan,
        NamedColor::BrightWhite,
        NamedColor::Foreground,
        NamedColor::Background,
        NamedColor::Cursor,
        NamedColor::DimBlack,
        NamedColor::DimRed,
        NamedColor::DimGreen,
        NamedColor::DimYellow,
        NamedColor::DimBlue,
        NamedColor::DimMagenta,
        NamedColor::DimCyan,
        NamedColor::DimWhite,
        NamedColor::BrightForeground,
        NamedColor::DimForeground,
    ];

    /// What one encode produced: the record, its header and the extras it pushed.
    struct Encoded {
        bytes: Vec<u8>,
        header: RowHeader,
        extras: Vec<Arc<CellExtra>>,
    }

    impl Encoded {
        fn layout(&self) -> Layout {
            read_layout(&self.bytes)
        }
    }

    /// Encode `row` behind unrelated bytes and extras (a record starts wherever the caller's
    /// buffers end), check that it decodes back to `row` exactly, and return what was encoded.
    fn roundtrip(row: &Row<Cell>) -> Encoded {
        let mut out = vec![0xAA; 3];
        let mut extras = VecDeque::from([Arc::new(CellExtra::default())]);
        let header = Cell::encode(row, &mut out, &mut extras);

        let bytes = out[3..].to_vec();
        assert_eq!(read_header(&bytes), header, "read_header disagrees with encode");
        assert_eq!(header.width as usize, row.len());
        let row_extras: Vec<_> = extras.iter().skip(1).cloned().collect();
        assert_eq!(row_extras.len(), header.extras as usize, "extras pushed != header.extras");

        // Bytes after the record (the next record in a block) must not matter, and the decoder
        // must consume exactly the record.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0xFF; 8]);
        let mut into = Row::new(7);
        assert_eq!(decode_cell_row(&padded, &row_extras, &mut into), bytes.len());
        assert_rows_equal(row, &into);

        // The trait entry point decodes the same.
        let mut again = Row::default();
        assert_eq!(Cell::decode(&bytes, &row_extras, &mut again), bytes.len());
        assert_rows_equal(row, &again);

        // And so does reading one cell at a time.
        for col in 0..row.len() {
            let cell = Cell::decode_cell(&bytes, &row_extras, col);
            assert_eq!(cell, row[Column(col)], "decode_cell, column {col}");
        }

        Encoded { bytes, header, extras: row_extras }
    }

    fn assert_rows_equal(expected: &Row<Cell>, actual: &Row<Cell>) {
        assert_eq!(actual.len(), expected.len(), "width");
        for col in 0..expected.len() {
            assert_eq!(actual[Column(col)], expected[Column(col)], "column {col}");
        }
        assert_eq!(actual.occ, expected.len(), "decoded occ must be the width");
    }

    fn put_text(row: &mut Row<Cell>, col: usize, text: &str, template: &Cell) -> usize {
        let mut col = col;
        for c in text.chars() {
            row[Column(col)] = Cell { c, ..template.clone() };
            col += 1;
        }
        col
    }

    fn fg(color: Color) -> Cell {
        Cell { fg: color, ..Cell::default() }
    }

    #[test]
    fn roundtrip_blank_row() {
        let encoded = roundtrip(&Row::new(126));
        let layout = encoded.layout();
        assert_eq!(encoded.header, RowHeader {
            width: 126,
            wraps: false,
            clear: true,
            extras: 0,
            len: 0
        });
        assert_eq!((layout.runs, layout.marks), (0, 0));
        assert!(encoded.bytes.len() <= 20, "{} bytes", encoded.bytes.len());
    }

    #[test]
    fn roundtrip_short_ascii_line() {
        let mut row = Row::new(126);
        let end = put_text(&mut row, 0, "user@host:~/src$ ls ", &Cell::default());
        assert_eq!(end, 20);

        let encoded = roundtrip(&row);
        let layout = encoded.layout();
        // The trailing space of the prompt equals the blank tail, so 19 cells are stored.
        assert_eq!(encoded.header.len, 19);
        assert_eq!((layout.runs, layout.marks), (1, 0));
        assert!(encoded.bytes.len() <= 80, "{} bytes", encoded.bytes.len());
    }

    #[test]
    fn roundtrip_log_line_six_runs() {
        let mut row = Row::new(126);
        let bold_red =
            Cell { fg: Color::Named(NamedColor::Red), flags: Flags::BOLD, ..Cell::default() };
        let segments: [(&str, Cell); 6] = [
            ("2026-10-25T09:14:03.512Z ", fg(Color::Named(NamedColor::Blue))),
            ("[worker", fg(Color::Indexed(244))),
            ("-17] ", fg(Color::Spec(Rgb { r: 0x9a, g: 0x7b, b: 0xff }))),
            ("ERROR ", bold_red),
            ("request failed: upstream 10.0.0.12:8443 timed out", Cell::default()),
            (" (retry 3/5)", fg(Color::Named(NamedColor::Green))),
        ];
        let mut col = 0;
        for (text, template) in &segments {
            col = put_text(&mut row, col, text, template);
        }
        assert!(col >= 95, "line is {col} cells");

        let encoded = roundtrip(&row);
        assert_eq!(encoded.layout().runs, 6);
        assert!(encoded.bytes.len() <= 300, "{} bytes", encoded.bytes.len());
    }

    #[test]
    fn roundtrip_every_cell_different() {
        let width = 126;
        let mut row = Row::new(width);
        for col in 0..width {
            let (r, g, b) = (col as u8, 255 - col as u8, (col as u8).wrapping_mul(3));
            let template = Cell {
                fg: Color::Spec(Rgb { r, g, b }),
                bg: Color::Indexed(col as u8),
                flags: if col % 2 == 0 { Flags::ITALIC } else { Flags::UNDERLINE },
                ..Cell::default()
            };
            put_text(&mut row, col, "x", &template);
        }

        let encoded = roundtrip(&row);
        assert_eq!(encoded.header.len as usize, width - 1);
        assert_eq!(encoded.layout().runs, width - 1);
        assert!(encoded.bytes.len() <= 24 * width, "{} bytes", encoded.bytes.len());
    }

    #[test]
    fn roundtrip_wide_chars_and_spacers() {
        let mut row = Row::new(126);
        for i in 0..40 {
            row[Column(2 * i)] = Cell { c: '中', flags: Flags::WIDE_CHAR, ..Cell::default() };
            row[Column(2 * i + 1)] = Cell { flags: Flags::WIDE_CHAR_SPACER, ..Cell::default() };
        }

        let encoded = roundtrip(&row);
        let layout = encoded.layout();
        assert_eq!(encoded.header.len, 80);
        assert_eq!((layout.runs, layout.marks), (1, 80));
    }

    #[test]
    fn roundtrip_leading_wide_spacer_and_wrapline() {
        // A wide character that did not fit: the last cell is a leading spacer and wraps.
        let mut row = Row::new(10);
        put_text(&mut row, 0, "abcdefghi", &Cell::default());
        row[Column(9)] =
            Cell { flags: Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE, ..Cell::default() };
        let encoded = roundtrip(&row);
        assert!(encoded.header.wraps);
        assert!(!encoded.header.clear);
        assert_eq!(encoded.header.len, 9);

        // The continuation line: the wide character at column 0, wrapping again mid-word.
        let mut row = Row::new(10);
        row[Column(0)] = Cell { c: '字', flags: Flags::WIDE_CHAR, ..Cell::default() };
        row[Column(1)] = Cell { flags: Flags::WIDE_CHAR_SPACER, ..Cell::default() };
        put_text(&mut row, 2, "jklmnopq", &Cell::default());
        row[Column(9)].flags.insert(Flags::WRAPLINE);
        let encoded = roundtrip(&row);
        assert!(encoded.header.wraps);
        assert_eq!(encoded.layout().marks, 2);
    }

    #[test]
    fn roundtrip_bg_colored_erased_tail() {
        // `ESC [ 41 m ESC [ K` after "error": the erased tail keeps the red background.
        let red = Cell::from(Color::Named(NamedColor::Red));
        let mut row = Row::new(126);
        row.reset(&red);
        put_text(&mut row, 0, "error", &Cell::default());

        let encoded = roundtrip(&row);
        assert_eq!(encoded.header.len, 5, "P stops where the colored blank run starts");
        assert!(!encoded.header.clear);
        let small = encoded.bytes.len();

        // Same row with `occ` at the width: the encoder scans every cell, same result.
        let mut full = row.clone();
        let _ = &mut full[..];
        assert_eq!(full.occ, 126);
        let encoded = roundtrip(&full);
        assert_eq!(encoded.header.len, 5);
        assert_eq!(encoded.bytes.len(), small);
    }

    #[test]
    fn roundtrip_tabs_kept() {
        let mut row = Row::new(40);
        put_text(&mut row, 0, "a\t", &Cell::default());
        row[Column(8)] = Cell { c: 'b', ..Cell::default() };
        row[Column(9)] = Cell { c: '\t', ..Cell::default() };
        row[Column(16)] = Cell { c: 'c', ..Cell::default() };

        let encoded = roundtrip(&row);
        assert_eq!(encoded.header.len, 17);

        // A row of only tabs is clear, as `Cell::is_empty` treats '\t' as blank.
        let mut tabs = Row::new(8);
        put_text(&mut tabs, 0, "\t\t", &Cell::default());
        let encoded = roundtrip(&tabs);
        assert!(encoded.header.clear);
        assert_eq!(encoded.header.len, 2);
    }

    #[test]
    fn roundtrip_extras_shared_and_unique() {
        let mut link = Cell::default();
        link.set_hyperlink(Some(Hyperlink::new(Some("doc"), "https://example.com/doc".into())));

        // Ten cells share one hyperlink `Arc`; three cells with combining marks own theirs.
        let mut row = Row::new(126);
        put_text(&mut row, 0, "click here", &link);
        for col in 10..13 {
            let mut cell = Cell { c: 'e', ..Cell::default() };
            cell.push_zerowidth('\u{301}');
            row[Column(col)] = cell;
        }

        let encoded = roundtrip(&row);
        assert_eq!(encoded.layout().runs, 4, "one run for the link, one per combining cell");
        assert_eq!(encoded.header.extras, 4, "one queue entry for the link, one per mark");
        let link_arc = link.extra.as_ref().unwrap();
        assert!(Arc::ptr_eq(&encoded.extras[0], link_arc));

        // Decoded cells share the original `Arc`s.
        let mut into = Row::default();
        Cell::decode(&encoded.bytes, &encoded.extras, &mut into);
        for col in 0..13 {
            let decoded = into[Column(col)].extra.as_ref().unwrap();
            assert!(Arc::ptr_eq(decoded, row[Column(col)].extra.as_ref().unwrap()), "{col}");
        }

        // A link across a color change: two runs, still one queue entry.
        let mut row = Row::new(40);
        put_text(&mut row, 0, "see ", &Cell { fg: Color::Named(NamedColor::Red), ..link.clone() });
        put_text(&mut row, 4, "docs", &link);
        let encoded = roundtrip(&row);
        assert_eq!(encoded.layout().runs, 2);
        assert_eq!(encoded.header.extras, 1);

        // A link to the end of the row: everything is the tail, which refers to the extra.
        let mut row = Row::new(12);
        for cell in &mut row {
            *cell = link.clone();
        }
        let encoded = roundtrip(&row);
        assert_eq!((encoded.header.len, encoded.header.extras), (0, 1));
    }

    /// Debug builds check the `Row` `occ` invariant the encoder relies on.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Row occ invariant broken")]
    fn encode_checks_occ_invariant() {
        let cells = vec![Cell::default(), Cell { c: 'x', ..Cell::default() }, Cell::default()];
        let row = Row::from_vec(cells, 0);
        Cell::encode(&row, &mut Vec::new(), &mut VecDeque::new());
    }

    #[test]
    fn color_pack_every_variant() {
        let mut colors: Vec<Color> = ALL_NAMED.iter().map(|&named| Color::Named(named)).collect();
        colors.extend((0..=255).map(Color::Indexed));
        for i in 0..=255u8 {
            colors.push(Color::Spec(Rgb { r: i, g: 255 - i, b: i ^ 0x5a }));
            colors.push(Color::Spec(Rgb { r: i, g: i, b: i }));
        }
        for color in colors {
            assert_eq!(unpack_color(pack_color(color)), color);
        }

        // The table of named colors is exactly vte's set, with no duplicate codes.
        let decodable: Vec<NamedColor> = (0..=u16::MAX).filter_map(named_from_code).collect();
        assert_eq!(decodable.len(), ALL_NAMED.len());
        for named in ALL_NAMED {
            assert_eq!(named_from_code(named_code(named)), Some(named));
        }
    }

    #[test]
    fn flags_codec_knows_every_flag() {
        // Fails when upstream adds a `Flags` bit: classify it as structural (kept out of runs,
        // stored as a mark) or style (stored in runs) in `STRUCTURAL`/`STYLE`.
        assert_eq!(STRUCTURAL | STYLE, Flags::all());
        assert!(STRUCTURAL.intersection(STYLE).is_empty());
        let marked = MARKS.iter().fold(Flags::empty(), |flags, (flag, _)| flags | *flag);
        assert_eq!(marked, STRUCTURAL, "every structural flag needs a mark bit");
        for (flag, bit) in MARKS {
            assert_eq!((mark_bits(flag), mark_flags(bit)), (bit, flag));
        }
    }

    #[test]
    fn header_bits_without_decode() {
        let red_bg = Cell::from(Color::Named(NamedColor::Red));
        let mut wrapped_text = Row::new(30);
        put_text(&mut wrapped_text, 0, "long line", &Cell::default());
        wrapped_text[Column(29)].flags.insert(Flags::WRAPLINE);
        let mut wrap_only = Row::<Cell>::new(30);
        wrap_only[Column(29)].flags.insert(Flags::WRAPLINE);
        let mut colored = Row::new(30);
        colored.reset(&red_bg);
        let mut tabs = Row::new(30);
        put_text(&mut tabs, 3, "\t", &Cell::default());

        for (row, wraps, clear) in [
            (Row::new(30), false, true),
            (wrapped_text, true, false),
            (wrap_only, true, false),
            (colored, false, false),
            (tabs, false, true),
        ] {
            let mut out = Vec::new();
            let mut extras = VecDeque::new();
            let encoded = Cell::encode(&row, &mut out, &mut extras);
            let header = read_header(&out);
            assert_eq!(header, encoded);
            assert_eq!((header.wraps, header.clear), (wraps, clear));
            assert_eq!(header.wraps, Cell::row_wraps(&row));
            assert_eq!(header.clear, row.is_clear());
            assert_eq!(header.width, 30);
        }
    }

    fn random_color(rng: &mut ChaCha8Rng) -> Color {
        match rng.gen_range(0..3) {
            0 => Color::Named(ALL_NAMED[rng.gen_range(0..ALL_NAMED.len())]),
            1 => Color::Indexed(rng.r#gen()),
            _ => Color::Spec(Rgb { r: rng.r#gen(), g: rng.r#gen(), b: rng.r#gen() }),
        }
    }

    fn random_char(rng: &mut ChaCha8Rng) -> char {
        match rng.gen_range(0..10) {
            0..=2 => ' ',
            3 => '\t',
            4 => ['é', '中', '😀', '\u{301}', 'ß', '\u{10ffff}'][rng.gen_range(0..6)],
            5 => loop {
                if let Some(c) = char::from_u32(rng.gen_range(0..=0x10ffff)) {
                    break c;
                }
            },
            _ => rng.gen_range('!'..='~'),
        }
    }

    fn random_extra(rng: &mut ChaCha8Rng, pool: &[Arc<CellExtra>]) -> Option<Arc<CellExtra>> {
        match rng.gen_range(0..10) {
            0..=5 => None,
            6..=8 => Some(pool[rng.gen_range(0..pool.len())].clone()),
            _ => {
                let mut cell = Cell::default();
                cell.push_zerowidth(random_char(rng));
                cell.extra
            },
        }
    }

    fn random_cell(rng: &mut ChaCha8Rng, pool: &[Arc<CellExtra>]) -> Cell {
        Cell {
            c: random_char(rng),
            fg: random_color(rng),
            bg: random_color(rng),
            flags: Flags::from_bits_truncate(rng.r#gen()),
            extra: random_extra(rng, pool),
        }
    }

    #[test]
    #[should_panic(expected = "outside the row")]
    fn decode_cell_past_the_row_panics() {
        let encoded = roundtrip(&Row::new(4));
        Cell::decode_cell(&encoded.bytes, &[], 4);
    }

    #[test]
    fn roundtrip_random_rows() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x0fe4_7e2c);

        // Extras shared across cells and rows, as the parser shares them.
        let mut pool = Vec::new();
        for i in 0..4 {
            let mut cell = Cell::default();
            cell.set_hyperlink(Some(Hyperlink::new(None::<String>, format!("https://e/{i}"))));
            pool.push(cell.extra.unwrap());
            let mut cell = Cell::default();
            cell.set_underline_color(Some(random_color(&mut rng)));
            pool.push(cell.extra.unwrap());
        }

        let mut out = Vec::new();
        let mut extras = VecDeque::new();
        let mut into = Row::default();
        for n in 0..10_000 {
            let width = rng.gen_range(1..=160);
            let tail =
                if rng.gen_bool(0.5) { Cell::default() } else { random_cell(&mut rng, &pool) };
            let prefix = rng.gen_range(0..=width);
            let mut cells = Vec::with_capacity(width);
            let mut previous = random_cell(&mut rng, &pool);
            for _ in 0..prefix {
                // Mostly continue the previous cell's style, so rows have runs of every length.
                let mut cell = random_cell(&mut rng, &pool);
                if rng.gen_bool(0.7) {
                    cell = Cell { c: cell.c, ..previous.clone() };
                }
                previous = cell.clone();
                cells.push(cell);
            }
            cells.resize(width, tail);
            // Any `occ` from the first cell of the equal tail on is valid.
            let occ = rng.gen_range(prefix..=width);
            let row = Row::from_vec(cells, occ);

            let start = out.len();
            let first_extra = extras.len();
            // The encoder stays within `max_record_bytes`, bytes it cuts off again included: a
            // buffer with exactly that much room never reallocates.
            let bound = Cell::max_record_bytes(width).unwrap();
            let mut exact = Vec::with_capacity(bound);
            let capacity = exact.capacity();
            Cell::encode(&row, &mut exact, &mut VecDeque::new());
            assert_eq!(exact.capacity(), capacity, "row {n}: encode exceeded max_record_bytes");
            let header = Cell::encode(&row, &mut out, &mut extras);
            assert_eq!(&out[start..], &exact[..], "row {n}");
            assert_eq!(read_header(&out[start..]), header, "row {n}");
            assert_eq!(extras.len() - first_extra, header.extras as usize, "row {n}");

            let row_extras: Vec<_> = extras.range(first_extra..).cloned().collect();
            let used = decode_cell_row(&out[start..], &row_extras, &mut into);
            assert_eq!(used, out.len() - start, "row {n}");
            assert_eq!(into.len(), width, "row {n}");
            assert_eq!(into.occ, width, "row {n}");
            for col in 0..width {
                assert_eq!(into[Column(col)], row[Column(col)], "row {n}, column {col}");
                let cell = Cell::decode_cell(&out[start..], &row_extras, col);
                assert_eq!(cell, row[Column(col)], "decode_cell: row {n}, column {col}");
            }

            // Keep the buffers small; what matters is that records start anywhere.
            if out.len() > 1 << 16 {
                out.drain(..start);
                extras.drain(..first_extra);
            }
        }
    }

    /// Rows around the header's one-byte-count limit: with `P + 1 < 128` the header is written
    /// first and its counts patched at the end; from there on the body is written first and moved
    /// up behind the header. Every count (`R`, `M`, `E`) at its largest: a different style, a
    /// mark and a fresh extra on every stored cell.
    #[test]
    fn header_counts_at_one_byte_limit() {
        for stored in [0, 1, 125, 126, 127, 128, 129, 200, 300] {
            let width = stored + 1;
            let mut row = Row::new(width);
            for col in 0..stored {
                let mut cell = Cell {
                    c: if col % 3 == 0 { '😀' } else { 'a' },
                    fg: Color::Indexed(col as u8),
                    flags: if col % 2 == 0 { Flags::WIDE_CHAR } else { Flags::WIDE_CHAR_SPACER },
                    ..Cell::default()
                };
                cell.push_zerowidth('\u{301}');
                row[Column(col)] = cell;
            }
            let mut tail = Cell { c: 'z', ..Cell::default() };
            tail.push_zerowidth('\u{302}');
            row[Column(stored)] = tail;

            let encoded = roundtrip(&row);
            let layout = encoded.layout();
            let bound = Cell::max_record_bytes(width).unwrap();
            assert!(encoded.bytes.len() + 30 <= bound, "stored {stored}: above max_record_bytes");
            assert_eq!(encoded.header.len as usize, stored, "stored {stored}");
            assert_eq!((layout.runs, layout.marks), (stored, stored), "stored {stored}");
            assert_eq!(encoded.header.extras as usize, stored + 1, "stored {stored}");
        }
    }

    /// The ASCII text path of the encoder and decoder against rows whose text is ASCII except
    /// for one character anywhere, so the fallbacks run with every prefix length.
    #[test]
    fn text_ascii_and_fallback() {
        for width in [1, 2, 3, 40] {
            for at in 0..width {
                for c in ['a', 'é', '中', '😀', '\u{7f}', '\u{80}'] {
                    let mut row = Row::new(width);
                    put_text(&mut row, 0, &"x".repeat(width), &Cell::default());
                    row[Column(at)].c = c;
                    roundtrip(&row);
                }
            }
        }
    }

    /// `P` uses full `Cell` equality: cells whose extras are different `Arc`s with equal
    /// contents equal the tail, as they did before the faster comparison.
    #[test]
    fn stored_cells_compare_extras_by_value() {
        let mut row = Row::<Cell>::new(10);
        for cell in &mut row[..] {
            cell.push_zerowidth('\u{301}');
        }
        put_text(&mut row, 0, "ab", &Cell::default());
        let encoded = roundtrip(&row);
        assert_eq!(encoded.header.len, 2);
        assert_eq!(encoded.header.extras, 1, "only the tail's extra");

        // Runs still break on a different `Arc`, even with equal contents.
        let mut row = Row::<Cell>::new(4);
        for cell in &mut row[..][..2] {
            cell.c = 'q';
            cell.push_zerowidth('\u{301}');
        }
        assert_eq!(roundtrip(&row).layout().runs, 2);
    }

    /// The reflow's byte-copy fast path (`RowSource::next_reflow`): `reflow_info` describes the
    /// tail exactly, and whenever its conditions hold, the record with only `W` changed
    /// (`copy_with_width`) is byte for byte the record of the row grown or cut as the reflow
    /// loop would (decode, `Row::grow` or `Row::shrink`, encode).
    #[test]
    fn byte_copy_matches_decode_and_encode() {
        let mut rng = ChaCha8Rng::seed_from_u64(0xb17e);
        let mut pool = Vec::new();
        for i in 0..3 {
            let mut cell = Cell::default();
            cell.set_hyperlink(Some(Hyperlink::new(None::<String>, format!("https://e/{i}"))));
            pool.push(cell.extra.unwrap());
        }

        let (mut grown, mut shrunk) = (0, 0);
        for n in 0..20_000 {
            let width = rng.gen_range(1..=40);
            let tail = match rng.gen_range(0..4) {
                0 => Cell::default(),
                1 => Cell { c: '\t', ..Cell::default() },
                2 => Cell { flags: Flags::BOLD, ..Cell::default() },
                _ => random_cell(&mut rng, &pool),
            };
            let prefix = rng.gen_range(0..=width);
            let mut cells: Vec<Cell> = (0..prefix).map(|_| random_cell(&mut rng, &pool)).collect();
            cells.resize(width, tail);
            let row = Row::from_vec(cells, width);

            let mut extras = VecDeque::new();
            let mut record = Vec::new();
            Cell::encode(&row, &mut record, &mut extras);
            let extras: Vec<_> = extras.into_iter().collect();
            let info = Cell::reflow_info(&record).unwrap();
            let last = &row[Column(width - 1)];
            assert_eq!(info.width, width, "row {n}");
            assert_eq!(info.tail_default, *last == Cell::default(), "row {n}");
            let empty = last.is_empty() && last.extra.is_none();
            assert_eq!(info.tail_empty, empty, "row {n}");

            let mut decoded = Row::default();
            let reencode = |row: &Row<Cell>| {
                let mut out = Vec::new();
                Cell::encode(row, &mut out, &mut VecDeque::new());
                out
            };
            let copied = |new_width| {
                let mut out = Vec::new();
                copy_with_width(&record, new_width, &mut out);
                out
            };

            if info.tail_default {
                let new_width = width + rng.gen_range(1..300);
                Cell::decode(&record, &extras, &mut decoded);
                decoded.grow(new_width);
                assert_eq!(copied(new_width), reencode(&decoded), "row {n}: grow");
                grown += 1;
            }

            let new_width = rng.gen_range(1..=width);
            if info.stored < new_width {
                Cell::decode(&record, &extras, &mut decoded);
                decoded.shrink(new_width);
                assert_eq!(copied(new_width), reencode(&decoded), "row {n}: shrink");
                shrunk += 1;
            }
        }
        assert!(grown > 1000 && shrunk > 1000, "{grown} grown, {shrunk} shrunk");
    }

    /// Which rows `plain_text` reads straight from the record, and what it reads.
    #[test]
    fn plain_text_conditions() {
        let plain = |row: &Row<Cell>| {
            let bytes = roundtrip(row).bytes;
            plain_text(&bytes).map(|p| (p.text.to_vec(), p.tail, p.width, p.wraps))
        };

        // Text, colors and style flags: plain.
        let mut row = Row::new(12);
        put_text(&mut row, 0, "ab ", &fg(Color::Indexed(3)));
        put_text(&mut row, 3, "cd", &Cell { flags: Flags::BOLD, ..Cell::default() });
        assert_eq!(plain(&row), Some((b"ab cd".to_vec(), b' ', 12, false)));

        // A wrapping row: the tail carries `WRAPLINE`.
        let mut wrapping = Row::new(4);
        put_text(&mut wrapping, 0, "wxyz", &Cell::default());
        wrapping[Column(3)].flags.insert(Flags::WRAPLINE);
        assert_eq!(plain(&wrapping), Some((b"wxy".to_vec(), b'z', 4, true)));

        // A background-colored tail of another character.
        let mut tail = Row::new(5);
        tail[..].fill(Cell { c: '-', bg: Color::Indexed(1), ..Cell::default() });
        put_text(&mut tail, 0, "a", &Cell::default());
        assert_eq!(plain(&tail), Some((b"a".to_vec(), b'-', 5, false)));

        // Not plain: a tab (text or tail), a non-ASCII character (text or tail), a zero-width
        // character, a hyperlink, a wide character, a structural flag on a stored cell, a leading
        // wide-character spacer in the tail.
        let mut cases = Vec::new();
        for text in ["a\tb", "aéb"] {
            let mut row = Row::new(6);
            put_text(&mut row, 0, text, &Cell::default());
            cases.push(row);
        }
        for c in ['\t', 'é'] {
            let mut row = Row::new(6);
            row[..].fill(Cell { c, ..Cell::default() });
            put_text(&mut row, 0, "a", &Cell::default());
            cases.push(row);
        }
        let mut zero_width = Row::new(6);
        put_text(&mut zero_width, 0, "e", &Cell::default());
        zero_width[Column(0)].push_zerowidth('\u{301}');
        cases.push(zero_width);
        let mut link = Cell::default();
        link.set_hyperlink(Some(Hyperlink::new(Some("l"), "https://e".into())));
        let mut linked = Row::new(6);
        put_text(&mut linked, 0, "go", &link);
        cases.push(linked);
        let mut wide = Row::new(6);
        row_wide_pair(&mut wide, 0);
        cases.push(wide);
        let mut marked = Row::new(6);
        put_text(&mut marked, 0, "abc", &Cell::default());
        marked[Column(1)].flags.insert(Flags::WRAPLINE);
        cases.push(marked);
        let mut leading = Row::new(6);
        put_text(&mut leading, 0, "abcde", &Cell::default());
        leading[Column(5)].flags.insert(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE);
        cases.push(leading);
        for (i, row) in cases.iter().enumerate() {
            assert_eq!(plain(row), None, "case {i}");
        }
    }

    /// Encode and decode cost of typical 126-column rows (`cargo test --release --lib
    /// codec_timing -- --ignored --nocapture --test-threads=1`); a tool for tuning the codec,
    /// not a gate (`benches/ingest.rs` and `benches/reads.rs` are).
    #[test]
    #[ignore]
    fn codec_timing() {
        use std::time::Instant;

        let width = 126;
        let mut uniform = Row::new(width);
        uniform[..].fill(Cell::from(Color::Indexed(3)));
        uniform.set_equal_from(0);
        let mut plain = Row::<Cell>::new(width);
        for (i, cell) in plain[..].iter_mut().enumerate() {
            cell.c = char::from(b'a' + (i % 26) as u8);
        }
        let colored = |cells: usize, run: usize| {
            let mut row = Row::new(width);
            for i in 0..cells {
                let fg = Color::Indexed((i / run) as u8);
                row[Column(i)] = Cell { c: 'q', fg, ..Cell::default() };
            }
            row
        };
        let mut wide = Row::new(width);
        for i in 0..width / 2 {
            row_wide_pair(&mut wide, 2 * i);
        }
        let rows = [
            ("uniform", uniform),
            ("plain", plain),
            ("short", colored(20, 10)),
            ("log", colored(95, 16)),
            ("dense", colored(width, 1)),
            ("wide", wide),
        ];

        let n = 200_000;
        for (name, row) in rows {
            let mut out = Vec::with_capacity(1 << 16);
            let mut extras = VecDeque::new();
            let start = Instant::now();
            for _ in 0..n {
                out.clear();
                Cell::encode(std::hint::black_box(&row), &mut out, &mut extras);
            }
            let encode = start.elapsed().as_nanos() as f64 / n as f64;
            let mut into = Row::default();
            let start = Instant::now();
            for _ in 0..n {
                decode_cell_row(std::hint::black_box(&out), &[], &mut into);
            }
            let decode = start.elapsed().as_nanos() as f64 / n as f64;
            println!(
                "{name:8} encode {encode:7.1} ns, decode {decode:7.1} ns, {} bytes",
                out.len()
            );
        }
    }

    fn row_wide_pair(row: &mut Row<Cell>, col: usize) {
        row[Column(col)] = Cell { c: '中', flags: Flags::WIDE_CHAR, ..Cell::default() };
        row[Column(col + 1)] = Cell { flags: Flags::WIDE_CHAR_SPACER, ..Cell::default() };
    }
}
