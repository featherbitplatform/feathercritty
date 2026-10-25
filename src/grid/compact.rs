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
    /// `Arc<CellExtra>`). The history keeps the extras of all its rows in one queue.
    type Extra;

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

    /// Decode the record at the start of `bytes` (bytes after it are ignored) into `into`,
    /// which is resized only when its width differs. `extras` is the row's own slice of the
    /// queue `encode` pushed to. The decoded row's `occ` is its width.
    fn decode(bytes: &[u8], extras: &[Self::Extra], into: &mut Row<Self>);
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
    write_varint(out, header.width as usize);
    write_varint(out, header.len as usize);
    write_varint(out, runs);
    write_varint(out, marks);
    write_varint(out, header.extras as usize);
    let mut bits = 0;
    if header.wraps {
        bits |= HEADER_WRAPS;
    }
    if header.clear {
        bits |= HEADER_CLEAR;
    }
    out.push(bits);
}

/// Read the header of the record at the start of `bytes` without decoding the row.
#[cfg_attr(not(test), expect(dead_code, reason = "the block store (next change) reads headers"))]
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
        *into = if width == 0 { Row::default() } else { Row::new(width) };
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

/// Whether `a` and `b` belong to the same run: everything but the character and the
/// structural flags is equal, the extra by identity.
#[inline]
fn same_run(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.flags.difference(STRUCTURAL) == b.flags.difference(STRUCTURAL)
        && match (&a.extra, &b.extra) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
}

#[inline]
fn write_char(out: &mut Vec<u8>, c: char) {
    if c.is_ascii() {
        out.push(c as u8);
    } else {
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

/// Append colors, `flags` and the extra reference of `cell`, pushing its extra onto `extras`
/// unless it is the entry this row pushed last.
#[inline]
fn write_style(
    out: &mut Vec<u8>,
    cell: &Cell,
    flags: Flags,
    extras: &mut VecDeque<Arc<CellExtra>>,
    first_extra: usize,
) {
    out.extend_from_slice(&pack_color(cell.fg));
    out.extend_from_slice(&pack_color(cell.bg));
    out.extend_from_slice(&flags.bits().to_le_bytes());
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
    write_varint(out, reference);
}

/// Decode a `Cell` record into `into` and return its length in bytes.
fn decode_cell_row(bytes: &[u8], extras: &[Arc<CellExtra>], into: &mut Row<Cell>) -> usize {
    let mut reader = RecordReader::new(bytes);
    let Layout { header, runs, marks } = reader.layout();
    debug_assert_eq!(extras.len(), header.extras as usize, "extras slice does not match row");
    let width = header.width as usize;
    let stored = header.len as usize;
    let cells = prepare_row(into, width);

    for cell in &mut cells[..stored] {
        cell.c = reader.char();
    }

    let mut col = 0;
    for _ in 0..runs {
        let len = reader.varint();
        let style = reader.style(extras);
        for cell in &mut cells[col..col + len] {
            style.apply(cell);
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
        let style = reader.style(extras);
        for cell in &mut cells[stored..] {
            cell.c = c;
            style.apply(cell);
        }
    }

    reader.position()
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

        // `P`: cells from `occ` on all equal the tail (the `Row` invariant), so the scan for the
        // last differing cell starts there.
        let scan_end = row.occ.min(width - 1);
        let stored = cells[..scan_end].iter().rposition(|cell| cell != tail).map_or(0, |i| i + 1);
        debug_assert!(
            cells[stored..].iter().all(|cell| cell == tail),
            "Row occ invariant broken: cells from occ on are not all equal"
        );
        let explicit = &cells[..stored];

        // The body first; the header, whose counts are known only at the end, is moved in front
        // of it afterwards.
        for cell in explicit {
            write_char(out, cell.c);
        }

        let mut runs = 0;
        let mut col = 0;
        while col < stored {
            let first = &explicit[col];
            let len = explicit[col + 1..]
                .iter()
                .position(|cell| !same_run(cell, first))
                .map_or(stored - col, |n| n + 1);
            write_varint(out, len);
            write_style(out, first, first.flags.difference(STRUCTURAL), extras, first_extra);
            runs += 1;
            col += len;
        }

        let mut marks = 0;
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

        write_char(out, tail.c);
        write_style(out, tail, tail.flags, extras, first_extra);

        let header = RowHeader {
            width: u32::try_from(width).expect("row wider than u32::MAX cells"),
            wraps: tail.flags.contains(Flags::WRAPLINE),
            clear: tail.is_empty() && explicit.iter().all(GridCell::is_empty),
            extras: (extras.len() - first_extra) as u32,
            len: stored as u32,
        };
        let body_end = out.len();
        write_header(out, &header, runs, marks);
        let header_len = out.len() - body_end;
        out[start..].rotate_right(header_len);

        header
    }

    fn decode(bytes: &[u8], extras: &[Self::Extra], into: &mut Row<Self>) {
        decode_cell_row(bytes, extras, into);
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
        Cell::decode(&bytes, &row_extras, &mut again);
        assert_rows_equal(row, &again);

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
            let header = Cell::encode(&row, &mut out, &mut extras);
            assert_eq!(read_header(&out[start..]), header, "row {n}");
            assert_eq!(extras.len() - first_extra, header.extras as usize, "row {n}");

            let row_extras: Vec<_> = extras.range(first_extra..).cloned().collect();
            let used = decode_cell_row(&out[start..], &row_extras, &mut into);
            assert_eq!(used, out.len() - start, "row {n}");
            assert_eq!(into.len(), width, "row {n}");
            assert_eq!(into.occ, width, "row {n}");
            for col in 0..width {
                assert_eq!(into[Column(col)], row[Column(col)], "row {n}, column {col}");
            }

            // Keep the buffers small; what matters is that records start anywhere.
            if out.len() > 1 << 16 {
                out.drain(..start);
                extras.drain(..first_extra);
            }
        }
    }
}
