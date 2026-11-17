// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Feeds arbitrary bytes through [`InlineImageParser`], the OSC 1337 inline-image state machine,
//! the way `Term` feeds it: the arguments once at the start, then the payload in arbitrary slices.
//!
//! Framing, which the seeds under `fuzz/seeds/inline/` follow: the input is one sequence written
//! the way the protocol sends it — the `File=` argument text, a `:`, and the base64 payload after
//! it (an input without a `:` is arguments with no payload). The payload arrives in chunks whose
//! size comes from the byte they start at, so a mutation moves every split point after it, which is
//! what carries a padded base64 group across two `put` calls. The same bytes are then run three more
//! ways: cut short by an abort, with the payload stopped halfway, and as a lone abort with no start
//! (the vendored parser passing its argument limit). Every image that comes back is checked against
//! the size caps.

#![no_main]

use feathercritty::graphics::inline::InlineImageParser;
use feathercritty::graphics::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, MAX_PAYLOAD_BYTES};
use libfuzzer_sys::fuzz_target;

/// Payload slices, sized from the byte each starts at.
fn chunks(parser: &mut InlineImageParser, payload: &[u8]) {
    let mut offset = 0;
    while offset < payload.len() {
        let chunk = 1 + payload[offset % payload.len()] as usize % 7;
        let end = (offset + chunk).min(payload.len());
        parser.put(&payload[offset..end]);
        offset = end;
    }
}

/// One sequence, `aborted` standing for the `CAN` or bare `ESC` that cuts it short. The image is
/// what an embedder would have to store.
fn run(args: &[u8], payload: &[u8], cut: usize, aborted: bool) {
    let mut parser = InlineImageParser::new();
    parser.start(args);
    chunks(&mut parser, &payload[..cut]);
    if aborted {
        parser.abort();
        assert!(parser.end().is_none(), "an aborted sequence displays nothing");
        return;
    }
    let Some(image) = parser.end() else { return };
    assert!(image.bytes.len() <= MAX_PAYLOAD_BYTES, "a payload of {} bytes", image.bytes.len());
    let (width, height) = image.pixel_size;
    let frame = (width as u64) * (height as u64) * 4;
    assert!(width <= MAX_IMAGE_SIDE && height <= MAX_IMAGE_SIDE, "{width}x{height} over the side cap");
    assert!(frame <= MAX_IMAGE_BYTES, "{width}x{height} is {frame} bytes of RGBA8, over the cap");
    assert!(image.inline, "a returned image is always one to display");
}

fuzz_target!(|data: &[u8]| {
    // The protocol's own split: `File = <args> : <payload>`.
    let (args, payload) = match data.iter().position(|&byte| byte == b':') {
        Some(colon) => (&data[..colon], &data[colon + 1..]),
        None => (data, &[][..]), // arguments only: the sequence carries no payload
    };

    run(args, payload, payload.len(), false);
    run(args, payload, payload.len(), true);
    run(args, payload, payload.len() / 2, false);

    // A lone abort with no start: the argument limit passed while the args were still read.
    let mut parser = InlineImageParser::new();
    parser.abort();
    assert!(parser.end().is_none(), "a lone abort holds nothing");
});
