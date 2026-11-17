// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Tests for [`PlacementTable`] as driven through [`Term`]: placements following the grid,
//! scrollback placeholders, ids and replies.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::event::{Event, EventListener, ImageData, ImageDecodeError, ImageKey};
use crate::graphics::kitty_diacritics::{
    PLACEHOLDER_CHAR, ROW_COLUMN_DIACRITICS, decode_placeholder_cell,
};
use crate::graphics::limits::MAX_IMAGE_SIDE;
use crate::graphics::placements::{
    GridWindow, MAX_PLACEHOLDERS, MAX_PLACEMENTS, PlacementTable, Screen,
};
use crate::grid::{Dimensions, Scroll};
use crate::index::{Column, Line, Point};
use crate::term::{Config, Term};
use crate::vte::ansi::{Processor, Rgb};

/// The concatenation of everything an event batch wrote back to the pty (kitty replies ride on it).
fn writes_of(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PtyWrite(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn released_of(events: &[Event]) -> Vec<ImageKey> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ImageReleased(key) => Some(*key),
            _ => None,
        })
        .collect()
}

fn received_of(events: &[Event]) -> Vec<(ImageKey, ImageData)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ImageReceived { key, data } => Some((*key, data.clone())),
            _ => None,
        })
        .collect()
}

/// A test grid size.
struct Size {
    columns: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn columns(&self) -> usize {
        self.columns
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn total_lines(&self) -> usize {
        self.lines
    }
}

/// Collects every event the terminal sends, so tests can assert on deliveries.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

impl Recorder {
    /// Everything collected since the last drain, clearing the log.
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn received(&self) -> Vec<(ImageKey, ImageData)> {
        self.take()
            .into_iter()
            .filter_map(|event| match event {
                Event::ImageReceived { key, data } => Some((key, data)),
                _ => None,
            })
            .collect()
    }

    fn released(&self) -> Vec<ImageKey> {
        self.take()
            .into_iter()
            .filter_map(|event| match event {
                Event::ImageReleased(key) => Some(key),
                _ => None,
            })
            .collect()
    }

    /// The concatenation of everything written back to the pty (kitty replies ride on it).
    fn writes(&self) -> String {
        self.take()
            .into_iter()
            .filter_map(|event| match event {
                Event::PtyWrite(text) => Some(text),
                _ => None,
            })
            .collect()
    }
}

/// A terminal with a recording listener and its recorder.
fn term(columns: usize, lines: usize) -> (Term<Recorder>, Recorder) {
    let recorder = Recorder::default();
    let term = Term::new(
        Config { scrolling_history: 10_000, ..Config::default() },
        &Size { columns, lines },
        recorder.clone(),
    );
    (term, recorder)
}

fn feed(term: &mut Term<Recorder>, bytes: &[u8]) {
    let mut parser: Processor = Processor::new();
    parser.advance(term, bytes);
}

/// A complete `a=T` transmission of raw pixels, placed with any extra control keys (`,c=5,r=2`
/// style string) appended to the frame.
fn transmit(
    id: u32,
    format: &str,
    size: Option<(u32, u32)>,
    pixels: &[u8],
    extra: &str,
) -> Vec<u8> {
    let mut out = format!("\x1b_Ga=T,i={id},{format}{extra}").into_bytes();
    if let Some((width, height)) = size {
        out.extend(format!(",s={width},v={height}").as_bytes());
    }
    out.push(b';');
    out.extend(STANDARD.encode(pixels).into_bytes());
    out.extend(b"\x1b\\");
    out
}

/// RGBA payload for a `f=32` image of the given size.
fn rgba(width: u32, height: u32) -> Vec<u8> {
    vec![0x7F; (width as usize) * (height as usize) * 4]
}

/// RGB payload for a `f=24` image of the given size.
fn rgb(width: u32, height: u32) -> Vec<u8> {
    vec![0x7F; (width as usize) * (height as usize) * 3]
}

/// A one-pixel black sixel sequence: DCS `q`, register 0, one painted column.
const SIXEL: &[u8] = b"\x1bPq#0?~\x1b\\";

/// The minimal valid 1x1 RGBA PNG (header through IEND).
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// The viewport row and column of the terminal's cursor.
fn cursor(term: &Term<Recorder>) -> (i32, usize) {
    let point = term.renderable_content().cursor.point;
    (point.line.0, point.column.0)
}

#[test]
fn sixel_places_at_cursor_and_moves_cursor() {
    let (mut term, recorder) = term(10, 10);
    feed(&mut term, b"\x1b[3;4H"); // row 2, column 3

    feed(&mut term, SIXEL);
    let images = recorder.received();
    assert_eq!(images.len(), 1, "one image was received");
    let (key, data) = &images[0];
    let (width, height) = match data {
        ImageData::Rgba { width, height, pixels } => {
            assert_eq!(pixels.len(), (*width as usize) * (*height as usize) * 4);
            (*width, *height)
        },
        ImageData::Encoded { .. } => panic!("sixels deliver decoded pixels"),
    };
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].key, *key);
    assert_eq!((placements[0].row_in_view, placements[0].col), (2, 3));
    assert_eq!(placements[0].cols, width); // cell size is unset: each cell counts as one pixel
    assert_eq!(placements[0].rows, height);

    // DECSDM off (the default): the cursor lands below the image, same column.
    assert_eq!(cursor(&term), (2 + height as i32, 3));
    assert!(recorder.released().is_empty(), "the placement still references it");

    // DECSDM set: a second sixel displays without moving the cursor.
    feed(&mut term, b"\x1b[?80h\x1b[5;6H");
    let before = cursor(&term);
    feed(&mut term, SIXEL);
    assert_eq!(cursor(&term), before, "DECSDM set parks the cursor");
    assert_eq!(term.visible_placements().len(), 2);

    // Scrolling both images off converts them to placeholders and releases each exactly once.
    for _ in 0..20 {
        feed(&mut term, b"\n");
    }
    assert_eq!(term.visible_placements().len(), 0);
    assert_eq!(recorder.released().len(), 2);
}

#[test]
fn kitty_place_with_cells() {
    let (mut term, recorder) = term(20, 20);
    term.set_cell_pixel_size(8, 16);
    feed(&mut term, b"\x1b[4;3H"); // row 3, column 2

    // A 40x32 RGBA image displayed as 5 cells by 2.
    let seq = transmit(1, "f=32", Some((40, 32)), &rgba(40, 32), ",c=5,r=2");
    feed(&mut term, &seq);
    let events = recorder.take();
    let received = received_of(&events);
    assert_eq!(received.len(), 1);
    let key = received[0].0;
    assert!(writes_of(&events).contains("i=1"), "raw pixels are answered immediately");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].key, key);
    assert_eq!((placements[0].row_in_view, placements[0].col), (3, 2));
    assert_eq!((placements[0].cols, placements[0].rows), (5, 2));
    // `C=0`: last row of the image, one cell past its last column.
    assert_eq!(cursor(&term), (4, 7));

    // A second placement with a pid adds one; re-placing the same pid replaces it.
    feed(&mut term, b"\x1b[1;1H\x1b_Ga=p,i=1,p=7,c=2,r=1\x1b\\");
    assert_eq!(term.visible_placements().len(), 2);
    feed(&mut term, b"\x1b[6;1H\x1b_Ga=p,i=1,p=7,c=3,r=1\x1b\\");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 2, "the same pid replaces its placement");
    assert!(placements.iter().any(|p| p.row_in_view == 5 && p.col == 0 && p.cols == 3));
    assert!(placements.iter().any(|p| p.row_in_view == 3 && p.col == 2), "the original survives");

    // `C=1` places without moving the cursor.
    feed(&mut term, b"\x1b[2;2H\x1b_Ga=p,i=1,C=1\x1b\\");
    assert_eq!(cursor(&term), (1, 1));
}

