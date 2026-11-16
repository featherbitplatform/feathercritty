// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Image placements that follow the grid (feathercritty).
//!
//! [`PlacementTable`] is the terminal's bookkeeping of where images sit: one entry per display of
//! a transmitted image (a *placement*), the invisible prototypes hosts draw with Unicode
//! placeholders (virtual placements), and the few-byte [`Placeholder`] records that replace
//! placements whose rows have scrolled out of the viewport when scrollback images are not kept.
//! The table owns the ids clients use: kitty image ids and image numbers map to the [`ImageKey`]s
//! the embedder sees, and every image the table forgets is reported exactly once with
//! `Event::ImageReleased`.
//!
//! A placement's row is a *stable line*: `scrolled_lines + grid line` (see
//! [`Grid::scrolled_lines`](crate::grid::Grid::scrolled_lines)), which does not change while
//! output scrolls it upward or the screen grows or shrinks. That is what makes scrolling cheap: a
//! rotation of the whole viewport — the only one that moves rows into history — needs no placement
//! updates at all because the counter absorbs it; [`GridWindow`] carries the numbers needed to turn
//! stable lines back into viewport rows. Sub-region rotations move content without touching the
//! counter, so
//! [`after_scroll_up`](Self::after_scroll_up)/[`after_scroll_down`](Self::after_scroll_down)
//! shift the placements inside the rotated range and drop those whose content is destroyed.
//!
//! Growth is capped against hostile streams: [`MAX_PLACEMENTS`] placements, [`MAX_STORED_IMAGES`]
//! images and [`MAX_PENDING_DECODES`] transmissions awaiting a decode result at once; overflowing
//! evicts the oldest, releasing whatever stops being referenced. A row-only resize needs nothing
//! (stable lines survive it); a column resize drops placements whose anchor column is gone, and a
//! reflow that moves content between rows can leave an image following its new row — anchors track
//! their row's stable line, which is the documented limit of following.
//!
//! An image stays available (not released) while anything still references it: a placement, a
//! virtual placement, an id or number map entry (a stored image a client can re-place without
//! resending), or an in-flight decode. Kitty images therefore live until deleted, reset or evicted;
//! sixel and inline images with no live placement are released as soon as their last one goes.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use crate::event::{Event, ImageDecodeError, ImageKey};
use crate::graphics::kitty::{self, KittyError, Rect};

/// The most placements the table keeps at once; over it the oldest is removed.
pub const MAX_PLACEMENTS: usize = 4_096;

/// The most images (decoded or awaiting decode) the table tracks at once; over it the oldest image
/// without live references is evicted first, then the oldest with them.
pub const MAX_STORED_IMAGES: usize = 512;

/// The most transmissions that may await a decode result at once; [`new_image`](Self::new_image)
/// refuses beyond it.
pub const MAX_PENDING_DECODES: usize = 64;

/// A conservative bound on placement height, used only to bound the map scan when looking for
/// placements that cover a row from an anchor above it (real spans are bounded by the image size
/// limits).
const MAX_SPAN_ROWS: i64 = 10_000;

/// Which of the terminal's two grids a placement belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Screen {
    /// The normal screen.
    Primary,
    /// The alternate screen.
    Alternate,
}

impl Screen {
    fn index(self) -> usize {
        match self {
            Screen::Primary => 0,
            Screen::Alternate => 1,
        }
    }
}

/// The grid numbers needed to translate between stable lines and viewport rows.
///
/// Read them from the grid after every change: `stable` is [`Grid::scrolled_lines`], `lines` its
/// [`screen_lines`](crate::grid::Dimensions::screen_lines) and `history` its
/// [`history_size`](crate::grid::Dimensions::history_size). A stable line `s` maps to viewport row
/// `s - stable`, reachable when `-history <= s - stable`.
///
/// [`Grid::scrolled_lines`]: crate::grid::Grid::scrolled_lines
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridWindow {
    pub stable: i64,
    pub lines: usize,
    pub history: usize,
}

impl GridWindow {
    /// Whether grid row `index` (viewport-relative, bottom-anchored) is in view with a display
    /// offset of `offset` rows.
    fn in_view(self, index: i64, offset: usize) -> bool {
        let offset = offset as i64;
        index >= -offset && index < self.lines as i64 - offset
    }

    /// Whether the span `index..index + span` intersects that view.
    fn overlaps_view(self, index: i64, span: i64, offset: usize) -> bool {
        let offset = offset as i64;
        index < self.lines as i64 - offset && index + span > -offset
    }
}

/// One display of an image on a grid.
///
/// `line` is the stable line of the placement's top row (see [`GridWindow`]); `col..col + cols` and
/// `line..line + rows` are the cell rectangle it covers. `src` selects a source rectangle in image
/// pixels (`None` is the whole image), `x_off`/`y_off` offset the display within the anchor cell,
/// and `z` orders overlapping images for drawing (negative draws under text). `pid` is the kitty
/// placement id if it has one: re-placing with the same `(key, pid)` replaces the old placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub key: ImageKey,
    pub screen: Screen,
    pub line: i64,
    pub col: usize,
    pub cols: u32,
    pub rows: u32,
    pub src: Option<Rect>,
    pub x_off: u32,
    pub y_off: u32,
    pub z: i32,
    pub pid: Option<u32>,
}

/// A placement whose image has been released, kept so scrolled-off rows still say what was there.
/// Hosts draw its `[image WxH]` text (see [`PlacementTable::visible_placeholders`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub screen: Screen,
    pub line: i64,
    pub col: usize,
    pub pixel_size: (u32, u32),
}

