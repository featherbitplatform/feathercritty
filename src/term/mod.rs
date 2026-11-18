// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Exports the `Term` type which is a high-level API for the Grid.

use std::collections::HashMap;
use std::ops::{Index, IndexMut, Range};
use std::sync::Arc;
use std::time::Instant;
use std::{cmp, mem, ptr, slice, str};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;
use bitflags::bitflags;
use log::{debug, trace};
use unicode_width::UnicodeWidthChar;

use crate::event::{Event, EventListener, ImageData, ImageDecodeError, ImageKey};
use crate::graphics::{inline, kitty, limits, placements, sixel};
use crate::grid::{Dimensions, Done, Grid, GridIterator, Job, PlainText, RowReader, Scroll};
use crate::index::{self, Boundary, Column, Direction, Line, Point, Side};
use crate::selection::{Selection, SelectionRange, SelectionType};
use crate::term::cell::{Cell, Flags, LineLength};
use crate::term::color::Colors;
use crate::vi_mode::{ViModeCursor, ViMotion};
use crate::vte::ansi::{
    self, Attr, CharsetIndex, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, NamedColor, NamedMode, NamedPrivateMode, PrivateMode, Rgb,
    StandardCharset,
};

pub mod cell;
// feathercritty: grapheme cluster rules for mode 2027; not upstream.
mod cluster;
pub mod color;
pub mod search;

/// Minimum number of columns.
///
/// A minimum of 2 is necessary to hold fullwidth unicode characters.
pub const MIN_COLUMNS: usize = 2;

/// Minimum number of visible lines.
pub const MIN_SCREEN_LINES: usize = 1;

/// Max size of the window title stack.
const TITLE_STACK_MAX_DEPTH: usize = 4096;

/// Default semantic escape characters.
pub const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

/// Max size of the keyboard modes.
const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = TITLE_STACK_MAX_DEPTH;

/// Default tab interval, corresponding to terminfo `it` value.
const INITIAL_TABSTOPS: usize = 8;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TermMode: u32 {
        const NONE                    = 0;
        const SHOW_CURSOR             = 1;
        const APP_CURSOR              = 1 << 1;
        const APP_KEYPAD              = 1 << 2;
        const MOUSE_REPORT_CLICK      = 1 << 3;
        const BRACKETED_PASTE         = 1 << 4;
        const SGR_MOUSE               = 1 << 5;
        const MOUSE_MOTION            = 1 << 6;
        const LINE_WRAP               = 1 << 7;
        const LINE_FEED_NEW_LINE      = 1 << 8;
        const ORIGIN                  = 1 << 9;
        const INSERT                  = 1 << 10;
        const FOCUS_IN_OUT            = 1 << 11;
        const ALT_SCREEN              = 1 << 12;
        const MOUSE_DRAG              = 1 << 13;
        const UTF8_MOUSE              = 1 << 14;
        const ALTERNATE_SCROLL        = 1 << 15;
        const VI                      = 1 << 16;
        const URGENCY_HINTS           = 1 << 17;
        const DISAMBIGUATE_ESC_CODES  = 1 << 18;
        const REPORT_EVENT_TYPES      = 1 << 19;
        const REPORT_ALTERNATE_KEYS   = 1 << 20;
        const REPORT_ALL_KEYS_AS_ESC  = 1 << 21;
        const REPORT_ASSOCIATED_TEXT  = 1 << 22;
        // feathercritty: DEC private mode 2027, grapheme clusters in one cell; not upstream.
        const GRAPHEME_CLUSTER        = 1 << 23;
        // feathercritty: DEC private mode 80 (DECSDM), sixel images at the cursor; not upstream.
        const DECSDM                  = 1 << 24;
        const MOUSE_MODE              = Self::MOUSE_REPORT_CLICK.bits() | Self::MOUSE_MOTION.bits() | Self::MOUSE_DRAG.bits();
        const KITTY_KEYBOARD_PROTOCOL = Self::DISAMBIGUATE_ESC_CODES.bits()
                                      | Self::REPORT_EVENT_TYPES.bits()
                                      | Self::REPORT_ALTERNATE_KEYS.bits()
                                      | Self::REPORT_ALL_KEYS_AS_ESC.bits()
                                      | Self::REPORT_ASSOCIATED_TEXT.bits();
         const ANY                    = u32::MAX;
    }
}

impl From<KeyboardModes> for TermMode {
    fn from(value: KeyboardModes) -> Self {
        let mut mode = Self::empty();

        let disambiguate_esc_codes = value.contains(KeyboardModes::DISAMBIGUATE_ESC_CODES);
        mode.set(TermMode::DISAMBIGUATE_ESC_CODES, disambiguate_esc_codes);

        let report_event_types = value.contains(KeyboardModes::REPORT_EVENT_TYPES);
        mode.set(TermMode::REPORT_EVENT_TYPES, report_event_types);

        let report_alternate_keys = value.contains(KeyboardModes::REPORT_ALTERNATE_KEYS);
        mode.set(TermMode::REPORT_ALTERNATE_KEYS, report_alternate_keys);

        let report_all_keys_as_esc = value.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC);
        mode.set(TermMode::REPORT_ALL_KEYS_AS_ESC, report_all_keys_as_esc);

        let report_associated_text = value.contains(KeyboardModes::REPORT_ASSOCIATED_TEXT);
        mode.set(TermMode::REPORT_ASSOCIATED_TEXT, report_associated_text);

        mode
    }
}

impl Default for TermMode {
    fn default() -> TermMode {
        TermMode::SHOW_CURSOR
            | TermMode::LINE_WRAP
            | TermMode::ALTERNATE_SCROLL
            | TermMode::URGENCY_HINTS
    }
}

/// Convert a terminal point to a viewport relative point.
#[inline]
pub fn point_to_viewport(display_offset: usize, point: Point) -> Option<Point<usize>> {
    let viewport_line = point.line.0 + display_offset as i32;
    usize::try_from(viewport_line).ok().map(|line| Point::new(line, point.column))
}

/// Convert a viewport relative point to a terminal point.
#[inline]
pub fn viewport_to_point(display_offset: usize, point: Point<usize>) -> Point {
    let line = Line(point.line as i32) - display_offset;
    Point::new(line, point.column)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDamageBounds {
    /// Damaged line number.
    pub line: usize,

    /// Leftmost damaged column.
    pub left: usize,

    /// Rightmost damaged column.
    pub right: usize,
}

impl LineDamageBounds {
    #[inline]
    pub fn new(line: usize, left: usize, right: usize) -> Self {
        Self { line, left, right }
    }

    #[inline]
    pub fn undamaged(line: usize, num_cols: usize) -> Self {
        Self { line, left: num_cols, right: 0 }
    }

    #[inline]
    pub fn reset(&mut self, num_cols: usize) {
        *self = Self::undamaged(self.line, num_cols);
    }

    #[inline]
    pub fn expand(&mut self, left: usize, right: usize) {
        self.left = cmp::min(self.left, left);
        self.right = cmp::max(self.right, right);
    }

    #[inline]
    pub fn is_damaged(&self) -> bool {
        self.left <= self.right
    }
}

/// Terminal damage information collected since the last [`Term::reset_damage`] call.
#[derive(Debug)]
pub enum TermDamage<'a> {
    /// The entire terminal is damaged.
    Full,

    /// Iterator over damaged lines in the terminal.
    Partial(TermDamageIterator<'a>),
}

/// Iterator over the terminal's viewport damaged lines.
#[derive(Clone, Debug)]
pub struct TermDamageIterator<'a> {
    line_damage: slice::Iter<'a, LineDamageBounds>,
    display_offset: usize,
}

impl<'a> TermDamageIterator<'a> {
    pub fn new(line_damage: &'a [LineDamageBounds], display_offset: usize) -> Self {
        let num_lines = line_damage.len();
        // Filter out invisible damage.
        let line_damage = &line_damage[..num_lines.saturating_sub(display_offset)];
        Self { display_offset, line_damage: line_damage.iter() }
    }
}

impl Iterator for TermDamageIterator<'_> {
    type Item = LineDamageBounds;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_damage.find_map(|line| {
            line.is_damaged().then_some(LineDamageBounds::new(
                line.line + self.display_offset,
                line.left,
                line.right,
            ))
        })
    }
}

/// State of the terminal damage.
struct TermDamageState {
    /// Hint whether terminal should be damaged entirely regardless of the actual damage changes.
    full: bool,

    /// Information about damage on terminal lines.
    lines: Vec<LineDamageBounds>,

    /// Old terminal cursor point.
    last_cursor: Point,
}

impl TermDamageState {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        let lines =
            (0..num_lines).map(|line| LineDamageBounds::undamaged(line, num_cols)).collect();

        Self { full: true, lines, last_cursor: Default::default() }
    }

    #[inline]
    fn resize(&mut self, num_cols: usize, num_lines: usize) {
        // Reset point, so old cursor won't end up outside of the viewport.
        self.last_cursor = Default::default();
        self.full = true;

        self.lines.clear();
        self.lines.reserve(num_lines);
        for line in 0..num_lines {
            self.lines.push(LineDamageBounds::undamaged(line, num_cols));
        }
    }

    /// Damage point inside of the viewport.
    #[inline]
    fn damage_point(&mut self, point: Point<usize>) {
        self.damage_line(point.line, point.column.0, point.column.0);
    }

    /// Expand `line`'s damage to span at least `left` to `right` column.
    #[inline]
    fn damage_line(&mut self, line: usize, left: usize, right: usize) {
        self.lines[line].expand(left, right);
    }

    /// Reset information about terminal damage.
    fn reset(&mut self, num_cols: usize) {
        self.full = false;
        self.lines.iter_mut().for_each(|line| line.reset(num_cols));
    }
}

pub struct Term<T> {
    /// Terminal focus controlling the cursor shape.
    pub is_focused: bool,

    /// Cursor for keyboard selection.
    pub vi_mode_cursor: ViModeCursor,

    pub selection: Option<Selection>,

    /// Currently active grid.
    ///
    /// Tracks the screen buffer currently in use. While the alternate screen buffer is active,
    /// this will be the alternate grid. Otherwise it is the primary screen buffer.
    grid: Grid<Cell>,

    /// Currently inactive grid.
    ///
    /// Opposite of the active grid. While the alternate screen buffer is active, this will be the
    /// primary grid. Otherwise it is the alternate screen buffer.
    inactive_grid: Grid<Cell>,

    /// Index into `charsets`, pointing to what ASCII is currently being mapped to.
    active_charset: CharsetIndex,

    /// Tabstops.
    tabs: TabStops,

    /// Mode flags.
    mode: TermMode,

    /// Scroll region.
    ///
    /// Range going from top to bottom of the terminal, indexed from the top of the viewport.
    scroll_region: Range<Line>,

    /// Modified terminal colors.
    colors: Colors,

    /// Current style of the cursor.
    cursor_style: Option<CursorStyle>,

    /// Proxy for sending events to the event loop.
    event_proxy: T,

    /// Current title of the window.
    title: Option<String>,

    /// Stack of saved window titles. When a title is popped from this stack, the `title` for the
    /// term is set.
    title_stack: Vec<Option<String>>,

    /// The stack for the keyboard modes.
    keyboard_mode_stack: Vec<KeyboardModes>,

    /// Currently inactive keyboard mode stack.
    inactive_keyboard_mode_stack: Vec<KeyboardModes>,

    /// Information about damaged cells.
    damage: TermDamageState,

    // feathercritty: graphics protocol state (placements, parsers); not upstream.
    /// Image placements, kitty ids and scrollback placeholders.
    placements: placements::PlacementTable,

    /// The kitty graphics protocol parser, fed by the APC callbacks.
    kitty_parser: kitty::KittyParser,

    /// The sixel canvas the current DCS `q` sequence is drawing, if one is in progress.
    sixel_decoder: Option<sixel::SixelDecoder>,

    /// The OSC 1337 inline image being read, if one is in progress.
    inline_parser: inline::InlineImageParser,

    /// Placement events produced where the terminal has no `EventListener` bound (`resize`);
    /// sent by the next bounded operation ([`flush_image_events`](Self::flush_image_events)).
    pending_image_events: Vec<Event>,

    /// Whether images are supported at all ([`set_images_enabled`](Self::set_images_enabled), on
    /// by default). Off, no sequence stores or displays anything and the support reports say so.
    images_enabled: bool,

    /// Whether [`set_cell_pixel_size`](Self::set_cell_pixel_size) has been called: until it has,
    /// the pixel size reports (`CSI 14 t`, `CSI 16 t`) ask the host through
    /// [`Event::TextAreaSizeRequest`] instead of answering from the grid.
    cell_size_known: bool,

    /// Config directly for the terminal.
    config: Config,
}

/// Configuration options for the [`Term`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The maximum amount of scrolling history.
    pub scrolling_history: usize,

    /// Default cursor style to reset the cursor to.
    pub default_cursor_style: CursorStyle,

    /// Cursor style for Vi mode.
    pub vi_mode_cursor_style: Option<CursorStyle>,

    /// The characters which terminate semantic selection.
    ///
    /// The default value is [`SEMANTIC_ESCAPE_CHARS`].
    pub semantic_escape_chars: String,

    /// Whether to enable kitty keyboard protocol.
    pub kitty_keyboard: bool,

    /// OSC52 support mode.
    pub osc52: Osc52,

    /// Whether DEC private mode 2027 (grapheme clusters) is set when the terminal is created
    /// and after a full reset. Changing it on a live terminal never flips the mode.
    ///
    /// feathercritty: not upstream.
    pub grapheme_clusters: bool,

    /// Compress history blocks older than this many rows (`None`: never compress). Blocks change
    /// state only through history jobs (see [`Term::next_history_job`]); any value is accepted,
    /// 0 included. [`Term::new`] and [`Term::set_options`] apply it to both grids; a grid put in
    /// place otherwise (deserialized, or assigned through [`Term::grid_mut`]) has compression off
    /// until the next `set_options`.
    ///
    /// feathercritty: not upstream.
    pub history_compression: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrolling_history: 10000,
            semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(),
            default_cursor_style: Default::default(),
            vi_mode_cursor_style: Default::default(),
            kitty_keyboard: Default::default(),
            osc52: Default::default(),
            grapheme_clusters: false,
            history_compression: None,
        }
    }
}

/// Which of a [`Term`]'s grids a history job belongs to (feathercritty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryGrid {
    /// The normal screen's grid, wherever it sits: `grid`, or `inactive_grid` while the alternate
    /// screen is shown.
    Normal,

    /// The alternate screen's grid. Its history limit is 0, so it has no jobs, unless a host
    /// gives it a history through [`Term::grid_mut`] while it is shown
    /// (`alternate_grid_jobs_follow_their_grid` in `src/grid/tests.rs`).
    Alternate,
}

/// A history block to compress or decompress, from [`Term::next_history_job`] (feathercritty).
///
/// It holds a copy of the block's bytes and needs no access to the terminal, so
/// [`HistoryJob::run`] can run outside the terminal's lock; [`Term::install_history_job`] then
/// installs the result.
#[derive(Debug)]
pub struct HistoryJob {
    grid: HistoryGrid,

    /// The grid's history epoch when the job was taken (`Grid::history_epoch`).
    epoch: u64,
    job: Job,
}

/// The result of a [`HistoryJob`], for [`Term::install_history_job`] (feathercritty).
#[derive(Debug)]
pub struct HistoryDone {
    grid: HistoryGrid,
    epoch: u64,
    done: Done,
}

impl HistoryJob {
    /// Compress or decompress the copied block (no lock needed).
    pub fn run(self) -> HistoryDone {
        HistoryDone { grid: self.grid, epoch: self.epoch, done: self.job.run() }
    }
}

/// OSC 52 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(rename_all = "lowercase"))]
pub enum Osc52 {
    /// The handling of the escape sequence is disabled.
    Disabled,
    /// Only copy sequence is accepted.
    ///
    /// This option is the default as a compromise between entirely
    /// disabling it (the most secure) and allowing `paste` (the less secure).
    #[default]
    OnlyCopy,
    /// Only paste sequence is accepted.
    OnlyPaste,
    /// Both are accepted.
    CopyPaste,
}

impl<T> Term<T> {
    #[inline]
    pub fn scroll_display(&mut self, scroll: Scroll)
    where
        T: EventListener,
    {
        let old_display_offset = self.grid.display_offset();
        self.grid.scroll_display(scroll);
        self.event_proxy.send_event(Event::MouseCursorDirty);

        // Clamp vi mode cursor to the viewport.
        let viewport_start = -(self.grid.display_offset() as i32);
        let viewport_end = viewport_start + self.bottommost_line().0;
        let vi_cursor_line = &mut self.vi_mode_cursor.point.line.0;
        *vi_cursor_line = cmp::min(viewport_end, cmp::max(viewport_start, *vi_cursor_line));
        self.vi_mode_recompute_selection();

        // Damage everything if display offset changed.
        if old_display_offset != self.grid().display_offset() {
            self.mark_fully_damaged();
        }
    }

    pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T> {
        let num_cols = dimensions.columns();
        let num_lines = dimensions.screen_lines();

        let history_size = config.scrolling_history;
        let mut grid = Grid::new(num_lines, num_cols, history_size);
        let mut inactive_grid = Grid::new(num_lines, num_cols, 0);
        // feathercritty: history compression (see `set_options`).
        grid.set_history_compression(config.history_compression);
        inactive_grid.set_history_compression(config.history_compression);

        let tabs = TabStops::new(grid.columns());

        let scroll_region = Line(0)..Line(grid.screen_lines() as i32);

        // Initialize terminal damage, covering the entire terminal upon launch.
        let damage = TermDamageState::new(num_cols, num_lines);

        let mut term = Term {
            inactive_grid,
            scroll_region,
            event_proxy,
            damage,
            placements: Default::default(),
            kitty_parser: Default::default(),
            sixel_decoder: None,
            inline_parser: Default::default(),
            pending_image_events: Vec::new(),
            images_enabled: true,
            cell_size_known: false,
            config,
            grid,
            tabs,
            inactive_keyboard_mode_stack: Default::default(),
            keyboard_mode_stack: Default::default(),
            active_charset: Default::default(),
            vi_mode_cursor: Default::default(),
            cursor_style: Default::default(),
            colors: color::Colors::default(),
            title_stack: Default::default(),
            is_focused: Default::default(),
            selection: Default::default(),
            title: Default::default(),
            mode: Default::default(),
        };
        // feathercritty: upstream returns the struct directly; mode 2027 takes its default.
        term.reset_grapheme_cluster_mode();
        term
    }

    /// Set or clear DEC private mode 2027 (grapheme clusters) to the configured default
    /// ([`Config::grapheme_clusters`]), as a full reset does. Meant for a new program taking
    /// over an existing terminal, so it never inherits the mode from the one before.
    ///
    /// feathercritty: not upstream.
    pub fn reset_grapheme_cluster_mode(&mut self) {
        self.mode.set(TermMode::GRAPHEME_CLUSTER, self.config.grapheme_clusters);
    }

    /// Collect the information about the changes in the lines, which
    /// could be used to minimize the amount of drawing operations.
    ///
    /// The user controlled elements, like `Vi` mode cursor and `Selection` are **not** part of the
    /// collected damage state. Those could easily be tracked by comparing their old and new
    /// value between adjacent frames.
    ///
    /// After reading damage [`reset_damage`] should be called.
    ///
    /// [`reset_damage`]: Self::reset_damage
    #[must_use]
    pub fn damage(&mut self) -> TermDamage<'_> {
        // Ensure the entire terminal is damaged after entering insert mode.
        // Leaving is handled in the ansi handler.
        if self.mode.contains(TermMode::INSERT) {
            self.mark_fully_damaged();
        }

        let previous_cursor = mem::replace(&mut self.damage.last_cursor, self.grid.cursor.point);

        if self.damage.full {
            return TermDamage::Full;
        }

        // Add information about old cursor position and new one if they are not the same, so we
        // cover everything that was produced by `Term::input`.
        if self.damage.last_cursor != previous_cursor {
            // Cursor coordinates are always inside viewport even if you have `display_offset`.
            let point = Point::new(previous_cursor.line.0 as usize, previous_cursor.column);
            self.damage.damage_point(point);
        }

        // Always damage current cursor.
        self.damage_cursor();