#[test]
fn span_from_pixels_rounds_up() {
    let (mut term, recorder) = term(20, 20);
    term.set_cell_pixel_size(10, 20);
    feed(&mut term, b"\x1b[1;1H");

    // 25x21 pixels with no `c`/`r`: ceil(25/10) x ceil(21/20) cells.
    let seq = transmit(1, "f=24", Some((25, 21)), &rgb(25, 21), "");
    feed(&mut term, &seq);
    recorder.received();
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].cols, placements[0].rows), (3, 2));

    // One axis given keeps the aspect: 2 columns wide is 20px wide, and 21 source pixels tall
    // scales to under one 20px row — which still rounds up to one.
    feed(&mut term, b"\x1b[5;1H");
    let seq = transmit(2, "f=24", Some((25, 21)), &rgb(25, 21), ",c=2");
    feed(&mut term, &seq);
    recorder.received();
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 2);
    let second = placements.iter().find(|p| p.row_in_view == 4).unwrap();
    assert_eq!((second.cols, second.rows), (2, 1));
}

#[test]
fn tiny_and_oversized_spans() {
    let (mut term, recorder) = term(10, 10);

    // A 1x1 pixel image at the very last cell: span at least 1x1, cursor clamps to (9, 0).
    feed(&mut term, b"\x1b[10;10H");
    let seq = transmit(1, "f=24", Some((1, 1)), &rgb(1, 1), "");
    feed(&mut term, &seq);
    recorder.received();
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert!((placements[0].cols, placements[0].rows) >= (1, 1));
    assert_eq!((placements[0].row_in_view, placements[0].col), (9, 9));
    assert_eq!(cursor(&term), (9, 0), "the cursor wraps past the image onto its last row");

    // A six-row image whose bottom lands exactly on the screen's last row: the three rows
    // that do not fit scroll away and the anchor moves up with them — like six newlines
    // would — so the whole image stays visible, ending on the last row.
    feed(&mut term, b"\x1b[8;1H");
    let seq = transmit(3, "f=24", Some((1, 6)), &[9u8; 18], "");
    feed(&mut term, &seq);
    recorder.received();
    let placements = term.visible_placements();
    let middle = placements.iter().find(|p| p.rows == 6).unwrap();
    assert_eq!((middle.row_in_view, middle.col), (4, 0));
    assert_eq!(cursor(&term), (9, 1), "last row of the image, one cell past its last column");

    // A 500x300 pixel image from the top: it scrolls as text would and is clipped below.
    feed(&mut term, b"\x1b[1;1H");
    let seq = transmit(2, "f=24", Some((500, 300)), &rgb(500, 300), "");
    feed(&mut term, &seq);
    recorder.received();
    let placements = term.visible_placements();
    let big = placements.iter().find(|p| p.rows > 100).unwrap();
    assert_eq!((big.cols, big.rows), (500, 300));
    assert_eq!(big.row_in_view, 0, "its anchor is the cursor row after scrolling");
    assert_eq!(cursor(&term), (9, 9), "the cursor clamps to the screen's last cell");
}

#[test]
fn sixel_at_the_bottom_of_the_screen_is_fully_visible() {
    // An image tool run from a prompt sits at the bottom of a full screen. The image's rows
    // behave like its newlines: the screen scrolls to fit it, the anchor moves up with the
    // content that scrolled, and — because the DECSDM-off cursor rule wants the line below
    // the image, which does not exist when the image ends on the region's bottom row — one
    // more line scrolls so the cursor really lands below the image, as it does mid-screen.
    let (mut term, recorder) = term(10, 5);
    term.set_cell_pixel_size(1, 6); // one sixel band per cell row
    feed(&mut term, b"\x1b[5;1H"); // the screen's last row
    feed(&mut term, b"\x1bPq#0~-~\x1b\\"); // a 1x12 px sixel: two rows tall

    assert_eq!(recorder.received().len(), 1);
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].cols, placements[0].rows), (1, 2));
    assert_eq!(
        (placements[0].row_in_view, placements[0].col),
        (2, 0),
        "the whole image is on screen, clear of the bottom edge"
    );
    assert_eq!(cursor(&term), (4, 0), "the cursor sits below the image, same column");
    assert!(recorder.released().is_empty(), "the placement still references it");
}

#[test]
fn kitty_transmission_at_the_bottom_scrolls_its_text_up_with_it() {
    // Five lines fill a screen; an `a=T` image taller than the space left below the cursor
    // scrolls exactly as its own newlines would: the earlier text rides up into history,
    // the cursor moves up with its line, and the image ends fully on screen instead of half
    // hanging off the bottom edge.
    let (mut term, recorder) = term(10, 5); // cell size unset: each pixel is a cell
    feed(&mut term, b"1\r\n2\r\n3\r\n4\r\n5"); // cursor on line 4, column 1
    feed(&mut term, &transmit(1, "f=24", Some((1, 3)), &[7u8; 9], ",q=2")); // three rows tall

    let events = recorder.take();
    assert_eq!(received_of(&events).len(), 1);
    assert_eq!(writes_of(&events), "", "q=2 answers nothing");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].row_in_view, placements[0].col), (2, 1));
    assert_eq!((placements[0].cols, placements[0].rows), (1, 3), "the whole image is on screen");

    // The text moved up with its line rather than scrolling away for nothing: the last three
    // lines now sit at rows 0..2.
    let grid = term.grid();
    for (line, want) in [(0i32, '3'), (1, '4'), (2, '5')] {
        let cell = grid.cell(Point { line: Line(line), column: Column(0) });
        assert_eq!(cell.c, want);
    }
}

#[test]
fn scroll_moves_placements() {
    let (mut term, recorder) = term(10, 10);
    feed(&mut term, b"\x1b[3;7r"); // scroll region rows 2..6
    feed(&mut term, b"\x1b[5;1H");

    // A placement anchored at row 4 inside the region.
    let seq = transmit(1, "f=32", Some((8, 8)), &rgba(8, 8), ",c=1,r=2");
    feed(&mut term, &seq);
    recorder.received();
    assert_eq!(term.visible_placements()[0].row_in_view, 4);

    // Inserting two lines at region row 3 shoves content (and the anchor) down by two.
    feed(&mut term, b"\x1b[4;1H\x1b[2L");
    assert_eq!(term.visible_placements()[0].row_in_view, 6);

    // Deleting them again moves it back.
    feed(&mut term, b"\x1b[4;1H\x1b[2M");
    assert_eq!(term.visible_placements()[0].row_in_view, 4);

    // A full-viewport scroll needs no shifts: the anchor rides its own line into history while
    // the viewport slides up to the counter. Feed text until the screen scrolls twice.
    feed(&mut term, b"\x1b[r"); // reset the region to the whole screen
    feed(&mut term, b"\x1b[10;1H");
    for _ in 0..2 {
        feed(&mut term, b"x\n");
    }
    let placements = term.visible_placements();
    let moved = placements.iter().find(|p| p.key.0 == 0).unwrap();
    // Scrolled two rows up: row_in_view 4 - 2 (the image is not fully out yet).
    assert_eq!(moved.row_in_view, 2);
    assert!(
        recorder
            .take()
            .iter()
            .all(|event| !matches!(event, Event::ImageReceived { .. } | Event::ImageReleased(_)))
    );
}

