// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Feeds arbitrary bytes through `Processor` into a real `Term`, the way the PTY reader does, so
//! the whole wiring is exercised and not only the parser: the graphics parsers behind the vendored
//! parser's streaming callbacks, the placement table they feed, the events that leave it, and the
//! embedder calls that come back.
//!
//! Framing, which the seeds under `fuzz/seeds/parser_strings/` follow: the input is terminal output
//! as it arrives on the wire, in chunks whose size comes from the byte each starts at, so a mutation
//! moves every split point after it — a sequence cut inside its `ESC \`, or an `ESC` that ends one
//! chunk and gets its follower in the next, is reached by changing a length somewhere before it.
//! Between reads the target does what a host does: answers decode results for images whose keys its
//! listener kept, runs [`Term::tick`] on a clock that moves forward, resizes, switches to the
//! alternate screen and back, toggles scrollback images and evicts one. Afterwards it reads the view
//! back through [`Term::visible_placements`], [`Term::visible_placeholders`] and
//! [`kitty_diacritics::decode_placeholder_cell`], which is what an embedder draws with.

#![no_main]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use feathercritty::event::{Event, EventListener, ImageDecodeError, ImageKey};
use feathercritty::graphics::kitty_diacritics::{self, ROW_COLUMN_DIACRITICS};
use feathercritty::graphics::limits::MAX_IMAGE_SIDE;
use feathercritty::graphics::placements::{MAX_PLACEMENTS, MAX_PLACEHOLDERS};
use feathercritty::grid::Dimensions;
use feathercritty::index::{Column, Line, Point};
use feathercritty::term::{Config, Term};
use feathercritty::vte::ansi::Processor;
use libfuzzer_sys::fuzz_target;

/// Scrollback rows the terminal keeps: enough for placements to scroll into and back out of.
const HISTORY: usize = 1_000;

/// The most image keys the listener holds at once, so a hostile stream cannot grow it.
const KEYS: usize = 32;

/// A window size for `Term::new` and `Term::resize`, from two bytes of the input.
///
/// Columns start at 2: shrinking to a single column while a wide character sits where the cut
/// lands loops forever in the reflow — upstream 0.26.0 does the same — so it is left to its own
/// change rather than fuzzed away here. The README's Fuzzing section says the same.
fn size(cols: u8, lines: u8) -> Size {
    Size { cols: 2 + cols as usize % 119, lines: 1 + lines as usize % 40 }
}

/// A terminal size whose history is [`HISTORY`] rows deep.
struct Size {
    cols: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines + HISTORY
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Images whose decode result the terminal has not been given yet.
#[derive(Default)]
struct Pending {
    keys: Vec<ImageKey>,
}

/// Drops every event, keeping only what an embedder needs to act on it: the image keys it is asked
/// to decode. Shared with the target, which moved its own copy into the terminal.
struct Listener {
    pending: Rc<RefCell<Pending>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::ImageReceived { key, .. } => {
                let mut pending = self.pending.borrow_mut();
                if pending.keys.len() < KEYS {
                    pending.keys.push(key);
                }
            },
            Event::ImageReleased(key) => {
                // Released keys are never answered: the embedder has nothing left to decode.
                self.pending.borrow_mut().keys.retain(|held| *held != key);
            },
            _ => {},
        }
    }
}

/// The clock moved on by `secs`, or left where it was (a `checked_add` that cannot overflow).
fn later(from: Instant, secs: u64) -> Instant {
    from.checked_add(Duration::from_secs(secs)).unwrap_or(from)
}

/// Reports the next few pending decodes, mixing successes with both failure kinds, as an embedder
/// would.
fn answer(term: &mut Term<Listener>, shared: &RefCell<Pending>) {
    let batch: Vec<ImageKey> = {
        let mut pending = shared.borrow_mut();
        let take = pending.keys.len().min(4);
        pending.keys.drain(..take).collect()
    };
    for (index, key) in batch.into_iter().enumerate() {
        let result = match index % 4 {
            0 => Err(ImageDecodeError::Corrupt),
            1 => Err(ImageDecodeError::TooLarge),
            _ => Ok(()),
        };
        term.image_decoded(key, result);
    }
}

fuzz_target!(|data: &[u8]| {
    let shared = Rc::new(RefCell::new(Pending::default()));
    let first = data.first().copied().unwrap_or(40);
    let last = data.last().copied().unwrap_or(12);
    let mut term = Term::new(
        Config { scrolling_history: HISTORY, ..Config::default() },
        &size(first, last),
        Listener { pending: Rc::clone(&shared) },
    );
    // A host that knows its font, so pixel sizes turn into cell spans rather than falling back.
    term.set_cell_pixel_size(8, 16);

    // The default timeout handler, as `src/event_loop.rs` uses it.
    let mut parser: Processor = Processor::new();
    let mut now = Instant::now();
    let mut offset = 0;
    let mut read = 0usize;
    while offset < data.len() {
        // Chunk sizes come from content: editing a byte moves the splits after it.
        let chunk = 1 + data[offset % data.len()] as usize % 9;
        let end = (offset + chunk).min(data.len());
        parser.advance(&mut term, &data[offset..end]);
        offset = end;

        // What a host does between reads, on a schedule so every case is reachable.
        match read % 9 {
            0 => answer(&mut term, &shared),
            1 => {
                now = later(now, 4);
                term.tick(now);
            },
            2 => term.resize(size(data[offset % data.len()], data[(offset + 1) % data.len()])),
            3 => term.swap_alt(),
            4 => term.set_scrollback_images(read % 18 < 9),
            5 => {
                term.evict_oldest_scrollback_image();
            },
            // A window whose font size changed, including the axis a host reports as 0.
            6 => {
                let (a, b) = (data[offset % data.len()], data[(offset + 1) % data.len()]);
                term.set_cell_pixel_size(u32::from(a) % 24, u32::from(b) % 13);
            },
            // Off for a stretch every so often: the wipe of everything stored and displayed.
            7 => term.set_images_enabled(read % 90 < 45),
            _ => {},
        }
        read += 1;
    }

    // A client that stopped mid-upload while the terminal sat idle, then the embedder's read.
    now = later(now, 30);
    term.tick(now);
    answer(&mut term, &shared);

    let placements = term.visible_placements();
    assert!(placements.len() <= MAX_PLACEMENTS, "{} placements in view", placements.len());
    for placement in &placements {
        // The span rule: at least 1x1, and never more than a whole image side of cells.
        assert!(
            (1..=MAX_IMAGE_SIDE).contains(&placement.cols)
                && (1..=MAX_IMAGE_SIDE).contains(&placement.rows),
            "a {}x{} cell span",
            placement.cols,
            placement.rows
        );
    }
    let placeholders = term.visible_placeholders();
    assert!(placeholders.len() <= MAX_PLACEHOLDERS, "{} placeholder lines in view", placeholders.len());

    // The kitty Unicode placeholder path: decode the printed cells, then ask for their images.
    let grid = term.grid();
    let marks = ROW_COLUMN_DIACRITICS.len() as u32;
    for row in 0..term.screen_lines() {
        for col in 0..term.columns() {
            let cell = grid.cell(Point { line: Line(row as i32), column: Column(col) });
            if let Some((image, mark_row, mark_col)) = kitty_diacritics::decode_placeholder_cell(&cell) {
                assert!(mark_row < marks && mark_col < marks, "a placeholder at row {row}, column {col}");
                // The renderer resolves the cell against the virtual placements it was told about.
                let _ = term
                    .kitty_virtual_image(image, None)
                    .or_else(|| term.kitty_virtual_image(image, Some(mark_col)));
            }
        }
    }
});