        // NOTE: damage which changes all the content when the display offset is non-zero (e.g.
        // scrolling) is handled via full damage.
        let display_offset = self.grid().display_offset();
        TermDamage::Partial(TermDamageIterator::new(&self.damage.lines, display_offset))
    }

    /// Resets the terminal damage information.
    pub fn reset_damage(&mut self) {
        self.damage.reset(self.columns());
    }

    #[inline]
    fn mark_fully_damaged(&mut self) {
        self.damage.full = true;
    }

    /// Set new options for the [`Term`].
    pub fn set_options(&mut self, options: Config)
    where
        T: EventListener,
    {
        let old_config = mem::replace(&mut self.config, options);

        let title_event = match &self.title {
            Some(title) => Event::Title(title.clone()),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);

        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid.update_history(self.config.scrolling_history);
        } else {
            self.grid.update_history(self.config.scrolling_history);
        }

        // feathercritty: a smaller history may have made scrollback placements unreachable; they
        // go now. Not upstream.
        if !self.placements.is_empty() {
            let mut events = Vec::new();
            let (active, inactive) = self.screen_pair();
            self.placements.prune_out_of_view(self.grid_window(&self.grid), active, &mut events);
            self.placements.prune_out_of_view(
                self.grid_window(&self.inactive_grid),
                inactive,
                &mut events,
            );
            self.send_image_events(events);
        }

        // feathercritty: history compression, on both grids (the alternate one has a history
        // limit of 0, so it has no jobs unless a host gave it a history). Only marks blocks for
        // jobs.
        let compression = self.config.history_compression;
        self.grid.set_history_compression(compression);
        self.inactive_grid.set_history_compression(compression);

        if self.config.kitty_keyboard != old_config.kitty_keyboard {
            self.keyboard_mode_stack = Vec::new();
            self.inactive_keyboard_mode_stack = Vec::new();
            self.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        }

        // Damage everything on config updates.
        self.mark_fully_damaged();
    }

    /// Convert the active selection to a String.
    pub fn selection_to_string(&self) -> Option<String> {
        let selection_range = self.selection.as_ref().and_then(|s| s.to_range(self))?;
        let SelectionRange { start, end, .. } = selection_range;

        let mut res = String::new();

        // feathercritty: one reader for every `line_to_string` call (see there).
        let mut reader = RowReader::new();

        match self.selection.as_ref() {
            Some(Selection { ty: SelectionType::Block, .. }) => {
                for line in (start.line.0..end.line.0).map(Line::from) {
                    res += self
                        .line_to_string(
                            &mut reader,
                            line,
                            start.column..end.column,
                            start.column.0 != 0,
                        )
                        .trim_end();
                    res += "\n";
                }

                res += self
                    .line_to_string(&mut reader, end.line, start.column..end.column, true)
                    .trim_end();
            },
            Some(Selection { ty: SelectionType::Lines, .. }) => {
                res = self.bounds_to_string(start, end) + "\n";
            },
            _ => {
                res = self.bounds_to_string(start, end);
            },
        }

        Some(res)
    }

    /// Convert range between two points to a String.
    pub fn bounds_to_string(&self, start: Point, end: Point) -> String {
        let mut res = String::new();

        // feathercritty: one reader for every `line_to_string` call (see there).
        let mut reader = RowReader::new();

        for line in (start.line.0..=end.line.0).map(Line::from) {
            let start_col = if line == start.line { start.column } else { Column(0) };
            let end_col = if line == end.line { end.column } else { self.last_column() };

            res += &self.line_to_string(&mut reader, line, start_col..end_col, line == end.line);
        }

        res.strip_suffix('\n').map(str::to_owned).unwrap_or(res)
    }

    /// Convert a single line in the grid to a String.
    ///
    /// feathercritty: upstream indexed `self.grid[line]`; the line can be a history line, so the
    /// row is read through `reader` (decoded for history rows, the row itself on the screen).
    /// The caller keeps one reader across lines so the copies reuse one allocation. If upstream
    /// adds a grid read here, read it from `grid_line` or `self.grid.cell`.
    fn line_to_string(
        &self,
        reader: &mut RowReader<Cell>,
        line: Line,
        cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        // feathercritty: text fast path (no upstream counterpart). A history row that is plain
        // text (`Grid::history_plain_text`) is read straight from its record instead of being
        // decoded into cells; `plain_line_to_string` gives the same result as the body below.
        let plain = |row: PlainText<'_>| self.plain_line_to_string(row, cols.clone());
        if let Some(text) = self.grid.history_plain_text(line, plain) {
            return text;
        }

        self.cells_line_to_string(reader, line, cols, include_wrapped_wide)
    }

    /// `line_to_string` for a history row that is plain text (see `compact::plain_text`).
    ///
    /// feathercritty addition. It is upstream's `line_to_string` (below, as
    /// `cells_line_to_string`) reduced to what can happen on such a row: no cell is a
    /// wide-character spacer or a tab and none has zero-width characters, so the column range is
    /// never widened, nothing is skipped and every cell adds its character; only cells from `P` on
    /// (the tail) can have `WRAPLINE`; no cell is a leading wide-character spacer, so the line
    /// above is never read. When merging upstream: if `line_to_string`, `LineLength` or the
    /// spacer handling change, change this to match or narrow `compact::plain_text`; the
    /// differential test and `plain_line_to_string_matches_cells` check them against each other.
    fn plain_line_to_string(&self, row: PlainText<'_>, cols: Range<Column>) -> String {
        let PlainText { text, tail, width, wraps } = row;
        let stored = text.len();

        // `LineLength::line_length`: the whole row when its last cell wraps, else up to its last
        // cell that is not a space.
        let length = if wraps || tail != b' ' {
            width
        } else {
            text.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)
        };
        let line_length = cmp::min(length, cols.end.0 + 1);

        let mut line = String::new();
        let start = cols.start.0;
        if start < line_length {
            let head = &text[start.min(stored)..line_length.min(stored)];
            line.push_str(str::from_utf8(head).expect("plain history text is ASCII"));
            line.extend((start.max(stored)..line_length).map(|_| char::from(tail)));
        }

        // Upstream checks `WRAPLINE` on the last cell taken. With `cols` reaching the last
        // column, a wrapping row is taken whole, so that cell is the tail's.
        if cols.end >= self.columns() - 1 && (line_length == 0 || !wraps) {
            line.push('\n');
        }

        line
    }

    /// `line_to_string` for any line, through its cells (upstream's body).
    fn cells_line_to_string(
        &self,
        reader: &mut RowReader<Cell>,
        line: Line,
        mut cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        let mut text = String::new();

        let grid_line = reader.row(&self.grid, line);
        let line_length = cmp::min(grid_line.line_length(), cols.end + 1);

        // Include wide char when trailing spacer is selected.
        if grid_line[cols.start].flags.contains(Flags::WIDE_CHAR_SPACER) {
            cols.start -= 1;
        }

        let mut tab_mode = false;
        for column in (cols.start.0..line_length.0).map(Column::from) {
            let cell = &grid_line[column];

            // Skip over cells until next tab-stop once a tab was found.
            if tab_mode {
                if self.tabs[column] || cell.c != ' ' {
                    tab_mode = false;
                } else {
                    continue;
                }
            }

            if cell.c == '\t' {
                tab_mode = true;
            }

            if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                // Push cells primary character.
                text.push(cell.c);

                // Push zero-width characters.
                for c in cell.zerowidth().into_iter().flatten() {
                    text.push(*c);
                }
            }
        }

        // feathercritty: upstream read `self.grid[line]` again here; it is `grid_line`.
        if cols.end >= self.columns() - 1
            && (line_length.0 == 0 || !grid_line[line_length - 1].flags.contains(Flags::WRAPLINE))
        {
            text.push('\n');
        }

        // If wide char is not part of the selection, but leading spacer is, include it.
        //
        // feathercritty: upstream read `self.grid[line - 1i32][Column(0)]` (the line above, as
        // 0.26.0 does), which can be a history line: read through `Grid::cell`. On the topmost
        // line that line does not exist ("no row"); upstream's indexing then failed a
        // `debug_assert!` in debug builds and read whichever row its ring held in release
        // builds. Here that case adds nothing (a fork behavior change, also applied to the
        // differential test's oracle, `tests/oracle/PATCHES.md`). When merging upstream: keep
        // the topmost-line condition while this reads the line above.
        if line_length == self.columns()
            && line_length.0 >= 2
            && grid_line[line_length - 1].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && include_wrapped_wide
            && line > self.grid.topmost_line()
        {
            text.push(self.grid.cell(Point::new(line - 1i32, Column(0))).c);
        }

        text
    }

    // --- graphics (feathercritty; not upstream) ---------------------------------------------

    /// The [`placements::Screen`] the visible grid is, and the one `inactive_grid` holds.
    #[inline]
    fn screen_pair(&self) -> (placements::Screen, placements::Screen) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            (placements::Screen::Alternate, placements::Screen::Primary)
        } else {
            (placements::Screen::Primary, placements::Screen::Alternate)
        }
    }

    /// The screen currently shown: the one new placements belong to.
    #[inline]
    fn active_screen(&self) -> placements::Screen {
        self.screen_pair().0
    }

    /// The numbers [`placements::GridWindow`] translates placement lines with, read from `grid`.
    #[inline]
    fn grid_window(&self, grid: &Grid<crate::term::cell::Cell>) -> placements::GridWindow {
        placements::GridWindow {
            stable: grid.scrolled_lines(),
            lines: grid.screen_lines(),
            history: grid.history_size(),
        }
    }

    /// Hand placement-table events to the embedder.
    fn send_image_events(&mut self, events: Vec<Event>)
    where
        T: EventListener,
    {
        for event in events {
            self.event_proxy.send_event(event);
        }
    }

    /// Park events generated where no `EventListener` bound exists (`resize`); the next bounded
    /// operation sends them.
    fn queue_image_events(&mut self, events: Vec<Event>) {
        self.pending_image_events.extend(events);
    }

    /// Send what [`queue_image_events`](Self::queue_image_events) parked, preserving order.
    fn flush_image_events(&mut self)
    where
        T: EventListener,
    {
        if !self.pending_image_events.is_empty() {
            let pending = mem::take(&mut self.pending_image_events);
            for event in pending {
                self.event_proxy.send_event(event);
            }
        }
    }

    /// Answer a sequence the kitty parser rejected, unless its quiet level suppresses it.
    ///
    /// With image support off the answer becomes `ENOTSUPPORTED`: off, "not supported" is the only
    /// graphics answer this terminal gives, so a frame that would have been refused for a narrower
    /// reason (a bad key, a file medium, a stalled upload) is refused as unsupported instead.
    fn drain_kitty_reject(&mut self)
    where
        T: EventListener,
    {
        if let Some(reject) = self.kitty_parser.take_reject() {
            let error =
                if self.images_enabled { reject.error } else { kitty::KittyError::ENOTSUPPORTED };
            self.send_kitty_reply(reject.id, reject.number, None, Err(error), reject.quiet);
        }
    }

    /// Send one graphics protocol response frame (`None` result is `OK`).
    fn send_kitty_reply(
        &mut self,
        id: u32,
        number: Option<u32>,
        placement: Option<u32>,
        result: Result<(), kitty::KittyError>,
        quiet: u8,
    ) where
        T: EventListener,
    {
        if let Some(reply) = kitty::reply(id, number, placement, result, quiet) {
            let text = String::from_utf8_lossy(&reply).into_owned();
            self.event_proxy.send_event(Event::PtyWrite(text));
        }
    }

    /// Act on one completed graphics sequence: transmit, place, delete or the support query.
    ///
    /// With image support off ([`set_images_enabled`](Self::set_images_enabled)) nothing is stored
    /// or displayed and every frame gets the protocol's "not supported" answer instead.
    fn handle_kitty_command(&mut self, command: kitty::KittyCommand)
    where
        T: EventListener,
    {
        // feathercritty: off, the payload goes unused; only the answer remains (the quiet level
        // still suppresses it). Not upstream.
        if !self.images_enabled {
            let (id, number, placement, quiet) = match &command {
                kitty::KittyCommand::Transmit { id, number, quiet, .. } => {
                    (id.unwrap_or(0), *number, None, *quiet)
                },
                kitty::KittyCommand::Place(place) => {
                    (place.id.unwrap_or(0), place.number, place.placement_id, place.quiet)
                },
                kitty::KittyCommand::Delete(delete) => match delete.target {
                    kitty::DeleteTarget::ImageId { image, placement } => {
                        (image.unwrap_or(0), None, placement, delete.quiet)
                    },
                    kitty::DeleteTarget::Number { number, placement } => {
                        (0, number, placement, delete.quiet)
                    },
                    _ => (0, None, None, delete.quiet),
                },
                kitty::KittyCommand::Query { id, quiet } => (*id, None, None, *quiet),
            };
            let error = Err(kitty::KittyError::ENOTSUPPORTED);
            self.send_kitty_reply(id, number, placement, error, quiet);
            return;
        }

        match command {
            kitty::KittyCommand::Transmit { id, number, format, bytes, then_place, quiet } => {
                let (width, height) = match format {
                    kitty::KittyFormat::Png { width, height }
                    | kitty::KittyFormat::Rgb { width, height }
                    | kitty::KittyFormat::Rgba { width, height } => (width, height),
                };
                // feathercritty: a transmission addressed only by number gets an id assigned —
                // the protocol has the terminal pick one and answer it, and without one the
                // image could never be placed by id (`a=p`, `U=1`). Not upstream.
                let assigned =
                    (id.is_none() && number.is_some()).then(|| self.placements.next_unused_id());
                let kitty_id = id.or(assigned);
                let echo = kitty_id.unwrap_or(0);
                // The reply waits on the decode result: raw pixels are already decoded here and
                // answered at once; encoded data is answered by `image_decoded`. It echoes the
                // placement id an `a=T` frame carried.
                let place_pid = then_place.as_ref().and_then(|place| place.placement_id);
                let pending_reply = Some((echo, number, place_pid, quiet));
                match self.placements.new_image(kitty_id, number, width, height, pending_reply) {
                    Ok((key, events)) => {
                        self.send_image_events(events);
                        let data = match format {
                            kitty::KittyFormat::Png { .. } => ImageData::Encoded {
                                format: crate::event::ImageFormat::Png,
                                bytes,
                                width,
                                height,
                            },
                            kitty::KittyFormat::Rgb { .. } => {
                                ImageData::Rgba { width, height, pixels: rgb_to_rgba(&bytes) }
                            },
                            kitty::KittyFormat::Rgba { .. } => {
                                ImageData::Rgba { width, height, pixels: bytes }
                            },
                        };
                        let decoded_here = !matches!(format, kitty::KittyFormat::Png { .. });
                        self.event_proxy.send_event(Event::ImageReceived { key, data });
                        // Display is attempted before the answer goes out, so a failed placement
                        // can report its own error instead of `OK`.
                        let placed = match &then_place {
                            Some(place) => self.place_kitty_image(key, kitty_id, place, false),
                            None => Ok(()),
                        };
                        match (decoded_here, &placed) {
                            (true, Ok(())) => {
                                // Raw pixels need no work from the embedder: answer now.
                                let events = self.placements.image_decoded(key, Ok(()));
                                self.send_image_events(events);
                            },
                            (_, Err(error)) => {
                                let events = self.placements.fail_pending(key, *error);
                                self.send_image_events(events);
                            },
                            (false, Ok(())) => {}, // the PNG reply waits for `image_decoded`
                        }
                    },
                    Err(error) => self.send_kitty_reply(echo, number, place_pid, Err(error), quiet),
                }
            },
            kitty::KittyCommand::Place(place) => {
                match self.placements.resolve(place.id, place.number) {
                    Some(key) => {
                        let _ = self.place_kitty_image(key, place.id, &place, true);
                    },
                    None => self.send_kitty_reply(
                        place.id.unwrap_or(0),
                        place.number,
                        place.placement_id,
                        Err(kitty::KittyError::ENOENT),
                        place.quiet,
                    ),
                }
            },
            kitty::KittyCommand::Delete(delete) => {
                let (events, reply_to) = self.handle_kitty_delete(&delete);
                self.send_image_events(events);
                let result = if reply_to.found { Ok(()) } else { Err(kitty::KittyError::ENOENT) };
                self.send_kitty_reply(
                    reply_to.id,
                    reply_to.number,
                    reply_to.placement,
                    result,
                    delete.quiet,
                );
            },
            // The support query: `OK` unless a host turned image support off.
            kitty::KittyCommand::Query { id, quiet } => {
                self.send_kitty_reply(id, None, None, Ok(()), quiet);
            },
        }
    }

    /// Run one `a=d` selector against the placement table; returns the events and what a reply
    /// should echo.
    fn handle_kitty_delete(&mut self, delete: &kitty::KittyDelete) -> (Vec<Event>, DeleteReply) {
        let screen = self.active_screen();
        let window = self.grid_window(&self.grid);
        let offset = self.grid.display_offset() as i64;
        // `d=p`/`d=q`/`d=x`/`d=y` address viewport cells, 1-based; the view's top row is this
        // stable.
        let view_top = window.stable - offset;
        let free = delete.free_data;
        let mut events = Vec::new();
        let reply_to = match delete.target {
            kitty::DeleteTarget::All => {
                let found = self.placements.delete_all_in_view(
                    window,
                    screen,
                    self.grid.display_offset(),
                    free,
                    &mut events,
                );
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::ImageId { image, placement } => {
                let found =
                    image.and_then(|image| self.placements.resolve(Some(image), None)).is_some_and(
                        |key| self.placements.delete_image(key, placement, free, &mut events),
                    );
                DeleteReply { id: image.unwrap_or(0), number: None, placement, found }
            },
            kitty::DeleteTarget::Number { number, placement } => {
                let found = number.is_some_and(|number| {
                    self.placements.delete_number(number, placement, free, &mut events)
                });
                DeleteReply { id: 0, number, placement, found }
            },
            kitty::DeleteTarget::CursorCells => {
                let point = self.grid.cursor.point;
                let stable = window.stable + i64::from(point.line.0);
                let found = self.placements.delete_covering(
                    screen,
                    stable,
                    point.column.0,
                    None,
                    free,
                    &mut events,
                );
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::Cell { x, y } => {
                let stable = view_top + i64::from(y.saturating_sub(1));
                // feathercritty: saturating — the parser refuses zero coordinates for these
                // selectors now, and an unchecked subtraction here would panic in debug builds.
                let found = self.placements.delete_covering(
                    screen,
                    stable,
                    x.saturating_sub(1) as usize,
                    None,
                    free,
                    &mut events,
                );
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::CellZ { x, y, z } => {
                let stable = view_top + i64::from(y.saturating_sub(1));
                let found = self.placements.delete_covering(
                    screen,
                    stable,
                    x.saturating_sub(1) as usize,
                    Some(z),
                    free,
                    &mut events,
                );
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::Column(x) => {
                let found = self.placements.delete_column(
                    screen,
                    x.saturating_sub(1) as usize,
                    free,
                    &mut events,
                );
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::Row(y) => {
                let stable = view_top + i64::from(y.saturating_sub(1));
                let found = self.placements.delete_row(screen, stable, free, &mut events);
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::ZIndex(z) => {
                let found = self.placements.delete_z(screen, z, free, &mut events);
                DeleteReply::none(found)
            },
            kitty::DeleteTarget::IdRange { from, to } => {
                let found = self.placements.delete_id_range(from, to, free, &mut events);
                DeleteReply::none(found)
            },
        };
        (events, reply_to)
    }

    /// Display an image the table already has: `U=1` records a virtual placement (no grid entry);
    /// otherwise the image anchors at the cursor and scrolls to fit like text would.
    ///
    /// Returns the protocol outcome for the caller's reply; `reply` says whether this call owns
    /// the kitty answer (`a=p` does, an `a=T` frame answers through its transmission machinery).
    fn place_kitty_image(
        &mut self,
        key: ImageKey,
        kitty_id: Option<u32>,
        place: &kitty::KittyPlace,
        reply: bool,
    ) -> Result<(), kitty::KittyError>
    where
        T: EventListener,
    {
        let echo = place.id.unwrap_or(0);
        if place.virtual_ {
            // Placeholder cells reference virtual placements by image id, so one is required.
            // Over the cap `add_virtual` evicts the oldest; those releases ride out as events.
            let (created, eviction_events) = kitty_id.map_or((None, Vec::new()), |id| {
                self.placements.add_virtual(id, key, place.placement_id, place.cols, place.rows)
            });
            if !eviction_events.is_empty() {
                self.queue_image_events(eviction_events);
            }
            let result = match created {
                Some(_) => Ok(()),
                None => Err(kitty::KittyError::EINVAL),
            };
            if reply {
                self.send_kitty_reply(echo, place.number, place.placement_id, result, place.quiet);
            }
            return result;
        }
        let Some((width_px, height_px)) = self.placements.image_size(key) else {
            if reply {
                self.send_kitty_reply(
                    echo,
                    place.number,
                    place.placement_id,
                    Err(kitty::KittyError::ENOENT),
                    place.quiet,
                );
            }
            return Err(kitty::KittyError::ENOENT);
        };
        let (cols, rows) = self.placements.span_for(width_px, height_px, place.cols, place.rows);
        let col = self.grid.cursor.point.column.0;
        self.make_room(rows);
        let anchor = self.grid_window(&self.grid).stable + i64::from(self.grid.cursor.point.line.0);
        match self.placements.place(
            self.active_screen(),
            key,
            anchor,
            col,
            cols,
            rows,
            place.src,
            place.x_off,
            place.y_off,
            place.z,
            place.placement_id,
            false, // kitty keeps the aspect ratio: `c` with `r` letterboxes, never stretches
            None,  // the span is what the protocol asked for; no separate pixel box
        ) {
            Ok(events) => {
                self.send_image_events(events);
                if reply {
                    self.send_kitty_reply(
                        echo,
                        place.number,
                        place.placement_id,
                        Ok(()),
                        place.quiet,
                    );
                }
                if place.cursor_movement {
                    // `C=0`: the cursor lands on the first cell past the image's last row.
                    self.move_cursor_below_image(col, cols, rows, true);
                }
                Ok(())
            },
            Err(placements::PlaceError::NotFound) => {
                if reply {
                    self.send_kitty_reply(
                        echo,
                        place.number,
                        place.placement_id,
                        Err(kitty::KittyError::ENOENT),
                        place.quiet,
                    );
                }
                Err(kitty::KittyError::ENOENT)
            },
            Err(placements::PlaceError::Full) => {
                // Nothing could be evicted to make room; the image stays stored (a standalone
                // `a=p` leaves it so), and the caller answers ENOMEM.
                if reply {
                    self.send_kitty_reply(
                        echo,
                        place.number,
                        place.placement_id,
                        Err(kitty::KittyError::ENOMEM),
                        place.quiet,
                    );
                }
                Err(kitty::KittyError::ENOMEM)
            },
        }
    }

    /// Scroll the screen so an image of `rows` lines starting at the cursor line fits. Only a
    /// full-screen scroll region scrolls; inside a partial region the display is clipped at the
    /// region's bottom edge instead. When it does scroll, the cursor moves up with the content
    /// that rode through the rotation (clamped to the region top): the image's rows behave like
    /// `rows` newlines, so its anchor — taken from the cursor right after this call — lands on
    /// the line its content ends up on instead of staying at the viewport bottom while its
    /// earlier rows scroll into history for nothing.
    fn make_room(&mut self, rows: u32)
    where
        T: EventListener,
    {
        let overflow = i64::from(self.grid.cursor.point.line.0) + i64::from(rows)
            - i64::from(self.scroll_region.end.0);
        let full_region = self.scroll_region.start == Line(0)
            && self.scroll_region.end.0 as usize == self.screen_lines();
        if overflow > 0 && full_region {
            self.scroll_up_relative(Line(0), overflow as usize);

            // feathercritty: the cursor rides up with its line, exactly as a newline would
            // carry it when the screen scrolls under it. Not upstream.
            self.damage_cursor();
            let line = cmp::max(
                self.grid.cursor.point.line.0 - overflow as i32,
                self.scroll_region.start.0,
            );
            self.grid.cursor.point.line = Line(line);
            self.damage_cursor();
        }
    }

    /// The DECSDM-off sixel/inline rule puts the cursor on the line *below* the image. When
    /// the image's last row is the scroll region's bottom — the usual case at a prompt, where
    /// `make_room` scrolled to fit it — that line does not exist yet: scroll one more row, as
    /// the newline this placement stands in for would, so the cursor really ends below the
    /// image as it does mid-screen. The image's last row is its region bottom exactly when the
    /// cursor (left on the image's first row by `make_room`) plus `rows` names the row one
    /// past the region. Only a full-screen region scrolls, matching `make_room`.
    fn scroll_for_cursor_below_image(&mut self, rows: u32)
    where
        T: EventListener,
    {
        let full_region = self.scroll_region.start == Line(0)
            && self.scroll_region.end.0 as usize == self.screen_lines();
        if !full_region
            || i64::from(self.grid.cursor.point.line.0) + i64::from(rows)
                != i64::from(self.scroll_region.end.0)
        {
            return;
        }
        self.scroll_up_relative(Line(0), 1);
        self.damage_cursor();
        self.grid.cursor.point.line =
            cmp::max(self.grid.cursor.point.line - 1, self.scroll_region.start);
        self.damage_cursor();
    }

    /// Move the cursor past an image placed at viewport column `col` spanning `rows` lines.
    ///
    /// `advance_column` is the kitty rule (`C=0`: last row of the image, one cell past its last
    /// column, wrapping onto the next line); without it the cursor goes one line below the image in
    /// the same column — the DECSDM-off sixel and inline rule. Both clamp to the scroll region's
    /// bottom line.
    fn move_cursor_below_image(&mut self, col: usize, cols: u32, rows: u32, advance_column: bool) {
        // Arithmetic in i64 clamped to the scroll region's bottom line: a span request that
        // outgrows an i32 (a hostile `r`/`c` before placement clamps it) must not wrap negative.
        let bottom = i64::from(self.scroll_region.end.0 - 1);
        let below = i64::from(if advance_column { rows.saturating_sub(1) } else { rows });
        let mut line = (i64::from(self.grid.cursor.point.line.0) + below).clamp(0, bottom);
        let col = if advance_column {
            let columns = self.columns();
            let col = col.saturating_add(cols as usize);
            if col >= columns {
                line = (line + 1).min(bottom);
                col - columns
            } else {
                col
            }
        } else {
            col
        };
        self.damage_cursor();
        self.grid.cursor.point = Point::new(Line(line as i32), Column(col.min(self.columns() - 1)));
        self.grid.cursor.input_needs_wrap = false;
        self.damage_cursor();
    }

    /// Display a decoded sixel at the cursor. With DECSDM off (the default) the cursor ends below
    /// the image in the same column, as the standard specifies; set leaves it where it was.
    fn place_sixel(&mut self, image: sixel::SixelImage)
    where
        T: EventListener,
    {
        let (width, height) = (image.width, image.height);
        let Ok((key, events)) = self.placements.new_image(None, None, width, height, None) else {
            return; // no reply pending means the decode cap cannot trip; the image cap evicts first
        };
        self.send_image_events(events);
        let data = ImageData::Rgba { width, height, pixels: image.rgba };
        self.event_proxy.send_event(Event::ImageReceived { key, data });
        let Some((cols, rows)) = self.placements.image_span(key) else { return };
        let col = self.grid.cursor.point.column.0;
        self.make_room(rows);
        let anchor = self.grid_window(&self.grid).stable + i64::from(self.grid.cursor.point.line.0);
        match self.placements.place(
            self.active_screen(),
            key,
            anchor,
            col,
            cols,
            rows,
            None,
            0,
            0,
            0,
            None,
            false, // the span is the sixel's own pixels at the cell size; nothing asks to stretch
            None,  // the span is the sixel's own pixels at the cell size; no separate pixel box
        ) {
            Ok(events) => {
                self.send_image_events(events);
                if !self.mode.contains(TermMode::DECSDM) {
                    self.scroll_for_cursor_below_image(rows);
                    self.move_cursor_below_image(col, cols, rows, false);
                }
            },
            Err(_) => {
                // The table is full with nothing to evict; the image goes unused and releases.
                let events = self.placements.image_decoded(key, Ok(()));
                self.send_image_events(events);
            },
        }
    }

    /// Display an OSC 1337 inline image at the cursor, sized by its `width`/`height` arguments
    /// (cells, pixels or a percent of the terminal; `auto` is the image's own pixels divided by the
    /// cell size) and the DECSDM-off cursor rule. `preserveAspectRatio=0` reaches the host as
    /// `Placement::stretch` when both axes were given as sizes, so it fills the drawing box instead
    /// of letterboxing into it. When an axis was given in pixels or percent, the span is the
    /// rounded-up room the image occupies (which the cursor also counts), while
    /// `Placement::box_px` carries the exact pixel box the protocol asked for, so a host can draw
    /// the image at its requested size inside that room.
    fn place_inline(&mut self, image: inline::InlineImage)
    where
        T: EventListener,
    {
        let Some(format) = sniff_format(&image.bytes) else { return }; // the parser checked the header already
        let (width, height) = image.pixel_size;
        let (cw, ch) = self.placements.cell_pixel_size();
        let cols_req = inline_cells(image.width, cw, (self.columns() as u64 * cw as u64) as u32);
        let rows_req =
            inline_cells(image.height, ch, (self.screen_lines() as u64 * ch as u64) as u32);
        let Ok((key, events)) = self.placements.new_image(None, None, width, height, None) else {
            return;
        };
        self.send_image_events(events);
        // `auto` asks for 0 here, which `span_for` computes from the pixels (keeping the ratio
        // when only one axis was given); a failed placement releases the unused image.
        let (cols, rows) = self.placements.span_for(width, height, cols_req, rows_req);
        // feathercritty: the span rounds pixel and percent sizes up to whole cells, which is the
        // room the image occupies (and where the cursor lands) but not necessarily the size the
        // protocol asked for — `width=100px` in 18-pixel cells takes 6 cells and is drawn 108 px
        // wide if the host fills the span. The exact requested pixel box travels as `box_px`
        // instead, so a host can draw it at the requested size inside the room. It is expressed
        // in the cell size, so there is no box until the host has said what that is. Not
        // upstream.
        let box_px = self
            .cell_size_known
            .then(|| inline_box_px(self, &image, width, height, (cw, ch), (cols, rows)))
            .flatten();
        let data = ImageData::Encoded { format, bytes: image.bytes, width, height };
        self.event_proxy.send_event(Event::ImageReceived { key, data });
        // feathercritty: OSC 1337's `preserveAspectRatio=0` asks for the image to fill the drawing
        // box without its inherent ratio, which means something only when both axes are fixed by
        // the sender. With one axis `auto` — or a zero count, which `inline_cells` also turns into
        // "compute" — that axis comes from the ratio anyway, so there is nothing to fill and the
        // host letterboxes as it would with the ratio kept. Not upstream.
        let stretch = !image.preserve_aspect && cols_req != 0 && rows_req != 0;
        let col = self.grid.cursor.point.column.0;
        self.make_room(rows);
        let anchor = self.grid_window(&self.grid).stable + i64::from(self.grid.cursor.point.line.0);
        match self.placements.place(
            self.active_screen(),
            key,
            anchor,
            col,
            cols,
            rows,
            None,
            0,
            0,
            0,
            None,
            stretch, // true only for `preserveAspectRatio=0` with both axes given
            box_px,
        ) {
            Ok(events) => {
                self.send_image_events(events);
                if !self.mode.contains(TermMode::DECSDM) {
                    self.scroll_for_cursor_below_image(rows);
                    self.move_cursor_below_image(col, cols, rows, false);
                }
            },
            Err(_) => {
                let events = self.placements.image_decoded(key, Ok(()));
                self.send_image_events(events);
            },
        }
    }

    /// The text area's extent in physical pixels, `(width, height)`, from the cell size
    /// [`set_cell_pixel_size`](Self::set_cell_pixel_size) set and the grid's current size
    /// (feathercritty; not upstream). `u64` so a host with enormous reported metrics cannot
    /// overflow it.
    fn text_area_pixels(&self) -> (u64, u64) {
        let (cell_w, cell_h) = self.placements.cell_pixel_size();
        (
            (self.columns() as u64) * u64::from(cell_w),
            (self.screen_lines() as u64) * u64::from(cell_h),
        )
    }

    /// Set the physical pixel size of a text cell (feathercritty; not upstream).
    ///
    /// Placements convert pixel sizes to cell spans with this, so hosts that know their font
    /// metrics should set it before images arrive. Each axis is clamped to at least 1. It is also
    /// what `CSI 14 t` and `CSI 16 t` report; until it is set those ask the host instead (see
    /// [`Handler::text_area_size_pixels`](crate::vte::ansi::Handler::text_area_size_pixels)).
    #[inline]
    pub fn set_cell_pixel_size(&mut self, width: u32, height: u32) {
        self.placements.set_cell_pixel_size(width, height);
        self.cell_size_known = true;
    }

    /// Whether image support is on (feathercritty; not upstream; on by default).
    ///
    /// On, the terminal behaves like one with graphics support: DA1 advertises sixel, XTSMGRAPHICS
    /// reports the colour registers and the text area's pixel geometry, `CSI 14 t` and `CSI 16 t`
    /// report physical pixels, and kitty, sixel and OSC 1337 sequences display images.
    ///
    /// Off, every image sequence is consumed and discarded: no payload reaches the embedder, no
    /// placement is made, the kitty support query (`a=q`) answers `ENOTSUPPORTED` and every other
    /// completed kitty frame does too (subject to its quiet level), XTSMGRAPHICS requests answer
    /// with an error status, and DA1 leaves out the sixel bit. Switching off also drops everything
    /// already stored or displayed — placements, virtuals, ids and images, each image released
    /// exactly once — and abandons any transmission in progress; a decode result that arrives for
    /// one of those keys afterwards answers nothing, as after a full reset.
    ///
    /// The switch is the host's setting, not the program's: neither a full reset (`RIS`) nor
    /// [`set_options`](Self::set_options) changes it.
    pub fn set_images_enabled(&mut self, on: bool)
    where
        T: EventListener,
    {
        if self.images_enabled == on {
            return;
        }
        self.images_enabled = on;
        if on {
            return;
        }
        // Queued releases first: they name keys this is about to release too (see `reset_state`).
        let mut events = mem::take(&mut self.pending_image_events);
        self.placements.reset_all(&mut events);
        self.kitty_parser = Default::default();
        self.sixel_decoder = None;
        self.inline_parser = Default::default();
        for event in events {
            self.event_proxy.send_event(event);
        }
    }

    /// Whether image placements stay when they scroll into the history (feathercritty; not
    /// upstream).
    ///
    /// Off (the default) turns a placement whose rows have all left the viewport into a placeholder
    /// line (`[image 640×480]`, see [`Term::visible_placeholders`]) and releases its image. On
    /// keeps the placement so it scrolls back into view; [`Term::evict_oldest_scrollback_image`]
    /// then lets the embedder reclaim memory from the oldest one when it is over its own cap.
    #[inline]
    pub fn set_scrollback_images(&mut self, keep: bool) {
        self.placements.set_scrollback_images(keep);
    }

    /// The image placements intersecting the current view (feathercritty; not upstream).
    ///
    /// `row_in_view` counts from the top of the view (the scrollback offset is applied), negative
    /// when the placement starts above it. Draw in the returned order, filling a span or
    /// letterboxing into it as its `stretch` says (see `Placement::stretch`).
    pub fn visible_placements(&self) -> Vec<placements::VisiblePlacement> {
        let window = self.grid_window(&self.grid);
        self.placements.visible_placements(window, self.active_screen(), self.grid.display_offset())
    }

    /// The placeholder lines of scrolled-off images that are in view (feathercritty; not upstream).
    pub fn visible_placeholders(&self) -> Vec<placements::VisiblePlaceholder> {
        let window = self.grid_window(&self.grid);
        self.placements.visible_placeholders(
            window,
            self.active_screen(),
            self.grid.display_offset(),
        )
    }

    /// Resolve a kitty Unicode placeholder cell to its image (feathercritty; not upstream).
    ///
    /// Feed it the image id and placement id decoded from the printed cells
    /// (`kitty_diacritics::decode_placeholder_cell`); it returns the stored image's key and the
    /// virtual placement's cell span, or `None` when nothing matches.
    #[inline]
    pub fn kitty_virtual_image(
        &self,
        image_id: u32,
        placement_id: Option<u32>,
    ) -> Option<(ImageKey, u32, u32)> {
        self.placements.kitty_virtual_image(image_id, placement_id)
    }

    /// Report the embedder's decode result for an image (feathercritty; not upstream).
    ///
    /// Sends the kitty reply that was waiting on it (`OK`, `EBADPNG` or `ENOMEM`) and, when the
    /// decode failed, removes the image and its placements. Call for every `Event::ImageReceived`
    /// carrying `ImageData::Encoded`; `Rgba` payloads were decoded by the terminal already.
    pub fn image_decoded(&mut self, key: ImageKey, result: Result<(), ImageDecodeError>)
    where
        T: EventListener,
    {
        self.flush_image_events();
        let events = self.placements.image_decoded(key, result);
        self.send_image_events(events);
    }

    /// Run the graphics protocols' stall watchdog (feathercritty; not upstream).
    ///
    /// Drops a chunked kitty transmission that has not continued for ten seconds and answers it
    /// with `ENOMEM`, and delivers image events parked by [`resize`](Self::resize) — control-only
    /// output does not flush them, so this is what covers an idle terminal. Hosts call this
    /// periodically with the current time, or after feeding bytes.
    pub fn tick(&mut self, now: Instant)
    where
        T: EventListener,
    {
        self.flush_image_events();
        self.kitty_parser.tick(now);
        self.drain_kitty_reject();
    }

    /// Reclaim memory from the oldest placement in scrollback (feathercritty; not upstream).
    ///
    /// It becomes a placeholder line and its image is released outright — placements, id and number
    /// references and virtuals included — so an embedder over its own image-memory cap can call
    /// this until enough went. `None` when scrollback holds no placement.
    pub fn evict_oldest_scrollback_image(&mut self) -> Option<ImageKey>
    where
        T: EventListener,
    {
        let window = self.grid_window(&self.grid);
        let screen = self.active_screen();
        let (key, events) = self.placements.evict_oldest_scrollback_image(window, screen)?;
        self.send_image_events(events);
        Some(key)
    }

    /// Terminal content required for rendering.
    #[inline]
    pub fn renderable_content(&self) -> RenderableContent<'_>
    where
        T: EventListener,
    {
        RenderableContent::new(self)
    }

    /// Access to the raw grid data structure.
    pub fn grid(&self) -> &Grid<Cell> {
        &self.grid
    }

    /// The grid not shown: the normal screen while the alternate screen is active, and the
    /// other way around (fbterm patch: tab export).
    pub fn inactive_grid(&self) -> &Grid<Cell> {
        &self.inactive_grid
    }

    /// Mutable access to the raw grid data structure.
    pub fn grid_mut(&mut self) -> &mut Grid<Cell> {
        &mut self.grid
    }

    /// The grid `which` names, wherever it sits now (feathercritty).
    fn history_grid_mut(&mut self, which: HistoryGrid) -> &mut Grid<Cell> {
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        match (which, alt) {
            (HistoryGrid::Normal, false) | (HistoryGrid::Alternate, true) => &mut self.grid,
            _ => &mut self.inactive_grid,
        }
    }

    /// Whether [`Term::next_history_job`] may have a job (feathercritty: history compression,
    /// [`Config::history_compression`]). O(1): a host can check it after every batch of output.
    /// Always false with compression off and no compressed block left.
    pub fn has_history_jobs(&self) -> bool {
        self.grid.history().has_jobs() || self.inactive_grid.history().has_jobs()
    }

    /// The next history block that needs to be compressed or decompressed, with a copy of its
    /// bytes, or `None` (feathercritty). Taking a job changes nothing: until its result is
    /// installed, the next call offers the same block again. Run it with [`HistoryJob::run`],
    /// outside the lock, and install the result with [`Term::install_history_job`].
    pub fn next_history_job(&mut self) -> Option<HistoryJob> {
        for which in [HistoryGrid::Normal, HistoryGrid::Alternate] {
            let grid = self.history_grid_mut(which);
            if let Some(job) = grid.history_mut().next_job() {
                return Some(HistoryJob { grid: which, epoch: grid.history_epoch(), job });
            }
        }
        None
    }

    /// Up to `max` history blocks that need to be compressed or decompressed, oldest first, each
    /// with a copy of its bytes (feathercritty). Like [`Term::next_history_job`], taking jobs
    /// changes nothing; run each with [`HistoryJob::run`] outside the lock and install the
    /// results with [`Term::install_history_job`], in any order. A worker that takes a batch, and
    /// installs one batch and takes the next in a single lock hold, keeps up with output that
    /// holds the lock almost all the time; each block copied costs a few microseconds of that
    /// hold.
    pub fn next_history_jobs(&mut self, max: usize) -> Vec<HistoryJob> {
        let mut jobs = Vec::new();
        for which in [HistoryGrid::Normal, HistoryGrid::Alternate] {
            let grid = self.history_grid_mut(which);
            let epoch = grid.history_epoch();
            let more = grid.history_mut().next_jobs(max - jobs.len());
            jobs.extend(more.into_iter().map(|job| HistoryJob { grid: which, epoch, job }));
            if jobs.len() == max {
                break;
            }
        }
        jobs
    }

    /// Install the result of a [`HistoryJob`] (feathercritty). Returns whether it did: a result
    /// is refused, and dropped, when its grid's history was rebuilt since the job was taken (a
    /// column reflow) or its block changed, was evicted or cleared, or the setting changed
    /// meanwhile. Swapping the result in is cheap: it never compresses or decompresses.
    pub fn install_history_job(&mut self, done: HistoryDone) -> bool {
        let grid = self.history_grid_mut(done.grid);
        grid.history_epoch() == done.epoch && grid.history_mut().install(done.done)
    }

    /// Free the decompressed history blocks kept for reads (feathercritty: history compression;
    /// at most 4 blocks of 64 KiB per grid). A host calls it after a sweep of the history, such
    /// as a search or an export, so they do not stay resident. Reads afterwards decompress again.
    /// Does nothing when no block is cached, which is always the case with compression off.
    pub fn drop_history_cache(&mut self) {
        self.grid.history_mut().drop_read_cache();
        self.inactive_grid.history_mut().drop_read_cache();
    }

    /// Run and install every history job now (feathercritty: tests, and hosts that want no
    /// background work).
    pub fn run_history_jobs(&mut self) {
        self.grid.history_mut().run_jobs();
        self.inactive_grid.history_mut().run_jobs();
    }

    /// Resize terminal to new dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: S) {
        let old_cols = self.columns();
        let old_lines = self.screen_lines();

        let num_cols = size.columns();
        let num_lines = size.screen_lines();

        if old_cols == num_cols && old_lines == num_lines {
            debug!("Term::resize dimensions unchanged");
            return;
        }

        debug!("New num_cols is {num_cols} and num_lines is {num_lines}");

        // Move vi mode cursor with the content.
        let history_size = self.history_size();
        let mut delta = num_lines as i32 - old_lines as i32;
        let min_delta = cmp::min(0, num_lines as i32 - self.grid.cursor.point.line.0 - 1);
        delta = cmp::min(cmp::max(delta, min_delta), history_size as i32);
        self.vi_mode_cursor.point.line += delta;

        let is_alt = self.mode.contains(TermMode::ALT_SCREEN);

        // feathercritty: placements follow the resize. Each grid's anchors (their first cells)
        // go into `resize_tracked`, which replays the reflow's own decisions; outcomes move a
        // placement with its cell or, when it cannot follow, replace it with its placeholder
        // instead of dropping it silently (design spec 5). The alternate grid has no scroll
        // observer — nothing it does destroys content — so its anchors keep their cells through
        // the row passes and are shifted to their new absolute lines afterwards. Events have no
        // `EventListener` bound here, so they ride out to the next bounded operation. Not
        // upstream.
        let tracking = !self.placements.is_empty();
        let (active, inactive) = self.screen_pair();
        let grew_lines = num_lines > old_lines;
        // The inactive grid's own shrink scroll and history state, read before the resize:
        // whichever grid is inactive still runs its row passes without an observer.
        let alt_required =
            (self.inactive_grid.cursor.point.line.0 as usize + 1).saturating_sub(num_lines);
        let alt_history_full = self.inactive_grid.history_size() < alt_required;
        let (active_anchors, inactive_anchors): (
            Vec<placements::Placement>,
            Vec<placements::Placement>,
        ) = if tracking {
            (self.placements.anchor_list(active), self.placements.anchor_list(inactive))
        } else {
            (Vec::new(), Vec::new())
        };
        let mut active_outcomes = Vec::new();
        let mut inactive_outcomes = Vec::new();

        self.grid.resize_tracked(
            !is_alt,
            num_lines,
            num_cols,
            &active_anchors.iter().map(|p| (p.line, p.col)).collect::<Vec<_>>(),
            &mut active_outcomes,
        );
        self.inactive_grid.resize_tracked(
            is_alt,
            num_lines,
            num_cols,
            &inactive_anchors.iter().map(|p| (p.line, p.col)).collect::<Vec<_>>(),
            &mut inactive_outcomes,
        );

        // Invalidate selection and tabs only when necessary.
        if old_cols != num_cols {
            self.selection = None;

            // Recreate tabs list.
            self.tabs.resize(num_cols);
        } else if let Some(selection) = self.selection.take() {
            let max_lines = cmp::max(num_lines, old_lines) as i32;
            let range = Line(0)..Line(max_lines);
            self.selection = selection.rotate(self, &range, -delta);
        }

        // Clamp vi cursor to viewport.
        let vi_point = self.vi_mode_cursor.point;
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let viewport_bottom = viewport_top + self.bottommost_line();
        self.vi_mode_cursor.point.line =
            cmp::max(cmp::min(vi_point.line, viewport_bottom), viewport_top);
        self.vi_mode_cursor.point.column = cmp::min(vi_point.column, self.last_column());

        // Reset scrolling region.
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);

        // Resize damage information.
        self.damage.resize(num_cols, num_lines);

        if tracking {
            let mut events = Vec::new();

            // Alternate grid: rows pulled from (or pushed into) history slide the viewport, so
            // anchors outside it move by the count of lines that actually changed places;
            // growth ate scrollback for `num_lines - old_lines - from_history` rows only.
            if grew_lines {
                let from_history =
                    cmp::min(self.inactive_grid.history_size(), num_lines - old_lines);
                let shift = (num_lines - old_lines - from_history) as i64;
                self.placements.shift_anchored(
                    inactive,
                    i64::MIN..self.inactive_grid.scrolled_lines(),
                    shift,
                );
            } else if alt_required > 0 && !alt_history_full {
                // Only the rows that rode into history changed absolute position.
                let scrolled = self.inactive_grid.scrolled_lines();
                self.placements.shift_anchored(
                    inactive,
                    scrolled..i64::MAX,
                    -(alt_required as i64),
                );
            }

            // feathercritty: those shifts moved the inactive screen's records to their new
            // lines. `apply_reflow` looks each placement up where it sits now, so that
            // screen's anchors are re-read after the shifts — the copies collected before
            // the resize carry pre-shift lines, and pairing an outcome with one either
            // skipped a live record or inserted a stale copy beside it (which outlived
            // every removal and put `placement_total` out of step with the maps). Outcomes
            // come out in input order, so each is paired by insertion stamp: the one
            // identity a shift keeps. Not upstream.
            let shifted_inactive: HashMap<u64, placements::Placement> =
                self.placements.anchor_list(inactive).into_iter().map(|p| (p.seq, p)).collect();

            for (screen, anchors, outcomes) in [
                (active, &active_anchors, &active_outcomes),
                (inactive, &inactive_anchors, &inactive_outcomes),
            ] {
                let remapped: Vec<(placements::Placement, i64, usize, bool)> = anchors
                    .iter()
                    .copied()
                    .zip(outcomes.iter())
                    .map(|(p, &(line, col, lost))| {
                        let p = if screen == inactive {
                            shifted_inactive.get(&p.seq).copied().unwrap_or(p)
                        } else {
                            p
                        };
                        (p, line, col, lost)
                    })
                    .collect();
                self.placements.apply_reflow(&remapped, &mut events);
                self.placements.after_resize(
                    self.grid_window(if screen == active {
                        &self.grid
                    } else {
                        &self.inactive_grid
                    }),
                    screen,
                    num_cols,
                    &mut events,
                );
            }
            self.queue_image_events(events);
        }
    }

    /// Active terminal modes.
    #[inline]
    pub fn mode(&self) -> &TermMode {
        &self.mode
    }

    /// Swap primary and alternate screen buffer.
    pub fn swap_alt(&mut self)
    where
        T: EventListener,
    {
        // feathercritty: both transitions clear the alternate screen's placements — entering
        // starts a fresh alternate grid, leaving discards what was on it (the stored images stay
        // available while an id or number names them). Not upstream.
        if !self.placements.is_empty() {
            let mut events = Vec::new();
            self.placements.remove_screen(placements::Screen::Alternate, &mut events);
            self.send_image_events(events);
        }

        if !self.mode.contains(TermMode::ALT_SCREEN) {
            // Set alt screen cursor to the current primary screen cursor.
            self.inactive_grid.cursor = self.grid.cursor.clone();

            // Drop information about the primary screens saved cursor.
            self.grid.saved_cursor = self.grid.cursor.clone();

            // Reset alternate screen contents.
            self.inactive_grid.reset_region(..);
        }

        mem::swap(&mut self.keyboard_mode_stack, &mut self.inactive_keyboard_mode_stack);
        let keyboard_mode =
            self.keyboard_mode_stack.last().copied().unwrap_or(KeyboardModes::NO_MODE).into();
        self.set_keyboard_mode(keyboard_mode, KeyboardModesApplyBehavior::Replace);

        mem::swap(&mut self.grid, &mut self.inactive_grid);
        self.mode ^= TermMode::ALT_SCREEN;
        self.selection = None;
        self.mark_fully_damaged();
    }

    /// Scroll screen down.
    ///
    /// Text moves down; clear at bottom
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_down_relative(&mut self, origin: Line, mut lines: usize)
    where
        T: EventListener,
    {
        trace!("Scrolling down relative: origin={origin}, lines={lines}");

        lines = cmp::min(lines, (self.scroll_region.end - self.scroll_region.start).0 as usize);
        lines = cmp::min(lines, (self.scroll_region.end - origin).0 as usize);

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection =
            self.selection.take().and_then(|s| s.rotate(self, &region, -(lines as i32)));

        // Scroll vi mode cursor.
        let line = &mut self.vi_mode_cursor.point.line;
        if region.start <= *line && region.end > *line {
            *line = cmp::min(*line + lines, region.end - 1);
        }

        // Scroll between origin and bottom
        self.grid.scroll_down(&region, lines);

        // feathercritty: placements inside the rotated range move with their content; not upstream.
        if lines > 0 && !self.placements.is_empty() {
            let screen = self.active_screen();
            let window = self.grid_window(&self.grid);
            let events = self.placements.after_scroll_down(
                window,
                screen,
                region.start.0 as i64,
                region.end.0 as i64,
                lines,
            );
            self.send_image_events(events);
        }

        self.mark_fully_damaged();
    }

    /// Scroll screen up
    ///
    /// Text moves up; clear at top
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_up_relative(&mut self, origin: Line, mut lines: usize)
    where
        T: EventListener,
    {
        trace!("Scrolling up relative: origin={origin}, lines={lines}");

        lines = cmp::min(lines, (self.scroll_region.end - self.scroll_region.start).0 as usize);

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection = self.selection.take().and_then(|s| s.rotate(self, &region, lines as i32));

        self.grid.scroll_up(&region, lines);

        // feathercritty: placements inside a sub-region rotation move with their content; a
        // full-viewport one needs no shifts (its counter is the placement anchor's base) but does
        // scroll rows out of view. Not upstream.
        if lines > 0 && !self.placements.is_empty() {
            let screen = self.active_screen();
            let window = self.grid_window(&self.grid);
            let events = self.placements.after_scroll_up(
                window,
                screen,
                region.start.0 as i64,
                region.end.0 as i64,
                lines,
            );
            self.send_image_events(events);
        }

        // Scroll vi mode cursor.
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let top = if region.start == 0 { viewport_top } else { region.start };
        let line = &mut self.vi_mode_cursor.point.line;
        if (top <= *line) && region.end > *line {
            *line = cmp::max(*line - lines, top);
        }
        self.mark_fully_damaged();
    }

    fn deccolm(&mut self)
    where
        T: EventListener,
    {
        // Setting 132 column font makes no sense, but run the other side effects.
        // Clear scrolling region.
        self.set_scrolling_region(1, None);

        // Clear grid.
        self.grid.reset_region(..);
        self.mark_fully_damaged();
    }

    #[inline]
    pub fn exit(&mut self)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::Exit);
    }

    /// Toggle the vi mode.
    #[inline]
    pub fn toggle_vi_mode(&mut self)
    where
        T: EventListener,
    {
        self.mode ^= TermMode::VI;

        if self.mode.contains(TermMode::VI) {
            let display_offset = self.grid.display_offset() as i32;
            if self.grid.cursor.point.line > self.bottommost_line() - display_offset {
                // Move cursor to top-left if terminal cursor is not visible.
                let point = Point::new(Line(-display_offset), Column(0));
                self.vi_mode_cursor = ViModeCursor::new(point);
            } else {
                // Reset vi mode cursor position to match primary cursor.
                self.vi_mode_cursor = ViModeCursor::new(self.grid.cursor.point);
            }
        }

        // Update UI about cursor blinking state changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    /// Move vi mode cursor.
    #[inline]
    pub fn vi_motion(&mut self, motion: ViMotion)
    where
        T: EventListener,
    {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Move cursor.
        self.vi_mode_cursor = self.vi_mode_cursor.motion(self, motion);
        self.vi_mode_recompute_selection();
    }

    /// Move vi cursor to a point in the grid.
    #[inline]
    pub fn vi_goto_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        // Move viewport to make point visible.
        self.scroll_to_point(point);

        // Move vi cursor to the point.
        self.vi_mode_cursor.point = point;

        self.vi_mode_recompute_selection();
    }

    /// Update the active selection to match the vi mode cursor position.
    #[inline]
    fn vi_mode_recompute_selection(&mut self) {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Update only if non-empty selection is present.
        if let Some(selection) = self.selection.as_mut().filter(|s| !s.is_empty()) {
            selection.update(self.vi_mode_cursor.point, Side::Left);
            selection.include_all();
        }
    }

    /// Scroll display to point if it is outside of viewport.
    pub fn scroll_to_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        let display_offset = self.grid.display_offset() as i32;
        let screen_lines = self.grid.screen_lines() as i32;

        if point.line < -display_offset {
            let lines = point.line + display_offset;
            self.scroll_display(Scroll::Delta(-lines.0));
        } else if point.line >= (screen_lines - display_offset) {
            let lines = point.line + display_offset - screen_lines + 1i32;
            self.scroll_display(Scroll::Delta(-lines.0));
        }
    }

    /// Jump to the end of a wide cell.
    pub fn expand_wide(&self, mut point: Point, direction: Direction) -> Point {
        // feathercritty: upstream indexed `self.grid[...]` here and for `prev` below; both can be
        // history points, so they read through `Grid::cell`.
        let flags = self.grid.cell(point).flags;

        match direction {
            Direction::Right if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                point.column = Column(1);
                point.line += 1;
            },
            Direction::Right if flags.contains(Flags::WIDE_CHAR) => {
                point.column = cmp::min(point.column + 1, self.last_column());
            },
            Direction::Left if flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) => {
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    point.column -= 1;
                }

                let prev = point.sub(self, Boundary::Grid, 1);
                if self.grid.cell(prev).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    point = prev;
                }
            },
            _ => (),
        }

        point
    }

    #[inline]
    pub fn semantic_escape_chars(&self) -> &str {
        &self.config.semantic_escape_chars
    }

    #[cfg(test)]
    pub(crate) fn set_semantic_escape_chars(&mut self, semantic_escape_chars: &str) {
        self.config.semantic_escape_chars = semantic_escape_chars.into();
    }

    /// Active terminal cursor style.
    ///
    /// While vi mode is active, this will automatically return the vi mode cursor style.
    #[inline]
    pub fn cursor_style(&self) -> CursorStyle {
        let cursor_style = self.cursor_style.unwrap_or(self.config.default_cursor_style);

        if self.mode.contains(TermMode::VI) {
            self.config.vi_mode_cursor_style.unwrap_or(cursor_style)
        } else {
            cursor_style
        }
    }

    pub fn colors(&self) -> &Colors {
        &self.colors
    }

    /// Insert a linebreak at the current cursor position.
    #[inline]
    fn wrapline(&mut self)
    where
        T: EventListener,
    {
        if !self.mode.contains(TermMode::LINE_WRAP) {
            return;
        }

        trace!("Wrapping input");

        self.grid.cursor_cell().flags.insert(Flags::WRAPLINE);

        if self.grid.cursor.point.line + 1 >= self.scroll_region.end {
            self.linefeed();
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
        }

        self.grid.cursor.point.column = Column(0);
        self.grid.cursor.input_needs_wrap = false;
        self.damage_cursor();
    }

    /// Join `c` onto the cluster in the previous cell while mode 2027 is set, changing the
    /// cluster's width when `c` asks for it. Returns false when a grapheme boundary falls
    /// before `c` (or there is no cluster), so `c` is printed as usual.
    ///
    /// feathercritty: not upstream.
    fn join_cluster(&mut self, c: char) -> bool
    where
        T: EventListener,
    {
        let line = self.grid.cursor.point.line;
        let mut col = self.grid.cursor.point.column;
        if !self.grid.cursor.input_needs_wrap {
            col -= 1;
        }
        if col.0 > 0 && self.grid.screen()[line][col].flags.contains(Flags::WIDE_CHAR_SPACER) {
            col -= 1;
        }

        let cell = &self.grid.screen()[line][col];
        let pieces = cell.zerowidth().unwrap_or(&[]);
        // A space (blank or written, whatever its style) takes zero-width marks, as it does
        // without the mode, but never a visible joiner: that starts a cell of its own.
        let space = cell.c == ' ' && pieces.is_empty();
        if (space && c.width() != Some(0)) || !cluster::joins(cell.c, pieces, c) {
            return false;
        }
        if pieces.len() >= cluster::MAX_CLUSTER_PIECES {
            return true;
        }
        let (base, wide) = (cell.c, cell.flags.contains(Flags::WIDE_CHAR));

        self.grid.screen_mut()[line][col].push_zerowidth(c);
        self.damage.damage_line(line.0 as usize, col.0, col.0 + 1);
        match cluster::width_effect(base, c) {
            cluster::WidthEffect::Wide if !wide => self.widen_cluster(line, col),
            cluster::WidthEffect::Narrow if wide => self.narrow_cluster(line, col),
            _ => (),
        }
        true
    }

    /// Make the narrow cluster at `col` wide, with its spacer in the next column, and put the
    /// cursor after it.
    ///
    /// feathercritty: not upstream.
    fn widen_cluster(&mut self, line: Line, col: Column)
    where
        T: EventListener,
    {
        let columns = self.columns();
        // A one-column terminal has no room for a wide cluster anywhere: it stays narrow.
        if columns < 2 {
            return;
        }
        if col.0 + 1 >= columns {
            // No room in this row: like a wide character that does not fit, the cluster moves
            // whole to the start of the next line behind a leading spacer. Without line wrap
            // it stays narrow.
            if !self.mode.contains(TermMode::LINE_WRAP) {
                return;
            }
            let moved = self.grid.screen()[line][col].clone();
            self.grid.cursor.point.column = col;
            self.grid.cursor.template.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
            self.write_at_cursor(' ');
            self.grid.cursor.template.flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
            self.wrapline();

            // The cluster keeps its own colors, attributes, pieces and hyperlink, not the
            // current template's.
            self.write_at_cursor(moved.c);
            let line = self.grid.cursor.point.line;
            let cell = &mut self.grid.screen_mut()[line][Column(0)];
            let layout = Flags::WIDE_CHAR
                | Flags::WIDE_CHAR_SPACER
                | Flags::LEADING_WIDE_CHAR_SPACER
                | Flags::WRAPLINE;
            cell.fg = moved.fg;
            cell.bg = moved.bg;
            cell.flags = (cell.flags & layout) | (moved.flags & !layout);
            cell.extra = moved.extra.clone();
            self.widen_cluster(line, Column(0));
            return;
        }

        self.grid.screen_mut()[line][col].flags.insert(Flags::WIDE_CHAR);

        // In insert mode the spacer pushes the rest of the line right by one; a wide character
        // whose spacer falls off the end is cleared.
        if self.mode.contains(TermMode::INSERT) {
            let row = &mut self.grid.screen_mut()[line][..];
            for i in (col.0 + 1..columns - 1).rev() {
                row.swap(i + 1, i);
            }
            row[col.0 + 1] = Cell::default();
            if row[columns - 1].flags.contains(Flags::WIDE_CHAR) {
                row[columns - 1].clear_wide();
            }
            self.damage.damage_line(line.0 as usize, col.0, columns - 1);
        }

        self.grid.cursor.point.column = col + 1;
        self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR_SPACER);
        self.write_at_cursor(' ');
        self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR_SPACER);
        // The spacer takes the cluster's colors, as a wide character's spacer does.
        let row = &mut self.grid.screen_mut()[line];
        let (fg, bg) = (row[col].fg, row[col].bg);
        row[col + 1].fg = fg;
        row[col + 1].bg = bg;
        self.damage.damage_line(line.0 as usize, col.0, col.0 + 1);

        if col.0 + 2 < columns {
            self.grid.cursor.point.column = col + 2;
            self.grid.cursor.input_needs_wrap = false;
        } else {
            self.grid.cursor.input_needs_wrap = true;
        }
    }

    /// Make the wide cluster at `col` narrow, clearing its spacer, and put the cursor in the
    /// column after it.
    ///
    /// feathercritty: not upstream.
    fn narrow_cluster(&mut self, line: Line, col: Column) {
        let columns = self.columns();
        let last = Column(columns - 1);

        // A cluster that moved to this line because it was wide goes back to the end of the
        // line above, where a narrow one fits, with the wrap pending there.
        if col.0 == 0
            && line.0 > 0
            && self.grid.screen()[line - 1i32][last].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            let screen = self.grid.screen_mut();
            let mut cluster = screen[line][col].clone();
            cluster.flags.remove(Flags::WIDE_CHAR);
            let blank = Cell { bg: cluster.bg, ..Cell::default() };
            if columns > 1 {
                screen[line][Column(1)] = blank.clone();
            }
            screen[line][col] = blank;
            screen[line - 1i32][last] = cluster;
            self.damage.damage_line(line.0 as usize - 1, last.0, last.0);
            self.damage.damage_line(line.0 as usize, 0, 1);
            self.grid.cursor.point = Point::new(line - 1i32, last);
            self.grid.cursor.input_needs_wrap = true;
            return;
        }

        let screen = self.grid.screen_mut();
        screen[line][col].flags.remove(Flags::WIDE_CHAR);
        // A wide character in the last column has no spacer (the alternate screen does not
        // reflow, so a shrink can leave one there).
        if col.0 + 1 < columns {
            if self.mode.contains(TermMode::INSERT) {
                // In insert mode the spacer goes and the rest of the line moves back left,
                // undoing the shift that made room for it.
                let row = &mut screen[line][..];
                for i in col.0 + 1..columns - 1 {
                    row.swap(i, i + 1);
                }
                row[columns - 1] = Cell::default();
            } else {
                let spacer = &mut screen[line][col + 1];
                spacer.flags.remove(Flags::WIDE_CHAR_SPACER);
                spacer.c = ' ';
            }
            self.grid.cursor.point.column = col + 1;
            self.grid.cursor.input_needs_wrap = false;
        } else {
            self.grid.cursor.point.column = col;
            self.grid.cursor.input_needs_wrap = true;
        }
        self.damage.damage_line(line.0 as usize, col.0, columns - 1);
    }

    /// Write `c` to the cell at the cursor position.
    #[inline(always)]
    fn write_at_cursor(&mut self, c: char) {
        let c = self.grid.cursor.charsets[self.active_charset].map(c);
        let fg = self.grid.cursor.template.fg;
        let bg = self.grid.cursor.template.bg;
        let flags = self.grid.cursor.template.flags;
        let extra = self.grid.cursor.template.extra.clone();

        let mut cursor_cell = self.grid.cursor_cell();

        // Clear all related cells when overwriting a fullwidth cell.
        if cursor_cell.flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) {
            // Remove wide char and spacer.
            let wide = cursor_cell.flags.contains(Flags::WIDE_CHAR);
            let point = self.grid.cursor.point;
            // feathercritty: upstream indexed `self.grid[...]` for these writes; the cursor line is
            // on the screen, the line above it can be the newest history row.
            if wide && point.column < self.last_column() {
                self.grid.screen_mut()[point.line][point.column + 1]
                    .flags
                    .remove(Flags::WIDE_CHAR_SPACER);
            } else if point.column > 0 {
                self.grid.screen_mut()[point.line][point.column - 1].clear_wide();
            }

            // Remove leading spacers.
            //
            // feathercritty: the one write into history (design spec 5.5). With the cursor on
            // line 0 the line above is history line -1, changed through `History::update`.
            if point.column <= 1 && point.line != self.topmost_line() {
                let column = self.last_column();
                let line = point.line - 1i32;
                if line.0 >= 0 {
                    self.grid.screen_mut()[line][column]
                        .flags
                        .remove(Flags::LEADING_WIDE_CHAR_SPACER);
                } else {
                    self.grid.update_history_row(line, |row| {
                        row[column].flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
                    });
                }
            }

            cursor_cell = self.grid.cursor_cell();
        }

        cursor_cell.c = c;
        cursor_cell.fg = fg;
        cursor_cell.bg = bg;
        cursor_cell.flags = flags;
        cursor_cell.extra = extra;
    }

    #[inline]
    fn damage_cursor(&mut self) {
        // The normal cursor coordinates are always in viewport.
        let point =
            Point::new(self.grid.cursor.point.line.0 as usize, self.grid.cursor.point.column);
        self.damage.damage_point(point);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: TermMode, apply: KeyboardModesApplyBehavior) {
        let active_mode = self.mode & TermMode::KITTY_KEYBOARD_PROTOCOL;
        self.mode &= !TermMode::KITTY_KEYBOARD_PROTOCOL;
        let new_mode = match apply {
            KeyboardModesApplyBehavior::Replace => mode,
            KeyboardModesApplyBehavior::Union => active_mode.union(mode),
            KeyboardModesApplyBehavior::Difference => active_mode.difference(mode),
        };
        trace!("Setting keyboard mode to {new_mode:?}");
        self.mode |= new_mode;
    }
}