/// A placement as [`PlacementTable::visible_placements`] reports it for the current view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisiblePlacement {
    pub key: ImageKey,
    /// Top row relative to the top of the view; negative when partly scrolled off above it.
    pub row_in_view: i32,
    pub col: usize,
    pub cols: u32,
    pub rows: u32,
    pub src: Option<Rect>,
    pub x_off: u32,
    pub y_off: u32,
    pub z: i32,
}

/// A placeholder as [`PlacementTable::visible_placeholders`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisiblePlaceholder {
    pub row_in_view: i32,
    pub col: usize,
    pub text: String,
}

/// A virtual placement (`U=1`): the invisible prototype a host displays by printing placeholder
/// cells that refer to its image id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VirtualPlacement {
    key: ImageKey,
    pid: Option<u32>,
    cols: u32,
    rows: u32,
}

/// The reply owed to a client once its transmission's decode result is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingReply {
    /// The image id to echo (`0` for an unnamed image).
    id: u32,
    number: Option<u32>,
    placement: Option<u32>,
    quiet: u8,
}

/// One image the table knows: its size in pixels and whether a decode result or reply is owed.
#[derive(Clone, Debug)]
struct StoredImage {
    width_px: u32,
    height_px: u32,
    /// A transmission whose decode result has not arrived; it owes `reply`.
    pending: bool,
    reply: Option<PendingReply>,
}

/// The terminal's image placements, ids and scrollback placeholders; see the [module docs](self).
#[derive(Clone, Debug)]
pub struct PlacementTable {
    images: HashMap<ImageKey, StoredImage>,
    /// kitty image id -> key (ordered, so `d=r` can range it).
    ids: BTreeMap<u32, ImageKey>,
    /// kitty image number -> the newest key with that number.
    numbers: HashMap<u32, ImageKey>,
    /// kitty image id -> virtual placements of that image, oldest first.
    virtuals: HashMap<u32, Vec<VirtualPlacement>>,
    /// Placements by stable line (per screen), in insertion order within a line.
    placements: [BTreeMap<i64, Vec<Placement>>; 2],
    placeholders: [BTreeMap<i64, Vec<Placeholder>>; 2],
    /// The physical pixel size of one cell, from `Term::set_cell_pixel_size`; each axis is at
    /// least 1 (an unset size counts as a 1x1 pixel cell).
    cell_px: (u32, u32),
    /// Whether placements stay when they scroll into scrollback (`Term::set_scrollback_images`).
    keep_scrollback: bool,
    next_key: u64,
}

/// Where placement creation went wrong, for the kitty reply the caller sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// No image with that id or number (or its decode failed first).
    NotFound,
    /// Too many placements and none could be evicted.
    Full,
}

impl Default for PlacementTable {
    /// An empty table with no cell size set: each cell counts as one pixel by one pixel, so
    /// spans come straight from the image's pixels.
    fn default() -> Self {
        Self {
            images: HashMap::new(),
            ids: BTreeMap::new(),
            numbers: HashMap::new(),
            virtuals: HashMap::new(),
            placements: [BTreeMap::new(), BTreeMap::new()],
            placeholders: [BTreeMap::new(), BTreeMap::new()],
            cell_px: (1, 1),
            keep_scrollback: false,
            next_key: 0,
        }
    }
}

impl PlacementTable {
    /// An empty table with no cell size set (each cell then counts as one pixel by one pixel).
    pub fn new() -> Self {
        Self::default()
    }

    /// The physical pixel size of a text cell, used to turn pixels into cell spans.
    pub fn set_cell_pixel_size(&mut self, width: u32, height: u32) {
        self.cell_px = (width.max(1), height.max(1));
    }

    /// The physical pixel size of a text cell, as
    /// [`set_cell_pixel_size`](Self::set_cell_pixel_size) set it (each axis at least 1).
    pub fn cell_pixel_size(&self) -> (u32, u32) {
        self.cell_px
    }

    /// Whether placements stay in scrollback instead of becoming [`Placeholder`] records.
    pub fn set_scrollback_images(&mut self, keep: bool) {
        self.keep_scrollback = keep;
    }