#[test]
fn clear_resize_and_screen_switch() {
    let (mut term, recorder) = term(12, 12);

    // ED 2 removes placements anchored in the viewport and releases their images.
    feed(&mut term, b"\x1b[4;1H");
    feed(&mut term, SIXEL);
    assert_eq!(term.visible_placements().len(), 1);
    recorder.received();
    feed(&mut term, b"\x1b[2J");
    assert_eq!(term.visible_placements().len(), 0);
    let released = recorder.released();
    assert_eq!(released.len(), 1, "ED 2 releases the sixel exactly once");

    // ED 3 removes scrollback placements (with images kept, they were alive there until now).
    term.set_scrollback_images(true);
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, SIXEL);
    let scrollback_key = recorder.received()[0].0;
    for _ in 0..14 {
        feed(&mut term, b"\n");
    }
    assert!(recorder.released().is_empty(), "kept placements are not released");
    feed(&mut term, b"\x1b[3J");
    assert_eq!(recorder.released(), vec![scrollback_key], "ED 3 releases it once");
    term.set_scrollback_images(false);

    // RIS removes everything: every stored image reports its release exactly once.
    feed(&mut term, b"\x1b[2;2H");
    let seq = transmit(20, "f=24", Some((2, 2)), &rgb(2, 2), "");
    feed(&mut term, &seq);
    feed(&mut term, SIXEL);
    recorder.take(); // the receives are not what this step asserts
    feed(&mut term, b"\x1bc");
    let mut released = recorder.released();
    released.sort_unstable();
    assert_eq!(released.len(), 2, "the kitty image and the sixel both release");
    assert_ne!(released[0], released[1], "two distinct images, each exactly once");
    assert_eq!(term.visible_placements().len(), 0);

    // Leaving the alternate screen drops its placements; the primary screen's survive.
    feed(&mut term, SIXEL);
    let primary_key = recorder.received()[0].0;
    feed(&mut term, b"\x1b[?1049h");
    assert_eq!(term.visible_placements().len(), 0, "the alt screen starts without images");
    feed(&mut term, SIXEL);
    let alt_key = recorder.received()[0].0;
    assert_eq!(term.visible_placements().len(), 1);
    feed(&mut term, b"\x1b[?1049l");
    assert_eq!(recorder.released(), vec![alt_key], "leaving the alt screen releases its image");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].key, primary_key, "the primary placement is back in view");

    // A column resize drops placements whose anchor column reflowed away; a row-only resize
    // keeps anchors.
    feed(&mut term, b"\x1b[1;11H");
    let seq = transmit(30, "f=32", Some((24, 8)), &rgba(24, 8), ",c=3,r=1");
    feed(&mut term, &seq);
    recorder.received();
    term.resize(Size { columns: 5, lines: 12 });
    assert!(
        !term.visible_placements().iter().any(|p| p.key.0 > primary_key.0 && p.cols == 3),
        "the placement anchored past the new width is gone"
    );
    assert!(recorder.released().is_empty(), "its id still references the stored image");

    feed(&mut term, b"\x1b[3;1H");
    feed(&mut term, SIXEL);
    let row_key = recorder.received()[0].0;
    term.resize(Size { columns: 5, lines: 16 });
    assert!(
        term.visible_placements().iter().any(|p| p.key == row_key),
        "a row-only resize keeps anchors"
    );
    assert!(recorder.released().is_empty());
}

#[test]
fn scrolled_off_becomes_placeholder() {
    let (mut term, recorder) = term(10, 10);
    feed(&mut term, b"\x1b[1;1H");

    feed(&mut term, SIXEL);
    let images = recorder.received();
    assert_eq!(images.len(), 1);
    let (key, data) = &images[0];
    let ImageData::Rgba { width, height, .. } = data else { panic!("decoded pixels") };
    let (width, height) = (*width, *height);

    // Scroll the image fully out of view: exactly one release, and a placeholder on its line.
    for _ in 0..10 {
        feed(&mut term, b"\n");
    }
    assert_eq!(recorder.released(), vec![*key]);
    assert!(term.visible_placements().is_empty());

    // The placeholder says what scrolled off, as text on its row.
    use crate::grid::Scroll;
    term.scroll_display(Scroll::Top);
    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1);
    assert_eq!(placeholders[0].row_in_view, 0);
    assert_eq!(placeholders[0].col, 0);
    assert_eq!(placeholders[0].text, format!("[image {width}\u{d7}{height}]"));

    // It is text: scrolling back down past it takes the placeholder with the rows.
    term.scroll_display(Scroll::Bottom);
    assert!(term.visible_placeholders().is_empty());
    let _ = key;
}

#[test]
fn scrollback_kept_when_enabled() {
    let (mut term, recorder) = term(10, 10);
    term.set_scrollback_images(true);
    feed(&mut term, b"\x1b[1;1H");

    feed(&mut term, SIXEL);
    let key = recorder.received()[0].0;

    // Scroll it out of view: kept, and nothing is released.
    for _ in 0..10 {
        feed(&mut term, b"\n");
    }
    assert!(recorder.released().is_empty());
    assert!(term.visible_placeholders().is_empty(), "kept placements leave no placeholder");

    // It scrolls back into view unchanged.
    use crate::grid::Scroll;
    term.scroll_display(Scroll::Top);
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].key, key);
    assert_eq!(placements[0].row_in_view, 0);

    // And back out of view again with no further image events (scrolling itself may notify the
    // host about other things).
    term.scroll_display(Scroll::Bottom);
    assert!(term.visible_placements().is_empty());
    let rest = recorder.take();
    assert!(
        rest.iter()
            .all(|event| !matches!(event, Event::ImageReceived { .. } | Event::ImageReleased(_))),
        "scrolling kept images makes no image events"
    );
}

#[test]
fn evict_oldest_scrollback_image() {
    let (mut term, recorder) = term(10, 10);
    term.set_scrollback_images(true);
    feed(&mut term, b"\x1b[1;1H");

    // Two images into scrollback: the first placed is the oldest.
    feed(&mut term, SIXEL);
    let first = recorder.received()[0].0;
    for _ in 0..8 {
        feed(&mut term, b"\n");
    }
    feed(&mut term, SIXEL);
    let second = recorder.received()[0].0;
    for _ in 0..8 {
        feed(&mut term, b"\n");
    }
    assert!(recorder.released().is_empty(), "both are kept so far");

    // The embedder's cap: evicting takes the oldest placement fully above the viewport first.
    let evicted = term.evict_oldest_scrollback_image();
    assert_eq!(evicted, Some(first));
    assert_eq!(recorder.released(), vec![first], "eviction releases outright");

    use crate::grid::Scroll;
    term.scroll_display(Scroll::Top);
    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1, "the evicted placement became a placeholder");
    term.scroll_display(Scroll::Bottom);
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "the newer one is still in view and kept");
    assert_eq!(placements[0].key, second);

    // Scroll the second one fully into scrollback too, then drain the table.
    for _ in 0..8 {
        feed(&mut term, b"\n");
    }
    assert_eq!(term.evict_oldest_scrollback_image(), Some(second));
    assert_eq!(recorder.released(), vec![second]);
    assert_eq!(term.evict_oldest_scrollback_image(), None);
}

#[test]
fn virtual_placement_resolves_placeholder_cells() {
    let (mut term, recorder) = term(20, 20);
    term.set_cell_pixel_size(8, 16);

    // `U=1` records a virtual placement: no grid placement exists, but the id resolves.
    let seq = transmit(3, "f=24", Some((80, 80)), &rgb(80, 80), ",U=1,c=10,r=5");
    feed(&mut term, &seq);
    let events = recorder.take();
    let key = received_of(&events)[0].0;
    assert!(writes_of(&events).contains("OK"));
    assert!(term.visible_placements().is_empty(), "a virtual placement displays nothing itself");

    let resolved = term.kitty_virtual_image(3, None).expect("the id resolves");
    assert_eq!(resolved, (key, 10, 5));
    assert_eq!(term.kitty_virtual_image(3, Some(9)), None, "no virtual placement has that pid");
    assert_eq!(term.kitty_virtual_image(4, None), None, "unknown ids resolve to nothing");

    // A printed placeholder cell decodes to (image id, row, column) and resolves through Term.
    let mut cell = crate::term::cell::Cell {
        c: PLACEHOLDER_CHAR,
        fg: crate::vte::ansi::Color::Spec(Rgb { r: 0, g: 0, b: 3 }),
        ..crate::term::cell::Cell::default()
    };
    cell.push_zerowidth('\u{30D}'); // table index 1
    cell.push_zerowidth('\u{30E}'); // table index 2
    assert_eq!(decode_placeholder_cell(&cell), Some((3, 1, 2)));
    let resolved = term.kitty_virtual_image(3, None).unwrap();
    assert_eq!(resolved.0, key, "the renderer can resolve the cell to the stored image");

    // A third mark carries the high id byte; fewer than two marks inherit from the left.
    cell.push_zerowidth('\u{305}'); // table index 0
    assert_eq!(decode_placeholder_cell(&cell), Some((3, 1, 2)));
    let short = {
        let mut c = crate::term::cell::Cell {
            c: PLACEHOLDER_CHAR,
            fg: crate::vte::ansi::Color::Spec(Rgb { r: 0, g: 0, b: 3 }),
            ..crate::term::cell::Cell::default()
        };
        c.push_zerowidth('\u{30D}');
        c
    };
    assert_eq!(decode_placeholder_cell(&short), None);
}