impl<T> Dimensions for Term<T> {
    #[inline]
    fn columns(&self) -> usize {
        self.grid.columns()
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.grid.screen_lines()
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }
}

/// What a delete reply echoes: the addressed `(id, number, placement)` from the command, plus
/// whether anything went (the difference between `OK` and `ENOENT`).
struct DeleteReply {
    id: u32,
    number: Option<u32>,
    placement: Option<u32>,
    found: bool,
}

impl DeleteReply {
    /// A reply for a selector that addressed no image.
    fn none(found: bool) -> DeleteReply {
        DeleteReply { id: 0, number: None, placement: None, found }
    }
}

/// Expand kitty's raw 3-bytes-per-pixel payload to the straight RGBA8 layout `ImageData::Rgba`
/// uses, with every pixel opaque (feathercritty; not upstream).
fn rgb_to_rgba(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 3);
    for pixel in bytes.chunks_exact(3) {
        out.extend_from_slice(pixel);
        out.push(0xFF);
    }
    out
}

/// The encoded format of inline image bytes by their magic numbers (feathercritty; not upstream).
fn sniff_format(bytes: &[u8]) -> Option<crate::event::ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(crate::event::ImageFormat::Png)
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some(crate::event::ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(crate::event::ImageFormat::Gif)
    } else {
        None
    }
}