    /// Whether the table holds nothing at all (hosts use it to skip work).
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
            && self.virtuals.is_empty()
            && self.placements.iter().all(|map| map.is_empty())
            && self.placeholders.iter().all(|map| map.is_empty())
    }

    // --- images and ids -------------------------------------------------------------------

    /// The cell span for an image of `width_px` by `height_px` source pixels displayed with
    /// `cols`/`rows` requested (`0` means compute: from the other dimension and the aspect ratio,
    /// or from the pixel size and the cell size), rounded up, at least 1x1.
    pub fn span_for(&self, width_px: u32, height_px: u32, cols: u32, rows: u32) -> (u32, u32) {
        let (cw, ch) = self.cell_px;
        match (cols, rows) {
            (0, 0) => (div_ceil(width_px, cw).max(1), div_ceil(height_px, ch).max(1)),
            (c, 0) => {
                // `c` columns wide keeping the aspect: the display is `c * cw` pixels wide.
                let height_display =
                    pixel_aspect(width_px, height_px, u64::from(c) * u64::from(cw));
                (c.max(1), div_ceil_u64(height_display, u64::from(ch)).max(1) as u32)
            },
            (0, r) => {
                let width_display = pixel_aspect(width_px, height_px, u64::from(r) * u64::from(ch));
                (div_ceil_u64(width_display, u64::from(cw)).max(1) as u32, r.max(1))
            },
            (c, r) => (c.max(1), r.max(1)),
        }
    }

    /// The natural cell span of `key`'s whole image.
    pub fn image_span(&self, key: ImageKey) -> Option<(u32, u32)> {
        let (w, h) = self.image_size(key)?;
        Some(self.span_for(w, h, 0, 0))
    }

    /// Register a transmitted image and return its key.
    ///
    /// An id that names an existing image replaces it: re-transmitting deletes the old image, so
    /// its placements, placeholders' subject and virtuals are removed first and `Event::
    /// ImageReleased` is returned for the old key. `reply` is what a client waits for once the
    /// decode result is known (kitty transmissions pass one; sixel and inline images do not). A
    /// number always names a new image: it repoints the number map without touching other images.
    /// Refused with `ENOMEM` when too many decodes are in flight or nothing can be evicted to make
    /// room, `EINVAL` never (the caller checked sizes).
    pub fn new_image(
        &mut self,
        id: Option<u32>,
        number: Option<u32>,
        width_px: u32,
        height_px: u32,
        reply: Option<(u32, Option<u32>, Option<u32>, u8)>, // echo: id, number, placement, quiet
    ) -> Result<(ImageKey, Vec<Event>), KittyError> {
        let pending = reply.is_some();
        if self.images.values().filter(|i| i.pending).count() + usize::from(pending)
            > MAX_PENDING_DECODES
        {
            return Err(KittyError::ENOMEM);
        }
        let mut events = Vec::new();
        if let Some(id) = id {
            if let Some(old) = self.ids.get(&id).copied() {
                self.forget_image(old, &mut events);
            }
        }
        while self.images.len() >= MAX_STORED_IMAGES {
            if !self.evict_oldest_image(&mut events) {
                return Err(KittyError::ENOMEM);
            }
        }
        let key = ImageKey(self.next_key);
        self.next_key += 1;
        let reply = reply.map(|(id, number, placement, quiet)| PendingReply {
            id,
            number,
            placement,
            quiet,
        });
        self.images.insert(key, StoredImage { width_px, height_px, pending, reply });
        if let Some(id) = id {
            self.ids.insert(id, key);
        }
        if let Some(number) = number {
            self.numbers.insert(number, key);
        }
        Ok((key, events))
    }

    /// The key a kitty `i`/`I` reference resolves to (a number means the newest image with it).
    pub fn resolve(&self, id: Option<u32>, number: Option<u32>) -> Option<ImageKey> {
        id.and_then(|id| self.ids.get(&id).copied())
            .or_else(|| number.and_then(|n| self.numbers.get(&n).copied()))
    }

    /// The pixel size stored for `key`.
    pub fn image_size(&self, key: ImageKey) -> Option<(u32, u32)> {
        self.images.get(&key).map(|i| (i.width_px, i.height_px))
    }

    /// Report a transmission's decode result: flushes the reply owed for `key` and, on failure,
    /// removes everything referencing it. Returns the events to send (`PtyWrite` for the reply,
    /// `ImageReleased` when the image is gone). An unknown key (evicted meanwhile) does nothing.
    pub fn image_decoded(
        &mut self,
        key: ImageKey,
        result: Result<(), ImageDecodeError>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(image) = self.images.get_mut(&key) else { return events };
        image.pending = false;
        if let Some(reply) = image.reply.take() {
            let outcome = match result {
                Ok(()) => Ok(()),
                Err(ImageDecodeError::Corrupt) => Err(KittyError::EBADPNG),
                Err(ImageDecodeError::TooLarge) => Err(KittyError::ENOMEM),
            };
            push_reply(&mut events, reply, outcome);
        }
        if result.is_err() {
            self.forget_image(key, &mut events);
        } else {
            // Decoding finished; nothing references the pending state any more.
            self.release_if_unreferenced(key, &mut events);
        }
        events
    }

    /// Flush the reply owed for `key` with an exact protocol error and forget the image outright
    /// (used when displaying a transmitted image fails after it was registered). An unknown key
    /// (evicted meanwhile) still gets its placements cleaned and releases if a record remains.
    pub fn fail_pending(&mut self, key: ImageKey, error: KittyError) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(image) = self.images.get_mut(&key) {
            if let Some(reply) = image.reply.take() {
                push_reply(&mut events, reply, Err(error));
            }
            image.pending = false;
        }
        self.forget_image(key, &mut events);
        events
    }

    // --- placements -----------------------------------------------------------------------

    /// Display `key` with its top-left cell at stable line `line`, column `col`.
    ///
    /// A placement with the same `(key, pid)` (when `pid` is set) replaces the existing one. At
    /// [`MAX_PLACEMENTS`] the oldest placement is evicted first. Returns the events to send
    /// (`ImageReleased` for images that stopped being referenced).
    #[allow(clippy::too_many_arguments)]
    pub fn place(
        &mut self,
        screen: Screen,
        key: ImageKey,
        line: i64,
        col: usize,
        cols: u32,
        rows: u32,
        src: Option<Rect>,
        x_off: u32,
        y_off: u32,
        z: i32,
        pid: Option<u32>,
    ) -> Result<Vec<Event>, PlaceError> {
        if !self.images.contains_key(&key) {
            return Err(PlaceError::NotFound);
        }
        let mut events = Vec::new();
        if self.placement_count() >= MAX_PLACEMENTS && !self.evict_oldest_placement(&mut events) {
            return Err(PlaceError::Full);
        }
        if pid.is_some() {
            self.remove_placements_where(screen, |p| p.key == key && p.pid == pid, &mut events);
        }
        let placement = Placement { key, screen, line, col, cols, rows, src, x_off, y_off, z, pid };
        self.placements[screen.index()].entry(line).or_default().push(placement);
        Ok(events)
    }

    /// Create (or replace, for the same `pid`) a virtual placement for the image `id` names.
    /// `cols`/`rows` of 0 mean the whole image at its pixel size; the computed span is returned.
    /// Returns `None` when no stored image backs `key`.
    pub fn add_virtual(
        &mut self,
        id: u32,
        key: ImageKey,
        pid: Option<u32>,
        cols: u32,
        rows: u32,
    ) -> Option<(u32, u32)> {
        let (width_px, height_px) = self.image_size(key)?;
        let (cols, rows) = self.span_for(width_px, height_px, cols, rows);
        let list = self.virtuals.entry(id).or_default();
        match list.iter_mut().find(|v| v.pid == pid) {
            Some(existing) => {
                existing.key = key;
                existing.cols = cols;
                existing.rows = rows;
            },
            None => list.push(VirtualPlacement { key, pid, cols, rows }),
        }
        Some((cols, rows))
    }

    /// Resolve a placeholder cell's reference to `(key, cols, rows)`: the virtual placement of
    /// image `image_id` with placement id `pid`, or — when `pid` is absent or zero, where the
    /// protocol lets the terminal choose any — the first virtual placement of the image.
    pub fn kitty_virtual_image(
        &self,
        image_id: u32,
        pid: Option<u32>,
    ) -> Option<(ImageKey, u32, u32)> {
        let list = self.virtuals.get(&image_id)?;
        let placement = match pid.filter(|pid| *pid != 0) {
            Some(pid) => list.iter().find(|v| v.pid == Some(pid))?,
            None => list.first()?,
        };
        Some((placement.key, placement.cols, placement.rows))
    }

    // --- grid-following updates -----------------------------------------------------------

    /// Reconcile placements with a [`scroll_up`](crate::grid::Grid::scroll_up) of `positions` rows
    /// in screen-line range `region_start..region_end`; `window` is read from the grid afterwards.
    pub fn after_scroll_up(
        &mut self,
        window: GridWindow,
        screen: Screen,
        region_start: i64,
        region_end: i64,
        positions: usize,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let positions = positions as i64;
        if region_start != 0 {
            let from = window.stable + region_start;
            let to = window.stable + region_end;
            if region_end - region_start <= positions {
                // The grid reset the whole region: anchored placements lost their content.
                self.remove_placements_where(screen, |p| (from..to).contains(&p.line), &mut events);
                self.remove_placeholders_where(
                    screen,
                    |ph| (from..to).contains(&ph.line),
                    &mut events,
                );
            } else {
                // Content in `start + positions..end` moved up by `positions`; the region's first
                // `positions` rows were destroyed. Cells move with their content.
                self.shift_range(screen, from..to, -positions, from, &mut events);
            }
        }
        // A full-viewport rotation needs no shifts: its counter is in `window.stable`. Rows that
        // left the viewport are converted to placeholders or dropped here either way.
        self.prune_out_of_view(window, screen, &mut events);
        events
    }

    /// Reconcile placements with a [`scroll_down`](crate::grid::Grid::scroll_down) of `positions`
    /// rows in screen-line range `region_start..region_end`; `window` is read from the grid
    /// afterwards.
    pub fn after_scroll_down(
        &mut self,
        window: GridWindow,
        screen: Screen,
        region_start: i64,
        region_end: i64,
        positions: usize,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let positions = positions as i64;
        let from = window.stable + region_start;
        let to = window.stable + region_end;
        if region_end - region_start <= positions {
            self.remove_placements_where(screen, |p| (from..to).contains(&p.line), &mut events);
            self.remove_placeholders_where(screen, |ph| (from..to).contains(&ph.line), &mut events);
        } else {
            // Content moves down; rows pushed past the region's end were destroyed.
            self.shift_range(screen, from..to, positions, to, &mut events);
        }
        events
    }

    /// Remove everything anchored in stable lines `from..to` (those screen rows were wiped).
    pub fn wipe_range(&mut self, screen: Screen, from: i64, to: i64, events: &mut Vec<Event>) {
        self.remove_placements_where(screen, |p| (from..to).contains(&p.line), events);
        self.remove_placeholders_where(screen, |ph| (from..to).contains(&ph.line), events);
    }

    /// Drop placements and placeholders outside the grid's reach, and convert placements whose rows
    /// have fully left the viewport into [`Placeholder`] records unless
    /// [`set_scrollback_images`](Self::set_scrollback_images) says to keep them. A conversion
    /// releases the image when nothing else references it.
    pub fn prune_out_of_view(
        &mut self,
        window: GridWindow,
        screen: Screen,
        events: &mut Vec<Event>,
    ) {
        let oldest = window.stable - window.history as i64;
        let newest_exclusive = window.stable + window.lines as i64;
        let map = &self.placements[screen.index()];
        let doomed: Vec<(Placement, bool)> = map
            .values()
            .flat_map(|list| list.iter().copied())
            .filter_map(|p| {
                let index = p.line - window.stable;
                if index + i64::from(p.rows) > 0 {
                    return None; // still (partly) on screen
                }
                if p.line < oldest {
                    Some((p, false)) // evicted from scrollback: the rows are gone
                } else if !self.keep_scrollback {
                    Some((p, true)) // in scrollback: leave a placeholder behind
                } else {
                    None // kept for when it scrolls back into view
                }
            })
            .collect();
        for (p, convert) in doomed {
            let size = convert.then(|| self.image_size(p.key)).flatten();
            if let Some((width_px, height_px)) = size {
                self.placeholders[screen.index()].entry(p.line).or_default().push(Placeholder {
                    screen: p.screen,
                    line: p.line,
                    col: p.col,
                    pixel_size: (width_px, height_px),
                });
            }
            self.remove_placement(&p, events); // releases the image when nothing else needs it
        }
        let ph_dead: Vec<Placeholder> = {
            let map = &self.placeholders[screen.index()];
            map.iter()
                .filter(|(line, _)| **line < oldest || **line >= newest_exclusive)
                .flat_map(|(_, list)| list.iter().copied())
                .collect()
        };
        for ph in ph_dead {
            self.remove_placeholder(&ph);
        }
    }

    /// A resize or history change on `screen`: drop placements and placeholders whose anchor column
    /// no longer exists, then [`prune_out_of_view`](Self::prune_out_of_view). Row-only resizes keep
    /// stable lines intact; see the [module docs](self) for the reflow limit.
    pub fn after_resize(
        &mut self,
        window: GridWindow,
        screen: Screen,
        columns: usize,
        events: &mut Vec<Event>,
    ) {
        self.remove_placements_where(screen, |p| p.col >= columns, events);
        self.remove_placeholders_where(screen, |ph| ph.col >= columns, events);
        self.prune_out_of_view(window, screen, events);
    }

    // --- deletions ------------------------------------------------------------------------

    /// `d=a`/`d=A`: every placement intersecting the current view (`offset` rows scrolled up).
    /// Returns whether any went, for the caller's reply; `free_data` releases images that stop
    /// being referenced.
    pub fn delete_all_in_view(
        &mut self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let hits: Vec<ImageKey> = self.placements[screen.index()]
            .values()
            .flat_map(|list| list.iter().copied())
            .filter(|p| window.overlaps_view(p.line - window.stable, i64::from(p.rows), offset))
            .map(|p| p.key)
            .collect();
        let found = !hits.is_empty();
        self.remove_placements_where(
            screen,
            |p| window.overlaps_view(p.line - window.stable, i64::from(p.rows), offset),
            events,
        );
        if free_data {
            for key in hits {
                self.free_image(key, events);
            }
        }
        found
    }

    /// Placements covering grid cell `(stable, col)` — `d=c` at the cursor, `d=p`/`d=q` at a cell
    /// (`d=q` passes its z-index, and only placements at that z go). Placeholders are host-drawn
    /// text, not placements per the protocol, so delete selectors never remove them; only grid
    /// wipes and scroll-out do.
    pub fn delete_covering(
        &mut self,
        screen: Screen,
        stable: i64,
        col: usize,
        z: Option<i32>,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let covers = |p: &Placement| {
            p.line <= stable
                && stable < p.line + i64::from(p.rows)
                && p.col <= col
                && col < p.col + p.cols as usize
                && z.is_none_or(|z| p.z == z)
        };
        let hits: Vec<ImageKey> = self.placements[screen.index()]
            .range(stable.saturating_sub(MAX_SPAN_ROWS)..=stable)
            .flat_map(|(_, list)| list.iter().copied())
            .filter(&covers)
            .map(|p| p.key)
            .collect();
        let found = !hits.is_empty();
        self.remove_placements_where(screen, covers, events);
        if free_data {
            for key in hits {
                self.free_image(key, events);
            }
        }
        found
    }

    /// `d=y`/`d=Y`: placements intersecting grid row `stable`.
    pub fn delete_row(
        &mut self,
        screen: Screen,
        stable: i64,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let crosses = |p: &Placement| p.line <= stable && stable < p.line + i64::from(p.rows);
        let hits: Vec<ImageKey> = self.placements[screen.index()]
            .range(stable.saturating_sub(MAX_SPAN_ROWS)..=stable)
            .flat_map(|(_, list)| list.iter().copied())
            .filter(&crosses)
            .map(|p| p.key)
            .collect();
        let found = !hits.is_empty();
        self.remove_placements_where(screen, crosses, events);
        if free_data {
            for key in hits {
                self.free_image(key, events);
            }
        }
        found
    }

    /// `d=x`/`d=X`: placements intersecting column `col`, anywhere on `screen`.
    pub fn delete_column(
        &mut self,
        screen: Screen,
        col: usize,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let hits: Vec<ImageKey> = self.placements[screen.index()]
            .values()
            .flat_map(|list| list.iter().copied())
            .filter(|p| p.col <= col && col < p.col + p.cols as usize)
            .map(|p| p.key)
            .collect();
        let found = !hits.is_empty();
        self.remove_placements_where(
            screen,
            |p| p.col <= col && col < p.col + p.cols as usize,
            events,
        );
        if free_data {
            for key in hits {
                self.free_image(key, events);
            }
        }
        found
    }

    /// `d=z`/`d=Z`: placements at z-index `z`.
    pub fn delete_z(
        &mut self,
        screen: Screen,
        z: i32,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let hits: Vec<ImageKey> = self.placements[screen.index()]
            .values()
            .flat_map(|list| list.iter().copied())
            .filter(|p| p.z == z)
            .map(|p| p.key)
            .collect();
        let found = !hits.is_empty();
        self.remove_placements_where(screen, |p| p.z == z, events);
        if free_data {
            for key in hits {
                self.free_image(key, events);
            }
        }
        found
    }

    /// `d=i`/`d=I`: the image `key` names — all its placements, or (with `pid`) just that one.
    /// `free_data` also drops the stored image and its id references when nothing else needs them.
    pub fn delete_image(
        &mut self,
        key: ImageKey,
        pid: Option<u32>,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let hits = self.has_placement_of(key, pid);
        for screen in [Screen::Primary, Screen::Alternate] {
            self.remove_placements_where(
                screen,
                |p| p.key == key && pid.is_none_or(|pid| p.pid == Some(pid)),
                events,
            );
        }
        if free_data {
            self.free_image(key, events);
        }
        hits || self.images.contains_key(&key)
    }

    /// `d=n`/`d=N`: the same for the newest image with kitty number `number`, resolved here.
    pub fn delete_number(
        &mut self,
        number: u32,
        pid: Option<u32>,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        match self.numbers.get(&number).copied() {
            Some(key) => self.delete_image(key, pid, free_data, events),
            None => false,
        }
    }

    /// `d=r`/`d=R`: every placement of images whose id is in `from..=to`, and with `free_data` the
    /// images themselves.
    pub fn delete_id_range(
        &mut self,
        from: u32,
        to: u32,
        free_data: bool,
        events: &mut Vec<Event>,
    ) -> bool {
        let keys: Vec<ImageKey> = self.ids.range(from..=to).map(|(_, key)| *key).collect();
        let found = !keys.is_empty();
        for key in keys {
            self.delete_image(key, None, free_data, events);
        }
        found
    }

    /// Remove every placement of `screen` (leaving the alternate screen drops its images; their
    /// stored data stays available while an id or number still names it). Placeholders are text on
    /// that screen's rows and go with them.
    pub fn remove_screen(&mut self, screen: Screen, events: &mut Vec<Event>) {
        let taken = std::mem::take(&mut self.placements[screen.index()]);
        for (_, list) in taken {
            for p in list {
                if !self.references(p.key) {
                    self.drop_image(p.key, events);
                }
            }
        }
        let _ = std::mem::take(&mut self.placeholders[screen.index()]);
    }

    /// Forget everything: images, ids, virtuals, placements, placeholders (`RIS`). Releases every
    /// image that had a record, exactly once each. Keys are never reused, even after a reset.
    pub fn reset_all(&mut self, events: &mut Vec<Event>) {
        for screen in [Screen::Primary, Screen::Alternate] {
            let _ = std::mem::take(&mut self.placements[screen.index()]);
            let _ = std::mem::take(&mut self.placeholders[screen.index()]);
        }
        self.virtuals.clear();
        self.ids.clear();
        self.numbers.clear();
        let keys: Vec<ImageKey> = self.images.keys().copied().collect();
        for key in keys {
            self.drop_image(key, events);
        }
    }

    /// Turn the oldest placement fully inside scrollback into a placeholder and release its image
    /// outright — id/number references and virtuals included — so the embedder can enforce its own
    /// memory cap. Returns the evicted key and the events to send, or `None` when scrollback holds
    /// no placement.
    pub fn evict_oldest_scrollback_image(
        &mut self,
        window: GridWindow,
        screen: Screen,
    ) -> Option<(ImageKey, Vec<Event>)> {
        let oldest = self.placements[screen.index()]
            .values()
            .flat_map(|list| list.iter().copied())
            .filter(|p| p.line + i64::from(p.rows) <= window.stable) // fully above the viewport
            .min_by_key(|p| (p.key, p.line))?;
        let key = oldest.key;
        let size = self.image_size(key);
        let mut events = Vec::new();
        self.remove_placement(&oldest, &mut events);
        if let Some((width_px, height_px)) = size {
            self.placeholders[screen.index()].entry(oldest.line).or_default().push(Placeholder {
                screen: oldest.screen,
                line: oldest.line,
                col: oldest.col,
                pixel_size: (width_px, height_px),
            });
        }
        // Eviction is forced: the embedder asked for this image's memory back. Other placements of
        // the same image go with it; its reply, if any, dies unanswered.
        self.forget_image(key, &mut events);
        Some((key, events))
    }

    // --- queries --------------------------------------------------------------------------

    /// The placements of `screen` intersecting the current view (`offset` rows scrolled up), top
    /// row relative to the top of the view (negative when partly above it), in draw order.
    pub fn visible_placements(
        &self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
    ) -> Vec<VisiblePlacement> {
        let mut out: Vec<VisiblePlacement> = self.placements[screen.index()]
            .values()
            .flat_map(|list| list.iter())
            .filter(|p| window.overlaps_view(p.line - window.stable, i64::from(p.rows), offset))
            .map(|p| VisiblePlacement {
                key: p.key,
                row_in_view: (p.line - window.stable + offset as i64) as i32,
                col: p.col,
                cols: p.cols,
                rows: p.rows,
                src: p.src,
                x_off: p.x_off,
                y_off: p.y_off,
                z: p.z,
            })
            .collect();
        out.sort_by_key(|p| (p.row_in_view, p.col, p.z));
        out
    }

    /// The placeholders of `screen` whose row is in view, as the text to draw for them.
    pub fn visible_placeholders(
        &self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
    ) -> Vec<VisiblePlaceholder> {
        let mut out: Vec<VisiblePlaceholder> = self.placeholders[screen.index()]
            .values()
            .flat_map(|list| list.iter())
            .filter(|ph| window.in_view(ph.line - window.stable, offset))
            .map(|ph| VisiblePlaceholder {
                row_in_view: (ph.line - window.stable + offset as i64) as i32,
                col: ph.col,
                text: format!("[image {}×{}]", ph.pixel_size.0, ph.pixel_size.1),
            })
            .collect();
        out.sort_by_key(|p| (p.row_in_view, p.col));
        out
    }

    // --- internals ------------------------------------------------------------------------

    fn placement_count(&self) -> usize {
        self.placements.iter().map(|map| map.values().map(Vec::len).sum::<usize>()).sum()
    }

    /// Move placements and placeholders anchored in `range` by `delta` stable lines. Entries whose
    /// new line leaves the range are dropped (that side of the region lost its content): `edge` is
    /// the bound past which movement destroys (`from` for scroll up, `to` for scroll down).
    fn shift_range(
        &mut self,
        screen: Screen,
        range: Range<i64>,
        delta: i64,
        edge: i64,
        events: &mut Vec<Event>,
    ) {
        let moved: Vec<Placement> = self.placements[screen.index()]
            .iter()
            .filter(|(line, _)| range.contains(line))
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        for p in moved {
            let new_line = p.line + delta;
            let destroyed = if delta < 0 { new_line < edge } else { new_line >= edge };
            if destroyed || !range.contains(&new_line) {
                self.remove_placement(&p, events);
            } else {
                self.move_placement(p, new_line);
            }
        }
        let phs: Vec<Placeholder> = self.placeholders[screen.index()]
            .iter()
            .filter(|(line, _)| range.contains(line))
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        for ph in phs {
            let new_line = ph.line + delta;
            if range.contains(&new_line) {
                self.move_placeholder(ph, new_line);
            } else {
                self.remove_placeholder(&ph);
            }
        }
    }

    fn remove_placements_where(
        &mut self,
        screen: Screen,
        pred: impl Fn(&Placement) -> bool,
        events: &mut Vec<Event>,
    ) {
        let map = &mut self.placements[screen.index()];
        let dead_keys: Vec<ImageKey> = {
            let dead: Vec<Placement> =
                map.values().flat_map(|list| list.iter().copied()).filter(|p| pred(p)).collect();
            for p in &dead {
                if let Some(list) = map.get_mut(&p.line) {
                    list.retain(|other| !pred(other));
                    if list.is_empty() {
                        map.remove(&p.line);
                    }
                }
            }
            dead.iter().map(|p| p.key).collect()
        };
        for key in dead_keys {
            self.release_if_unreferenced(key, events);
        }
    }

    fn remove_placeholders_where(
        &mut self,
        screen: Screen,
        pred: impl Fn(&Placeholder) -> bool,
        _events: &mut Vec<Event>,
    ) {
        let map = &mut self.placeholders[screen.index()];
        let dead_lines: Vec<i64> =
            map.iter().filter(|(_, l)| l.iter().any(&pred)).map(|(line, _)| *line).collect();
        for line in dead_lines {
            if let Some(mut list) = map.remove(&line) {
                list.retain(|ph| !pred(ph));
                if !list.is_empty() {
                    map.insert(line, list);
                }
            }
        }
    }

    fn remove_placement(&mut self, p: &Placement, events: &mut Vec<Event>) {
        let key = p.key;
        let map = &mut self.placements[p.screen.index()];
        if let Some(list) = map.get_mut(&p.line) {
            list.retain(|other| !same_placement(other, p));
            if list.is_empty() {
                map.remove(&p.line);
            }
        }
        self.release_if_unreferenced(key, events);
    }

    fn move_placement(&mut self, p: Placement, new_line: i64) {
        let screen = p.screen.index();
        if let Some(list) = self.placements[screen].get_mut(&p.line) {
            list.retain(|other| !same_placement(other, &p));
            if list.is_empty() {
                self.placements[screen].remove(&p.line);
            }
        }
        let mut moved = p;
        moved.line = new_line;
        self.placements[screen].entry(new_line).or_default().push(moved);
    }

    fn remove_placeholder(&mut self, ph: &Placeholder) {
        let map = &mut self.placeholders[ph.screen.index()];
        if let Some(list) = map.get_mut(&ph.line) {
            list.retain(|other| !(other.col == ph.col && other.pixel_size == ph.pixel_size));
            if list.is_empty() {
                map.remove(&ph.line);
            }
        }
    }

    fn move_placeholder(&mut self, ph: Placeholder, new_line: i64) {
        let screen = ph.screen.index();
        if let Some(list) = self.placeholders[screen].get_mut(&ph.line) {
            list.retain(|other| !(other.col == ph.col && other.pixel_size == ph.pixel_size));
            if list.is_empty() {
                self.placeholders[screen].remove(&ph.line);
            }
        }
        let mut moved = ph;
        moved.line = new_line;
        self.placeholders[screen].entry(new_line).or_default().push(moved);
    }

    fn has_placement_of(&self, key: ImageKey, pid: Option<u32>) -> bool {
        self.placements.iter().flatten().any(|(_, list)| {
            list.iter().any(|p| p.key == key && pid.is_none_or(|pid| p.pid == Some(pid)))
        })
    }

    /// Whether anything still references `key`: a placement, a virtual placement, an id or number
    /// map entry (a stored image a client can re-place without resending), or an in-flight decode.
    fn references(&self, key: ImageKey) -> bool {
        self.still_referenced_elsewhere(key) || self.images.get(&key).is_some_and(|i| i.pending)
    }

    /// [`references`](Self::references) without the in-flight-decode check, and ignoring whatever
    /// the caller has already removed from the placement maps.
    fn still_referenced_elsewhere(&self, key: ImageKey) -> bool {
        self.ids.values().any(|k| *k == key)
            || self.numbers.values().any(|k| *k == key)
            || self.virtuals.values().flatten().any(|v| v.key == key)
            || self.placements.iter().flatten().any(|(_, list)| list.iter().any(|p| p.key == key))
    }

    /// Release `key` (one `ImageReleased`) when nothing references it any more.
    fn release_if_unreferenced(&mut self, key: ImageKey, events: &mut Vec<Event>) {
        if !self.references(key) && self.images.contains_key(&key) {
            self.drop_image(key, events);
        }
    }

    /// Remove the stored record and release, unconditionally (callers have cleared or are clearing
    /// every other reference).
    fn drop_image(&mut self, key: ImageKey, events: &mut Vec<Event>) {
        if self.images.remove(&key).is_some() {
            events.push(Event::ImageReleased(key));
        }
    }

    /// Forget `key` entirely: its placements go first (so their images may release), then id and
    /// number entries, virtuals and the record; releases `key` itself exactly once. A reply that
    /// was still owed dies unanswered. Placeholders keep only text and stay.
    fn forget_image(&mut self, key: ImageKey, events: &mut Vec<Event>) {
        if !self.images.contains_key(&key) {
            return;
        }
        for screen in [Screen::Primary, Screen::Alternate] {
            self.remove_placements_where(screen, |p| p.key == key, events);
        }
        let stale_ids: Vec<u32> =
            self.ids.iter().filter(|(_, k)| **k == key).map(|(id, _)| *id).collect();
        for id in stale_ids {
            self.ids.remove(&id);
        }
        let stale_numbers: Vec<u32> =
            self.numbers.iter().filter(|(_, k)| **k == key).map(|(n, _)| *n).collect();
        for n in stale_numbers {
            self.numbers.remove(&n);
        }
        let virtual_ids: Vec<u32> = self
            .virtuals
            .iter()
            .filter(|(_, list)| list.iter().any(|v| v.key == key))
            .map(|(id, _)| *id)
            .collect();
        for id in virtual_ids {
            if let Some(list) = self.virtuals.get_mut(&id) {
                list.retain(|v| v.key != key);
                if list.is_empty() {
                    self.virtuals.remove(&id);
                }
            }
        }
        self.drop_image(key, events);
    }

    /// Uppercase delete: free the stored image outright — record, id/number entries and virtuals
    /// included, exactly as the protocol's data-freeing selectors specify — even while an id still
    /// names it. Its placements go too, wherever they are; a pending reply dies unanswered.
    fn free_image(&mut self, key: ImageKey, events: &mut Vec<Event>) {
        if self.images.contains_key(&key) {
            self.forget_image(key, events);
        }
    }

    /// Evict the oldest image nothing references (the protocol's preference), else the oldest with
    /// placements (those go with it). `false` when there is nothing to evict.
    fn evict_oldest_image(&mut self, events: &mut Vec<Event>) -> bool {
        let unreferenced =
            self.images.keys().copied().filter(|k| !self.still_referenced_elsewhere(*k)).min();
        if let Some(key) = unreferenced {
            self.forget_image(key, events);
            return true;
        }
        match self.images.keys().copied().min() {
            Some(key) => {
                self.forget_image(key, events);
                true
            },
            None => false,
        }
    }

    /// Remove the oldest placement (lowest key, then line, then column) to make room. `false` when
    /// the table holds none.
    fn evict_oldest_placement(&mut self, events: &mut Vec<Event>) -> bool {
        let oldest = self
            .placements
            .iter()
            .flatten()
            .flat_map(|(_, list)| list.iter())
            .min_by_key(|p| (p.key, p.line, p.col))
            .copied();
        match oldest {
            Some(p) => {
                self.remove_placement(&p, events);
                true
            },
            None => false,
        }
    }
}

/// Whether two entries are the same placement (identity, for removal from the maps).
fn same_placement(a: &Placement, b: &Placement) -> bool {
    a.key == b.key
        && a.line == b.line
        && a.col == b.col
        && a.z == b.z
        && a.pid == b.pid
        && a.src == b.src
}

fn div_ceil(v: u32, d: u32) -> u32 {
    div_ceil_u64(u64::from(v), u64::from(d)) as u32
}

fn div_ceil_u64(v: u64, d: u64) -> u64 {
    v.div_ceil(d)
}

/// `display * height_px / width_px`, the other dimension's display size preserving the aspect.
fn pixel_aspect(width_px: u32, height_px: u32, display: u64) -> u64 {
    if width_px == 0 {
        return display;
    }
    (display * u64::from(height_px) + u64::from(width_px) - 1) / u64::from(width_px)
}

fn push_reply(events: &mut Vec<Event>, reply: PendingReply, result: Result<(), KittyError>) {
    if let Some(bytes) = kitty::reply(reply.id, reply.number, reply.placement, result, reply.quiet)
    {
        events.push(Event::PtyWrite(String::from_utf8_lossy(&bytes).into_owned()));
    }
}