#[test]
fn kitty_delete_selectors() {
    let (mut term, recorder) = term(12, 12);

    // Three placements: a sixel across rows 0..5, kitty id 3 on rows 1..2, and id 4 with pid 9
    // at z=5 on row 8.
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, SIXEL);
    let sixel_key = recorder.received()[0].0;
    feed(&mut term, b"\x1b[2;1H");
    feed(&mut term, &transmit(3, "f=32", Some((32, 32)), &rgba(32, 32), ",c=4,r=2"));
    let id3_key = recorder.received()[0].0;
    feed(&mut term, b"\x1b[9;1H");
    feed(&mut term, &transmit(4, "f=32", Some((8, 8)), &rgba(8, 8), ",c=1,r=1,p=9,z=5"));
    let id4_key = recorder.received()[0].0;
    assert_eq!(term.visible_placements().len(), 3);

    // `d=Y` (uppercase: placements and data) removes what crosses viewport row 9 — the z-5
    // image, whose key releases. With neither `i` nor `I` in the frame there is no answer.
    recorder.take();
    feed(&mut term, b"\x1b_Ga=d,d=Y,y=9\x1b\\");
    let events = recorder.take();
    assert_eq!(writes_of(&events), "", "an unnumbered delete answers nothing");
    assert_eq!(released_of(&events), vec![id4_key]);
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 2);
    assert!(!placements.iter().any(|p| p.key == id4_key));

    // `d=i` (lowercase) removes one image's placements but keeps the stored data.
    feed(&mut term, b"\x1b_Ga=d,d=i,i=3\x1b\\");
    let events = recorder.take();
    assert!(writes_of(&events).contains("OK"));
    assert!(released_of(&events).is_empty(), "lowercase deletes keep the data");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert!(!placements.iter().any(|p| p.key == id3_key));

    // `d=a` clears the view: the sixel (no id) releases; the numbered kitty image does not.
    feed(&mut term, b"\x1b_Ga=d,d=a\x1b\\");
    assert_eq!(recorder.released(), vec![sixel_key]);
    assert!(term.visible_placements().is_empty());

    // `d=I` frees an image by id outright, even with no placements left.
    feed(&mut term, b"\x1b_Ga=d,d=I,i=3\x1b\\");
    assert_eq!(recorder.released(), vec![id3_key]);

    // Unknown ids answer ENOENT.
    feed(&mut term, b"\x1b_Ga=d,d=i,i=99\x1b\\");
    assert!(recorder.writes().contains("ENOENT"));
}

#[test]
fn decode_failure_sends_ebadpng() {
    let (mut term, recorder) = term(10, 10);
    feed(&mut term, b"\x1b[1;1H");

    // A PNG transmission: the reply waits for the embedder's decode result.
    let seq = format!("\x1b_Ga=T,i=9,f=100,m=0;{}\x1b\\", STANDARD.encode(PNG_1X1));
    feed(&mut term, seq.as_bytes());
    let events = recorder.take();
    let received = received_of(&events);
    assert_eq!(received.len(), 1);
    let (key, data) = &received[0];
    match data {
        ImageData::Encoded { bytes, width, height, .. } => {
            assert_eq!(bytes.as_slice(), PNG_1X1);
            assert_eq!((*width, *height), (1, 1));
        },
        ImageData::Rgba { .. } => panic!("PNG travels encoded"),
    }
    assert_eq!(term.visible_placements().len(), 1, "it is laid out before decoding");
    assert!(writes_of(&events).is_empty(), "the reply waits on the decode result");

    // Failure: EBADPNG travels to the client, and the placements go with the image.
    term.image_decoded(*key, Err(ImageDecodeError::Corrupt));
    let events = recorder.take();
    let writes = writes_of(&events);
    assert!(writes.contains("i=9") && writes.contains("EBADPNG"));
    assert_eq!(released_of(&events), vec![*key]);
    assert!(term.visible_placements().is_empty());

    // Success answers OK and keeps the placement.
    feed(&mut term, b"\x1b[2;1H");
    let seq = format!("\x1b_Ga=T,i=10,f=100,m=0;{}\x1b\\", STANDARD.encode(PNG_1X1));
    feed(&mut term, seq.as_bytes());
    let good = received_of(&recorder.take())[0].0;
    term.image_decoded(good, Ok(()));
    let events = recorder.take();
    let writes = writes_of(&events);
    assert!(writes.contains("i=10") && writes.contains("OK"));
    assert_eq!(term.visible_placements().len(), 1);
    assert!(released_of(&events).is_empty());

    // `q=2` suppresses even the failure reply.
    feed(&mut term, b"\x1b[3;1H");
    let seq = format!("\x1b_Ga=T,i=11,f=100,m=0,q=2;{}\x1b\\", STANDARD.encode(PNG_1X1));
    feed(&mut term, seq.as_bytes());
    let quiet = received_of(&recorder.take())[0].0;
    term.image_decoded(quiet, Err(ImageDecodeError::TooLarge));
    let events = recorder.take();
    assert!(writes_of(&events).is_empty(), "q=2 hides every reply");
    assert_eq!(released_of(&events), vec![quiet]);

    term.tick(Instant::now()); // the stall watchdog must not trip on idle parsers
}

#[test]
fn huge_span_does_not_panic() {
    let (mut term, recorder) = term(12, 6);
    feed(&mut term, b"\x1b[1;1H");

    // A cell count near `u32::MAX` once overflowed the cursor arithmetic below the image:
    // `rows as i32` wrapped negative and the next printed character panicked.
    let seq = transmit(9, "f=32", Some((1, 1)), &rgba(1, 1), ",r=4294967295");
    feed(&mut term, &seq);
    recorder.received();
    feed(&mut term, b"x"); // the print that used to index a negative line

    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "the placement survives with its clamped span");
    assert_eq!(placements[0].rows, MAX_IMAGE_SIDE, "an `r` above the cap clamps to it");
    // The cursor parks at the bottom row (its column is the clamped image's right edge).
    assert_eq!(cursor(&term).0, 5, "the cursor stops at the bottom row, not past it");
}

#[test]
fn bottom_margin_scroll_keeps_fixed_row_placements() {
    let (mut term, recorder) = term(8, 5);
    feed(&mut term, b"\x1b[1;4r"); // DECSTBM: rows 0..3 scroll, row 4 is fixed below it
    feed(&mut term, b"\x1b[5;1H");
    feed(&mut term, SIXEL); // the placement anchors on the fixed row
    recorder.received();

    feed(&mut term, b"\x1b[4;1H"); // cursor back onto the region's last line
    feed(&mut term, b"\n\n"); // two rotations of the region swap the fixed row back

    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(
        placements[0].row_in_view, 4,
        "a placement anchored below a bottom margin keeps its screen row"
    );
}