/// An inline image's `width`/`height` argument in cells: `N` is already cells, `Npx` divides by
/// the cell size, `N%` takes that share of the terminal's pixels on that axis, and `auto` (and an
/// absent argument) asks for 0 — which [`PlacementTable::span_for`] computes from the image
/// itself (feathercritty; not upstream).
fn inline_cells(dim: inline::Dim, cell_px: u32, terminal_px: u32) -> u32 {
    // `span_for` treats 0 as "compute", so only a nonzero request is capped; the cap is the
    // image-side limit (`MAX_IMAGE_SIDE` cells), which no display could exceed anyway.
    let cap = |cells: u64| cells.clamp(1, u64::from(limits::MAX_IMAGE_SIDE)) as u32;
    match dim {
        inline::Dim::Auto => 0,
        // `span_for` treats 0 as "compute", so only a nonzero request is capped.
        inline::Dim::Cells(0) => 0,
        inline::Dim::Cells(cells) => cap(u64::from(cells)),
        inline::Dim::Pixels(pixels) => (u64::from(pixels) + u64::from(cell_px) - 1)
            .checked_div(u64::from(cell_px))
            .map_or(0, |cells| if cells == 0 { 0 } else { cap(cells) }),
        inline::Dim::Percent(percent) => {
            let pixels = u64::from(terminal_px) * u64::from(percent.min(100)) / 100;
            (pixels + u64::from(cell_px) - 1)
                .checked_div(u64::from(cell_px))
                .map_or(0, |cells| if cells == 0 { 0 } else { cap(cells) })
        },
    }
}

/// The exact pixel box an inline image's `width`/`height` asked for (feathercritty; not
/// upstream), or `None` when neither axis is in pixels or percent.
///
/// The request is in pixels or percent only when at least one axis is `Pixels(p)` or
/// `Percent(q)` — `None` otherwise, so cells-only and auto requests keep today's behaviour
/// exactly (the span is the only size that matters there). Once a request has a pixel or percent
/// axis, `(cw, ch)` (the cell pixel size), `(image_w, image_h)` (the image's own pixels) and
/// `(cols, rows)` (the span the request takes, as computed by
/// [`PlacementTable::span_for`](crate::graphics::placements::PlacementTable::span_for)) give the
/// box: `Pixels(p)` at `p`, `Percent(q)` at `terminal_px * min(q, 100) / 100` (the same integer
/// arithmetic `inline_cells` uses), and a `Cells(n)` axis at `n * cw` (or `n * ch`) — the cell
/// axis counts as a length only now that the request has a pixel or percent axis. An axis left
/// `auto` (or `0`) comes from the image's ratio instead, rounded to the nearest and at least 1.
/// Each side is then clamped to at least 1 and to the span's pixels, so the box never spills out
/// of the room the image occupies. Every product is `saturating` `u64`, so no input and no host
/// cell size can panic a debug build.
fn inline_box_px<T>(
    term: &Term<T>,
    image: &inline::InlineImage,
    image_w: u32,
    image_h: u32,
    (cw, ch): (u32, u32),
    (cols, rows): (u32, u32),
) -> Option<(u32, u32)> {
    // The request is in pixels or percent only when an axis says so; otherwise the span is the
    // only size that matters and there is no box.
    let in_pixels_or_percent =
        matches!(image.width, inline::Dim::Pixels(_) | inline::Dim::Percent(_))
            || matches!(image.height, inline::Dim::Pixels(_) | inline::Dim::Percent(_));
    if !in_pixels_or_percent {
        return None;
    }
    // The exact pixel lengths `inline_cells` rounds up to cells, one per axis: pixels stand on
    // their own, percent takes its share of the terminal's pixels on that axis (`min(q, 100)`,
    // as there), and a cell count is a length now that the request is in pixels or percent.
    fn pixel_len(dim: inline::Dim, cell_px: u32, terminal_px: u64) -> Option<u64> {
        match dim {
            inline::Dim::Auto | inline::Dim::Cells(0) => None,
            inline::Dim::Cells(n) => Some(u64::from(n).saturating_mul(u64::from(cell_px))),
            inline::Dim::Pixels(p) => Some(u64::from(p)),
            inline::Dim::Percent(percent) => {
                Some(terminal_px.saturating_mul(u64::from(percent.min(100))) / 100)
            },
        }
    }
    let (terminal_w, terminal_h) = (
        (term.columns() as u64 * cw as u64) as u32,
        (term.screen_lines() as u64 * ch as u64) as u32,
    );
    let (w, h) = (
        pixel_len(image.width, cw, u64::from(terminal_w)),
        pixel_len(image.height, ch, u64::from(terminal_h)),
    );
    // An axis left unset comes from the image's ratio in pixels, rounded to the nearest and at
    // least 1; the image's own pixels guard the divide.
    let (w, h) = match (w, h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let h = (w.saturating_mul(u64::from(image_h.max(1))) + u64::from(image_w) / 2)
                / u64::from(image_w.max(1));
            (w, h.max(1))
        },
        (None, Some(h)) => {
            let w = (h.saturating_mul(u64::from(image_w.max(1))) + u64::from(image_h) / 2)
                / u64::from(image_h.max(1));
            (w.max(1), h)
        },
        (None, None) => unreachable!("checked above"),
    };
    // The box sits inside the room the image occupies: neither side spills past the span, and
    // each keeps at least one pixel.
    let (w, h) = (
        w.clamp(1, u64::from(cols).saturating_mul(u64::from(cw))),
        h.clamp(1, u64::from(rows).saturating_mul(u64::from(ch))),
    );
    Some((w as u32, h as u32))
}

/// The XTSMGRAPHICS (`CSI ? Pi ; Pa ; Pv S`) items this terminal answers: `Pi=1` counts the colour
/// registers one sixel image gets, `Pi=2` is the sixel geometry in pixels. The protocol's third
/// item, ReGIS geometry, is not supported here (feathercritty; not upstream).
const XTSM_ITEM_COLOUR_REGISTERS: u16 = 1;
const XTSM_ITEM_SIXEL_GEOMETRY: u16 = 2;

/// The XTSMGRAPHICS actions (`Pa`) this terminal answers — `1` read, `2` reset to the default, `3`
/// set from `Pv`, `4` read the maximum allowed value (feathercritty; not upstream). Every item it
/// reports is a fixed capability, so all four answer with what the attribute already is.
const XTSM_ACTION_FIRST: u16 = 1;
const XTSM_ACTION_LAST: u16 = 4;

