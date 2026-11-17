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
//! Growth is capped against hostile streams: [`MAX_PLACEMENTS`] placements and as many virtual
//! placements, [`MAX_STORED_IMAGES`] images, [`MAX_PENDING_DECODES`] transmissions awaiting a
//! decode result and [`MAX_PLACEHOLDERS`] placeholder records at once; overflowing evicts the
//! oldest, releasing whatever stops being referenced. A row-only resize needs nothing
//! (stable lines survive it); a column resize moves each placement with its first cell — through
//! reflow exactly as the cursor follows it ([`crate::term::Term::resize`], and
//! [`apply_reflow`](Self::apply_reflow) below) — and an anchor that cannot follow becomes its
//! placeholder instead of vanishing. Placeholder line anchors inside a reflowed region are not
//! tracked (they are host-drawn text, not content), so after a column resize they stay on their
//! old stable lines; only the cap and the width prune touch them.
//!
//! An image stays available (not released) while anything still references it: a placement, a
//! virtual placement, an id or number map entry (a stored image a client can re-place without
//! resending), or an in-flight decode. Kitty images therefore live until deleted, reset or evicted;
//! sixel and inline images with no live placement are released as soon as their last one goes.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use crate::event::{Event, ImageDecodeError, ImageKey};
use crate::graphics::kitty::{self, KittyError, Rect};
use crate::graphics::limits::MAX_IMAGE_SIDE;

/// The most placements the table keeps at once (virtual placements are capped to the same number
/// separately), and the eviction point for each: over the cap the oldest is removed.
pub const MAX_PLACEMENTS: usize = 4_096;

/// The most images (decoded or awaiting decode) the table tracks at once; over it the oldest image
/// without live references is evicted first, then the oldest with them.
pub const MAX_STORED_IMAGES: usize = 512;

/// The most transmissions that may await a decode result at once; [`new_image`](Self::new_image)
/// refuses beyond it.
pub const MAX_PENDING_DECODES: usize = 64;

/// The most placeholder records the table keeps across both screens; over it the oldest is
/// dropped (they are a few bytes each, but a hostile stream must not grow them without bound).
pub const MAX_PLACEHOLDERS: usize = 100_000;