#[test]
fn ed1_above_with_display_offset_keeps_history_images() {
    let (mut term, recorder) = term(8, 5);
    term.set_scrollback_images(true);
    feed(&mut term, b"\x1b[3;1H");
    feed(&mut term, SIXEL); // placement on absolute line 2
    let key = recorder.received()[0].0;
    for _ in 0..6 {
        feed(&mut term, b"\n"); // scroll it into history (stable is 5 afterwards)
    }
    term.scroll_display(Scroll::Delta(3)); // the view shows absolute lines 2..

    // ED 1 concerns the screen above the cursor, not the scrolled-back view: erasing from the
    // top of the *view* used to wipe this placement and release its image.
    feed(&mut term, b"\x1b[1J");
    assert!(recorder.released().is_empty(), "ED 1 above does not touch scrollback");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "the history placement is still there");
    assert_eq!(placements[0].key, key);
}

#[test]
fn virtual_placements_are_capped_and_added_in_constant_time() {
    let (mut term, recorder) = term(8, 5);
    let base = term.placement_evict_examined();

    // One image, 20 000 virtual placements with distinct placement ids (`a=p` re-displays the
    // stored image; `U=1` records it virtually instead of placing it).
    feed(&mut term, &transmit(7, "f=24", Some((8, 8)), &rgb(8, 8), ",U=1,p=1"));
    for pid in 2..=20_000u32 {
        let frame = format!("\x1b_Ga=p,i=7,U=1,p={pid}\x1b\\");
        feed(&mut term, frame.as_bytes());
    }

    const _: () = assert!(20_000 > MAX_PLACEMENTS);
    // Deterministic proof of indexing (the wall clock this replaces flakes on slow CI
    // runners): every eviction at the cap goes through the age index — two examined entries
    // per eviction, against a bound of four per placement added. Scanning the virtual map per
    // eviction would examine thousands each and break it.
    let examined = term.placement_evict_examined() - base;
    assert!(
        examined <= 4 * 20_000,
        "{examined} entries examined evicting virtual placements down from 20 000 additions"
    );
    assert_eq!(term.kitty_virtual_image(7, Some(1)), None, "the oldest virtual evicted");
    assert!(term.kitty_virtual_image(7, Some(20_000)).is_some(), "the newest survives");
    assert!(
        recorder.released().is_empty(),
        "evicting a virtual whose image other virtuals still reference frees nothing"
    );
}

#[test]
fn placeholder_dedupe_keeps_the_latest_per_cell() {
    let (mut term, recorder) = term(10, 6);

    // Two placements anchored at the very same cell (line 0, column 0), different images.
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, &transmit(11, "f=32", Some((24, 8)), &rgba(24, 8), ",c=6,r=1"));
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, &transmit(12, "f=32", Some((16, 6)), &rgba(16, 6), ",c=4,r=1"));
    recorder.received();

    feed(&mut term, b"\x1b[6;1H");
    feed(&mut term, b"\n"); // both scroll off together and convert to placeholders
    term.scroll_display(Scroll::Delta(1)); // the placeholder line is back in view

    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1, "one record per cell, not one per placement");
    assert_eq!(placeholders[0].row_in_view, 0);
    assert_eq!(placeholders[0].col, 0);
    assert_eq!(placeholders[0].text, "[image 16×6]", "the latest stamp wins");
    // Numbered kitty images are kept by their ids even when no placement references them.
    assert!(recorder.released().is_empty());
}

#[test]
fn placeholder_cap_and_visible_range_scans() {
    // Scan bound: placements far above the view (beyond one clamped image's height of lookback)
    // must not be touched by a frame query.
    let mut table = PlacementTable::new();
    let (key, events) = table.new_image(Some(1), None, 8, 8, None).unwrap();
    assert!(events.is_empty());
    for line in [-50_000i64, -20_000, -12_000] {
        table.place(Screen::Primary, key, line, 0, 1, 1, None, 0, 0, 0, None).unwrap();
    }
    table.place(Screen::Primary, key, 3, 0, 1, 1, None, 0, 0, 0, None).unwrap();
    let window = GridWindow { stable: 0, lines: 5, history: 60_000 };
    let (out, scanned) = table.visible_placements_scanned(window, Screen::Primary, 0);
    assert_eq!(out.len(), 1, "only the in-view placement is returned");
    assert_eq!(scanned, 1, "the query touched one map line, not the far-away ones");

    // Scroll-off conversion turns the far-above placements into placeholders; a placeholder
    // query needs no lookback at all and scans none of their lines.
    table.after_scroll_up(
        GridWindow { stable: 1, lines: 5, history: 60_001 },
        Screen::Primary,
        0,
        5,
        1,
    );
    let (out, scanned) = table.visible_placeholders_scanned(window, Screen::Primary, 0);
    assert!(out.is_empty());
    assert_eq!(scanned, 0, "placeholders above the view are outside the query range");

    // Cap: one placeholder per scrolled-off row, oldest dropped past 100 000. A virtual holds
    // a reference so the shared image stays stored across the whole loop.
    let mut table = PlacementTable::new();
    let (key, _) = table.new_image(Some(2), None, 8, 8, None).unwrap();
    table.add_virtual(2, key, Some(1), 0, 0);
    for line in 0..(MAX_PLACEHOLDERS + 5) as i64 {
        table.place(Screen::Primary, key, line, 0, 1, 1, None, 0, 0, 0, None).unwrap();
        let events = table.after_scroll_up(
            GridWindow { stable: line + 1, lines: 1, history: (line + 1) as usize },
            Screen::Primary,
            0,
            1,
            1,
        );
        assert!(events.is_empty(), "the virtual keeps the image referenced");
    }
    let window = GridWindow { stable: 0, lines: MAX_PLACEHOLDERS + 5, history: 0 };
    let (out, _) = table.visible_placeholders_scanned(window, Screen::Primary, 0);
    assert_eq!(out.len(), MAX_PLACEHOLDERS, "the cap holds");
    assert_eq!(out[0].row_in_view, 5, "the five oldest placeholders were dropped");
}

#[test]
fn resize_reflow_moves_placement_with_its_first_cell() {
    let (mut term, recorder) = term(10, 8);
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, &[b'A'; 20]); // ten columns: fills rows 0 and 1, row 0 wraps
    feed(&mut term, b"\x1b[2;5H");
    feed(&mut term, &transmit(50, "f=32", Some((3, 1)), &rgba(3, 1), ",c=3,r=1"));
    let key = recorder.received()[0].0;

    // Growing to 20 columns merges the wrapped pair into row 0; the anchor cell (row 1,
    // column 4) moves with its content to column 14 of the merged row.
    term.resize(Size { columns: 20, lines: 8 });
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].key, key);
    assert_eq!((placements[0].row_in_view, placements[0].col), (0, 14));
    assert!(recorder.released().is_empty());
}

#[test]
fn resize_unfollowable_anchor_becomes_a_placeholder() {
    let (mut term, recorder) = term(12, 12);
    feed(&mut term, b"\x1b[1;11H"); // anchor column 10 of a 12-column screen
    feed(&mut term, &transmit(31, "f=32", Some((24, 8)), &rgba(24, 8), ",c=3,r=1"));
    let key = recorder.received()[0].0;

    // Shrinking to 5 columns destroys the anchor cell's column: spec 5 says the placement
    // becomes its placeholder instead of vanishing silently.
    term.resize(Size { columns: 5, lines: 12 });
    assert!(
        !term.visible_placements().iter().any(|p| p.key == key),
        "the placement itself is gone"
    );
    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1, "a placeholder stands in for it");
    assert_eq!((placeholders[0].row_in_view, placeholders[0].col), (0, 4));
    assert_eq!(placeholders[0].text, "[image 24×8]");
    assert!(recorder.released().is_empty(), "the id still references the stored image");
}