impl<T: EventListener> Handler for Term<T> {
    // feathercritty: graphics protocol sequences; not upstream. APC frames feed the kitty parser,
    // DCS `q` canvases the sixel decoder and OSC 1337 file transfers the inline image parser.

    #[inline]
    fn apc_start(&mut self) {
        self.flush_image_events();
        self.kitty_parser.start();
    }

    #[inline]
    fn apc_put(&mut self, bytes: &[u8]) {
        self.kitty_parser.put(bytes);
    }

    fn apc_end(&mut self) {
        if let Some(command) = self.kitty_parser.end(Instant::now()) {
            self.handle_kitty_command(command);
        }
        self.drain_kitty_reject();
    }

    #[inline]
    fn apc_abort(&mut self) {
        self.kitty_parser.abort();
        self.drain_kitty_reject();
    }

    fn sixel_start(&mut self, params: &[u16]) {
        self.flush_image_events();
        // feathercritty: with image support off the canvas is never built — `sixel_put` drops the
        // bytes and `sixel_end` has nothing to display. Not upstream.
        if !self.images_enabled {
            return;
        }
        // A black default background: the core does not know the host theme's colors, and
        // `P2=0|2` images paint their canvas anyway.
        self.sixel_decoder = Some(sixel::SixelDecoder::new(params, sixel::Rgba::new(0, 0, 0)));
    }

    #[inline]
    fn sixel_put(&mut self, bytes: &[u8]) {
        if let Some(decoder) = &mut self.sixel_decoder {
            decoder.put(bytes);
        }
    }

    fn sixel_end(&mut self) {
        if let Some(decoder) = self.sixel_decoder.take() {
            // A canvas that painted nothing displays nothing; no reply belongs to a sixel.
            if let Ok(image) = decoder.finish() {
                self.place_sixel(image);
            }
        }
    }

    #[inline]
    fn sixel_abort(&mut self) {
        self.sixel_decoder = None;
    }

    #[inline]
    fn inline_image_start(&mut self, args: &[u8]) {
        self.flush_image_events();
        // feathercritty: with image support off nothing is started, so the payload is consumed and
        // dropped by `put` without being collected. Not upstream.
        if !self.images_enabled {
            return;
        }
        self.inline_parser.start(args);
    }

    #[inline]
    fn inline_image_put(&mut self, bytes: &[u8]) {
        self.inline_parser.put(bytes);
    }

    fn inline_image_end(&mut self) {
        if let Some(image) = self.inline_parser.end() {
            self.place_inline(image);
        }
    }

    #[inline]
    fn inline_image_abort(&mut self) {
        self.inline_parser.abort();
    }

    /// XTSMGRAPHICS (`CSI ? Pi ; Pa ; Pv S`): the colour registers and the sixel geometry.
    ///
    /// Both are fixed capabilities here — the colour-register count the sixel decoder gives each
    /// image, and the text area in physical pixels capped at [`limits::MAX_IMAGE_SIDE`] per axis —
    /// so read, read-maximum, set and reset all answer success with what stands. Nothing is adopted
    /// from `Pv`, which is why the request's value goes unused; reporting a number the terminal did
    /// not take would claim a capability it does not have. An unknown item answers status 1 (error
    /// in `Pi`), an unknown action status 2 (error in `Pa`). With image support off
    /// ([`Term::set_images_enabled`]) every request answers status 1, without a value.
    fn graphics_attributes(&mut self, item: u16, action: u16, _value: Option<u16>) {
        trace!("XTSMGRAPHICS item {item}, action {action}");
        if !self.images_enabled {
            self.event_proxy.send_event(Event::PtyWrite(format!("\x1b[?{item};1S")));
            return;
        }
        let known_action = matches!(action, XTSM_ACTION_FIRST..=XTSM_ACTION_LAST);
        let reply = match item {
            // An action this terminal does not know, on an item it does: status 2 (error in `Pa`).
            XTSM_ITEM_COLOUR_REGISTERS | XTSM_ITEM_SIXEL_GEOMETRY if !known_action => {
                format!("\x1b[?{item};2S")
            },
            XTSM_ITEM_COLOUR_REGISTERS => format!("\x1b[?{item};0;{}S", sixel::REGISTERS),
            XTSM_ITEM_SIXEL_GEOMETRY => {
                let (width, height) = self.text_area_pixels();
                let cap = u64::from(limits::MAX_IMAGE_SIDE);
                format!("\x1b[?{item};0;{};{}S", width.min(cap), height.min(cap))
            },
            // An item this terminal does not have is status 1 (error in `Pi`), whatever it asked.
            _ => format!("\x1b[?{item};1S"),
        };
        self.event_proxy.send_event(Event::PtyWrite(reply));
    }

    /// A character to be displayed.
    #[inline(never)]
    fn input(&mut self, c: char) {
        // feathercritty: `resize` parks its placement events until a bounded operation runs; any
        // output counts as one. Not upstream.
        if !self.pending_image_events.is_empty() {
            self.flush_image_events();
        }

        // feathercritty: grapheme clusters while mode 2027 is set; not upstream. The fast path
        // (Latin-1, or column 0 without a pending wrap) never looks at the previous cell.
        if c > '\u{FF}' && self.mode.contains(TermMode::GRAPHEME_CLUSTER) {
            let cursor = &self.grid.cursor;
            if (cursor.point.column.0 > 0 || cursor.input_needs_wrap) && self.join_cluster(c) {
                return;
            }
            // A zero-width code point with no cluster to join is dropped.
            if c.width() == Some(0) {
                return;
            }
        }

        // Number of cells the char will occupy.
        let width = match c.width() {
            Some(width) => width,
            None => return,
        };

        // Handle zero-width characters.
        if width == 0 {
            // Get previous column.
            let mut column = self.grid.cursor.point.column;
            if !self.grid.cursor.input_needs_wrap {
                column.0 = column.saturating_sub(1);
            }

            // Put zerowidth characters over first fullwidth character cell.
            // feathercritty: upstream `self.grid[line]`; the cursor line is on the screen.
            let line = self.grid.cursor.point.line;
            if self.grid.screen()[line][column].flags.contains(Flags::WIDE_CHAR_SPACER) {
                column.0 = column.saturating_sub(1);
            }

            self.grid.screen_mut()[line][column].push_zerowidth(c);
            return;
        }

        // Move cursor to next line.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
        }

        // If in insert mode, first shift cells to the right.
        let columns = self.columns();
        if self.mode.contains(TermMode::INSERT) && self.grid.cursor.point.column + width < columns {
            let line = self.grid.cursor.point.line;
            let col = self.grid.cursor.point.column;
            // feathercritty: upstream `self.grid[line]`; the cursor line is on the screen.
            let row = &mut self.grid.screen_mut()[line][..];

            for col in (col.0..(columns - width)).rev() {
                row.swap(col + width, col);
            }
        }

