// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Streaming string sequences (feathercritty): APC, DCS sixel and OSC 1337 inline images, plus the
//! two CSI dispatches added for graphics support.
//!
//! Everything runs through [`Processor`], so these record what an embedder's [`Handler`] sees.

use core::time::Duration;

use crate::vte::ansi::{Handler, Hyperlink, Processor, Timeout};

/// APC (`ESC _ <string> ESC \`), as the kitty graphics protocol sends it.
const APC: &[u8] = b"\x1b_Ga=T,f=100;i=7,m=0:YWJjZGVmZw==\x1b\\";

/// The APC content between `ESC _` and the `ST`.
const APC_DATA: &[u8] = b"Ga=T,f=100;i=7,m=0:YWJjZGVmZw==";

/// DCS sixel (`ESC P 0 ; 1 ; 0 q <data> ESC \`).
const SIXEL: &[u8] = b"\x1bP0;1;0q#0;2;0;0;0!137A#1;0;0;0!137B\x1b\\";

/// The sixel data between `q` and the `ST`.
const SIXEL_DATA: &[u8] = b"#0;2;0;0;0!137A#1;0;0;0!137B";

/// OSC 1337 inline image (`ESC ] 1337 ; File = <args> : <data> BEL`).
const IMAGE: &[u8] = b"\x1b]1337;File=name.png:AAAAAAAAAAAAAAAA\x07";

/// The arguments of [`IMAGE`]: between `File=` and the first `:`.
const IMAGE_ARGS: &[u8] = b"name.png";

/// The payload of [`IMAGE`].
const IMAGE_DATA: &[u8] = b"AAAAAAAAAAAAAAAA";

/// A [`Timeout`] that never reports one: these tests do not use synchronized updates.
#[derive(Default)]
struct NoTimeout;

impl Timeout for NoTimeout {
    fn set_timeout(&mut self, _: Duration) {}

    fn clear_timeout(&mut self) {}

    fn pending_timeout(&self) -> bool {
        false
    }
}

/// What the handler saw, in order.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    ApcStart,
    ApcPut(Vec<u8>),
    ApcEnd,
    ApcAbort,
    SixelStart(Vec<u16>),
    SixelPut(Vec<u8>),
    SixelEnd,
    SixelAbort,
    ImageStart(Vec<u8>),
    ImagePut(Vec<u8>),
    ImageEnd,
    ImageAbort,
    GraphicsAttributes(u16, u16, Option<u16>),
    CellSizePixels,
    TextAreaSizePixels,
    TextAreaSizeChars,
    ScrollUp(usize),
    Hyperlink(Option<Hyperlink>),
}

#[derive(Default)]
struct Recorder {
    events: Vec<Event>,
}

impl Handler for Recorder {
    fn apc_start(&mut self) {
        self.events.push(Event::ApcStart);
    }

    fn apc_put(&mut self, bytes: &[u8]) {
        self.events.push(Event::ApcPut(bytes.to_vec()));
    }

    fn apc_end(&mut self) {
        self.events.push(Event::ApcEnd);
    }

    fn apc_abort(&mut self) {
        self.events.push(Event::ApcAbort);
    }

    fn sixel_start(&mut self, params: &[u16]) {
        self.events.push(Event::SixelStart(params.to_vec()));
    }

    fn sixel_put(&mut self, bytes: &[u8]) {
        self.events.push(Event::SixelPut(bytes.to_vec()));
    }

    fn sixel_end(&mut self) {
        self.events.push(Event::SixelEnd);
    }

    fn sixel_abort(&mut self) {
        self.events.push(Event::SixelAbort);
    }

    fn inline_image_start(&mut self, args: &[u8]) {
        self.events.push(Event::ImageStart(args.to_vec()));
    }

    fn inline_image_put(&mut self, bytes: &[u8]) {
        self.events.push(Event::ImagePut(bytes.to_vec()));
    }

    fn inline_image_end(&mut self) {
        self.events.push(Event::ImageEnd);
    }

    fn inline_image_abort(&mut self) {
        self.events.push(Event::ImageAbort);
    }

    fn graphics_attributes(&mut self, item: u16, action: u16, value: Option<u16>) {
        self.events.push(Event::GraphicsAttributes(item, action, value));
    }

    fn cell_size_pixels(&mut self) {
        self.events.push(Event::CellSizePixels);
    }

    fn text_area_size_pixels(&mut self) {
        self.events.push(Event::TextAreaSizePixels);
    }

    fn text_area_size_chars(&mut self) {
        self.events.push(Event::TextAreaSizeChars);
    }

    fn scroll_up(&mut self, rows: usize) {
        self.events.push(Event::ScrollUp(rows));
    }

    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        self.events.push(Event::Hyperlink(hyperlink));
    }
}