#[test]
fn uppercase_delete_keeps_data_while_another_placement_references_it() {
    let (mut term, recorder) = term(12, 12);
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, &transmit(40, "f=32", Some((8, 8)), &rgba(8, 8), ",c=4,r=1")); // row 0
    let key = recorder.received()[0].0;
    feed(&mut term, b"\x1b[9;1H");
    feed(&mut term, b"\x1b_Ga=p,i=40,c=4,r=1\x1b\\"); // a second placement of the same image
    assert_eq!(term.visible_placements().len(), 2);

    // `d=Y` (uppercase) deletes the placements crossing viewport row 9 — only the row-8 one,
    // and with neither `i` nor `I` in the frame it answers nothing.
    recorder.take();
    feed(&mut term, b"\x1b_Ga=d,d=Y,y=9\x1b\\");
    let events = recorder.take();
    assert_eq!(writes_of(&events), "");
    assert_eq!(term.visible_placements().len(), 1);
    assert!(
        released_of(&events).is_empty(),
        "the row-0 placement still references the image, so its data stays"
    );

    // Lowercase `d=i` removes both placements and still keeps the data; `d=I` frees it once.
    feed(&mut term, b"\x1b_Ga=d,d=i,i=40\x1b\\");
    let events = recorder.take();
    assert!(released_of(&events).is_empty());
    assert!(term.visible_placements().is_empty());
    feed(&mut term, b"\x1b_Ga=d,d=I,i=40\x1b\\");
    assert_eq!(released_of(&recorder.take()), vec![key]);
}

#[test]
fn scrollback_eviction_releases_only_when_unreferenced() {
    let (mut term, recorder) = term(12, 5);
    term.set_scrollback_images(true);
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, &transmit(41, "f=32", Some((8, 8)), &rgba(8, 8), ",c=4,r=1")); // line 0
    let key = recorder.received()[0].0;
    feed(&mut term, b"\x1b[3;1H");
    feed(&mut term, b"\x1b_Ga=p,i=41,c=4,r=1\x1b\\"); // line 2, same image
    for _ in 0..6 {
        feed(&mut term, b"\n"); // stable reaches 4: both placements end fully above the viewport
    }

    // Evicting the oldest scrollback placement turns it into its placeholder; the second
    // placement still references the image, so nothing releases.
    assert_eq!(term.evict_oldest_scrollback_image(), Some(key));
    let events = recorder.take();
    assert!(released_of(&events).is_empty(), "the surviving placement keeps the pixels");
    term.scroll_display(Scroll::Delta(4)); // scroll back to the evicted line
    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1);
    assert_eq!((placeholders[0].row_in_view, placeholders[0].col), (0, 0));
    assert_eq!(placeholders[0].text, "[image 8×8]");

    // Evicting the last placement converts it too; a numbered image is still kept by its id.
    term.scroll_display(Scroll::Bottom);
    recorder.take();
    assert_eq!(term.evict_oldest_scrollback_image(), Some(key));
    let events = recorder.take();
    assert!(released_of(&events).is_empty(), "the id still names the image");
    term.scroll_display(Scroll::Delta(4));
    assert_eq!(term.visible_placeholders().len(), 2, "both lines now show placeholders");
}

#[test]
fn cursor_position_image_follows_reflow_across_resizes() {
    let (mut term, recorder) = term(10, 8);
    feed(&mut term, b"\x1b[1;1H");
    feed(&mut term, b"Hello world! This is text."); // 26 chars: rows 0..2 at width 10

    // An image placed right after the text at the cursor (`C=1` leaves the cursor on its cell)
    // must follow its anchor through reflow exactly like a cursor sitting there does.
    feed(&mut term, &transmit(60, "f=32", Some((2, 2)), &rgba(2, 2), ",c=1,r=1,C=1"));
    let key = recorder.received()[0].0;
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].row_in_view, placements[0].col), (2, 6));

    // Grow merges the wrapped rows under it, then a shrink to 4 columns re-wraps everything:
    // the anchor rides the wrap chain, so no placeholder is created at any step.
    term.resize(Size { columns: 20, lines: 8 });
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].row_in_view, placements[0].col), (1, 6));
    term.resize(Size { columns: 4, lines: 8 });
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "the anchor followed the reflow, no placeholder");
    assert!(term.visible_placeholders().is_empty());
    assert_eq!((placements[0].row_in_view, placements[0].col), (1, 2));
    assert_eq!(placements[0].key, key);
    assert!(recorder.released().is_empty());
}

#[test]
fn cut_column_placeholder_sits_on_anchors_row() {
    let (mut term, recorder) = term(10, 8);
    feed(&mut term, b"\x1b[?1049h"); // the alternate grid's resize pass cuts without reflow
    feed(&mut term, b"\x1b[3;8H"); // row 2, column 7 of a 10-column screen
    feed(&mut term, &transmit(61, "f=32", Some((6, 2)), &rgba(6, 2), ",c=3,r=1"));
    let key = recorder.received()[0].0;
    assert_eq!(term.visible_placements().len(), 1);

    // Shrinking to 4 columns cuts the anchor column: the placeholder must sit on the row the
    // anchor cell ended up on — its own row, clamped to the last visible column — not one row
    // above it.
    term.resize(Size { columns: 4, lines: 8 });
    assert!(term.visible_placements().is_empty(), "the placement itself is gone");
    let placeholders = term.visible_placeholders();
    assert_eq!(placeholders.len(), 1);
    assert_eq!((placeholders[0].row_in_view, placeholders[0].col), (2, 3));
    assert_eq!(placeholders[0].text, "[image 6×2]");
    assert!(recorder.released().is_empty(), "the id still references the image");
    let _ = key;
}

#[test]
fn third_diacritic_above_a_byte_does_not_decode() {
    let mut cell = crate::term::cell::Cell {
        c: PLACEHOLDER_CHAR,
        fg: crate::vte::ansi::Color::Spec(Rgb { r: 0, g: 0, b: 3 }),
        ..crate::term::cell::Cell::default()
    };
    cell.push_zerowidth(ROW_COLUMN_DIACRITICS[1]); // row 1
    cell.push_zerowidth(ROW_COLUMN_DIACRITICS[2]); // column 2
    assert_eq!(decode_placeholder_cell(&cell), Some((3, 1, 2)), "two marks decode");

    // The third mark carries the most significant id byte; table index 256 does not fit in a
    // byte, so the cell must decode to `None` rather than a truncated id.
    cell.push_zerowidth(ROW_COLUMN_DIACRITICS[256]);
    assert_eq!(decode_placeholder_cell(&cell), None);
}

#[test]
fn adjacent_placeholders_survive_a_down_shift() {
    // Placeholders at (0, 4) and (1, 4), moved down together: ascending iteration made each move
    // delete the target cell's not-yet-moved record, collapsing them into one. The shift must
    // iterate in travel direction so both survive.
    let mut table = PlacementTable::new();
    let (a, _) = table.new_image(Some(10), None, 8, 6, None).unwrap();
    let (b, _) = table.new_image(Some(11), None, 7, 5, None).unwrap();
    table.place(Screen::Primary, a, 0, 4, 1, 1, None, 0, 0, 0, None).unwrap();
    table.after_scroll_up(GridWindow { stable: 1, lines: 5, history: 1 }, Screen::Primary, 0, 5, 1);
    table.place(Screen::Primary, b, 1, 4, 1, 1, None, 0, 0, 0, None).unwrap();
    let events = table.after_scroll_up(
        GridWindow { stable: 2, lines: 5, history: 2 },
        Screen::Primary,
        0,
        5,
        1,
    );
    assert!(events.is_empty(), "numbered images keep their data when scrolled off");

    // One row down (a `scroll_down` of the full region): both records shift, neither overwrites
    // the other.
    table.after_scroll_down(
        GridWindow { stable: 0, lines: 5, history: 2 },
        Screen::Primary,
        0,
        5,
        1,
    );
    let window = GridWindow { stable: 0, lines: 5, history: 2 };
    let (out, _) = table.visible_placeholders_scanned(window, Screen::Primary, 0);
    assert_eq!(out.len(), 2, "adjacent placeholders must not collapse when moved down");
    assert_eq!((out[0].row_in_view, out[0].col), (1, 4));
    assert_eq!(out[0].text, "[image 8×6]");
    assert_eq!((out[1].row_in_view, out[1].col), (2, 4));
    assert_eq!(out[1].text, "[image 7×5]");
}