        if width == 1 {
            self.write_at_cursor(c);
        } else {
            if self.grid.cursor.point.column + 1 >= columns {
                if self.mode.contains(TermMode::LINE_WRAP) {
                    // Insert placeholder before wide char if glyph does not fit in this row.
                    self.grid.cursor.template.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.write_at_cursor(' ');
                    self.grid.cursor.template.flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.wrapline();
                } else {
                    // Prevent out of bounds crash when linewrapping is disabled.
                    self.grid.cursor.input_needs_wrap = true;
                    return;
                }
            }

            // Write full width glyph to current cursor cell.
            self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR);
            self.write_at_cursor(c);
            self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR);

            // Write spacer to cell following the wide glyph.
            self.grid.cursor.point.column += 1;
            self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR_SPACER);
            self.write_at_cursor(' ');
            self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR_SPACER);
        }

        if self.grid.cursor.point.column + 1 < columns {
            self.grid.cursor.point.column += 1;
        } else {
            self.grid.cursor.input_needs_wrap = true;
        }
    }

    #[inline]
    fn decaln(&mut self) {
        trace!("Decalnning");

        for line in (0..self.screen_lines()).map(Line::from) {
            for column in 0..self.columns() {
                // feathercritty: upstream `self.grid[line]`; screen lines only.
                let cell = &mut self.grid.screen_mut()[line][Column(column)];
                *cell = Cell::default();
                cell.c = 'E';
            }
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn goto(&mut self, line: i32, col: usize) {
        let line = Line(line);
        let col = Column(col);

        trace!("Going to: line={line}, col={col}");
        let (y_offset, max_y) = if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_region.start, self.scroll_region.end - 1)
        } else {
            (Line(0), self.bottommost_line())
        };

        self.damage_cursor();
        self.grid.cursor.point.line = cmp::max(cmp::min(line + y_offset, max_y), Line(0));
        self.grid.cursor.point.column = cmp::min(col, self.last_column());
        self.damage_cursor();
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn goto_line(&mut self, line: i32) {
        trace!("Going to line: {line}");
        self.goto(line, self.grid.cursor.point.column.0)
    }

    #[inline]
    fn goto_col(&mut self, col: usize) {
        trace!("Going to column: {col}");
        self.goto(self.grid.cursor.point.line.0, col)
    }

    #[inline]
    fn insert_blank(&mut self, count: usize) {
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure inserting within terminal bounds
        let count = cmp::min(count, self.columns() - cursor.point.column.0);

        let source = cursor.point.column;
        let destination = cursor.point.column.0 + count;
        let num_cells = self.columns() - destination;

        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, 0, self.columns() - 1);

        // feathercritty: upstream `self.grid[line]`; the cursor line is on the screen.
        let row = &mut self.grid.screen_mut()[line][..];

        for offset in (0..num_cells).rev() {
            row.swap(destination + offset, source.0 + offset);
        }

        // Cells were just moved out toward the end of the line;
        // fill in between source and dest with blanks.
        for cell in &mut row[source.0..destination] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_up(&mut self, lines: usize) {
        trace!("Moving up: {lines}");

        let line = self.grid.cursor.point.line - lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_down(&mut self, lines: usize) {
        trace!("Moving down: {lines}");

        let line = self.grid.cursor.point.line + lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_forward(&mut self, cols: usize) {
        trace!("Moving forward: {cols}");
        let last_column = cmp::min(self.grid.cursor.point.column + cols, self.last_column());

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(cursor_line, self.grid.cursor.point.column.0, last_column.0);

        self.grid.cursor.point.column = last_column;
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn move_backward(&mut self, cols: usize) {
        trace!("Moving backward: {cols}");
        let column = self.grid.cursor.point.column.saturating_sub(cols);

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(cursor_line, column, self.grid.cursor.point.column.0);

        self.grid.cursor.point.column = Column(column);
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => {
                trace!("Reporting primary device attributes");
                // feathercritty: with image support on, the answer adds the sixel graphics bit
                // (`4`) and ANSI colour (`22`) to the VT500-class conformance level (`62`); off it
                // is what a terminal without graphics support says. Not upstream.
                let text = if self.images_enabled {
                    String::from("\x1b[?62;4;22c")
                } else {
                    String::from("\x1b[?6c")
                };
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            Some('>') => {
                trace!("Reporting secondary device attributes");
                let version = version_number(env!("CARGO_PKG_VERSION"));
                let text = format!("\x1b[>0;{version};1c");
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            _ => debug!("Unsupported device attributes intermediate"),
        }
    }

    #[inline]
    fn report_keyboard_mode(&mut self) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Reporting active keyboard mode");
        let current_mode =
            self.keyboard_mode_stack.last().unwrap_or(&KeyboardModes::NO_MODE).bits();
        let text = format!("\x1b[?{current_mode}u");
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    #[inline]
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Pushing `{mode:?}` keyboard mode into the stack");

        if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
            let removed = self.keyboard_mode_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of keyboard mode stack that exceeds its \
                 maximum depth"
            );
        }

        self.keyboard_mode_stack.push(mode);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Attempting to pop {to_pop} keyboard modes from the stack");
        let new_len = self.keyboard_mode_stack.len().saturating_sub(to_pop as usize);
        self.keyboard_mode_stack.truncate(new_len);

        // Reload active mode.
        let mode = self.keyboard_mode_stack.last().copied().unwrap_or(KeyboardModes::NO_MODE);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, apply: KeyboardModesApplyBehavior) {
        if !self.config.kitty_keyboard {
            return;
        }

        self.set_keyboard_mode(mode.into(), apply);
    }

    #[inline]
    fn device_status(&mut self, arg: usize) {
        trace!("Reporting device status: {arg}");
        match arg {
            5 => {
                let text = String::from("\x1b[0n");
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            6 => {
                let pos = self.grid.cursor.point;
                let text = format!("\x1b[{};{}R", pos.line + 1, pos.column + 1);
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            _ => debug!("unknown device status query: {arg}"),
        };
    }

    #[inline]
    fn move_down_and_cr(&mut self, lines: usize) {
        trace!("Moving down and cr: {lines}");

        let line = self.grid.cursor.point.line + lines;
        self.goto(line.0, 0)
    }

    #[inline]
    fn move_up_and_cr(&mut self, lines: usize) {
        trace!("Moving up and cr: {lines}");

        let line = self.grid.cursor.point.line - lines;
        self.goto(line.0, 0)
    }

    /// Insert tab at cursor position.
    #[inline]
    fn put_tab(&mut self, mut count: u16) {
        // A tab after the last column is the same as a linebreak.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
            return;
        }

        while self.grid.cursor.point.column < self.columns() && count != 0 {
            count -= 1;

            let c = self.grid.cursor.charsets[self.active_charset].map('\t');
            let cell = self.grid.cursor_cell();
            if cell.c == ' ' {
                cell.c = c;
            }

            loop {
                if (self.grid.cursor.point.column + 1) == self.columns() {
                    break;
                }

                self.grid.cursor.point.column += 1;

                if self.tabs[self.grid.cursor.point.column] {
                    break;
                }
            }
        }
    }

    /// Backspace.
    #[inline]
    fn backspace(&mut self) {
        trace!("Backspace");

        if self.grid.cursor.point.column > Column(0) {
            let line = self.grid.cursor.point.line.0 as usize;
            let column = self.grid.cursor.point.column.0;
            self.grid.cursor.point.column -= 1;
            self.grid.cursor.input_needs_wrap = false;
            self.damage.damage_line(line, column - 1, column);
        }
    }

    /// Carriage return.
    #[inline]
    fn carriage_return(&mut self) {
        trace!("Carriage return");
        let new_col = 0;
        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, new_col, self.grid.cursor.point.column.0);
        self.grid.cursor.point.column = Column(new_col);
        self.grid.cursor.input_needs_wrap = false;
    }

    /// Linefeed.
    #[inline]
    fn linefeed(&mut self) {
        trace!("Linefeed");
        let next = self.grid.cursor.point.line + 1;
        if next == self.scroll_region.end {
            self.scroll_up(1);
        } else if next < self.screen_lines() {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
            self.damage_cursor();
        }
    }

    /// Set current position as a tabstop.
    #[inline]
    fn bell(&mut self) {
        trace!("Bell");
        self.event_proxy.send_event(Event::Bell);
    }

    #[inline]
    fn substitute(&mut self) {
        trace!("[unimplemented] Substitute");
    }

    /// Run LF/NL.
    ///
    /// LF/NL mode has some interesting history. According to ECMA-48 4th
    /// edition, in LINE FEED mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause only movement of the active position in
    /// > the direction of the line progression.
    ///
    /// In NEW LINE mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause movement to the line home position on
    /// > the following line, the following form, etc. In the case of LF this is
    /// > referred to as the New Line (NL) option.
    ///
    /// Additionally, ECMA-48 4th edition says that this option is deprecated.
    /// ECMA-48 5th edition only mentions this option (without explanation)
    /// saying that it's been removed.
    ///
    /// As an emulator, we need to support it since applications may still rely
    /// on it.
    #[inline]
    fn newline(&mut self) {
        self.linefeed();

        if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        trace!("Setting horizontal tabstop");
        self.tabs[self.grid.cursor.point.column] = true;
    }

    #[inline]
    fn scroll_up(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_up_relative(origin, lines);
    }

    #[inline]
    fn scroll_down(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_down_relative(origin, lines);
    }

    #[inline]
    fn insert_blank_lines(&mut self, lines: usize) {
        trace!("Inserting blank {lines} lines");

        let origin = self.grid.cursor.point.line;
        if self.scroll_region.contains(&origin) {
            self.scroll_down_relative(origin, lines);
        }
    }

    #[inline]
    fn delete_lines(&mut self, lines: usize) {
        let origin = self.grid.cursor.point.line;
        let lines = cmp::min(self.screen_lines() - origin.0 as usize, lines);

        trace!("Deleting {lines} lines");

        if lines > 0 && self.scroll_region.contains(&origin) {
            self.scroll_up_relative(origin, lines);
        }
    }

    #[inline]
    fn erase_chars(&mut self, count: usize) {
        let cursor = &self.grid.cursor;

        trace!("Erasing chars: count={}, col={}", count, cursor.point.column);

        let start = cursor.point.column;
        let end = cmp::min(start + count, Column(self.columns()));

        // Cleared cells have current background color set.
        let bg = self.grid.cursor.template.bg;
        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, start.0, end.0);
        // feathercritty: upstream `self.grid[line]`; the cursor line is on the screen.
        let row = &mut self.grid.screen_mut()[line];
        for cell in &mut row[start..end] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn delete_chars(&mut self, count: usize) {
        let columns = self.columns();
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure deleting within terminal bounds.
        let count = cmp::min(count, columns);

        let start = cursor.point.column.0;
        let end = cmp::min(start + count, columns - 1);
        let num_cells = columns - end;

        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, 0, self.columns() - 1);
        // feathercritty: upstream `self.grid[line]`; the cursor line is on the screen.
        let row = &mut self.grid.screen_mut()[line][..];

        for offset in 0..num_cells {
            row.swap(start + offset, end + offset);
        }

        // Clear last `count` cells in the row. If deleting 1 char, need to delete
        // 1 cell.
        let end = columns - count;
        for cell in &mut row[end..] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_backward_tabs(&mut self, count: u16) {
        trace!("Moving backward {count} tabs");

        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == 0 {
                break;
            }

            for i in (0..(col.0)).rev() {
                if self.tabs[index::Column(i)] {
                    col = index::Column(i);
                    break;
                }
            }
            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, self.grid.cursor.point.column.0, old_col);
    }

    #[inline]
    fn move_forward_tabs(&mut self, count: u16) {
        trace!("Moving forward {count} tabs");

        let num_cols = self.columns();
        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == num_cols - 1 {
                break;
            }

            for i in col.0 + 1..num_cols {
                col = index::Column(i);
                if self.tabs[col] {
                    break;
                }
            }

            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, old_col, self.grid.cursor.point.column.0);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        trace!("Saving cursor position");

        self.grid.saved_cursor = self.grid.cursor.clone();
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        trace!("Restoring cursor position");

        self.damage_cursor();
        self.grid.cursor = self.grid.saved_cursor.clone();
        self.damage_cursor();
    }

    #[inline]
    fn clear_line(&mut self, mode: ansi::LineClearMode) {
        trace!("Clearing line: {mode:?}");

        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;
        let point = cursor.point;

        let (left, right) = match mode {
            ansi::LineClearMode::Right if cursor.input_needs_wrap => return,
            ansi::LineClearMode::Right => (point.column, Column(self.columns())),
            ansi::LineClearMode::Left => (Column(0), point.column + 1),
            ansi::LineClearMode::All => (Column(0), Column(self.columns())),
        };

        self.damage.damage_line(point.line.0 as usize, left.0, right.0 - 1);

        // feathercritty: upstream `self.grid[point.line]`; the cursor line is on the screen.
        let row = &mut self.grid.screen_mut()[point.line];
        for cell in &mut row[left..right] {
            *cell = bg.into();
        }
        // feathercritty: upstream left `occ` at `right` (the range write raises it). When the
        // erase reaches the end of the line every cell from `left` on is now `bg.into()`, so
        // `occ` drops to `left`: a row erased with a background color (`CSI 41 m CSI 2 K`) then
        // scrolls into history without the encoder comparing every cell (see
        // `Row::set_equal_from`). Nothing visible changes. When merging upstream: if this loop
        // changes, keep the call only where every cell from `left` to the end gets the same reset
        // cell.
        if right.0 == row.len() {
            row.set_equal_from(left.0);
        }

        let range = self.grid.cursor.point.line..=self.grid.cursor.point.line;
        self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
    }

    /// Set the indexed color value.
    #[inline]
    fn set_color(&mut self, index: usize, color: Rgb) {
        trace!("Setting color[{index}] = {color:?}");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index] != Some(color) {
            self.mark_fully_damaged();
        }

        self.colors[index] = Some(color);
    }

    /// Respond to a color query escape sequence.
    #[inline]
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        trace!("Requested write of escape sequence for color code {prefix}: color[{index}]");

        let terminator = terminator.to_owned();
        self.event_proxy.send_event(Event::ColorRequest(
            index,
            Arc::new(move |color| {
                format!(
                    "\x1b]{};rgb:{1:02x}{1:02x}/{2:02x}{2:02x}/{3:02x}{3:02x}{4}",
                    prefix, color.r, color.g, color.b, terminator
                )
            }),
        ));
    }

    /// Reset the indexed color to original value.
    #[inline]
    fn reset_color(&mut self, index: usize) {
        trace!("Resetting color[{index}]");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index].is_some() {
            self.mark_fully_damaged();
        }

        self.colors[index] = None;
    }

    /// Store data into clipboard.
    #[inline]
    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        if !matches!(self.config.osc52, Osc52::OnlyCopy | Osc52::CopyPaste) {
            debug!("Denied osc52 store");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        if let Ok(bytes) = Base64.decode(base64) {
            if let Ok(text) = String::from_utf8(bytes) {
                self.event_proxy.send_event(Event::ClipboardStore(clipboard_type, text));
            }
        }
    }

    /// Load data from clipboard.
    #[inline]
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        if !matches!(self.config.osc52, Osc52::OnlyPaste | Osc52::CopyPaste) {
            debug!("Denied osc52 load");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        let terminator = terminator.to_owned();

        self.event_proxy.send_event(Event::ClipboardLoad(
            clipboard_type,
            Arc::new(move |text| {
                let base64 = Base64.encode(text);
                format!("\x1b]52;{};{}{}", clipboard as char, base64, terminator)
            }),
        ));
    }

    #[inline]
    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        trace!("Clearing screen: {mode:?}");
        let bg = self.grid.cursor.template.bg;

        let screen_lines = self.screen_lines();

        match mode {
            ansi::ClearMode::Above => {
                let cursor = self.grid.cursor.point;

                // If clearing more than one line.
                if cursor.line > 1 {
                    // Fully clear all lines before the current line.
                    self.grid.reset_region(..cursor.line);
                }

                // Clear up to the current column in the current line.
                let end = cmp::min(cursor.column + 1, Column(self.columns()));
                // feathercritty: upstream `self.grid[cursor.line]`; the cursor is on the screen.
                for cell in &mut self.grid.screen_mut()[cursor.line][..end] {
                    *cell = bg.into();
                }

                let range = Line(0)..=cursor.line;
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));

                // feathercritty: placements anchored in the cleared rows are removed. The clear
                // concerns the screen, not the scrolled-back view: `reset_region` only ever
                // touches grid rows `0..=cursor.line`, so the wipe starts at the viewport's top
                // stable line, never earlier (`display_offset` rows of history stay). Not upstream.
                if !self.placements.is_empty() {
                    let window = self.grid_window(&self.grid);
                    let from = window.stable;
                    let to = window.stable + i64::from(cursor.line.0) + 1;
                    let mut events = Vec::new();
                    self.placements.wipe_range(self.active_screen(), from, to, &mut events);
                    self.send_image_events(events);
                }
            },
            ansi::ClearMode::Below => {
                let cursor = self.grid.cursor.point;
                // feathercritty: upstream `self.grid[cursor.line]`; the cursor is on the screen.
                for cell in &mut self.grid.screen_mut()[cursor.line][cursor.column..] {
                    *cell = bg.into();
                }

                if (cursor.line.0 as usize) < screen_lines - 1 {
                    self.grid.reset_region((cursor.line + 1)..);
                }

                let range = cursor.line..Line(screen_lines as i32);
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));

                // feathercritty: placements anchored below the cursor's row are removed; one
                // anchored on it survives its tail being erased (the documented anchor rule). Not
                // upstream.
                if !self.placements.is_empty() {
                    let window = self.grid_window(&self.grid);
                    let from = window.stable + i64::from(cursor.line.0) + 1;
                    let to = window.stable + screen_lines as i64;
                    let mut events = Vec::new();
                    self.placements.wipe_range(self.active_screen(), from, to, &mut events);
                    self.send_image_events(events);
                }
            },
            ansi::ClearMode::All => {
                // feathercritty: an ED 2 erases every placement anchored in the viewport (the
                // rows they covered are gone even where their content scrolls on); scrollback
                // placements keep their stable lines, as their rows do. Not upstream.
                let wipe_from = self.grid.scrolled_lines();
                let mut events = Vec::new();
                if !self.placements.is_empty() {
                    self.placements.wipe_range(
                        self.active_screen(),
                        wipe_from,
                        wipe_from + screen_lines as i64,
                        &mut events,
                    );
                }

                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.grid.reset_region(..);
                } else {
                    let old_offset = self.grid.display_offset();

                    self.grid.clear_viewport();

                    // Compute number of lines scrolled by clearing the viewport.
                    let lines = self.grid.display_offset().saturating_sub(old_offset);

                    self.vi_mode_cursor.point.line =
                        (self.vi_mode_cursor.point.line - lines).grid_clamp(self, Boundary::Grid);

                    // feathercritty: riding the viewport into history may have pushed the oldest
                    // rows past the scrollback limit; placements that fell off the end go with
                    // them — the same prune a scrolling rotation runs. Not upstream.
                    if !self.placements.is_empty() {
                        let window = self.grid_window(&self.grid);
                        self.placements.prune_out_of_view(
                            window,
                            self.active_screen(),
                            &mut events,
                        );
                    }
                }

                self.selection = None;

                if !events.is_empty() {
                    self.send_image_events(events);
                }
            },
            ansi::ClearMode::Saved if self.history_size() > 0 => {
                // feathercritty: rows purged from history take their placements (and placeholders)
                // with them; everything below the viewport's new top line is gone. Not upstream.
                let purge_to = self.grid.scrolled_lines();
                let mut events = Vec::new();
                if !self.placements.is_empty() {
                    self.placements.wipe_range(
                        self.active_screen(),
                        purge_to - (self.history_size() as i64 + 1),
                        purge_to,
                        &mut events,
                    );
                }

                self.grid.clear_history();

                self.vi_mode_cursor.point.line =
                    self.vi_mode_cursor.point.line.grid_clamp(self, Boundary::Cursor);

                self.selection = self.selection.take().filter(|s| !s.intersects_range(..Line(0)));

                if !events.is_empty() {
                    self.send_image_events(events);
                }
            },
            // We have no history to clear.
            ansi::ClearMode::Saved => (),
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn clear_tabs(&mut self, mode: ansi::TabulationClearMode) {
        trace!("Clearing tabs: {mode:?}");
        match mode {
            ansi::TabulationClearMode::Current => {
                self.tabs[self.grid.cursor.point.column] = false;
            },
            ansi::TabulationClearMode::All => {
                self.tabs.clear_all();
            },
        }
    }

    /// Reset all important fields in the term struct.
    #[inline]
    fn reset_state(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            mem::swap(&mut self.grid, &mut self.inactive_grid);
        }
        self.active_charset = Default::default();
        self.cursor_style = None;
        self.grid.reset();
        self.inactive_grid.reset();
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);
        self.tabs = TabStops::new(self.columns());
        self.title_stack = Vec::new();
        self.title = None;
        self.selection = None;
        self.vi_mode_cursor = Default::default();
        self.keyboard_mode_stack = Default::default();
        self.inactive_keyboard_mode_stack = Default::default();

        // Preserve vi mode across resets.
        self.mode &= TermMode::VI;
        self.mode.insert(TermMode::default());
        // feathercritty: mode 2027 returns to its configured default; not upstream.
        self.reset_grapheme_cluster_mode();

        // feathercritty: images, ids, placements and placeholders do not survive a full reset;
        // every stored image reports its release exactly once (queued `resize` events first — they
        // concern keys the reset is about to release too). Not upstream.
        let mut graphic_events = mem::take(&mut self.pending_image_events);
        self.placements.reset_all(&mut graphic_events);
        self.kitty_parser = Default::default();
        self.sixel_decoder = None;
        self.inline_parser = Default::default();
        for event in graphic_events {
            self.event_proxy.send_event(event);
        }

        self.event_proxy.send_event(Event::CursorBlinkingChange);
        self.mark_fully_damaged();
    }

    #[inline]
    fn reverse_index(&mut self) {
        trace!("Reversing index");
        // If cursor is at the top.
        if self.grid.cursor.point.line == self.scroll_region.start {
            self.scroll_down(1);
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line = cmp::max(self.grid.cursor.point.line - 1, Line(0));
            self.damage_cursor();
        }
    }

    #[inline]
    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        trace!("Setting hyperlink: {hyperlink:?}");
        self.grid.cursor.template.set_hyperlink(hyperlink.map(|e| e.into()));
    }

    /// Set a terminal attribute.
    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        trace!("Setting attribute: {attr:?}");
        let cursor = &mut self.grid.cursor;
        match attr {
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::UnderlineColor(color) => cursor.template.set_underline_color(color),
            Attr::Reset => {
                cursor.template.fg = Color::Named(NamedColor::Foreground);
                cursor.template.bg = Color::Named(NamedColor::Background);
                cursor.template.flags = Flags::empty();
                cursor.template.set_underline_color(None);
            },
            Attr::Reverse => cursor.template.flags.insert(Flags::INVERSE),
            Attr::CancelReverse => cursor.template.flags.remove(Flags::INVERSE),
            Attr::Bold => cursor.template.flags.insert(Flags::BOLD),
            Attr::CancelBold => cursor.template.flags.remove(Flags::BOLD),
            Attr::Dim => cursor.template.flags.insert(Flags::DIM),
            Attr::CancelBoldDim => cursor.template.flags.remove(Flags::BOLD | Flags::DIM),
            Attr::Italic => cursor.template.flags.insert(Flags::ITALIC),
            Attr::CancelItalic => cursor.template.flags.remove(Flags::ITALIC),
            Attr::Underline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERLINE);
            },
            Attr::DoubleUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOUBLE_UNDERLINE);
            },
            Attr::Undercurl => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERCURL);
            },
            Attr::DottedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOTTED_UNDERLINE);
            },
            Attr::DashedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DASHED_UNDERLINE);
            },
            Attr::CancelUnderline => cursor.template.flags.remove(Flags::ALL_UNDERLINES),
            Attr::Hidden => cursor.template.flags.insert(Flags::HIDDEN),
            Attr::CancelHidden => cursor.template.flags.remove(Flags::HIDDEN),
            Attr::Strike => cursor.template.flags.insert(Flags::STRIKEOUT),
            Attr::CancelStrike => cursor.template.flags.remove(Flags::STRIKEOUT),
            _ => {
                debug!("Term got unhandled attr: {attr:?}");
            },
        }
    }

    #[inline]
    fn set_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            // feathercritty: vte has no named variant for mode 2027; not upstream.
            PrivateMode::Unknown(2027) => {
                self.mode.insert(TermMode::GRAPHEME_CLUSTER);
                return;
            },
            // feathercritty: DECSDM, sixel images at (not under) the cursor; not upstream.
            PrivateMode::Unknown(80) => {
                self.mode.insert(TermMode::DECSDM);
                return;
            },
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_private_mode");
                return;
            },
        };

        trace!("Setting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.insert(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            NamedPrivateMode::ShowCursor => self.mode.insert(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.insert(TermMode::APP_CURSOR),
            // Mouse protocols are mutually exclusive.
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportFocusInOut => self.mode.insert(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.insert(TermMode::BRACKETED_PASTE),
            // Mouse encodings are mutually exclusive.
            NamedPrivateMode::SgrMouse => {
                self.mode.remove(TermMode::UTF8_MOUSE);
                self.mode.insert(TermMode::SGR_MOUSE);
            },
            NamedPrivateMode::Utf8Mouse => {
                self.mode.remove(TermMode::SGR_MOUSE);
                self.mode.insert(TermMode::UTF8_MOUSE);
            },
            NamedPrivateMode::AlternateScroll => self.mode.insert(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.insert(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => {
                self.mode.insert(TermMode::ORIGIN);
                self.goto(0, 0);
            },
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                style.blinking = true;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            },
            NamedPrivateMode::SyncUpdate => (),
        }
    }

    #[inline]
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            // feathercritty: vte has no named variant for mode 2027; not upstream.
            PrivateMode::Unknown(2027) => {
                self.mode.remove(TermMode::GRAPHEME_CLUSTER);
                return;
            },
            // feathercritty: DECSDM, sixel images at (not under) the cursor; not upstream.
            PrivateMode::Unknown(80) => {
                self.mode.remove(TermMode::DECSDM);
                return;
            },
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_private_mode");
                return;
            },
        };

        trace!("Unsetting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.remove(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            NamedPrivateMode::ShowCursor => self.mode.remove(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.remove(TermMode::APP_CURSOR),
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportFocusInOut => self.mode.remove(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.remove(TermMode::BRACKETED_PASTE),
            NamedPrivateMode::SgrMouse => self.mode.remove(TermMode::SGR_MOUSE),
            NamedPrivateMode::Utf8Mouse => self.mode.remove(TermMode::UTF8_MOUSE),
            NamedPrivateMode::AlternateScroll => self.mode.remove(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.remove(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => self.mode.remove(TermMode::ORIGIN),
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                style.blinking = false;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            },
            NamedPrivateMode::SyncUpdate => (),
        }
    }

    #[inline]
    fn report_private_mode(&mut self, mode: PrivateMode) {
        trace!("Reporting private mode {mode:?}");
        let state = match mode {
            PrivateMode::Named(mode) => match mode {
                NamedPrivateMode::CursorKeys => self.mode.contains(TermMode::APP_CURSOR).into(),
                NamedPrivateMode::Origin => self.mode.contains(TermMode::ORIGIN).into(),
                NamedPrivateMode::LineWrap => self.mode.contains(TermMode::LINE_WRAP).into(),
                NamedPrivateMode::BlinkingCursor => {
                    let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                    style.blinking.into()
                },
                NamedPrivateMode::ShowCursor => self.mode.contains(TermMode::SHOW_CURSOR).into(),
                NamedPrivateMode::ReportMouseClicks => {
                    self.mode.contains(TermMode::MOUSE_REPORT_CLICK).into()
                },
                NamedPrivateMode::ReportCellMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_DRAG).into()
                },
                NamedPrivateMode::ReportAllMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_MOTION).into()
                },
                NamedPrivateMode::ReportFocusInOut => {
                    self.mode.contains(TermMode::FOCUS_IN_OUT).into()
                },
                NamedPrivateMode::Utf8Mouse => self.mode.contains(TermMode::UTF8_MOUSE).into(),
                NamedPrivateMode::SgrMouse => self.mode.contains(TermMode::SGR_MOUSE).into(),
                NamedPrivateMode::AlternateScroll => {
                    self.mode.contains(TermMode::ALTERNATE_SCROLL).into()
                },
                NamedPrivateMode::UrgencyHints => {
                    self.mode.contains(TermMode::URGENCY_HINTS).into()
                },
                NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                    self.mode.contains(TermMode::ALT_SCREEN).into()
                },
                NamedPrivateMode::BracketedPaste => {
                    self.mode.contains(TermMode::BRACKETED_PASTE).into()
                },
                NamedPrivateMode::SyncUpdate => ModeState::Reset,
                NamedPrivateMode::ColumnMode => ModeState::NotSupported,
            },
            // feathercritty: mode 2027 is reported; not upstream.
            PrivateMode::Unknown(2027) => self.mode.contains(TermMode::GRAPHEME_CLUSTER).into(),
            PrivateMode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[?{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_mode");
                return;
            },
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => self.mode.insert(TermMode::INSERT),
            NamedMode::LineFeedNewLine => self.mode.insert(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn unset_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_mode");
                return;
            },
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => {
                self.mode.remove(TermMode::INSERT);
                self.mark_fully_damaged();
            },
            NamedMode::LineFeedNewLine => self.mode.remove(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn report_mode(&mut self, mode: ansi::Mode) {
        trace!("Reporting mode {mode:?}");
        let state = match mode {
            ansi::Mode::Named(mode) => match mode {
                NamedMode::Insert => self.mode.contains(TermMode::INSERT).into(),
                NamedMode::LineFeedNewLine => {
                    self.mode.contains(TermMode::LINE_FEED_NEW_LINE).into()
                },
            },
            ansi::Mode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        // Fallback to the last line as default.
        let bottom = bottom.unwrap_or_else(|| self.screen_lines());

        if top >= bottom {
            debug!("Invalid scrolling region: ({top};{bottom})");
            return;
        }

        // Bottom should be included in the range, but range end is not
        // usually included. One option would be to use an inclusive
        // range, but instead we just let the open range end be 1
        // higher.
        let start = Line(top as i32 - 1);
        let end = Line(bottom as i32);

        trace!("Setting scrolling region: ({start};{end})");

        let screen_lines = Line(self.screen_lines() as i32);
        self.scroll_region.start = cmp::min(start, screen_lines);
        self.scroll_region.end = cmp::min(end, screen_lines);
        self.goto(0, 0);
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        trace!("Setting keypad application mode");
        self.mode.insert(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        trace!("Unsetting keypad application mode");
        self.mode.remove(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        trace!("Configuring charset {index:?} as {charset:?}");
        self.grid.cursor.charsets[index] = charset;
    }

    #[inline]
    fn set_active_charset(&mut self, index: CharsetIndex) {
        trace!("Setting active charset {index:?}");
        self.active_charset = index;
    }

    #[inline]
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        trace!("Setting cursor style {style:?}");
        self.cursor_style = style;

        // Notify UI about blinking changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    #[inline]
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        trace!("Setting cursor shape {shape:?}");

        let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
        style.shape = shape;
    }

    #[inline]
    fn set_title(&mut self, title: Option<String>) {
        trace!("Setting title to '{title:?}'");

        self.title.clone_from(&title);

        let title_event = match title {
            Some(title) => Event::Title(title),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);
    }

    #[inline]
    fn push_title(&mut self) {
        trace!("Pushing '{:?}' onto title stack", self.title);

        if self.title_stack.len() >= TITLE_STACK_MAX_DEPTH {
            let removed = self.title_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of title stack that exceeds its maximum depth"
            );
        }

        self.title_stack.push(self.title.clone());
    }

    #[inline]
    fn pop_title(&mut self) {
        trace!("Attempting to pop title from stack...");

        if let Some(popped) = self.title_stack.pop() {
            trace!("Title '{popped:?}' popped from stack");
            self.set_title(popped);
        }
    }

    #[inline]
    fn text_area_size_pixels(&mut self) {
        // feathercritty: once the host has reported its cell size (`set_cell_pixel_size`) the
        // answer comes straight from the grid and that size; without it the host is asked, as
        // before. Not upstream.
        if self.cell_size_known {
            let (width, height) = self.text_area_pixels();
            let text = format!("\x1b[4;{height};{width}t");
            self.event_proxy.send_event(Event::PtyWrite(text));
            return;
        }
        self.event_proxy.send_event(Event::TextAreaSizeRequest(Arc::new(move |window_size| {
            let height = window_size.num_lines * window_size.cell_height;
            let width = window_size.num_cols * window_size.cell_width;
            format!("\x1b[4;{height};{width}t")
        })));
    }

    /// Report the cell size in pixels (`CSI 16 t`): `\x1b[6;<height>;<width>t`.
    ///
    /// feathercritty: from the size `set_cell_pixel_size` set, so image programs sizing their
    /// output by it see the same numbers placements do; until one is set the host is asked through
    /// [`Event::TextAreaSizeRequest`], as `CSI 14 t` does. Not upstream.
    #[inline]
    fn cell_size_pixels(&mut self) {
        if self.cell_size_known {
            let (width, height) = self.placements.cell_pixel_size();
            let text = format!("\x1b[6;{height};{width}t");
            self.event_proxy.send_event(Event::PtyWrite(text));
            return;
        }
        self.event_proxy.send_event(Event::TextAreaSizeRequest(Arc::new(move |window_size| {
            format!("\x1b[6;{};{}t", window_size.cell_height, window_size.cell_width)
        })));
    }

    #[inline]
    fn text_area_size_chars(&mut self) {
        let text = format!("\x1b[8;{};{}t", self.screen_lines(), self.columns());
        self.event_proxy.send_event(Event::PtyWrite(text));
    }
}

/// The state of the [`Mode`] and [`PrivateMode`].
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum ModeState {
    /// The mode is not supported.
    NotSupported = 0,
    /// The mode is currently set.
    Set = 1,
    /// The mode is currently not set.
    Reset = 2,
}

impl From<bool> for ModeState {
    fn from(value: bool) -> Self {
        if value { Self::Set } else { Self::Reset }
    }
}

/// Terminal version for escape sequence reports.
///
/// This returns the current terminal version as a unique number based on alacritty_terminal's
/// semver version. The different versions are padded to ensure that a higher semver version will
/// always report a higher version number.
fn version_number(mut version: &str) -> usize {
    if let Some(separator) = version.rfind('-') {
        version = &version[..separator];
    }

    let mut version_number = 0;

    let semver_versions = version.split('.');
    for (i, semver_version) in semver_versions.rev().enumerate() {
        let semver_number = semver_version.parse::<usize>().unwrap_or(0);
        version_number += usize::pow(100, i as u32) * semver_number;
    }

    version_number
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

struct TabStops {
    tabs: Vec<bool>,
}

impl TabStops {
    #[inline]
    fn new(columns: usize) -> TabStops {
        TabStops { tabs: (0..columns).map(|i| i % INITIAL_TABSTOPS == 0).collect() }
    }

    /// Remove all tabstops.
    #[inline]
    fn clear_all(&mut self) {
        unsafe {
            ptr::write_bytes(self.tabs.as_mut_ptr(), 0, self.tabs.len());
        }
    }

    /// Increase tabstop capacity.
    #[inline]
    fn resize(&mut self, columns: usize) {
        let mut index = self.tabs.len();
        self.tabs.resize_with(columns, || {
            let is_tabstop = index % INITIAL_TABSTOPS == 0;
            index += 1;
            is_tabstop
        });
    }
}

impl Index<Column> for TabStops {
    type Output = bool;

    fn index(&self, index: Column) -> &bool {
        &self.tabs[index.0]
    }
}

impl IndexMut<Column> for TabStops {
    fn index_mut(&mut self, index: Column) -> &mut bool {
        self.tabs.index_mut(index.0)
    }
}

/// Terminal cursor rendering information.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct RenderableCursor {
    pub shape: CursorShape,
    pub point: Point,
}

impl RenderableCursor {
    fn new<T>(term: &Term<T>) -> Self {
        // Cursor position.
        let vi_mode = term.mode().contains(TermMode::VI);
        let mut point = if vi_mode { term.vi_mode_cursor.point } else { term.grid.cursor.point };
        // feathercritty: upstream `term.grid[point]`; the vi cursor can be in history.
        if term.grid.cell(point).flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column -= 1;
        }

        // Cursor shape.
        let shape = if !vi_mode && !term.mode().contains(TermMode::SHOW_CURSOR) {
            CursorShape::Hidden
        } else {
            term.cursor_style().shape
        };

        Self { shape, point }
    }
}

/// Visible terminal content.
///
/// This contains all content required to render the current terminal view.
pub struct RenderableContent<'a> {
    pub display_iter: GridIterator<'a, Cell>,
    pub selection: Option<SelectionRange>,
    pub cursor: RenderableCursor,
    pub display_offset: usize,
    pub colors: &'a color::Colors,
    pub mode: TermMode,
}

impl<'a> RenderableContent<'a> {
    fn new<T>(term: &'a Term<T>) -> Self {
        Self {
            display_iter: term.grid().display_iter(),
            display_offset: term.grid().display_offset(),
            cursor: RenderableCursor::new(term),
            selection: term.selection.as_ref().and_then(|s| s.to_range(term)),
            colors: &term.colors,
            mode: *term.mode(),
        }
    }
}

/// Terminal test helpers.
pub mod test {
    use super::*;

    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};

    use crate::event::VoidListener;

    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct TermSize {
        pub columns: usize,
        pub screen_lines: usize,
    }

    impl TermSize {
        pub fn new(columns: usize, screen_lines: usize) -> Self {
            Self { columns, screen_lines }
        }
    }

    impl Dimensions for TermSize {
        fn total_lines(&self) -> usize {
            self.screen_lines()
        }

        fn screen_lines(&self) -> usize {
            self.screen_lines
        }

        fn columns(&self) -> usize {
            self.columns
        }
    }

    /// Construct a terminal from its content as string.
    ///
    /// A `\n` will break line and `\r\n` will break line without wrapping.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use feathercritty::term::test::mock_term;
    ///
    /// // Create a terminal with the following cells:
    /// //
    /// // [h][e][l][l][o] <- WRAPLINE flag set
    /// // [:][)][ ][ ][ ]
    /// // [t][e][s][t][ ]
    /// mock_term(
    ///     "\
    ///     hello\n:)\r\ntest",
    /// );
    /// ```
    pub fn mock_term(content: &str) -> Term<VoidListener> {
        let lines: Vec<&str> = content.split('\n').collect();
        let num_cols = lines
            .iter()
            .map(|line| line.chars().filter(|c| *c != '\r').map(|c| c.width().unwrap()).sum())
            .max()
            .unwrap_or(0);

        // Create terminal with the appropriate dimensions.
        let size = TermSize::new(num_cols, lines.len());
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Fill terminal with content.
        //
        // feathercritty: upstream `term.grid[line]`; a mock terminal has no history, every
        // line is a screen line.
        let screen = term.grid.screen_mut();
        for (line, text) in lines.iter().enumerate() {
            let line = Line(line as i32);
            if !text.ends_with('\r') && line + 1 != lines.len() {
                screen[line][Column(num_cols - 1)].flags.insert(Flags::WRAPLINE);
            }

            let mut index = 0;
            for c in text.chars().take_while(|c| *c != '\r') {
                screen[line][Column(index)].c = c;

                // Handle fullwidth characters.
                let width = c.width().unwrap();
                if width == 2 {
                    screen[line][Column(index)].flags.insert(Flags::WIDE_CHAR);
                    screen[line][Column(index + 1)].flags.insert(Flags::WIDE_CHAR_SPACER);
                }

                index += width;
            }
        }

        term
    }
}