/// Run `bytes` through a fresh parser and handler.
fn record(bytes: &[u8]) -> Vec<Event> {
    let mut processor: Processor<NoTimeout> = Processor::new();
    let mut recorder = Recorder::default();
    processor.advance(&mut recorder, bytes);
    recorder.events
}

/// Run `bytes` through a fresh parser in two pieces, split at `split`.
fn record_split(bytes: &[u8], split: usize) -> Vec<Event> {
    let mut processor: Processor<NoTimeout> = Processor::new();
    let mut recorder = Recorder::default();
    processor.advance(&mut recorder, &bytes[..split]);
    processor.advance(&mut recorder, &bytes[split..]);
    recorder.events
}

/// Concatenate adjacent `*_put` events of the same kind.
///
/// A `*_put` carries whatever slice of the run the reader happened to hand over, so recordings made
/// from differently chunked reads differ only here. Comparing them after joining says "the handler
/// saw the same bytes in the same places".
fn join_puts(events: Vec<Event>) -> Vec<Event> {
    let mut joined: Vec<Event> = Vec::with_capacity(events.len());

    for event in events {
        let joined_in_place = match joined.last_mut() {
            Some(Event::ApcPut(previous)) => match &event {
                Event::ApcPut(next) => {
                    previous.extend_from_slice(next);
                    true
                },
                _ => false,
            },
            Some(Event::SixelPut(previous)) => match &event {
                Event::SixelPut(next) => {
                    previous.extend_from_slice(next);
                    true
                },
                _ => false,
            },
            Some(Event::ImagePut(previous)) => match &event {
                Event::ImagePut(next) => {
                    previous.extend_from_slice(next);
                    true
                },
                _ => false,
            },
            _ => false,
        };

        if !joined_in_place {
            joined.push(event);
        }
    }

    joined
}

/// APC content is streamed: start, the bytes as they arrive, end. One call per run of bytes.
#[test]
fn apc_round_trip() {
    assert_eq!(record(APC), vec![Event::ApcStart, Event::ApcPut(APC_DATA.to_vec()), Event::ApcEnd]);

    // A bare `ESC` (not only `ST`) ends the string, and parsing goes on after it (the `CSI 31 m`
    // that follows is an attribute change, which this recorder does not track).
    assert_eq!(record(b"\x1b_a=1\x1b[31m"), vec![
        Event::ApcStart,
        Event::ApcPut(b"a=1".to_vec()),
        Event::ApcEnd
    ]);
}

/// `DCS 0 ; 1 ; 0 q` is sixel: its parameters and payload reach the handler, other DCS do not.
#[test]
fn sixel_round_trip() {
    assert_eq!(record(SIXEL), vec![
        Event::SixelStart(vec![0, 1, 0]),
        Event::SixelPut(SIXEL_DATA.to_vec()),
        Event::SixelEnd,
    ]);

    // A DCS with an intermediate is not sixel; `hook` and `put` stay what they were (nothing here).
    assert_eq!(record(b"\x1bP1$tx\x1b\\"), Vec::new());

    // CAN inside the string aborts it instead of ending it.
    assert_eq!(record(b"\x1bP0;1;0q#0!137A\x18"), vec![
        Event::SixelStart(vec![0, 1, 0]),
        Event::SixelPut(b"#0!137A".to_vec()),
        Event::SixelAbort,
    ]);
}

/// `OSC 1337 ; File = <args> :` streams the payload; args keep their `;`.
#[test]
fn inline_image_round_trip() {
    assert_eq!(record(IMAGE), vec![
        Event::ImageStart(IMAGE_ARGS.to_vec()),
        Event::ImagePut(IMAGE_DATA.to_vec()),
        Event::ImageEnd,
    ]);

    // Arguments are everything between `File=` and the first `:`; a `;` inside them is argument
    // text, and anything after that first `:` is payload.
    assert_eq!(record(b"\x1b]1337;File=name;width=10:inline=1:AAAA\x1b\\"), vec![
        Event::ImageStart(b"name;width=10".to_vec()),
        Event::ImagePut(b"inline=1:AAAA".to_vec()),
        Event::ImageEnd,
    ]);

    // Without a `:` there is no payload and nothing starts: an ordinary OSC dispatches (and 1337
    // dispatches nothing an embedder would see).
    assert_eq!(record(b"\x1b]1337;File=name\x07"), Vec::new());

    // Any other first parameter, or any other second parameter, is an ordinary OSC too.
    assert_eq!(record(b"\x1b]1338;File=name:AAAA\x07"), Vec::new());
    assert_eq!(record(b"\x1b]1337;SetUserVar=x:AAAA\x07"), Vec::new());

    // An ordinary OSC still dispatches (OSC 8 with an empty URI clears the hyperlink).
    assert_eq!(record(b"\x1b]8;;\x07"), vec![Event::Hyperlink(None)]);

    // Args up to the 4 KiB cap are handed over whole.
    let args = vec![b'a'; 4 * 1024];
    let mut capped: Vec<u8> = b"\x1b]1337;File=".to_vec();
    capped.extend_from_slice(&args);
    capped.extend_from_slice(b":AAAA\x07");
    assert_eq!(record(&capped), vec![
        Event::ImageStart(args.clone()),
        Event::ImagePut(b"AAAA".to_vec()),
        Event::ImageEnd,
    ]);

    // One byte more and the attempt is aborted: nothing started, and the rest of the string is
    // dropped without being buffered.
    let mut oversized: Vec<u8> = b"\x1b]1337;File=".to_vec();
    oversized.extend_from_slice(&args);
    oversized.push(b'a');
    oversized.extend_from_slice(b":AAAA\x07");
    assert_eq!(record(&oversized), vec![Event::ImageAbort]);
}

