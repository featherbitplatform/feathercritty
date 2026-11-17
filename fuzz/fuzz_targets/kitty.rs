// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Feeds arbitrary bytes through [`KittyParser`], the kitty graphics protocol state machine, the
//! way `Term` feeds it: one APC sequence at a time, its content in arbitrary slices.
//!
//! Framing, which the seeds under `fuzz/seeds/kitty/` follow: the input is a run of APC contents
//! separated by `0xff`, a byte that never appears in APC text. Frame `n` is one sequence —
//! `start`, its bytes in chunks of `1 + frame.len() % 7`, then `end(now)`, or `abort` every seventh
//! frame the way `CAN` would cut it. The clock moves on by `frame.len() % 25` seconds per frame, so
//! a chunked upload is seen stalling and the watchdog fires; every fourth frame runs `tick` first,
//! and the input ends with a far-future `tick` plus an `end` for a frame left open. Whatever
//! rejection waits after a frame is answered with `reply` — which answers nothing for a command
//! carrying neither an id nor a number — and what the parser holds is checked against the caps
//! after every frame.

#![no_main]

use std::time::{Duration, Instant};

use feathercritty::graphics::kitty::{KittyParser, reply};
use feathercritty::graphics::limits::{MAX_IMAGE_BYTES, MAX_PAYLOAD_BYTES};
use libfuzzer_sys::fuzz_target;

/// What one parser may hold: a transmission's payload cap, plus the most an `o=z` payload of it can
/// inflate to ([`held_bytes`](KittyParser::held_bytes) counts the accumulated output too), plus the
/// few bytes of staged control text and carried base64 characters.
const HELD_CAP: usize = MAX_PAYLOAD_BYTES + MAX_IMAGE_BYTES as usize + 4_096;

/// The clock moved on by `secs`, or left where it was (a `checked_add` that cannot overflow).
fn later(from: Instant, secs: u64) -> Instant {
    from.checked_add(Duration::from_secs(secs)).unwrap_or(from)
}

fuzz_target!(|data: &[u8]| {
    let mut parser = KittyParser::new();
    let mut now = Instant::now();

    for (index, frame) in data.split(|&byte| byte == 0xff).enumerate() {
        if index % 4 == 0 {
            parser.tick(now);
        }
        parser.start();
        for chunk in frame.chunks(1 + frame.len() % 7) {
            parser.put(chunk);
        }
        now = later(now, frame.len() as u64 % 25);
        if index % 7 == 6 {
            parser.abort();
        } else {
            parser.end(now);
        }
        answer(&mut parser);
        let held = parser.held_bytes();
        assert!(held <= HELD_CAP, "the parser holds {held} bytes, over the {HELD_CAP} cap");
    }

    // A client that stops mid-upload while the terminal sits idle.
    now = later(now, 60);
    parser.tick(now);
    parser.end(now);
    answer(&mut parser);
    let held = parser.held_bytes();
    assert!(held <= HELD_CAP, "the parser holds {held} bytes after the last tick");
});

/// Answers the rejection waiting on `take_reject`, as `Term` does.
fn answer(parser: &mut KittyParser) {
    if let Some(reject) = parser.take_reject() {
        let answered = reply(reject.id, reject.number, None, Err(reject.error), reject.quiet);
        // A failure at `q < 2` travels when the command named an image; a rejection with
        // neither id nor number gets no answer at all (the protocol ties acknowledgements to
        // an id, and such clients do not read one).
        debug_assert!(
            reject.quiet >= 2 || (reject.id == 0 && reject.number.is_none()) || answered.is_some()
        );
    }
}