// feathercritty: upstream's tests indexed `Grid` directly; screen writes go through
// `Grid::screen_mut()`/`screen()`, history writes through `Grid::update_line`.
#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::event::VoidListener;
    use crate::grid::{Grid, Scroll};
    use crate::index::{Column, Point, Side};
    use crate::selection::{Selection, SelectionType};
    use crate::term::cell::{Cell, Flags};
    use crate::term::test::TermSize;
    use crate::vte::ansi::{self, CharsetIndex, Handler, StandardCharset};

    #[test]
    fn scroll_display_page_up() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Scrollable amount to top is 11.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 10);

        // Scrollable amount to top is 1.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);

        // Scrollable amount to top is 0.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);
    }

    #[test]
    fn scroll_display_page_down() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Change display_offset to topmost.
        term.grid_mut().scroll_display(Scroll::Top);
        term.vi_mode_cursor = ViModeCursor::new(Point::new(Line(-11), Column(0)));

        // Scrollable amount to bottom is 11.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 1);

        // Scrollable amount to bottom is 1.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);

        // Scrollable amount to bottom is 0.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn simple_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 0..4 {
            if i == 1 {
                continue;
            }

            grid.screen_mut()[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid.screen_mut()[Line(i)][Column(j)].c = 'a';
            }

            grid.screen_mut()[Line(i)][Column(4)].c = '"';
        }
        grid.screen_mut()[Line(2)][Column(0)].c = ' ';
        grid.screen_mut()[Line(2)][Column(4)].c = ' ';
        grid.screen_mut()[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid.screen_mut()[Line(3)][Column(0)].c = ' ';

        // Multiple lines contain an empty line.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point { line: Line(0), column: Column(0) },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(2), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\"aaa\"\n\n aaa ")));

        // A wrapline.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point { line: Line(2), column: Column(0) },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from(" aaa  aaa\"")));
    }

    #[test]
    fn semantic_selection_works() {
        let size = TermSize::new(5, 3);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(3, 5, 0);
        for i in 0..5 {
            for j in 0..2 {
                grid.screen_mut()[Line(j)][Column(i)].c = 'a';
            }
        }
        grid.screen_mut()[Line(0)][Column(0)].c = '"';
        grid.screen_mut()[Line(0)][Column(3)].c = '"';
        grid.screen_mut()[Line(1)][Column(2)].c = '"';
        grid.screen_mut()[Line(0)][Column(4)].flags.insert(Flags::WRAPLINE);

        let mut escape_chars = String::from("\"");

        mem::swap(&mut term.grid, &mut grid);
        mem::swap(&mut term.config.semantic_escape_chars, &mut escape_chars);

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(0), column: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(0), column: Column(4) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(1), column: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }
    }

    #[test]
    fn line_selection_works() {
        let size = TermSize::new(5, 1);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(1, 5, 0);
        for i in 0..5 {
            grid.screen_mut()[Line(0)][Column(i)].c = 'a';
        }
        grid.screen_mut()[Line(0)][Column(0)].c = '"';
        grid.screen_mut()[Line(0)][Column(3)].c = '"';

        mem::swap(&mut term.grid, &mut grid);

        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point { line: Line(0), column: Column(3) },
            Side::Left,
        ));
        assert_eq!(term.selection_to_string(), Some(String::from("\"aa\"a\n")));
    }

    #[test]
    fn block_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 1..4 {
            grid.screen_mut()[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid.screen_mut()[Line(i)][Column(j)].c = 'a';
            }

            grid.screen_mut()[Line(i)][Column(4)].c = '"';
        }
        grid.screen_mut()[Line(2)][Column(2)].c = ' ';
        grid.screen_mut()[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid.screen_mut()[Line(3)][Column(4)].c = ' ';

        term.selection = Some(Selection::new(
            SelectionType::Block,
            Point { line: Line(0), column: Column(3) },
            Side::Left,
        ));

        // The same column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(3) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\na\na\na")));

        // The first column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(0) }, Side::Left);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\n\"aa\n\"a\n\"aa")));

        // The last column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\na\"\na\"\na")));
    }

    /// Check that the grid can be serialized back and forth losslessly.
    ///
    /// This test is in the term module as opposed to the grid since we want to
    /// test this property with a T=Cell.
    #[test]
    #[cfg(feature = "serde")]
    fn grid_serde() {
        let grid: Grid<Cell> = Grid::new(24, 80, 0);
        let serialized = serde_json::to_string(&grid).expect("ser");
        let deserialized = serde_json::from_str::<Grid<Cell>>(&serialized).expect("de");

        assert_eq!(deserialized, grid);
    }

    #[test]
    fn input_line_drawing_character() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let cursor = Point::new(Line(0), Column(0));
        term.configure_charset(CharsetIndex::G0, StandardCharset::SpecialCharacterAndLineDrawing);
        term.input('a');

        assert_eq!(term.grid().screen()[cursor].c, '▒');
    }

    #[test]
    fn clearing_viewport_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
    }

    #[test]
    fn clearing_viewport_with_vi_mode_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-5), Column(3)));
    }

    #[test]
    fn clearing_scrollback_resets_display_offset() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn clearing_scrollback_sets_vi_cursor_into_viewport() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(3)));
    }

    #[test]
    fn clear_saved_lines() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Add one line of scrollback.
        term.grid.scroll_up(&(Line(0)..Line(1)), 1);

        // Clear the history.
        term.clear_screen(ansi::ClearMode::Saved);

        // Make sure that scrolling does not change the grid.
        let mut scrolled_grid = term.grid.clone();
        scrolled_grid.scroll_display(Scroll::Top);

        // Truncate grids for comparison.
        scrolled_grid.truncate();
        term.grid.truncate();

        assert_eq!(term.grid, scrolled_grid);
    }

    #[test]
    fn vi_cursor_keep_pos_on_scrollback_buffer() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point.line = Line(-11);

        term.linefeed();
        assert_eq!(term.vi_mode_cursor.point.line, Line(-12));
    }

    #[test]
    fn grow_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn grow_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn damage_public_usage() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        // Test that we damage input form [`Term::input`].

        let left = term.grid.cursor.point.column.0;
        term.input('d');
        term.input('a');
        term.input('m');
        term.input('a');
        term.input('g');
        term.input('e');
        let right = term.grid.cursor.point.column.0;

        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(damaged_lines.next(), Some(LineDamageBounds { line: 0, left, right }));
        assert_eq!(damaged_lines.next(), None);
        term.reset_damage();

        // Create scrollback.
        for _ in 0..20 {
            term.newline();
        }

        match term.damage() {
            TermDamage::Full => (),
            TermDamage::Partial(_) => panic!("Expected Full damage, however got Partial "),
        };
        term.reset_damage();

        term.scroll_display(Scroll::Delta(10));
        term.reset_damage();

        // No damage when scrolled into viewport.
        for idx in 0..term.columns() {
            term.goto(idx as i32, idx);
        }
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(damaged_lines.next(), None);

        // Scroll back into the viewport, so we have 2 visible lines which terminal can write
        // to.
        term.scroll_display(Scroll::Delta(-2));
        term.reset_damage();

        term.goto(0, 0);
        term.goto(1, 0);
        term.goto(2, 0);
        let display_offset = term.grid().display_offset();
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds { line: display_offset, left: 0, right: 0 })
        );
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds { line: display_offset + 1, left: 0, right: 0 })
        );
        assert_eq!(damaged_lines.next(), None);
    }

    #[test]
    fn damage_cursor_movements() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let num_cols = term.columns();
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        term.goto(1, 1);

        // NOTE While we can use `[Term::damage]` to access terminal damage information, in the
        // following tests we will be accessing `term.damage.lines` directly to avoid adding extra
        // damage information (like cursor and Vi cursor), which we're not testing.

        assert_eq!(term.damage.lines[0], LineDamageBounds { line: 0, left: 0, right: 0 });
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 1 });
        term.damage.reset(num_cols);

        term.move_forward(3);
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 4 });
        term.damage.reset(num_cols);

        term.move_backward(8);
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 0, right: 4 });
        term.goto(5, 5);
        term.damage.reset(num_cols);

        term.backspace();
        term.backspace();
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 5 });
        term.damage.reset(num_cols);

        term.move_up(1);
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 3 });
        assert_eq!(term.damage.lines[4], LineDamageBounds { line: 4, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.move_down(1);
        term.move_down(1);
        assert_eq!(term.damage.lines[4], LineDamageBounds { line: 4, left: 3, right: 3 });
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 3 });
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.wrapline();
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 0 });
        term.move_forward(3);
        term.move_up(1);
        term.damage.reset(num_cols);

        term.linefeed();
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.carriage_return();
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 3 });
        term.damage.reset(num_cols);

        term.erase_chars(5);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 5 });
        term.damage.reset(num_cols);

        term.delete_chars(3);
        let right = term.columns() - 1;
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right });
        term.move_forward(term.columns());
        term.damage.reset(num_cols);

        term.move_backward_tabs(1);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right });
        term.save_cursor_position();
        term.goto(1, 1);
        term.damage.reset(num_cols);

        term.restore_cursor_position();
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 1 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right: 8 });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::All);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Left);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 8 });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Right);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right });
        term.damage.reset(num_cols);

        term.reverse_index();
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right: 8 });
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 8, right: 8 });
    }

    #[test]
    fn full_damage() {
        let size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        assert!(term.damage.full);
        for _ in 0..20 {
            term.newline();
        }
        term.reset_damage();

        term.clear_screen(ansi::ClearMode::Above);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_display(Scroll::Top);
        assert!(term.damage.full);
        term.reset_damage();

        // Sequential call to scroll display without doing anything shouldn't damage.
        term.scroll_display(Scroll::Top);
        assert!(!term.damage.full);
        term.reset_damage();

        term.set_options(Config::default());
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_down_relative(Line(5), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_up_relative(Line(3), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.deccolm();
        assert!(term.damage.full);
        term.reset_damage();

        term.decaln();
        assert!(term.damage.full);
        term.reset_damage();

        term.set_mode(NamedMode::Insert.into());
        // Just setting `Insert` mode shouldn't mark terminal as damaged.
        assert!(!term.damage.full);
        term.reset_damage();

        let color_index = 257;
        term.set_color(color_index, Rgb::default());
        assert!(term.damage.full);
        term.reset_damage();

        // Setting the same color once again shouldn't trigger full damage.
        term.set_color(color_index, Rgb::default());
        assert!(!term.damage.full);

        term.reset_color(color_index);
        assert!(term.damage.full);
        term.reset_damage();

        // We shouldn't trigger fully damage when cursor gets update.
        term.set_color(NamedColor::Cursor as usize, Rgb::default());
        assert!(!term.damage.full);

        // However requesting terminal damage should mark terminal as fully damaged in `Insert`
        // mode.
        let _ = term.damage();
        assert!(term.damage.full);
        term.reset_damage();

        term.unset_mode(NamedMode::Insert.into());
        assert!(term.damage.full);
        term.reset_damage();

        // Keep this as a last check, so we don't have to deal with restoring from alt-screen.
        term.swap_alt();
        assert!(term.damage.full);
        term.reset_damage();

        let size = TermSize::new(10, 10);
        term.resize(size);
        assert!(term.damage.full);
    }

    #[test]
    fn window_title() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Title None by default.
        assert_eq!(term.title, None);

        // Title can be set.
        term.set_title(Some("Test".into()));
        assert_eq!(term.title, Some("Test".into()));

        // Title can be pushed onto stack.
        term.push_title();
        term.set_title(Some("Next".into()));
        assert_eq!(term.title, Some("Next".into()));
        assert_eq!(term.title_stack.first().unwrap(), &Some("Test".into()));

        // Title can be popped from stack and set as the window title.
        term.pop_title();
        assert_eq!(term.title, Some("Test".into()));
        assert!(term.title_stack.is_empty());

        // Title stack doesn't grow infinitely.
        for _ in 0..4097 {
            term.push_title();
        }
        assert_eq!(term.title_stack.len(), 4096);

        // Title and title stack reset when terminal state is reset.
        term.push_title();
        term.reset_state();
        assert_eq!(term.title, None);
        assert!(term.title_stack.is_empty());

        // Title stack pops back to default.
        term.title = None;
        term.push_title();
        term.set_title(Some("Test".into()));
        term.pop_title();
        assert_eq!(term.title, None);

        // Title can be reset to default.
        term.title = Some("Test".into());
        term.set_title(None);
        assert_eq!(term.title, None);
    }

    /// Overwriting a wide character at the start of the screen's first line clears the leading
    /// spacer on the last cell of the line above, the newest history row (the one history
    /// write, `Grid::update_history_row`).
    #[test]
    fn input_clears_leading_spacer_in_history() {
        use crate::vte::ansi::Processor;

        let size = TermSize::new(4, 2);
        let config = Config { scrolling_history: 10, ..Config::default() };
        let mut term = Term::new(config, &size, VoidListener);
        let mut parser: Processor = Processor::new();
        // "abc" and a wide character that does not fit: a leading spacer ends line 0 and the
        // character wraps to line 1. A newline then scrolls line 0 into history.
        parser.advance(&mut term, "abc\u{4e2d}\n".as_bytes());
        assert_eq!(term.history_size(), 1);
        let spacer = Point::new(Line(-1), Column(3));
        assert!(term.grid.cell(spacer).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));

        // Overwrite the wide character, now at the top left of the screen.
        parser.advance(&mut term, b"\x1b[1;1Hx");
        assert_eq!(term.grid.screen()[Line(0)][Column(0)].c, 'x');
        assert!(!term.grid.cell(spacer).flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
        assert!(term.grid.cell(spacer).flags.contains(Flags::WRAPLINE));
        assert_eq!(term.grid.cell(Point::new(Line(-1), Column(0))).c, 'a');
    }

    /// The text fast path (`plain_line_to_string`) gives exactly what reading the cells
    /// (`cells_line_to_string`) gives, for every history row that takes it, over random column
    /// ranges; rows with tabs, wide characters, zero-width characters, hyperlinks, colored and
    /// wrapping tails make sure the conditions of `compact::plain_text` are exercised both ways.
    #[test]
    fn plain_line_to_string_matches_cells() {
        use rand::{Rng, SeedableRng};
        use rand_chacha::ChaCha8Rng;

        use crate::vte::ansi::Processor;

        let mut rng = ChaCha8Rng::seed_from_u64(0x7e47);
        let pieces: [&[u8]; 14] = [
            b"plain words  ",
            b"   ",
            b"\t",
            b"\x1b[31mred\x1b[0m",
            b"\x1b[1;4mbold\x1b[0m",
            b"\x1b[44m\x1b[K",
            b"\x1b[42mx\x1b[0m",
            "\u{4e2d}\u{6587}".as_bytes(),
            "e\u{301}".as_bytes(),
            b"\x1b]8;;https://e\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b[58;5;3m\x1b[4munder\x1b[0m",
            b"0123456789",
            b"\r\n",
            b"\r\n",
        ];
        // At least 2 columns: a wide character does not fit in one (upstream panics there).
        for columns in [2, 3, 7, 20, 33] {
            let size = TermSize::new(columns, 4);
            let config = Config { scrolling_history: 400, ..Config::default() };
            let mut term = Term::new(config, &size, VoidListener);
            let mut parser: Processor = Processor::new();
            for _ in 0..1500 {
                parser.advance(&mut term, pieces[rng.gen_range(0..pieces.len())]);
            }
            assert!(term.history_size() > 100);

            let (mut plain, mut other) = (0, 0);
            let mut reader = RowReader::new();
            for line in (term.topmost_line().0..0).map(Line) {
                let checked = term.grid.history_plain_text(line, |row| {
                    for _ in 0..4 {
                        let start = Column(rng.gen_range(0..columns));
                        let end = Column(rng.gen_range(start.0..columns));
                        let wide = rng.r#gen();
                        let expected =
                            term.cells_line_to_string(&mut reader, line, start..end, wide);
                        let fast = term.plain_line_to_string(row, start..end);
                        assert_eq!(
                            fast, expected,
                            "{columns} columns, line {}, {start:?}..{end:?}",
                            line.0
                        );
                        let text = term.line_to_string(&mut reader, line, start..end, wide);
                        assert_eq!(text, expected);
                    }
                });
                match checked {
                    Some(()) => plain += 1,
                    None => other += 1,
                }
            }
            assert!(plain > 10 && other > 10, "{columns} columns: {plain} plain, {other} other");
        }
    }

    #[test]
    fn parse_cargo_version() {
        assert!(version_number(env!("CARGO_PKG_VERSION")) >= 10_01);
        assert_eq!(version_number("0.0.1-dev"), 1);
        assert_eq!(version_number("0.1.2-dev"), 1_02);
        assert_eq!(version_number("1.2.3-dev"), 1_02_03);
        assert_eq!(version_number("999.99.99"), 9_99_99_99);
    }

    // feathercritty: tests for grapheme clusters (DEC private mode 2027); not upstream.
    mod clusters {
        use std::sync::{Arc, Mutex};

        use super::*;
        use crate::event::{Event, EventListener};
        use crate::vte::ansi::Processor;

        /// Collects what the terminal writes back to the program.
        #[derive(Clone, Default)]
        pub(super) struct Replies(Arc<Mutex<Vec<String>>>);

        impl EventListener for Replies {
            fn send_event(&self, event: Event) {
                if let Event::PtyWrite(text) = event {
                    self.0.lock().unwrap().push(text);
                }
            }
        }

        pub(super) fn term_with(config: Config, cols: usize, lines: usize) -> Term<Replies> {
            Term::new(config, &TermSize::new(cols, lines), Replies::default())
        }

        pub(super) fn feed<T: EventListener>(term: &mut Term<T>, bytes: &[u8]) {
            let mut parser: Processor = Processor::new();
            parser.advance(term, bytes);
        }

        fn on(term: &Term<Replies>) -> bool {
            term.mode().contains(TermMode::GRAPHEME_CLUSTER)
        }

        use crate::term::cluster::MAX_CLUSTER_PIECES;

        /// A terminal with mode 2027 set.
        fn clustering(cols: usize, lines: usize) -> Term<Replies> {
            let mut term = term_with(Config::default(), cols, lines);
            feed(&mut term, b"\x1b[?2027h");
            term
        }

        fn cell<T>(term: &Term<T>, line: i32, col: usize) -> &Cell {
            &term.grid.screen()[Line(line)][Column(col)]
        }

        fn pieces<T>(term: &Term<T>, line: i32, col: usize) -> Vec<char> {
            cell(term, line, col).zerowidth().map(<[char]>::to_vec).unwrap_or_default()
        }

        fn cursor_col<T>(term: &Term<T>) -> usize {
            term.grid.cursor.point.column.0
        }

        /// Asserts `base` + `rest` sits wide in columns `col`..`col + 2` of line 0.
        fn assert_wide_cluster<T>(term: &Term<T>, col: usize, base: char, rest: &[char]) {
            let c = cell(term, 0, col);
            assert_eq!(c.c, base);
            assert!(c.flags.contains(Flags::WIDE_CHAR), "not wide: {:?}", c.flags);
            assert_eq!(pieces(term, 0, col), rest);
            assert!(cell(term, 0, col + 1).flags.contains(Flags::WIDE_CHAR_SPACER));
        }

        #[test]
        fn cluster_flag_one_wide_cell() {
            let mut term = clustering(10, 2);
            feed(&mut term, "🇮🇹".as_bytes());
            assert_wide_cluster(&term, 0, '🇮', &['🇹']);
            assert_eq!(cursor_col(&term), 2);
        }

        #[test]
        fn cluster_skin_tone() {
            let mut term = clustering(10, 2);
            feed(&mut term, "👍🏽".as_bytes());
            assert_wide_cluster(&term, 0, '👍', &['\u{1F3FD}']);
            assert_eq!(cursor_col(&term), 2);
        }

        #[test]
        fn cluster_family_zwj() {
            let mut term = clustering(10, 2);
            feed(&mut term, "👨\u{200D}👩\u{200D}👧".as_bytes());
            assert_wide_cluster(&term, 0, '👨', &['\u{200D}', '👩', '\u{200D}', '👧']);
            assert_eq!(cursor_col(&term), 2);
        }

        #[test]
        fn cluster_vs16_widens() {
            let mut term = clustering(10, 2);
            feed(&mut term, "❤\u{FE0F}x".as_bytes());
            assert_wide_cluster(&term, 0, '❤', &['\u{FE0F}']);
            assert_eq!(cell(&term, 0, 2).c, 'x');
        }

        #[test]
        fn cluster_vs15_narrows() {
            let mut term = clustering(10, 2);
            feed(&mut term, "⌚\u{FE0E}x".as_bytes());
            let c = cell(&term, 0, 0);
            assert_eq!((c.c, pieces(&term, 0, 0)), ('⌚', vec!['\u{FE0E}']));
            assert!(!c.flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER));
            assert_eq!(cell(&term, 0, 1).c, 'x');
            assert!(!cell(&term, 0, 1).flags.contains(Flags::WIDE_CHAR_SPACER));
        }

        #[test]
        fn cluster_vs_on_plain_base_ignored() {
            let mut term = clustering(10, 2);
            feed(&mut term, "a\u{FE0F}b".as_bytes());
            assert!(!cell(&term, 0, 0).flags.contains(Flags::WIDE_CHAR));
            assert_eq!(cell(&term, 0, 1).c, 'b');
        }

        #[test]
        fn cluster_split_across_feeds() {
            let mut term = clustering(10, 2);
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, "👨\u{200D}".as_bytes());
            parser.advance(&mut term, "👩".as_bytes());
            assert_wide_cluster(&term, 0, '👨', &['\u{200D}', '👩']);
            assert_eq!(cursor_col(&term), 2);
        }

        #[test]
        fn cluster_orphan_zero_width_dropped() {
            let mut term = clustering(10, 2);
            feed(&mut term, "\u{200D}".as_bytes());
            assert!(pieces(&term, 0, 0).is_empty());
            assert_eq!(cursor_col(&term), 0);
        }

        #[test]
        fn cluster_piece_cap() {
            let mut term = clustering(10, 2);
            feed(&mut term, format!("👨{}", "\u{200D}".repeat(100)).as_bytes());
            assert_eq!(pieces(&term, 0, 0).len(), MAX_CLUSTER_PIECES);
        }

        #[test]
        fn cluster_mode_reset_unchanged() {
            let mut term = term_with(Config::default(), 10, 2);
            feed(&mut term, "🇮🇹".as_bytes());
            assert_eq!(cell(&term, 0, 0).c, '🇮');
            assert_eq!(cell(&term, 0, 1).c, '🇹');
            assert_eq!(cursor_col(&term), 2);
        }

        #[test]
        fn cluster_mode_enabled_midway() {
            let mut term = term_with(Config::default(), 10, 2);
            feed(&mut term, "e\u{301}".as_bytes());
            feed(&mut term, "\x1b[?2027h\u{302}".as_bytes());
            assert_eq!(pieces(&term, 0, 0), ['\u{301}', '\u{302}']);
            assert_eq!(cursor_col(&term), 1);
        }

        /// Line 0's cells as `(char, wide, spacer)` triples.
        fn row<T>(term: &Term<T>, line: i32) -> Vec<(char, bool, bool)> {
            (0..term.columns())
                .map(|col| {
                    let c = cell(term, line, col);
                    let f = c.flags;
                    (c.c, f.contains(Flags::WIDE_CHAR), f.contains(Flags::WIDE_CHAR_SPACER))
                })
                .collect()
        }

        #[test]
        fn cluster_widens_in_last_column_wraps() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abcd❤\u{FE0F}".as_bytes());
            let last = cell(&term, 0, 4);
            assert!(last.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
            assert!(last.flags.contains(Flags::WRAPLINE));
            let wrapped = cell(&term, 1, 0);
            assert_eq!((wrapped.c, pieces(&term, 1, 0)), ('❤', vec!['\u{FE0F}']));
            assert!(wrapped.flags.contains(Flags::WIDE_CHAR));
            assert!(cell(&term, 1, 1).flags.contains(Flags::WIDE_CHAR_SPACER));
            assert_eq!(term.grid.cursor.point, Point::new(Line(1), Column(2)));
        }

        #[test]
        fn cluster_widens_in_last_column_no_wrap() {
            let mut term = clustering(5, 3);
            feed(&mut term, "\x1b[?7labcd❤\u{FE0F}".as_bytes());
            let last = cell(&term, 0, 4);
            assert_eq!((last.c, pieces(&term, 0, 4)), ('❤', vec!['\u{FE0F}']));
            assert!(!last.flags.contains(Flags::WIDE_CHAR));
            assert_eq!(cell(&term, 1, 0).c, ' ');
            assert_eq!(term.grid.cursor.point.line, Line(0));
        }

        #[test]
        fn cluster_joins_under_pending_wrap() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abc👍\u{1F3FD}".as_bytes());
            assert_eq!(pieces(&term, 0, 3), ['\u{1F3FD}']);
            assert_eq!(cell(&term, 1, 0).c, ' ');
            assert!(term.grid.cursor.input_needs_wrap);
        }

        #[test]
        fn cluster_narrows_clears_spacer() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abc⌚\u{FE0E}".as_bytes());
            assert!(!cell(&term, 0, 3).flags.contains(Flags::WIDE_CHAR));
            assert!(!cell(&term, 0, 4).flags.contains(Flags::WIDE_CHAR_SPACER));
            assert!(!term.grid.cursor.input_needs_wrap);
            assert_eq!(cursor_col(&term), 4);
        }

        #[test]
        fn cluster_widens_in_insert_mode_shifts() {
            let mut term = clustering(5, 3);
            feed(&mut term, "xyz\r\x1b[4h❤\u{FE0F}".as_bytes());
            let expected = [
                ('❤', true, false),
                (' ', false, true),
                ('x', false, false),
                ('y', false, false),
                ('z', false, false),
            ];
            assert_eq!(row(&term, 0), expected);
        }

        #[test]
        fn cluster_widens_in_insert_mode_cuts_wide_at_edge() {
            let mut term = clustering(5, 3);
            feed(&mut term, "xy中\r\x1b[4h❤\u{FE0F}".as_bytes());
            let expected = [
                ('❤', true, false),
                (' ', false, true),
                ('x', false, false),
                ('y', false, false),
                (' ', false, false),
            ];
            assert_eq!(row(&term, 0), expected);
        }

        #[test]
        fn cluster_overwrite_half_clears() {
            let mut term = clustering(5, 3);
            feed(&mut term, "🇮🇹\x1b[1;2Hx".as_bytes());
            assert_eq!((cell(&term, 0, 0).c, pieces(&term, 0, 0)), (' ', vec![]));
            assert_eq!(row(&term, 0)[1], ('x', false, false));

            let mut term = clustering(5, 3);
            feed(&mut term, "🇮🇹\x1b[1;1Hx".as_bytes());
            assert_eq!((cell(&term, 0, 0).c, pieces(&term, 0, 0)), ('x', vec![]));
            assert_eq!(row(&term, 0)[1], (' ', false, false));
        }

        #[test]
        fn cluster_survives_history() {
            let mut term = clustering(5, 3);
            feed(&mut term, "🇮🇹\r\na\r\nb\r\nc\r\nd".as_bytes());
            assert!(term.grid.history_size() >= 1);
            let line = Line(-(term.grid.history_size() as i32));
            let kept: Cell = term.grid.cell(Point::new(line, Column(0)));
            assert_eq!(kept.c, '🇮');
            assert_eq!(kept.zerowidth(), Some(&['🇹'][..]));
            assert!(kept.flags.contains(Flags::WIDE_CHAR));
        }

        #[test]
        fn cluster_survives_reflow() {
            // The cluster must end up where a plain wide character ends up, whole.
            let mut term = clustering(6, 3);
            feed(&mut term, "ab🇮🇹".as_bytes());
            let mut plain = clustering(6, 3);
            feed(&mut plain, "ab中".as_bytes());
            term.resize(TermSize::new(3, 3));
            term.resize(TermSize::new(6, 3));
            plain.resize(TermSize::new(3, 3));
            plain.resize(TermSize::new(6, 3));

            let lines = -(plain.grid.history_size() as i32)..plain.screen_lines() as i32;
            let at = lines
                .flat_map(|l| (0..6).map(move |c| Point::new(Line(l), Column(c))))
                .find(|&p| plain.grid.cell(p).c == '中')
                .expect("the plain wide character is somewhere");
            let kept: Cell = term.grid.cell(at);
            assert_eq!(kept.c, '🇮');
            assert_eq!(kept.zerowidth(), Some(&['🇹'][..]));
            assert!(kept.flags.contains(Flags::WIDE_CHAR));
            let spacer: Cell = term.grid.cell(Point::new(at.line, at.column + 1));
            assert!(spacer.flags.contains(Flags::WIDE_CHAR_SPACER));
        }

        #[test]
        fn cluster_narrows_without_spacer_after_alt_screen_shrink() {
            // The alternate screen does not reflow: shrinking by one column leaves a wide
            // character in the last column without its spacer, with a wrap pending.
            let mut term = clustering(6, 3);
            feed(&mut term, "\x1b[?1049habcd⌚".as_bytes());
            term.resize(TermSize::new(5, 3));
            feed(&mut term, "\u{FE0E}".as_bytes());
            let c = cell(&term, 0, 4);
            assert_eq!((c.c, pieces(&term, 0, 4)), ('⌚', vec!['\u{FE0E}']));
            assert!(!c.flags.contains(Flags::WIDE_CHAR));
        }

        #[test]
        fn cluster_widening_in_one_column_stays_narrow() {
            let mut term = clustering(1, 3);
            feed(&mut term, "❤\u{FE0F}".as_bytes());
            assert_eq!((cell(&term, 0, 0).c, pieces(&term, 0, 0)), ('❤', vec!['\u{FE0F}']));
            assert!(!cell(&term, 0, 0).flags.contains(Flags::WIDE_CHAR));
        }

        #[test]
        fn cluster_moved_at_the_edge_keeps_its_colors() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abcd\x1b[31m❤\x1b[0m\u{FE0F}".as_bytes());
            let moved = cell(&term, 1, 0);
            assert_eq!(moved.c, '❤');
            assert!(moved.flags.contains(Flags::WIDE_CHAR));
            assert_eq!(moved.fg, Color::Named(NamedColor::Red));
            assert_eq!(cell(&term, 1, 1).fg, Color::Named(NamedColor::Red));
        }

        #[test]
        fn cluster_after_a_space_ignores_its_style() {
            // A visible joiner never joins a space, styled or not.
            let mut term = clustering(10, 2);
            feed(&mut term, "a\x1b[4m \x1b[0m\u{1F3FD}".as_bytes());
            assert_eq!((cell(&term, 0, 1).c, pieces(&term, 0, 1)), (' ', vec![]));
            assert_eq!(cell(&term, 0, 2).c, '\u{1F3FD}');
            // A zero-width mark joins a space, styled or not, as it does without the mode.
            let mut term = clustering(10, 2);
            feed(&mut term, "a \u{301}b\x1b[4m \x1b[0m\u{200D}".as_bytes());
            assert_eq!(pieces(&term, 0, 1), ['\u{301}']);
            assert_eq!(cell(&term, 0, 2).c, 'b');
            assert_eq!(pieces(&term, 0, 3), ['\u{200D}']);
        }

        #[test]
        fn cluster_narrows_in_insert_mode_shifts_back() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abc\r\x1b[4h⌚\u{FE0E}x".as_bytes());
            let expected = [
                ('⌚', false, false),
                ('x', false, false),
                ('a', false, false),
                ('b', false, false),
                ('c', false, false),
            ];
            assert_eq!(row(&term, 0), expected);
        }

        #[test]
        fn cluster_narrowed_after_a_wrap_moves_back() {
            let mut term = clustering(5, 3);
            feed(&mut term, "abcd⌚\u{FE0E}".as_bytes());
            let back = cell(&term, 0, 4);
            assert_eq!((back.c, pieces(&term, 0, 4)), ('⌚', vec!['\u{FE0E}']));
            assert!(
                !back.flags.intersects(
                    Flags::WIDE_CHAR | Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE
                )
            );
            assert_eq!(row(&term, 1)[0], (' ', false, false));
            assert_eq!(row(&term, 1)[1], (' ', false, false));
            assert_eq!(term.grid.cursor.point, Point::new(Line(0), Column(4)));
            assert!(term.grid.cursor.input_needs_wrap);
            feed(&mut term, b"y");
            assert!(cell(&term, 0, 4).flags.contains(Flags::WRAPLINE));
            assert_eq!(cell(&term, 1, 0).c, 'y');
        }

        #[test]
        fn mode_2027_is_reset_by_default() {
            let term = term_with(Config::default(), 10, 2);
            assert!(!on(&term));
        }

        #[test]
        fn mode_2027_set_and_reset() {
            let mut term = term_with(Config::default(), 10, 2);
            feed(&mut term, b"\x1b[?2027h");
            assert!(on(&term));
            feed(&mut term, b"\x1b[?2027l");
            assert!(!on(&term));
        }

        #[test]
        fn mode_2027_reports_state() {
            let mut term = term_with(Config::default(), 10, 2);
            let replies = term.event_proxy.clone();
            feed(&mut term, b"\x1b[?2027$p");
            feed(&mut term, b"\x1b[?2027h\x1b[?2027$p");
            assert_eq!(*replies.0.lock().unwrap(), ["\x1b[?2027;2$y", "\x1b[?2027;1$y"]);
        }

        #[test]
        fn mode_2027_default_from_config() {
            let config = Config { grapheme_clusters: true, ..Config::default() };
            let mut term = term_with(config, 10, 2);
            assert!(on(&term));
            feed(&mut term, b"\x1b[?2027l");
            assert!(!on(&term));
            feed(&mut term, b"\x1bc");
            assert!(on(&term));
        }

        #[test]
        fn reset_grapheme_cluster_mode_restores_default() {
            let mut term = term_with(Config::default(), 10, 2);
            feed(&mut term, b"\x1b[?2027h");
            term.reset_grapheme_cluster_mode();
            assert!(!on(&term));
        }

        #[test]
        fn set_options_never_flips_mode_2027() {
            let mut term = term_with(Config::default(), 10, 2);
            feed(&mut term, b"\x1b[?2027h");
            term.set_options(Config::default());
            assert!(on(&term));

            let config = Config { grapheme_clusters: true, ..Config::default() };
            let mut term = term_with(config.clone(), 10, 2);
            feed(&mut term, b"\x1b[?2027l");
            term.set_options(config);
            assert!(!on(&term));
        }
    }

    // feathercritty: what the support reports answer (DA1, XTSMGRAPHICS, `CSI 14 t`/`16 t`) and
    // the master switch; not upstream.
    mod image_support {
        use std::sync::{Arc, Mutex};

        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        use super::*;
        use crate::event::{Event, EventListener, ImageKey, WindowSize};
        use crate::graphics::limits::MAX_IMAGE_SIDE;
        use crate::vte::ansi::Processor;

        /// A host that records what the terminal writes back and which images it is handed,
        /// answering a window-size request with the size it was configured with (what a real host
        /// does with its own font metrics).
        #[derive(Clone, Default)]
        struct Host(Arc<Mutex<Log>>);

        struct Log {
            /// The window size this host reports when asked; the tests set their grid's.
            window: WindowSize,
            writes: Vec<String>,
            received: Vec<ImageKey>,
            released: Vec<ImageKey>,
            /// Times the terminal asked for the window size instead of answering itself.
            asked: usize,
        }

        /// A host with no size set yet reports this one (no test reads it without setting one).
        impl Default for Log {
            fn default() -> Self {
                Self {
                    window: WindowSize {
                        num_lines: 24,
                        num_cols: 80,
                        cell_width: 9,
                        cell_height: 18,
                    },
                    writes: Vec::new(),
                    received: Vec::new(),
                    released: Vec::new(),
                    asked: 0,
                }
            }
        }

        impl EventListener for Host {
            fn send_event(&self, event: Event) {
                let mut log = self.0.lock().unwrap();
                match event {
                    // The host formats its own window size; the answer goes to the pty.
                    Event::TextAreaSizeRequest(format) => {
                        let window = log.window;
                        log.asked += 1;
                        log.writes.push(format(window));
                    },
                    Event::PtyWrite(text) => log.writes.push(text),
                    Event::ImageReceived { key, .. } => log.received.push(key),
                    Event::ImageReleased(key) => log.released.push(key),
                    _ => {},
                }
            }
        }

        impl Host {
            fn set_window(&self, window: WindowSize) {
                self.0.lock().unwrap().window = window;
            }

            /// Everything written back since the last drain, concatenated.
            fn writes(&self) -> String {
                std::mem::take(&mut self.0.lock().unwrap().writes).concat()
            }

            /// The image keys handed to the host since the last drain.
            fn received(&self) -> Vec<ImageKey> {
                std::mem::take(&mut self.0.lock().unwrap().received)
            }

            /// The images released since the last drain.
            fn released(&self) -> Vec<ImageKey> {
                std::mem::take(&mut self.0.lock().unwrap().released)
            }

            /// How many times the host was asked for its window size since the last drain.
            fn asked(&self) -> usize {
                std::mem::take(&mut self.0.lock().unwrap().asked)
            }
        }

        fn new_term(columns: usize, lines: usize) -> (Term<Host>, Host) {
            let host = Host::default();
            let term = Term::new(Config::default(), &TermSize::new(columns, lines), host.clone());
            (term, host)
        }

        fn feed(term: &mut Term<Host>, bytes: &[u8]) {
            let mut parser: Processor = Processor::new();
            parser.advance(term, bytes);
        }

        fn cursor(term: &Term<Host>) -> (i32, usize) {
            let point = term.grid().cursor.point;
            (point.line.0, point.column.0)
        }

        /// A one-pixel black sixel sequence (the same one the placement tests use).
        const SIXEL: &[u8] = b"\x1bPq#0?~\x1b\\";

        /// The answer every disabled kitty frame gets, for an image addressed as `i=<id>` (`id` 0
        /// leaves the address out).
        fn enotsupported(id: u32) -> String {
            if id == 0 {
                return String::from("\x1b_G;ENOTSUPPORTED:not supported by this terminal\x1b\\");
            }
            format!("\x1b_Gi={id};ENOTSUPPORTED:not supported by this terminal\x1b\\")
        }

        /// A complete `a=T` transmission of raw pixels, with extra control keys (`,c=2,r=2` style)
        /// appended to the frame.
        fn transmit(
            id: u32,
            format: &str,
            size: (u32, u32),
            pixels: &[u8],
            extra: &str,
        ) -> Vec<u8> {
            let (width, height) = size;
            let mut out =
                format!("\x1b_Ga=T,i={id},{format},s={width},v={height}{extra}").into_bytes();
            out.push(b';');
            out.extend_from_slice(STANDARD.encode(pixels).as_bytes());
            out.extend(b"\x1b\\");
            out
        }

        /// A PNG declaring `w`×`h`: only its IHDR is read, so the chunk CRC stays zero and no image
        /// data follows (the same shape the inline parser's own tests build).
        fn png(w: u32, h: u32) -> Vec<u8> {
            let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
            v.extend_from_slice(&13u32.to_be_bytes());
            v.extend_from_slice(b"IHDR");
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, color type, compression, filter, interlace
            v.extend_from_slice(&[0, 0, 0, 0]); // CRC: never looked at
            v
        }

        /// An OSC 1337 sequence carrying `png(w, h)` as an inline image.
        fn inline_image(w: u32, h: u32) -> Vec<u8> {
            let mut out = b"\x1b]1337;File=inline=1:".to_vec();
            out.extend_from_slice(STANDARD.encode(png(w, h)).as_bytes());
            out.extend(b"\x1b\\");
            out
        }

        #[test]
        fn da1_advertises_sixel_when_on() {
            let (mut term, host) = new_term(10, 4);

            feed(&mut term, b"\x1b[c");
            assert_eq!(host.writes(), "\x1b[?62;4;22c", "sixel and ANSI colour are advertised");

            // The secondary answer (the crate version) is not the switch's to change.
            let version = version_number(env!("CARGO_PKG_VERSION"));
            feed(&mut term, b"\x1b[>c");
            assert_eq!(host.writes(), format!("\x1b[>0;{version};1c"));

            // Off: what a terminal without graphics support says. The switch is the host's, so a
            // full reset leaves it alone.
            term.set_images_enabled(false);
            feed(&mut term, b"\x1bc\x1b[c");
            assert_eq!(host.writes(), "\x1b[?6c", "no sixel bit without image support");

            // And on again.
            term.set_images_enabled(true);
            feed(&mut term, b"\x1b[c");
            assert_eq!(host.writes(), "\x1b[?62;4;22c");
        }

        #[test]
        fn xtsmgraphics_replies() {
            let (mut term, host) = new_term(10, 4);
            term.set_cell_pixel_size(8, 16); // a text area of 80 by 64 pixels

            // Reads answer what this terminal has.
            feed(&mut term, b"\x1b[?1;1S\x1b[?2;1S");
            assert_eq!(host.writes(), "\x1b[?1;0;1024S\x1b[?2;0;80;64S");

            // The geometry never asks for a side bigger than an image may have.
            term.set_cell_pixel_size(4_000, 4_000);
            feed(&mut term, b"\x1b[?2;1S");
            let cap = u64::from(MAX_IMAGE_SIDE);
            assert_eq!(host.writes(), format!("\x1b[?2;0;{cap};{cap}S"));

            // Set and reset succeed without changing anything, so they report what stands: the
            // registers as they are, the geometry as the grid and cell size make it.
            term.set_cell_pixel_size(8, 16);
            feed(&mut term, b"\x1b[?1;2S\x1b[?1;3;256S\x1b[?2;3;10;20S\x1b[?2;4S");
            assert_eq!(
                host.writes(),
                "\x1b[?1;0;1024S\x1b[?1;0;1024S\x1b[?2;0;80;64S\x1b[?2;0;80;64S"
            );

            // An item this terminal does not have, and an action it does not know.
            feed(&mut term, b"\x1b[?7;1S\x1b[?1;9S");
            assert_eq!(host.writes(), "\x1b[?7;1S\x1b[?1;2S");

            // With images off every request is an error.
            term.set_images_enabled(false);
            feed(&mut term, b"\x1b[?1;1S\x1b[?2;3;256S\x1b[?7;1S");
            assert_eq!(host.writes(), "\x1b[?1;1S\x1b[?2;1S\x1b[?7;1S");

            // A cell size the host never set: the geometry counts each cell as one pixel.
            let (mut term, host) = new_term(3, 2);
            feed(&mut term, b"\x1b[?2;1S");
            assert_eq!(host.writes(), "\x1b[?2;0;3;2S");
            assert_eq!(host.asked(), 0, "the geometry comes from the grid, not the host");
        }

        #[test]
        fn pixel_size_reports_physical() {
            let (mut term, host) = new_term(10, 4);
            host.set_window(WindowSize {
                num_lines: 4,
                num_cols: 10,
                cell_width: 9,
                cell_height: 18,
            });

            // No cell size known yet: both reports ask the host, as they did before image support.
            feed(&mut term, b"\x1b[14t\x1b[16t");
            assert_eq!(host.asked(), 2, "both fall back to asking the host");
            assert_eq!(host.writes(), "\x1b[4;72;90t\x1b[6;18;9t");

            // Once the host has reported its metrics the answers come from the grid and that size.
            term.set_cell_pixel_size(8, 16);
            feed(&mut term, b"\x1b[14t\x1b[16t");
            assert_eq!(host.asked(), 0, "nothing is asked any more");
            assert_eq!(host.writes(), "\x1b[4;64;80t\x1b[6;16;8t");

            // A resize moves the text area, not the cell.
            term.resize(TermSize::new(20, 5));
            feed(&mut term, b"\x1b[14t\x1b[16t");
            assert_eq!(host.writes(), "\x1b[4;80;160t\x1b[6;16;8t");

            // The switch does not silence the size reports: a host that shows no images still has
            // cells of a known size.
            term.set_images_enabled(false);
            feed(&mut term, b"\x1b[14t\x1b[16t");
            assert_eq!(host.writes(), "\x1b[4;80;160t\x1b[6;16;8t");

            // `CSI 18 t` still answers in characters.
            feed(&mut term, b"\x1b[18t");
            assert_eq!(host.writes(), "\x1b[8;5;20t");
        }

        #[test]
        fn disabled_images_are_inert() {
            let (mut term, host) = new_term(12, 6);
            term.set_cell_pixel_size(8, 16); // so the images below span one cell each
            term.set_images_enabled(false);

            // DA1 leaves the sixel bit out.
            feed(&mut term, b"\x1b[c");
            assert_eq!(host.writes(), "\x1b[?6c");

            // The support query answers with the protocol's "not supported".
            feed(&mut term, b"\x1b_Ga=q,i=7\x1b\\");
            assert_eq!(host.writes(), enotsupported(7));

            // A sixel is consumed where an image would have been placed: nothing reaches the host,
            // nothing is placed, and the cursor stays (DECSDM off moves it below an image).
            feed(&mut term, b"\x1b[2;3H");
            let before = cursor(&term);
            feed(&mut term, SIXEL);
            assert_eq!(cursor(&term), before, "a discarded sixel does not move the cursor");
            assert!(host.received().is_empty(), "no payload for the host");
            assert!(host.released().is_empty());
            assert!(term.visible_placements().is_empty(), "nothing was displayed");

            // A kitty transmission with its placement, then a standalone place and a delete: each
            // frame is answered "not supported" and leaves nothing behind.
            feed(&mut term, &transmit(3, "f=24", (1, 1), &[0, 0, 0], ",c=1,r=1"));
            assert!(host.received().is_empty(), "no image was stored");
            assert!(term.visible_placements().is_empty());
            assert_eq!(host.writes(), enotsupported(3));
            feed(&mut term, b"\x1b_Ga=p,i=3\x1b\\");
            assert_eq!(host.writes(), enotsupported(3));
            feed(&mut term, b"\x1b_Ga=d,d=a\x1b\\");
            assert_eq!(host.writes(), "", "a delete with neither id nor number answers nothing");

            // A frame the parser refuses is refused as unsupported too — off, that is the only
            // graphics answer this terminal gives. The quiet level still applies: `a=f` (animation)
            // is refused at dispatch, after its `q` was read.
            feed(&mut term, b"\x1b_Ga=x\x1b\\"); // an action the protocol does not define
            assert_eq!(host.writes(), "", "and with no id or number even that answers nothing");
            feed(&mut term, b"\x1b_Ga=f,i=8,q=2\x1b\\");
            assert_eq!(host.writes(), "", "q=2 suppresses the answer as it always did");

            // An OSC 1337 image is consumed without collecting its payload.
            feed(&mut term, &inline_image(2, 2));
            assert!(host.received().is_empty());
            assert!(term.visible_placements().is_empty());

            // The very same sequences do display once the host turns images back on.
            term.set_images_enabled(true);
            feed(&mut term, SIXEL);
            feed(&mut term, &inline_image(2, 2));
            feed(&mut term, &transmit(9, "f=32", (8, 8), &[0x7F; 8 * 8 * 4], ",c=1,r=1"));
            assert_eq!(host.received().len(), 3, "sixel, inline and kitty all display");
            assert_eq!(term.visible_placements().len(), 3);
            assert_eq!(host.writes(), "\x1b_Gi=9;OK\x1b\\");
            feed(&mut term, b"\x1b_Ga=q,i=9\x1b\\");
            assert_eq!(host.writes(), "\x1b_Gi=9;OK\x1b\\", "the query is OK again");

            // Switching off drops what was stored and displayed, releasing each image exactly once.
            term.set_images_enabled(false);
            assert_eq!(host.released().len(), 3, "every image releases once");
            assert!(term.visible_placements().is_empty());
            feed(&mut term, b"\x1b_Ga=p,i=9\x1b\\");
            assert_eq!(
                host.writes(),
                enotsupported(9),
                "off, every frame answers 'not supported' (and its data went with the switch)"
            );
            assert!(host.released().is_empty(), "nothing releases a second time");

            // Switching off again, with nothing stored, changes nothing.
            term.set_images_enabled(false);
            assert!(host.released().is_empty());
        }
    }
}