/// CAN and SUB cancel the string they are inside of.
#[test]
fn can_aborts_apc() {
    for cancel in *b"\x18\x1a" {
        // The APC of `apc_round_trip`, cut short by CAN or SUB instead of terminated.
        let mut input = Vec::with_capacity(APC_DATA.len() + 3);
        input.extend_from_slice(&APC[..2]);
        input.extend_from_slice(APC_DATA);
        input.push(cancel);

        assert_eq!(
            record(&input),
            vec![Event::ApcStart, Event::ApcPut(APC_DATA.to_vec()), Event::ApcAbort],
            "{:?}",
            input
        );
    }

    // CAN after the `:` aborts a started image; before it nothing was ever announced.
    assert_eq!(record(b"\x1b]1337;File=n:AAAA\x18"), vec![
        Event::ImageStart(b"n".to_vec()),
        Event::ImagePut(b"AAAA".to_vec()),
        Event::ImageAbort,
    ]);
    assert_eq!(record(b"\x1b]1337;File=nn\x18"), Vec::new());
}

/// Every split point gives the handler the same bytes in the same places.
#[test]
fn split_at_every_byte_is_identical() {
    for input in [APC, SIXEL, IMAGE] {
        let whole = join_puts(record(input));
        assert_eq!(whole.len(), 3, "{:?}", input);

        for split in 1..input.len() {
            assert_eq!(join_puts(record_split(input, split)), whole, "split at {}", split);
        }
    }
}

/// `CSI ? Pi ; Pa ; Pv S` (XTSMGRAPHICS) reaches the handler; `CSI S` still scrolls.
#[test]
fn xtsmgraphics_dispatch() {
    assert_eq!(record(b"\x1b[?2;1;100S"), vec![Event::GraphicsAttributes(2, 1, Some(100))]);

    // A query has no value.
    assert_eq!(record(b"\x1b[?1;1S"), vec![Event::GraphicsAttributes(1, 1, None)]);
    assert_eq!(record(b"\x1b[?2;1S"), vec![Event::GraphicsAttributes(2, 1, None)]);

    // Only the first component of a subparameter-separated value is reported.
    assert_eq!(record(b"\x1b[?2;1;640:480S"), vec![Event::GraphicsAttributes(2, 1, Some(640))]);

    // Without the private marker it is the scroll it always was.
    assert_eq!(record(b"\x1b[3S"), vec![Event::ScrollUp(3)]);
}

/// `CSI 16 t` reports the cell size in pixels; its neighbours keep their handlers.
#[test]
fn csi_16t_dispatch() {
    assert_eq!(record(b"\x1b[16t"), vec![Event::CellSizePixels]);
    assert_eq!(record(b"\x1b[14t"), vec![Event::TextAreaSizePixels]);
    assert_eq!(record(b"\x1b[18t"), vec![Event::TextAreaSizeChars]);
}

/// SOS (`ESC X`) and PM (`ESC ^`) are still discarded; only APC streams.
#[test]
fn sos_pm_still_ignored() {
    let inputs: [&[u8]; 3] =
        [b"\x1bXsome data\x1b\\", b"\x1b^some data\x1b\\", b"\x1bXsome data\x18more"];
    for input in inputs {
        assert_eq!(record(input), Vec::new(), "{:?}", input);
    }

    // The same content in an APC (`ESC _`) is streamed.
    assert_eq!(record(b"\x1b_some data\x1b\\"), vec![
        Event::ApcStart,
        Event::ApcPut(b"some data".to_vec()),
        Event::ApcEnd
    ]);
}
