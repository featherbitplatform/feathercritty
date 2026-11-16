// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Incremental base64 for the streamed image payloads (feathercritty).
//!
//! Both graphics protocols carry their payload as base64 text arriving in arbitrary slices, and
//! both need the same three things: at most three characters — an incomplete quad — carried between
//! slices, a final group accepted with or without its padding, and refusal of anything that is not
//! the protocol's base64. [`Decoder`] holds that state; a parser resets it when a transmission
//! ends, is dropped, or starts over.
//!
//! Padding ends a payload. Data after it is refused whether it arrives in the same slice as the
//! padded group or in a later one: a sender that pads every chunk of one blob is not sending
//! base64, and guessing at those bytes would store an image whose content depends on where the
//! reads were cut. An empty slice after padding is not data and completes the payload unchanged.
//!
//! The size a slice decodes to follows from its length, so it never has to be decoded to find out:
//! `put` and [`finish`](Decoder::finish) take the room the caller has left for this payload and
//! answer [`TooBig`](B64Error::TooBig) when the slice would pass it — before any bytes are
//! produced. The size is three quarters of the quad-aligned part, less the one or two padding
//! characters at its end; that is the rounded-up 3/4 figure with the shortening padding taken back
//! out, so a payload that fits is never refused for rounding (a slice that is not valid base64 is
//! refused either way).

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

/// Why a payload slice was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum B64Error {
    /// Not base64 as the protocols define it: a symbol outside the alphabet, padding that does not
    /// end the payload, or a final group too short to carry a whole byte.
    Invalid,
    /// The slice's decoded bytes would pass the room its caller had left for this payload.
    TooBig,
}

/// Base64 text arriving in slices: the characters carried between them and the padding state.
#[derive(Default)]
pub(super) struct Decoder {
    /// At most three characters of an incomplete quad, kept for the next slice.
    stash: Vec<u8>,
    /// A padded group has been decoded: nothing may follow it, not even more padding.
    padded: bool,
}

impl Decoder {
    /// Characters carried over for the next slice (at most three).
    pub(super) fn carried(&self) -> usize {
        self.stash.len()
    }

    /// Forget everything: a transmission ended, was dropped, or another one starts.
    pub(super) fn reset(&mut self) {
        self.stash.clear();
        self.padded = false;
    }

    /// Decode the quad-aligned part of `stash + bytes`, keeping at most three characters for the
    /// next slice. `room` is how many more decoded bytes the caller can still store for this
    /// payload; a slice whose decoded size would pass it is refused with
    /// [`TooBig`](B64Error::TooBig) before anything is decoded, so an oversized payload never
    /// becomes bytes in memory.
    pub(super) fn put(&mut self, bytes: &[u8], room: usize) -> Result<Vec<u8>, B64Error> {
        if self.padded && !bytes.is_empty() {
            return Err(B64Error::Invalid); // padding ended this payload; nothing may follow it
        }
        let quads = (self.stash.len() + bytes.len()) & !3;
        if decoded_len(&self.stash, bytes, quads) > room {
            return Err(B64Error::TooBig);
        }
        let mut all = std::mem::take(&mut self.stash);
        all.extend_from_slice(bytes);
        self.stash = all.split_off(quads); // `all` is the quad-aligned prefix now
        if quads == 0 {
            return Ok(Vec::new()); // nothing but a longer carry
        }
        // Padding can only sit at the end of the last quad; anywhere inside it the decode below
        // fails, and that is what makes data after padding invalid rather than silently dropped.
        let decoded = STANDARD.decode(&all).map_err(|_| B64Error::Invalid)?;
        self.padded = all.ends_with(b"=");
        Ok(decoded)
    }

    /// Decode what the carried characters hold at the end of a payload: an incomplete final group,
    /// with or without its padding. `room` is as in [`put`](Decoder::put). A single leftover symbol
    /// carries no whole byte and padding with nothing to pad is not data; either is
    /// [`Invalid`](B64Error::Invalid).
    pub(super) fn finish(&mut self, room: usize) -> Result<Vec<u8>, B64Error> {
        let tail = std::mem::take(&mut self.stash);
        if tail.is_empty() {
            return Ok(Vec::new()); // the payload ended on a quad boundary (padding included)
        }
        if self.padded {
            return Err(B64Error::Invalid); // data after a padded group, across slices
        }
        let Some(at) = tail.iter().position(|&byte| byte != b'=') else {
            return Err(B64Error::Invalid); // padding with nothing to pad
        };
        let body = &tail[at..];
        if body.len() == 1 {
            return Err(B64Error::Invalid); // one symbol carries no whole byte
        }
        // An unpadded group of `n` symbols decodes to `n - 1` bytes (the carry holds at most three,
        // so here `n` is 2 or 3).
        if body.len() - 1 > room {
            return Err(B64Error::TooBig);
        }
        STANDARD_NO_PAD.decode(body).map_err(|_| B64Error::Invalid)
    }
}

/// The decoded size of the quad-aligned part of `stash + bytes`: three quarters of it, less the
/// padding characters at its end, which are the only thing that makes a group shorter than three
/// bytes. Exact for anything that decodes, and never an overestimate, so a slice that fits is not
/// refused for rounding.
fn decoded_len(stash: &[u8], bytes: &[u8], quads: usize) -> usize {
    if quads == 0 {
        return 0;
    }
    let pad = (1..=2).take_while(|&back| byte_at(stash, bytes, quads - back) == Some(b'=')).count();
    3 * (quads / 4) - pad
}

/// The byte at index `i` of the concatenation `stash + bytes`, which is never materialised for the
/// size check.
fn byte_at(stash: &[u8], bytes: &[u8], i: usize) -> Option<u8> {
    match i.checked_sub(stash.len()) {
        Some(i) => bytes.get(i).copied(),
        None => stash.get(i).copied(),
    }
}