#[test]
fn placement_cap_eviction_is_indexed() {
    let (mut term, recorder) = term(8, 5);
    // `C=1` pins the cursor so every placement lands on one cell: at the cap each new `a=p`
    // evicts exactly the oldest placement.
    feed(&mut term, &transmit(1, "f=24", Some((2, 2)), &rgb(2, 2), ",c=1,r=1,C=1"));
    recorder.take();

    let base = term.placement_evict_examined();
    for _ in 0..100_000u32 {
        feed(&mut term, b"\x1b_Ga=p,i=1,C=1\x1b\\");
    }
    // Deterministic proof of indexing (the wall clock this replaces flaked on a slow CI
    // runner): each placement at the cap evicts once through the age index. A full scan would
    // examine MAX_PLACEMENTS records per eviction — 4096 x 100 000 — against this bound of
    // four per eviction.
    let examined = term.placement_evict_examined() - base;
    assert!(
        examined <= 4 * 100_000,
        "{examined} entries examined evicting 100 000 placements at the cap"
    );
    assert_eq!(term.visible_placements().len(), MAX_PLACEMENTS, "the cap holds exactly");
    assert!(recorder.released().is_empty(), "the shared image never loses its last reference");
}

#[test]
fn alternate_screen_resize_does_not_duplicate_primary_placements() {
    // An image on the shell screen, a full-screen program (`?1049h`) and two resizes: while the
    // alternate screen is active the primary grid's placements are shifted to their post-scroll
    // lines, and `apply_reflow` must move *those* records — collecting the anchor list before
    // the shift handed it stale copies, which left a second record beside the shifted one (and,
    // past the next removal, a `placement_total` out of step with the maps).
    let (mut term, recorder) = term(20, 8);
    term.set_cell_pixel_size(4, 8);
    feed(&mut term, b"\x1b[7;2H\x1bPq#0~~~~-~~\x1b\\"); // a 4x12 px sixel at row 6, column 1
    let key = recorder.received()[0].0;
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1);
    // The image's two rows land exactly on the screen's last row, so one line scrolls to put
    // the cursor below it (see `sixel_at_the_bottom_of_the_screen_is_fully_visible`).
    assert_eq!((placements[0].row_in_view, placements[0].col), (5, 1));

    feed(&mut term, b"\x1b[?1049h"); // a full-screen program takes over; the image stays put
    term.resize(Size { columns: 9, lines: 6 });
    term.resize(Size { columns: 8, lines: 5 });
    feed(&mut term, b"\x1b[?1049l"); // back to the shell screen

    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "one record for one placement, not a stale copy beside it");
    assert_eq!(placements[0].key, key);

    // Scrolling it off releases it exactly once...
    feed(
        &mut term,
        b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\
                       \r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n",
    );
    assert!(term.visible_placements().is_empty(), "the image scrolled off the shell screen");
    assert_eq!(recorder.released(), vec![key]);

    // ...and a later image still displays: the desync used to jam every placement after it.
    feed(&mut term, b"\x1b[1;1H\x1bPq#0~~~~-~~\x1b\\");
    let events = recorder.take();
    assert_eq!(received_of(&events).len(), 1, "the new sixel is received");
    assert!(released_of(&events).is_empty(), "and nothing releases with it");
    let placements = term.visible_placements();
    assert_eq!(placements.len(), 1, "the new sixel is placed and visible");
    assert_eq!((placements[0].row_in_view, placements[0].col), (0, 0));
}

/// A tiny deterministic generator: a linear congruential PRNG with Knuth's multiplier, seeded by
/// the test. Fixed seeds so any failure reproduces exactly, and no dependency beyond the crate.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 32
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn image_lifecycle_balances_across_mixed_operations() {
    // The release-once contract under a mixed stream — images of all three protocols, newlines,
    // alternate-screen switches, resizes, `ED` and scroll regions, ending in a full reset. Every
    // key handed to the host must come back as exactly one `ImageReleased`, nothing may stay
    // placed after `RIS`, and placement must still work afterwards: a desynced placement count
    // used to refuse every later image until then. Fixed seeds, so a failure reproduces.
    for seed in 0..24u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_85EB_C6C7).wrapping_add(1));
        let columns = 4 + rng.below(20) as usize;
        let lines = 4 + rng.below(8) as usize;
        let (mut term, recorder) = term(columns, lines);
        term.set_cell_pixel_size(4, 8);

        let mut received: Vec<ImageKey> = Vec::new();
        let mut released: Vec<ImageKey> = Vec::new();
        let mut unanswered: Vec<ImageKey> = Vec::new();
        let mut alt = false;
        for step in 0..150u64 {
            // Answer the oldest encoded payload every so often, as a host would.
            if step % 5 == 4 && !unanswered.is_empty() {
                let key = unanswered.remove(0);
                term.image_decoded(key, Ok(()));
            }

            let bytes: Vec<u8> = match rng.below(9) {
                // A two-row sixel at a random cell.
                0 => {
                    let line = 1 + rng.below(term.screen_lines() as u64);
                    let col = 1 + rng.below(term.columns() as u64);
                    format!("\x1b[{line};{col}H\x1bPq#0~-~\x1b\\").into_bytes()
                },
                // A small kitty transmission placing at the cursor.
                1 | 2 => {
                    let id = 1 + rng.below(4) as u32;
                    let (w, h) = (1 + rng.below(3) as u32, 1 + rng.below(3) as u32);
                    transmit(id, "f=32", Some((w, h)), &rgba(w, h), "")
                },
                // An OSC 1337 inline PNG at the cursor.
                3 => {
                    let mut out = b"\x1b]1337;File=inline=1:".to_vec();
                    out.extend_from_slice(STANDARD.encode(PNG_1X1).as_bytes());
                    out.extend(b"\x1b\\");
                    out
                },
                // Newlines.
                4 => vec![b'\r'; 2 * (1 + rng.below(3) as usize)]
                    .into_iter()
                    .flat_map(|c| [c, b'\n'])
                    .collect(),
                // Alternate screen in, alternate screen out.
                5 => {
                    alt = !alt;
                    if alt { b"\x1b[?1049h".to_vec() } else { b"\x1b[?1049l".to_vec() }
                },
                // A window resize.
                6 => {
                    term.resize(Size {
                        columns: 3 + rng.below(30) as usize,
                        lines: 2 + rng.below(12) as usize,
                    });
                    Vec::new()
                },
                // Erase in display.
                7 => match rng.below(3) {
                    0 => b"\x1b[2J".to_vec(),
                    1 => b"\x1b[1J".to_vec(),
                    _ => b"\x1b[0J\x1b[H".to_vec(),
                },
                // A scroll region, or back to the whole screen.
                _ => {
                    if rng.below(2) == 0 {
                        b"\x1b[r".to_vec()
                    } else {
                        let lines = term.screen_lines();
                        let start = (1 + rng.below(lines as u64)) as usize;
                        let end = start + 1 + rng.below((lines - start).max(1) as u64) as usize;
                        format!("\x1b[{start};{end}r").into_bytes()
                    }
                },
            };
            if !bytes.is_empty() {
                feed(&mut term, &bytes);
            }

            for event in recorder.take() {
                match event {
                    Event::ImageReceived { key, data } => {
                        received.push(key);
                        if matches!(data, ImageData::Encoded { .. }) {
                            unanswered.push(key);
                        }
                    },
                    Event::ImageReleased(key) => released.push(key),
                    _ => {},
                }
            }
        }

        // A full reset releases everything the table still held.
        feed(&mut term, b"\x1bc");
        for event in recorder.take() {
            match event {
                Event::ImageReceived { key, .. } => received.push(key),
                Event::ImageReleased(key) => released.push(key),
                _ => {},
            }
        }

        let mut expected = received.clone();
        expected.sort_unstable();
        expected.dedup();
        released.sort_unstable();
        assert_eq!(released, expected, "seed {seed}: every received key releases exactly once");
        assert!(term.visible_placements().is_empty(), "seed {seed}: nothing is placed after RIS");

        // The table still works: a new image places and stays visible.
        feed(&mut term, SIXEL);
        let events = recorder.take();
        assert_eq!(
            received_of(&events).len(),
            1,
            "seed {seed}: a sixel still places after the reset"
        );
        assert!(released_of(&events).is_empty(), "seed {seed}: nothing releases with it");
        assert_eq!(term.visible_placements().len(), 1, "seed {seed}: the new image is visible");
    }
}