/// A conservative bound on placement height — [`MAX_IMAGE_SIDE`] cells, which is what span
/// computation clamps to — bounding how far back a visible-range query must look for tall
/// placements anchored above it and how far `delete_covering`/`delete_row` scans.
const MAX_SPAN_ROWS: i64 = MAX_IMAGE_SIDE as i64;

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
/// cells that refer to its image id. Stored in [`PlacementTable::virtuals`] keyed by
/// `(image id, placement id)`; `seq` is its eviction order stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VirtualEntry {
    seq: u64,
    key: ImageKey,
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
    /// Virtual placements keyed by `(image id, placement id)`.
    virtuals: HashMap<(u32, Option<u32>), VirtualEntry>,
    /// Virtual placement eviction order: oldest sequence first.
    virtual_order: BTreeMap<u64, (u32, Option<u32>)>,
    virtual_seq: u64,
    /// Placements by stable line (per screen), in insertion order within a line.
    placements: [BTreeMap<i64, Vec<Placement>>; 2],
    placeholders: [BTreeMap<i64, Vec<Placeholder>>; 2],
    /// One entry per live placeholder: its `(line, col)` cell (the dedupe key) to its eviction
    /// stamp.
    placeholder_stamp: [BTreeMap<(i64, usize), u64>; 2],
    /// Placeholder eviction order across both screens: oldest stamp first.
    placeholder_age: BTreeMap<u64, (usize, i64, usize)>,
    placeholder_seq: u64,
    /// Live placeholder records across both screens.
    placeholder_count: usize,
    /// Per-image count of live references from the placement maps, the virtual placements and the
    /// id/number maps — what [`still_referenced_elsewhere`](Self::still_referenced_elsewhere)
    /// answers from instead of scanning ([`references`](Self::references) adds in-flight decodes).
    refs: HashMap<ImageKey, u32>,
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
            virtual_order: BTreeMap::new(),
            virtual_seq: 0,
            placements: [BTreeMap::new(), BTreeMap::new()],
            placeholders: [BTreeMap::new(), BTreeMap::new()],
            placeholder_stamp: [BTreeMap::new(), BTreeMap::new()],
            placeholder_age: BTreeMap::new(),
            placeholder_seq: 0,
            placeholder_count: 0,
            refs: HashMap::new(),
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
    /// or from the pixel size and the cell size), rounded up, at least 1x1 and at most
    /// [`MAX_IMAGE_SIDE`] cells per axis (a hostile request cannot ask for a span the terminal
    /// could not clamp to the screen).
    pub fn span_for(&self, width_px: u32, height_px: u32, cols: u32, rows: u32) -> (u32, u32) {
        let (cw, ch) = self.cell_px;
        match (cols, rows) {
            (0, 0) => (
                clamp_cells(div_ceil_u64(u64::from(width_px), u64::from(cw))),
                clamp_cells(div_ceil_u64(u64::from(height_px), u64::from(ch))),
            ),
            (c, 0) => {
                // `c` columns wide keeping the aspect: the display is `c * cw` pixels wide.
                let height_display =
                    pixel_aspect(width_px, height_px, u64::from(c) * u64::from(cw));
                (
                    clamp_cells(u64::from(c)),
                    clamp_cells(div_ceil_u64(height_display, u64::from(ch))),
                )
            },
            (0, r) => {
                let width_display = pixel_aspect(width_px, height_px, u64::from(r) * u64::from(ch));
                (clamp_cells(div_ceil_u64(width_display, u64::from(cw))), clamp_cells(u64::from(r)))
            },
            (c, r) => (clamp_cells(u64::from(c)), clamp_cells(u64::from(r))),
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
            let old = self.ids.insert(id, key);
            self.retarget(old, key);
        }
        if let Some(number) = number {
            let old = self.numbers.insert(number, key);
            self.retarget(old, key);
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
        self.add_ref(key);
        Ok(events)
    }

    /// Create (or replace, for the same `pid`) a virtual placement for the image `id` names.
    /// `cols`/`rows` of 0 mean the whole image at its pixel size; the computed span is returned
    /// (`None` when no stored image backs `key`), alongside the events an eviction produced.
    /// Virtual placements are capped at [`MAX_PLACEMENTS`] in total: over it the oldest evicts,
    /// releasing its image when nothing else references it.
    pub fn add_virtual(
        &mut self,
        id: u32,
        key: ImageKey,
        pid: Option<u32>,
        cols: u32,
        rows: u32,
    ) -> (Option<(u32, u32)>, Vec<Event>) {
        let Some((width_px, height_px)) = self.image_size(key) else {
            return (None, Vec::new());
        };
        let (cols, rows) = self.span_for(width_px, height_px, cols, rows);
        let vk = (id, pid);
        match self.virtuals.get_mut(&vk) {
            Some(existing) => {
                let replaced = (existing.key != key).then_some(existing.key);
                existing.cols = cols;
                existing.rows = rows;
                if let Some(old) = replaced {
                    // The entry re-points at another image: move the counted reference.
                    self.drop_ref(old);
                    self.add_ref(key);
                    self.virtuals.get_mut(&vk).expect("still present").key = key;
                }
            },
            None => {
                let seq = self.virtual_seq;
                self.virtual_seq += 1;
                self.virtuals.insert(vk, VirtualEntry { seq, key, cols, rows });
                self.virtual_order.insert(seq, vk);
                self.add_ref(key);
            },
        }
        let mut events = Vec::new();
        while self.virtuals.len() > MAX_PLACEMENTS {
            if !self.evict_oldest_virtual(&mut events) {
                break;
            }
        }
        (Some((cols, rows)), events)
    }

    /// Resolve a placeholder cell's reference to `(key, cols, rows)`: the virtual placement of
    /// image `image_id` with placement id `pid`, or — when `pid` is absent or zero, where the
    /// protocol lets the terminal choose any — the first (oldest) virtual placement of the image.
    pub fn kitty_virtual_image(
        &self,
        image_id: u32,
        pid: Option<u32>,
    ) -> Option<(ImageKey, u32, u32)> {
        let vk = match pid.filter(|pid| *pid != 0) {
            Some(pid) => (image_id, Some(pid)),
            None => *self.virtual_order.iter().find(|(_, (id, _))| *id == image_id)?.1,
        };
        let entry = self.virtuals.get(&vk)?;
        Some((entry.key, entry.cols, entry.rows))
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
        } else {
            // A rotation starting at the top raised the counter by `positions`. It needs no
            // shifts for the rows that rode up (the counter absorbs them), but with a bottom
            // margin the grid swapped the rows fixed below the region back to their screen rows,
            // so anchors anchored there must move with the counter to keep the same row.
            let base = window.stable - positions;
            self.shift_anchored(screen, base + region_end..base + window.lines as i64, positions);
        }
        // Rows that left the viewport are converted to placeholders or dropped here either way.
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
        // Only placements anchored above the viewport's top row can have left it; scanning stops
        // at `window.stable` instead of walking the whole map.
        let doomed: Vec<(Placement, bool)> = self.placements[screen.index()]
            .range(..window.stable)
            .flat_map(|(_, list)| list.iter().copied())
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
                self.insert_placeholder(Placeholder {
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
            std::iter::empty()
                .chain(map.range(..oldest).flat_map(|(_, list)| list.iter().copied()))
                .chain(map.range(newest_exclusive..).flat_map(|(_, list)| list.iter().copied()))
                .collect()
        };
        for ph in ph_dead {
            self.remove_placeholder(&ph);
        }
    }

    /// The placements anchored on `screen`, ordered by stable line then column — the anchor list
    /// to hand a column resize (`Term::resize` collects it, runs the grid reflow, and passes the
    /// outcomes back); [`apply_reflow`](Self::apply_reflow) consumes it in order.
    pub fn anchor_list(&self, screen: Screen) -> Vec<Placement> {
        self.placements[screen.index()].values().flat_map(|list| list.iter().copied()).collect()
    }

    /// Apply a column resize's per-anchor outcomes to the table. `remapped` pairs each entry of
    /// [`anchor_list`](Self::anchor_list) with where its first cell went: `(line, col, false)` when
    /// it followed the reflow, or `(line, col, true)` when it cannot follow — its column was cut
    /// (no reflow) or its line was merged away — and it becomes its placeholder instead of being
    /// removed silently.
    pub fn apply_reflow(
        &mut self,
        remapped: &[(Placement, i64, usize, bool)],
        events: &mut Vec<Event>,
    ) {
        for (p, new_line, new_col, lost) in remapped {
            let p = *p;
            if *lost {
                let size = self.image_size(p.key);
                self.remove_placement(&p, events);
                if let Some((width_px, height_px)) = size {
                    self.insert_placeholder(Placeholder {
                        screen: p.screen,
                        line: *new_line,
                        col: *new_col,
                        pixel_size: (width_px, height_px),
                    });
                }
            } else if *new_line != p.line || *new_col != p.col {
                let screen = p.screen.index();
                if let Some(list) = self.placements[screen].get_mut(&p.line) {
                    if let Some(pos) = list.iter().position(|other| same_placement(other, &p)) {
                        list.swap_remove(pos);
                    }
                    if list.is_empty() {
                        self.placements[screen].remove(&p.line);
                    }
                }
                let mut moved = p;
                moved.line = *new_line;
                moved.col = *new_col;
                self.placements[screen].entry(*new_line).or_default().push(moved);
                // Same placement, new anchor: the reference count is unchanged.
            }
        }
    }

    /// Finish a resize on `screen`: drop placeholder records whose column no longer exists (they
    /// are text on rows that were cut; placements are handled by
    /// [`apply_reflow`](Self::apply_reflow) or keep spans that merely overflow the narrower grid),
    /// then [`prune_out_of_view`](Self::prune_out_of_view). Row-only resizes keep stable lines
    /// intact.
    pub fn after_resize(
        &mut self,
        window: GridWindow,
        screen: Screen,
        columns: usize,
        events: &mut Vec<Event>,
    ) {
        self.remove_placeholders_where(screen, |ph| ph.col >= columns, events);
        self.prune_out_of_view(window, screen, events);
    }

    // --- deletions ------------------------------------------------------------------------

    /// `d=a`/`d=A`: every placement intersecting the current view (`offset` rows scrolled up).
    /// Returns whether any went, for the caller's reply. `free_data` (uppercase) additionally
    /// frees a hit image's stored data — but only when no placement of it survives elsewhere on
    /// either screen; a placement still on screen or in scrollback needs the pixels.
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
            self.free_hits(hits, events);
        }
        found
    }

    /// Uppercase delete's data-freeing: for each hit image (duplicated hits counted once), forget
    /// it outright when no placement of it remains anywhere; otherwise its data stays for the
    /// placements that still reference it.
    fn free_hits(&mut self, mut hits: Vec<ImageKey>, events: &mut Vec<Event>) {
        hits.sort_unstable();
        hits.dedup();
        for key in hits {
            if !self.has_placement_of(key, None) {
                self.forget_image(key, events);
            }
        }
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
            self.free_hits(hits, events);
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
            self.free_hits(hits, events);
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
            self.free_hits(hits, events);
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
            self.free_hits(hits, events);
        }
        found
    }

    /// `d=i`/`d=I`: the image `key` names — all its placements, or (with `pid`) just that one.
    /// `free_data` (uppercase) also drops the stored image and its id/number references and
    /// virtuals — but only when no placement of it survives on either screen; a surviving
    /// placement keeps the pixels alive.
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
        if free_data && !self.has_placement_of(key, None) {
            self.forget_image(key, events);
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
        let mut touched: Vec<ImageKey> = Vec::new();
        for (_, list) in taken {
            for p in list {
                self.drop_ref(p.key);
                touched.push(p.key);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for key in touched {
            self.release_if_unreferenced(key, events);
        }
        let idx = screen.index();
        for &seq in self.placeholder_stamp[idx].values() {
            self.placeholder_age.remove(&seq);
        }
        self.placeholder_stamp[idx].clear();
        self.placeholder_count -= self.placeholders[idx].values().map(Vec::len).sum::<usize>();
        let _ = std::mem::take(&mut self.placeholders[idx]);
    }

    /// Forget everything: images, ids, virtuals, placements, placeholders (`RIS`). Releases every
    /// image that had a record, exactly once each. Keys are never reused, even after a reset.
    pub fn reset_all(&mut self, events: &mut Vec<Event>) {
        for screen in [Screen::Primary, Screen::Alternate] {
            let _ = std::mem::take(&mut self.placements[screen.index()]);
            let _ = std::mem::take(&mut self.placeholders[screen.index()]);
            self.placeholder_stamp[screen.index()].clear();
        }
        self.placeholder_age.clear();
        self.placeholder_count = 0;
        self.virtuals.clear();
        self.virtual_order.clear();
        self.ids.clear();
        self.numbers.clear();
        self.refs.clear();
        let keys: Vec<ImageKey> = self.images.keys().copied().collect();
        for key in keys {
            self.drop_image(key, events);
        }
    }

    /// Turn the oldest placement fully above the viewport into its placeholder, releasing the
    /// image only when nothing else references it (other placements, virtuals, id/number entries
    /// or an in-flight decode keep it alive) — so the embedder can drain scrollback toward its own
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
        // Removing the placement already releases the image exactly when nothing else references
        // it; whatever still does keeps its pixels.
        self.remove_placement(&oldest, &mut events);
        if let Some((width_px, height_px)) = size {
            self.insert_placeholder(Placeholder {
                screen: oldest.screen,
                line: oldest.line,
                col: oldest.col,
                pixel_size: (width_px, height_px),
            });
        }
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
        self.visible_placements_scanned(window, screen, offset).0
    }

    /// [`visible_placements`](Self::visible_placements) plus the number of stable lines its map
    /// scan touched — a frame query must stay inside the visible range plus one clamped image's
    /// height of lookback for tall placements (test hook).
    pub(crate) fn visible_placements_scanned(
        &self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
    ) -> (Vec<VisiblePlacement>, usize) {
        let start = (window.stable - offset as i64).saturating_sub(MAX_SPAN_ROWS);
        let end = window.stable + window.lines as i64 - offset as i64;
        let mut out: Vec<VisiblePlacement> = Vec::new();
        let mut scanned = 0usize;
        for (_, list) in self.placements[screen.index()].range(start..end) {
            scanned += 1;
            for p in list {
                if window.overlaps_view(p.line - window.stable, i64::from(p.rows), offset) {
                    out.push(VisiblePlacement {
                        key: p.key,
                        row_in_view: (p.line - window.stable + offset as i64) as i32,
                        col: p.col,
                        cols: p.cols,
                        rows: p.rows,
                        src: p.src,
                        x_off: p.x_off,
                        y_off: p.y_off,
                        z: p.z,
                    });
                }
            }
        }
        out.sort_by_key(|p| (p.row_in_view, p.col, p.z));
        (out, scanned)
    }

    /// The placeholders of `screen` whose row is in view, as the text to draw for them.
    pub fn visible_placeholders(
        &self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
    ) -> Vec<VisiblePlaceholder> {
        self.visible_placeholders_scanned(window, screen, offset).0
    }

    /// [`visible_placeholders`](Self::visible_placeholders) plus the number of stable lines its
    /// map scan touched (test hook; placeholders are single-row records, so the query needs no
    /// lookback).
    pub(crate) fn visible_placeholders_scanned(
        &self,
        window: GridWindow,
        screen: Screen,
        offset: usize,
    ) -> (Vec<VisiblePlaceholder>, usize) {
        let start = window.stable - offset as i64;
        let end = window.stable + window.lines as i64 - offset as i64;
        let mut out: Vec<VisiblePlaceholder> = Vec::new();
        let mut scanned = 0usize;
        for (_, list) in self.placeholders[screen.index()].range(start..end) {
            scanned += 1;
            for ph in list {
                if window.in_view(ph.line - window.stable, offset) {
                    out.push(VisiblePlaceholder {
                        row_in_view: (ph.line - window.stable + offset as i64) as i32,
                        col: ph.col,
                        text: format!("[image {}×{}]", ph.pixel_size.0, ph.pixel_size.1),
                    });
                }
            }
        }
        out.sort_by_key(|p| (p.row_in_view, p.col));
        (out, scanned)
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

    /// Move everything anchored in stable lines `range` by `delta`, with no destroy rule: the
    /// rows they point at stay on screen, only the counter moved underneath them. Resize uses
    /// this on the alternate grid, whose row passes slide the viewport without an anchor
    /// observer (see `Term::resize`).
    pub fn shift_anchored(&mut self, screen: Screen, range: Range<i64>, delta: i64) {
        if delta == 0 || range.is_empty() {
            return;
        }
        let moved: Vec<Placement> = self.placements[screen.index()]
            .range(range.clone())
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        for p in moved {
            self.move_placement(p, p.line + delta);
        }
        let phs: Vec<Placeholder> = self.placeholders[screen.index()]
            .range(range)
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        for ph in phs {
            self.move_placeholder(ph, ph.line + delta);
        }
    }

    fn remove_placements_where(
        &mut self,
        screen: Screen,
        pred: impl Fn(&Placement) -> bool,
        events: &mut Vec<Event>,
    ) {
        let idx = screen.index();
        // The collected entries are exactly what the `retain` passes below remove, so each drops
        // one counted reference — once, not once per matching pass.
        let dead: Vec<Placement> = self.placements[idx]
            .values()
            .flat_map(|list| list.iter().copied())
            .filter(|p| pred(p))
            .collect();
        if dead.is_empty() {
            return;
        }
        for p in &dead {
            if let Some(list) = self.placements[idx].get_mut(&p.line) {
                list.retain(|other| !pred(other));
                if list.is_empty() {
                    self.placements[idx].remove(&p.line);
                }
            }
        }
        for p in &dead {
            self.drop_ref(p.key);
        }
        let mut keys: Vec<ImageKey> = dead.iter().map(|p| p.key).collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            self.release_if_unreferenced(key, events);
        }
    }

    fn remove_placeholders_where(
        &mut self,
        screen: Screen,
        pred: impl Fn(&Placeholder) -> bool,
        _events: &mut Vec<Event>,
    ) {
        let idx = screen.index();
        let dead_lines: Vec<i64> = self.placeholders[idx]
            .iter()
            .filter(|(_, l)| l.iter().any(&pred))
            .map(|(line, _)| *line)
            .collect();
        for line in dead_lines {
            if let Some(list) = self.placeholders[idx].remove(&line) {
                let mut keep = Vec::new();
                for ph in list {
                    if pred(&ph) {
                        if let Some(seq) = self.placeholder_stamp[idx].remove(&(line, ph.col)) {
                            self.placeholder_age.remove(&seq);
                            self.placeholder_count -= 1;
                        }
                    } else {
                        keep.push(ph);
                    }
                }
                if !keep.is_empty() {
                    self.placeholders[idx].insert(line, keep);
                }
            }
        }
    }

    fn remove_placement(&mut self, p: &Placement, events: &mut Vec<Event>) {
        let key = p.key;
        // Remove one occurrence by identity: identical twins (a `pid`-less image placed twice at
        // the same cell) each need their own call, and this drops only the reference it removed.
        let idx = p.screen.index();
        let went = {
            match self.placements[idx].get_mut(&p.line) {
                Some(list) => match list.iter().position(|other| same_placement(other, p)) {
                    Some(pos) => {
                        list.swap_remove(pos);
                        true
                    },
                    None => false,
                },
                None => false,
            }
        };
        if went {
            if let Some(list) = self.placements[idx].get(&p.line) {
                if list.is_empty() {
                    self.placements[idx].remove(&p.line);
                }
            }
            self.drop_ref(key);
            self.release_if_unreferenced(key, events);
        }
    }

    fn move_placement(&mut self, p: Placement, new_line: i64) {
        let screen = p.screen.index();
        if let Some(list) = self.placements[screen].get_mut(&p.line) {
            if let Some(pos) = list.iter().position(|other| same_placement(other, &p)) {
                list.swap_remove(pos);
            }
            if list.is_empty() {
                self.placements[screen].remove(&p.line);
            }
        }
        let mut moved = p;
        moved.line = new_line;
        self.placements[screen].entry(new_line).or_default().push(moved);
    }

    /// Remove the placeholder record anchored at `(line, col)`, if any; `true` when one went.
    fn placeholder_take(&mut self, screen: usize, line: i64, col: usize) -> bool {
        let Some(seq) = self.placeholder_stamp[screen].remove(&(line, col)) else {
            return false;
        };
        self.placeholder_age.remove(&seq);
        if let Some(list) = self.placeholders[screen].get_mut(&line) {
            list.retain(|other| other.col != col);
            if list.is_empty() {
                self.placeholders[screen].remove(&line);
            }
        }
        self.placeholder_count -= 1;
        true
    }

    /// Record a placeholder: dedupe per cell (the newest record is the current truth about that
    /// row and column), cap the total, and drop the oldest records past it.
    fn insert_placeholder(&mut self, ph: Placeholder) {
        let screen = ph.screen.index();
        self.placeholder_take(screen, ph.line, ph.col);
        let seq = self.placeholder_seq;
        self.placeholder_seq += 1;
        self.placeholders[screen].entry(ph.line).or_default().push(ph);
        self.placeholder_stamp[screen].insert((ph.line, ph.col), seq);
        self.placeholder_age.insert(seq, (screen, ph.line, ph.col));
        self.placeholder_count += 1;
        while self.placeholder_count > MAX_PLACEHOLDERS {
            let oldest = match self.placeholder_age.first_key_value() {
                Some((_, &(screen, line, col))) => (screen, line, col),
                // unreachable while the count agrees with the maps
                None => break,
            };
            self.placeholder_take(oldest.0, oldest.1, oldest.2);
        }
    }

    fn remove_placeholder(&mut self, ph: &Placeholder) {
        self.placeholder_take(ph.screen.index(), ph.line, ph.col);
    }

    /// Move a placeholder record to `new_line`, keeping its eviction stamp; the moving record is
    /// the newest at that cell and replaces any record already there.
    fn move_placeholder(&mut self, ph: Placeholder, new_line: i64) {
        let screen = ph.screen.index();
        let Some(seq) = self.placeholder_stamp[screen].get(&(ph.line, ph.col)).copied() else {
            return;
        };
        self.placeholder_take(screen, ph.line, ph.col);
        self.placeholder_take(screen, new_line, ph.col);
        let mut moved = ph;
        moved.line = new_line;
        self.placeholders[screen].entry(new_line).or_default().push(moved);
        self.placeholder_stamp[screen].insert((new_line, ph.col), seq);
        self.placeholder_age.insert(seq, (screen, new_line, ph.col));
        self.placeholder_count += 1;
    }

    fn has_placement_of(&self, key: ImageKey, pid: Option<u32>) -> bool {
        self.placements.iter().flatten().any(|(_, list)| {
            list.iter().any(|p| p.key == key && pid.is_none_or(|pid| p.pid == Some(pid)))
        })
    }

    // --- reference counting ---------------------------------------------------------------

    /// Count one live reference to `key` (a placement pushed, a virtual entry added, a map entry
    /// pointed at it).
    fn add_ref(&mut self, key: ImageKey) {
        *self.refs.entry(key).or_insert(0) += 1;
    }

    /// Undo one counted reference the caller has just removed from its map.
    fn drop_ref(&mut self, key: ImageKey) {
        let n = self.refs.get_mut(&key).expect("a counted reference");
        *n -= 1;
    }

    /// Record that a map entry (an id or number slot) used to point at `old`, if any, and now
    /// points at `key`.
    fn retarget(&mut self, old: Option<ImageKey>, key: ImageKey) {
        match old {
            Some(old) if old != key => {
                self.drop_ref(old);
                self.add_ref(key);
            },
            Some(_) => {},
            None => self.add_ref(key),
        }
    }

    /// Whether anything still references `key`: a placement, a virtual placement, an id or number
    /// map entry (a stored image a client can re-place without resending), or an in-flight decode.
    fn references(&self, key: ImageKey) -> bool {
        self.still_referenced_elsewhere(key) || self.images.get(&key).is_some_and(|i| i.pending)
    }

    /// [`references`](Self::references) without the in-flight-decode check, answered from the
    /// per-image count maintained by every map mutation.
    fn still_referenced_elsewhere(&self, key: ImageKey) -> bool {
        self.refs.get(&key).is_some_and(|n| *n > 0)
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
            if self.ids.remove(&id) == Some(key) {
                self.drop_ref(key);
            }
        }
        let stale_numbers: Vec<u32> =
            self.numbers.iter().filter(|(_, k)| **k == key).map(|(n, _)| *n).collect();
        for n in stale_numbers {
            if self.numbers.remove(&n) == Some(key) {
                self.drop_ref(key);
            }
        }
        let stale_virtuals: Vec<(u32, Option<u32>)> =
            self.virtuals.iter().filter(|(_, e)| e.key == key).map(|(vk, _)| *vk).collect();
        for vk in stale_virtuals {
            if let Some(e) = self.virtuals.remove(&vk) {
                self.virtual_order.remove(&e.seq);
                self.drop_ref(e.key);
            }
        }
        self.drop_image(key, events);
        self.refs.remove(&key);
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

    /// Remove the oldest virtual placement (lowest sequence stamp) to enforce the cap, releasing
    /// its image when nothing else references it. `false` when there is none.
    fn evict_oldest_virtual(&mut self, events: &mut Vec<Event>) -> bool {
        let Some((_, vk)) = self.virtual_order.pop_first() else {
            return false;
        };
        if let Some(e) = self.virtuals.remove(&vk) {
            self.drop_ref(e.key);
            self.release_if_unreferenced(e.key, events);
        }
        true
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

/// A computed cell span: at least 1 and at most [`MAX_IMAGE_SIDE`] per axis.
fn clamp_cells(cells: u64) -> u32 {
    cells.clamp(1, u64::from(MAX_IMAGE_SIDE)) as u32
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