#[test]
fn delete_selectors_require_one_based_coordinates() {
    // `d=p`/`d=q`/`d=x` (and their uppercase forms) name viewport cells with 1-based
    // coordinates: a missing or zero `x`/`y` is refused with EINVAL instead of subtracting
    // through zero, which panicked in debug builds on thirteen remote bytes.
    let (mut term, recorder) = term(10, 5);
    feed(&mut term, &transmit(1, "f=24", Some((1, 1)), &rgb(1, 1), "")); // at (0, 0)
    let key = recorder.received()[0].0;

    feed(&mut term, b"\x1b_Ga=d,d=p,i=1\x1b\\"); // no coordinates at all
    assert_eq!(recorder.writes(), "\x1b_Gi=1;EINVAL:invalid control data\x1b\\");
    feed(&mut term, b"\x1b_Ga=d,d=q,i=1,x=0,y=2\x1b\\"); // a zero column
    assert_eq!(recorder.writes(), "\x1b_Gi=1;EINVAL:invalid control data\x1b\\");
    feed(&mut term, b"\x1b_Ga=d,d=P,i=1,x=3,y=0\x1b\\"); // uppercase form, zero row
    assert_eq!(recorder.writes(), "\x1b_Gi=1;EINVAL:invalid control data\x1b\\");

    // `d=x` needs its column the same way; with no id or number to echo the refusal is
    // silent (see `reply_without_image_or_number_is_silent`).
    feed(&mut term, b"\x1b_Ga=d,d=x\x1b\\");
    assert_eq!(recorder.writes(), "");

    // The refusals deleted nothing; a well-addressed delete still works. `d=X` frees the
    // stored image as well as the placement, and answers nothing (no id, no number).
    assert_eq!(term.visible_placements().len(), 1);
    feed(&mut term, b"\x1b_Ga=d,d=X,x=1\x1b\\");
    let events = recorder.take();
    assert_eq!(writes_of(&events), "");
    assert!(term.visible_placements().is_empty());
    assert_eq!(released_of(&events), vec![key]);
}

#[test]
fn reply_without_image_or_number_is_silent() {
    // The protocol ties acknowledgements to the image id: a client that sends neither `i` nor
    // `I` does not read a reply, and writing one anyway puts bytes in the program's stdin —
    // or, after it exits, at the shell prompt.
    let (mut term, recorder) = term(10, 5);
    feed(&mut term, b"\x1b_Ga=T,f=24,s=1,v=1;AQID\x1b\\");
    let events = recorder.take();
    assert_eq!(received_of(&events).len(), 1, "the image itself is still received");
    assert_eq!(term.visible_placements().len(), 1, "and still placed");
    assert_eq!(writes_of(&events), "", "nothing goes back to the pty");

    // A refusal with neither travels too: the parser's EINVAL for a coordinate-less delete
    // has no id to echo and says nothing (see `delete_selectors_require_one_based_coordinates`
    // for the id-carrying cases).
    feed(&mut term, b"\x1b_Ga=d,d=q\x1b\\");
    assert_eq!(recorder.writes(), "");
}

#[test]
fn transmission_replies_carry_the_placement_id() {
    // `a=T,i=7,p=3` answers `i=7,p=3;OK`: the placement id belongs in the answer. Raw pixels
    // are answered at once; an encoded transmission's pending reply carries it to
    // `image_decoded`, which flushes it.
    let (mut term, recorder) = term(10, 5);

    feed(&mut term, &transmit(7, "f=24", Some((1, 1)), &rgb(1, 1), ",p=3"));
    let events = recorder.take();
    assert_eq!(received_of(&events).len(), 1);
    assert_eq!(writes_of(&events), "\x1b_Gi=7,p=3;OK\x1b\\");

    let seq = format!("\x1b_Ga=T,i=8,f=100,p=2,m=0;{}\x1b\\", STANDARD.encode(PNG_1X1));
    feed(&mut term, seq.as_bytes());
    let events = recorder.take();
    let key = received_of(&events)[0].0;
    assert!(writes_of(&events).is_empty(), "the reply waits on the decode result");
    term.image_decoded(key, Ok(()));
    assert_eq!(recorder.writes(), "\x1b_Gi=8,p=2;OK\x1b\\");
}

#[test]
fn number_only_transmission_gets_an_id_it_can_place_by() {
    // The protocol has the terminal assign an id when a client transmits by number alone, and
    // answer `i=<new>,I=<n>`. Without an assigned id such an image cannot back a virtual
    // placement either: `a=p` needs an id.
    let (mut term, recorder) = term(10, 5);
    let seq = format!("\x1b_Ga=T,I=13,f=24,s=1,v=1;{}\x1b\\", STANDARD.encode(rgb(1, 1)));
    feed(&mut term, seq.as_bytes());

    let events = recorder.take();
    assert_eq!(received_of(&events).len(), 1);
    assert_eq!(writes_of(&events), "\x1b_Gi=1,I=13;OK\x1b\\", "the terminal assigns the id");

    // The assigned id resolves: a virtual placement by it lands, and `kitty_virtual_image`
    // finds the image behind it.
    feed(&mut term, b"\x1b_Ga=p,i=1,U=1\x1b\\");
    assert_eq!(writes_of(&recorder.take()), "\x1b_Gi=1;OK\x1b\\");
    let resolved = term.kitty_virtual_image(1, None).expect("the number-named image places by id");
    assert_eq!(resolved.2, 1, "the virtual placement spans one cell");
}

#[test]
fn tick_delivers_releases_queued_by_a_resize() {
    // `resize` parks its placement events (it has no listener bound), and control-only output
    // does not flush them: a program that answers a resize with sequences alone would otherwise
    // hold its `ImageReleased` until the next printable character. Hosts call `tick`
    // periodically, so `tick` delivers what is parked.
    let (mut term, recorder) = term(10, 8);
    feed(&mut term, b"\x1b[?1049h");
    feed(&mut term, b"\x1b[3;8H"); // row 2, column 7 of a 10-column screen
    feed(&mut term, SIXEL); // a one-cell sixel anchored there
    let key = recorder.received()[0].0;

    // Shrinking to 4 columns cuts the anchor column: the placement becomes a placeholder and
    // the sixel's last reference goes — but the release sits in the queue.
    term.resize(Size { columns: 4, lines: 8 });
    assert!(term.visible_placements().is_empty(), "the placement itself is gone");

    // Control-only output delivers nothing.
    feed(&mut term, b"\x1b[2J");
    assert_eq!(released_of(&recorder.take()), Vec::new(), "the release still waits");

    // `tick` sends it.
    term.tick(Instant::now());
    assert_eq!(recorder.released(), vec![key]);
}
