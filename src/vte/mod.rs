// Copied from vte 0.15.0 (Apache-2.0 OR MIT) by the featherbit authors for feathercritty; see
// README.md for the list of changes.

//! Parser for implementing virtual terminal emulators
//!
//! [`Parser`] is implemented according to [Paul Williams' ANSI parser state
//! machine]. The state machine doesn't assign meaning to the parsed data and is
//! thus not itself sufficient for writing a terminal emulator. Instead, it is
//! expected that an implementation of [`Perform`] is provided which does
//! something useful with the parsed data. The [`Parser`] handles the book
//! keeping, and the [`Perform`] gets to simply handle actions.
//!
//! # Examples
//!
//! For an example of using the [`Parser`] please see the examples folder. The
//! example included there simply logs all the actions [`Perform`] does. One
//! quick way to see it in action is to pipe `printf` into it
//!
//! ```sh
//! printf '\x1b[31mExample' | cargo run --example parselog
//! ```
//!
//! # Differences from original state machine description
//!
//! * UTF-8 Support for Input
//! * OSC Strings can be terminated by 0x07
//! * Only supports 7-bit codes
//!
//! feathercritty adds string sequences that are streamed to the [`Perform`] implementation instead
//! of buffered, so a large payload costs no parser memory: APC strings ([`Perform::apc_start`],
//! [`Perform::apc_put`], [`Perform::apc_end`], [`Perform::apc_abort`]), the payload of an OSC 1337
//! inline image (`ESC ] 1337 ; File = <args> : <data>`, [`Perform::osc_stream_start`] and friends)
//! and runs of device control string bytes ([`Perform::put_slice`]). SOS and PM are discarded, as
//! they were.
//!
//! [`Parser`]: struct.Parser.html
//! [`Perform`]: trait.Perform.html
//! [Paul Williams' ANSI parser state machine]: https://vt100.net/emu/dec_ansi_parser

// feathercritty: clippy releases newer than the copied code flag useless borrows in its format
// strings; allowed instead of rewriting the copy, as `clippy::question_mark` is in `src/lib.rs`.
#![allow(clippy::useless_borrows_in_formatting)]

use core::mem::MaybeUninit;
use core::str;

mod params;

pub mod ansi;
// feathercritty: tests for the streamed string sequences added by this fork.
#[cfg(test)]
mod tests_strings;

pub use params::{Params, ParamsIter};

const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC_PARAMS: usize = 16;
const MAX_OSC_RAW: usize = 1024;

// feathercritty: maximum raw bytes kept for an OSC string. Longer OSC strings are discarded
// (not dispatched) unless their first parameter is 1337, whose arguments carry inline image data.
pub const MAX_OSC_BYTES: usize = 64 * 1024;

// feathercritty: maximum bytes of the arguments of an OSC 1337 inline image (`File=<args>:`). The
// args are the only part of such an image the parser keeps; the payload is streamed. An image whose
// args pass this limit without a `:` is aborted (see `OscStream`).
const MAX_OSC_STREAM_ARGS: usize = 4 * 1024;

// feathercritty: the marker in an OSC 1337 string that starts an inline image, matched byte by
// byte so a read split inside it still matches.
const OSC_FILE_MARKER: &[u8] = b"File=";

/// Parser for raw _VTE_ protocol which delegates actions to a [`Perform`]
///
/// [`Perform`]: trait.Perform.html
///
/// Generic over the value for the size of the raw Operating System Command
/// buffer. Only used when the `std` feature is not enabled.
#[derive(Default)]
pub struct Parser<const OSC_RAW_BUF_SIZE: usize = MAX_OSC_RAW> {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_idx: usize,
    params: Params,
    param: u16,
    osc_raw: Vec<u8>,
    osc_params: [(usize, usize); MAX_OSC_PARAMS],
    osc_num_params: usize,
    ignoring: bool,
    // feathercritty: set once an OSC exceeds `MAX_OSC_BYTES` and is not exempt from the cap.
    // While it is set no more bytes are stored and the finished OSC is not dispatched.
    osc_too_long: bool,
    // feathercritty: how far the current OSC string has been recognised as an inline image; see
    // `OscStream`. Reset with the rest of the OSC state.
    osc_stream: OscStream,
    partial_utf8: [u8; 4],
    partial_utf8_len: usize,
}

impl Parser {
    /// Create a new Parser
    pub fn new() -> Parser {
        Default::default()
    }
}

impl<const OSC_RAW_BUF_SIZE: usize> Parser<OSC_RAW_BUF_SIZE> {
    #[inline]
    fn params(&self) -> &Params {
        &self.params
    }

    #[inline]
    fn intermediates(&self) -> &[u8] {
        &self.intermediates[..self.intermediate_idx]
    }

    /// Advance the parser state.
    ///
    /// Requires a [`Perform`] implementation to handle the triggered actions.
    ///
    /// [`Perform`]: trait.Perform.html
    #[inline]
    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut i = 0;

        // Handle partial codepoints from previous calls to `advance`.
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }

        while i != bytes.len() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                // feathercritty: these states consume a run of bytes at a time (see the methods
                // they call). Like the ground state they are not re-entered between the bytes of a
                // run; no action they dispatch can end the parse early.
                State::DcsPassthrough => i += self.advance_dcs_passthrough(performer, &bytes[i..]),
                State::ApcString => i += self.advance_apc_string(performer, &bytes[i..]),
                State::OscImagePayload => {
                    i += self.advance_osc_image_payload(performer, &bytes[i..])
                },
                _ => {
                    // Inlining it results in worse codegen.
                    let byte = bytes[i];
                    self.change_state(performer, byte);
                    i += 1;
                },
            }
        }
    }

    /// Partially advance the parser state.
    ///
    /// This is equivalent to [`Self::advance`], but stops when
    /// [`Perform::terminated`] is true after reading a byte.
    ///
    /// Returns the number of bytes read before termination.
    ///
    /// See [`Perform::advance`] for more details.
    #[inline]
    #[must_use = "Returned value should be used to processs the remaining bytes"]
    pub fn advance_until_terminated<P: Perform>(
        &mut self,
        performer: &mut P,
        bytes: &[u8],
    ) -> usize {
        let mut i = 0;

        // Handle partial codepoints from previous calls to `advance`.
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }

        while i != bytes.len() && !performer.terminated() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                // feathercritty: run-based states (see `Self::advance`); [`Perform::terminated`] is
                // checked once per run instead of once per byte, and none of the actions in a run
                // can set it.
                State::DcsPassthrough => i += self.advance_dcs_passthrough(performer, &bytes[i..]),
                State::ApcString => i += self.advance_apc_string(performer, &bytes[i..]),
                State::OscImagePayload => {
                    i += self.advance_osc_image_payload(performer, &bytes[i..])
                },
                _ => {
                    // Inlining it results in worse codegen.
                    let byte = bytes[i];
                    self.change_state(performer, byte);
                    i += 1;
                },
            }
        }

        i
    }

    #[inline(always)]
    fn change_state<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match self.state {
            State::CsiEntry => self.advance_csi_entry(performer, byte),
            State::CsiIgnore => self.advance_csi_ignore(performer, byte),
            State::CsiIntermediate => self.advance_csi_intermediate(performer, byte),
            State::CsiParam => self.advance_csi_param(performer, byte),
            State::DcsEntry => self.advance_dcs_entry(performer, byte),
            State::DcsIgnore => self.anywhere(performer, byte),
            State::DcsIntermediate => self.advance_dcs_intermediate(performer, byte),
            State::DcsParam => self.advance_dcs_param(performer, byte),
            State::Escape => self.advance_esc(performer, byte),
            State::EscapeIntermediate => self.advance_esc_intermediate(performer, byte),
            State::OscString => self.advance_osc_string(performer, byte),
            State::SosPmApcString => self.anywhere(performer, byte),
            // feathercritty: `DcsPassthrough` and the two streaming string states consume runs of
            // bytes, so the read loop calls them itself with the rest of the input; they are never
            // reached one byte at a time.
            State::DcsPassthrough | State::ApcString | State::OscImagePayload | State::Ground => {
                unreachable!()
            },
        }
    }

    #[inline(always)]
    fn advance_csi_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::CsiIntermediate
            },
            0x30..=0x39 => {
                self.action_paramnext(byte);
                self.state = State::CsiParam
            },
            0x3A => {
                self.action_subparam();
                self.state = State::CsiParam
            },
            0x3B => {
                self.action_param();
                self.state = State::CsiParam
            },
            0x3C..=0x3F => {
                self.action_collect(byte);
                self.state = State::CsiParam
            },
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_ignore<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x3F => (),
            0x40..=0x7E => self.state = State::Ground,
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::CsiIntermediate
            },
            0x30..=0x39 => self.action_paramnext(byte),
            0x3A => self.action_subparam(),
            0x3B => self.action_param(),
            0x3C..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::DcsIntermediate
            },
            0x30..=0x39 => {
                self.action_paramnext(byte);
                self.state = State::DcsParam
            },
            0x3A => {
                self.action_subparam();
                self.state = State::DcsParam
            },
            0x3B => {
                self.action_param();
                self.state = State::DcsParam
            },
            0x3C..=0x3F => {
                self.action_collect(byte);
                self.state = State::DcsParam
            },
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::DcsIntermediate
            },
            0x30..=0x39 => self.action_paramnext(byte),
            0x3A => self.action_subparam(),
            0x3B => self.action_param(),
            0x3C..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    /// Handle a run of device control string bytes.
    ///
    /// feathercritty: the bytes vte hands to the handler one at a time are collected into a run and
    /// passed with [`Perform::put_slice`], so a sixel payload arrives in pieces instead of single
    /// bytes. Which bytes are passed on, which are dropped and which end the string is unchanged;
    /// `CAN` and `SUB` report the cancellation with [`Perform::unhook_cancelled`].
    ///
    /// Returns the number of bytes consumed, up to and including the byte that left the state (or
    /// stopped the run), so the read loop re-enters with whatever state that byte set.
    #[inline]
    fn advance_dcs_passthrough<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // The handler receives `0x00..=0x17`, `0x19` and `0x1C..=0x7E`; the byte that stops the run
        // is one of the ones matched below.
        let run = bytes.iter().position(|&byte| !matches!(byte, 0x00..=0x17 | 0x19 | 0x1C..=0x7E));
        let run = run.unwrap_or(bytes.len());

        if run != 0 {
            performer.put_slice(&bytes[..run]);
            if run == bytes.len() {
                return run;
            }
        }

        match bytes[run] {
            0x18 | 0x1A => {
                performer.unhook_cancelled();
                performer.execute(bytes[run]);
                self.state = State::Ground
            },
            0x1B => {
                performer.unhook();
                self.reset_params();
                self.state = State::Escape
            },
            0x9C => {
                performer.unhook();
                self.state = State::Ground
            },
            // `DEL` and the other C1 controls are dropped, as in vte; the state does not change.
            _ => (),
        }

        run + 1
    }

    #[inline(always)]
    fn advance_esc<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::EscapeIntermediate
            },
            0x30..=0x4F => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x50 => {
                self.reset_params();
                self.state = State::DcsEntry
            },
            0x51..=0x57 => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            // feathercritty: SOS is discarded as before; APC (`0x5F`) streams (see
            // `advance_apc_string`).
            0x58 => self.state = State::SosPmApcString,
            0x59..=0x5A => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x5B => {
                self.reset_params();
                self.state = State::CsiEntry
            },
            0x5C => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x5D => {
                // feathercritty: this also clears the inline-image streaming state (see
                // `reset_osc`).
                self.reset_osc();
                self.state = State::OscString
            },
            // feathercritty: PM (`ESC ^`) is discarded as before; APC (`ESC _`) streams.
            0x5E => self.state = State::SosPmApcString,
            0x5F => {
                performer.apc_start();
                self.state = State::ApcString
            },
            0x60..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            // Anywhere.
            0x18 | 0x1A => {
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => (),
            _ => (),
        }
    }

    #[inline(always)]
    fn advance_esc_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_osc_string<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        // feathercritty: an OSC 1337 whose second parameter starts with `File=` is streamed (see
        // `OscStream`). The marker and the arguments are buffered like any other OSC string, so a
        // string that turns out not to be an inline image dispatches exactly what vte dispatched;
        // only the payload after the `:` bypasses the buffer.
        match self.osc_stream {
            OscStream::Marker { matched } if OSC_FILE_MARKER.get(matched) == Some(&byte) => {
                self.action_osc_put(byte);
                self.osc_stream = if matched + 1 == OSC_FILE_MARKER.len() {
                    OscStream::Args { begin: self.osc_raw.len() }
                } else {
                    OscStream::Marker { matched: matched + 1 }
                };
                return;
            },
            // Not `File=` after all: an ordinary OSC, this byte included.
            OscStream::Marker { .. } => self.osc_stream = OscStream::Buffered,
            OscStream::Args { begin } => return self.advance_osc_args(performer, byte, begin),
            // An aborted image: read on to the terminator and dispatch nothing.
            OscStream::Dead => {
                match byte {
                    0x07 => {
                        self.reset_osc();
                        self.state = State::Ground
                    },
                    0x18 | 0x1A => {
                        self.reset_osc();
                        performer.execute(byte);
                        self.state = State::Ground
                    },
                    0x1B => {
                        self.reset_osc();
                        self.reset_params();
                        self.state = State::Escape
                    },
                    _ => (),
                }

                return;
            },
            OscStream::Buffered => (),
        }

        match byte {
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x07 => {
                self.osc_end(performer, byte);
                self.state = State::Ground
            },
            0x18 | 0x1A => {
                self.osc_end(performer, byte);
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                self.osc_end(performer, byte);
                self.reset_params();
                self.state = State::Escape
            },
            0x3B => {
                // feathercritty: separators up to `MAX_OSC_PARAMS - 1` only end a parameter and are
                // not stored. From there on they are kept in the buffer so the last parameter holds
                // their exact bytes (OSC 1337 arguments contain ';'), including empty parameters.
                if self.osc_num_params < MAX_OSC_PARAMS {
                    self.action_osc_put_param();
                }
                if self.osc_num_params >= MAX_OSC_PARAMS {
                    self.action_osc_put(0x3B);
                }

                // feathercritty: `1337` as the first parameter starts the marker check.
                if self.osc_num_params == 1 && self.osc_is_1337() {
                    self.osc_stream = OscStream::Marker { matched: 0 };
                }
            },
            _ => self.action_osc_put(byte),
        }
    }

    /// Handle one byte of an inline image's arguments (`File = <args> :`).
    ///
    /// The args run from `begin` in the buffer to the first `:`. Nothing has been announced yet, so
    /// a string that ends here dispatches its buffered parameters as an ordinary OSC, as vte does;
    /// one whose args pass `MAX_OSC_STREAM_ARGS` before a `:` aborts the image and is dropped.
    fn advance_osc_args<P: Perform>(&mut self, performer: &mut P, byte: u8, begin: usize) {
        match byte {
            b':' => {
                let args = &self.osc_raw[begin..];
                performer.osc_stream_start(args);
                self.reset_osc();
                self.state = State::OscImagePayload;
            },
            // The string ends before any payload: an ordinary OSC.
            0x07 => {
                self.osc_end(performer, byte);
                self.state = State::Ground
            },
            0x18 | 0x1A => {
                self.osc_end(performer, byte);
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                self.osc_end(performer, byte);
                self.reset_params();
                self.state = State::Escape
            },
            // The C0 controls an OSC string drops are dropped here too.
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => (),
            // Arguments keep their separators: they are the text between `File=` and the `:`. Past
            // the cap with no `:` in sight, the image is aborted and the string dropped.
            _ => {
                if self.osc_raw.len().saturating_sub(begin) >= MAX_OSC_STREAM_ARGS {
                    performer.osc_stream_abort();
                    self.reset_osc();
                    self.osc_stream = OscStream::Dead;
                } else {
                    self.action_osc_put(byte);
                }
            },
        }
    }

    /// Handle a run of APC string bytes.
    ///
    /// feathercritty: vte discards APC content; here every byte up to the terminator is passed on
    /// in one piece, so an APC string of any size costs the parser no memory. Everything but `ESC`,
    /// `CAN` and `SUB` is content. `ESC` ends the string (the `ST` that normally follows it is
    /// dispatched as an escape sequence, which the handler already ignores); `CAN` and `SUB` cancel
    /// it. Returns the bytes consumed, so the read loop re-enters with the state the last byte left
    /// behind.
    #[inline]
    fn advance_apc_string<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        let run = bytes.iter().position(|&byte| matches!(byte, 0x18 | 0x1A | 0x1B));
        let run = run.unwrap_or(bytes.len());

        if run != 0 {
            performer.apc_put(&bytes[..run]);
            if run == bytes.len() {
                return run;
            }
        }

        // Only `ESC` (end) and `CAN`/`SUB` (cancel) can stop a run.
        if bytes[run] == 0x1B {
            performer.apc_end();
            self.reset_params();
            self.state = State::Escape;
        } else {
            let byte = bytes[run];
            performer.apc_abort();
            performer.execute(byte);
            self.state = State::Ground;
        }

        run + 1
    }

    /// Handle a run of inline image payload bytes.
    ///
    /// feathercritty: the bytes an ordinary OSC string would have kept are given to
    /// [`Perform::osc_stream_put`] in one piece instead of buffered, and the C0 controls an OSC
    /// string drops are dropped here, so the handler sees exactly the payload the buffered path
    /// would have dispatched. `BEL` and `ST` end it, `CAN` and `SUB` abort it. Returns the bytes
    /// consumed, so the read loop re-enters with the state the last byte left behind.
    #[inline]
    fn advance_osc_image_payload<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // Below `0x20` sit only the terminators (`BEL`, `CAN`, `SUB`, `ESC`) and the controls an
        // OSC string drops anyway, so a run is the printable part of the input.
        let run = bytes.iter().position(|&byte| byte < 0x20).unwrap_or(bytes.len());

        if run != 0 {
            performer.osc_stream_put(&bytes[..run]);
            if run == bytes.len() {
                return run;
            }
        }

        match bytes[run] {
            0x07 => {
                self.osc_stream_end(performer);
                self.state = State::Ground
            },
            0x1B => {
                self.osc_stream_end(performer);
                self.reset_params();
                self.state = State::Escape
            },
            0x18 | 0x1A => {
                self.osc_stream_abort(performer);
                performer.execute(bytes[run]);
                self.state = State::Ground
            },
            // A control character, dropped as in an ordinary OSC string; the state does not change.
            _ => (),
        }

        run + 1
    }

    /// feathercritty: the inline image payload ended (`BEL` or `ST`).
    #[inline]
    fn osc_stream_end<P: Perform>(&mut self, performer: &mut P) {
        performer.osc_stream_end();
        self.reset_osc();
    }

    /// feathercritty: the inline image payload was cancelled by `CAN` or `SUB`.
    #[inline]
    fn osc_stream_abort<P: Perform>(&mut self, performer: &mut P) {
        performer.osc_stream_abort();
        self.reset_osc();
    }

    #[inline(always)]
    fn anywhere<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x18 | 0x1A => {
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                self.reset_params();
                self.state = State::Escape
            },
            _ => (),
        }
    }

    #[inline]
    fn action_csi_dispatch<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.csi_dispatch(self.params(), self.intermediates(), self.ignoring, byte as char);

        self.state = State::Ground
    }

    #[inline]
    fn action_hook<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.hook(self.params(), self.intermediates(), self.ignoring, byte as char);
        self.state = State::DcsPassthrough;
    }

    #[inline]
    fn action_collect(&mut self, byte: u8) {
        if self.intermediate_idx == MAX_INTERMEDIATES {
            self.ignoring = true;
        } else {
            self.intermediates[self.intermediate_idx] = byte;
            self.intermediate_idx += 1;
        }
    }

    /// Advance to the next subparameter.
    #[inline]
    fn action_subparam(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.extend(self.param);
            self.param = 0;
        }
    }

    /// Advance to the next parameter.
    #[inline]
    fn action_param(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
            self.param = 0;
        }
    }

    /// Advance inside the parameter without terminating it.
    #[inline]
    fn action_paramnext(&mut self, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            // Continue collecting bytes into param.
            self.param = self.param.saturating_mul(10);
            self.param = self.param.saturating_add((byte - b'0') as u16);
        }
    }

    /// Add OSC param separator.
    #[inline]
    fn action_osc_put_param(&mut self) {
        let idx = self.osc_raw.len();

        let param_idx = self.osc_num_params;
        match param_idx {
            // First param is special - 0 to current byte index.
            0 => self.osc_params[param_idx] = (0, idx),

            // Only process up to MAX_OSC_PARAMS.
            MAX_OSC_PARAMS => return,

            // All other params depend on previous indexing.
            _ => {
                let prev = self.osc_params[param_idx - 1];
                let begin = prev.1;
                self.osc_params[param_idx] = (begin, idx);
            },
        }

        self.osc_num_params += 1;
    }

    #[inline(always)]
    fn action_osc_put(&mut self, byte: u8) {
        // feathercritty: bound the OSC buffer. Once it reaches `MAX_OSC_BYTES`, an OSC whose first
        // parameter is not 1337 stops collecting and is discarded; 1337 keeps growing (inline image
        // data). The crossing point triggers the check exactly once per OSC string.
        if self.osc_too_long {
            return;
        }
        if self.osc_raw.len() == MAX_OSC_BYTES && !self.osc_is_1337() {
            self.osc_too_long = true;
            return;
        }
        self.osc_raw.push(byte);
    }

    /// Whether the current OSC string's first parameter is `1337`, exempt from `MAX_OSC_BYTES`.
    #[cold]
    fn osc_is_1337(&self) -> bool {
        // The first parameter runs to where the first separator was recorded (`osc_params[0].1`),
        // or to the end while none has arrived. Separators are not stored in `osc_raw` before the
        // cap, so scanning raw for a ';' would miss it; read the recorded boundary instead.
        let end = if self.osc_num_params == 0 { self.osc_raw.len() } else { self.osc_params[0].1 };
        matches!(str::from_utf8(&self.osc_raw[..end]), Ok(code) if code.parse::<u64>() == Ok(1337))
    }

    fn osc_end<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        self.action_osc_put_param();
        // feathercritty: discard oversized non-1337 OSC strings instead of dispatching them.
        if !self.osc_too_long {
            self.osc_dispatch(performer, byte);
        }
        self.reset_osc();
    }

    /// Reset the OSC string state, including the inline-image streaming state (feathercritty).
    #[inline]
    fn reset_osc(&mut self) {
        self.osc_raw.clear();
        self.osc_num_params = 0;
        self.osc_too_long = false;
        self.osc_stream = OscStream::default();
    }

    /// Reset escape sequence parameters and intermediates.
    #[inline]
    fn reset_params(&mut self) {
        self.intermediate_idx = 0;
        self.ignoring = false;
        self.param = 0;

        self.params.clear();
    }

    /// Separate method for osc_dispatch that borrows self as read-only
    ///
    /// The aliasing is needed here for multiple slices into self.osc_raw
    #[inline]
    fn osc_dispatch<P: Perform>(&self, performer: &mut P, byte: u8) {
        let mut slices: [MaybeUninit<&[u8]>; MAX_OSC_PARAMS] =
            unsafe { MaybeUninit::uninit().assume_init() };

        for (i, slice) in slices.iter_mut().enumerate().take(self.osc_num_params) {
            let indices = self.osc_params[i];
            // feathercritty: parameters past `MAX_OSC_PARAMS` are kept in the last parameter
            // instead of dropped; their separators are stored in the buffer (see the 0x3B
            // handling), so this slice runs to the end and holds the remaining bytes exactly,
            // including empty ones.
            let end = if self.osc_num_params == MAX_OSC_PARAMS && i + 1 == MAX_OSC_PARAMS {
                self.osc_raw.len()
            } else {
                indices.1
            };
            *slice = MaybeUninit::new(&self.osc_raw[indices.0..end]);
        }

        unsafe {
            let num_params = self.osc_num_params;
            let params = &slices[..num_params] as *const [MaybeUninit<&[u8]>] as *const [&[u8]];
            performer.osc_dispatch(&*params, byte == 0x07);
        }
    }

    /// Advance the parser state from ground.
    ///
    /// The ground state is handled separately since it can only be left using
    /// the escape character (`\x1b`). This allows more efficient parsing by
    /// using SIMD search with [`memchr`].
    #[inline]
    fn advance_ground<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // Find the next escape character.
        let num_bytes = bytes.len();
        let plain_chars = memchr::memchr(0x1B, bytes).unwrap_or(num_bytes);

        // If the next character is ESC, just process it and short-circuit.
        if plain_chars == 0 {
            self.state = State::Escape;
            self.reset_params();
            return 1;
        }

        match str::from_utf8(&bytes[..plain_chars]) {
            Ok(parsed) => {
                Self::ground_dispatch(performer, parsed);
                let mut processed = plain_chars;

                // If there's another character, it must be escape so process it directly.
                if processed < num_bytes {
                    self.state = State::Escape;
                    self.reset_params();
                    processed += 1;
                }

                processed
            },
            // Handle invalid and partial utf8.
            Err(err) => {
                // Dispatch all the valid bytes.
                let valid_bytes = err.valid_up_to();
                let parsed = unsafe { str::from_utf8_unchecked(&bytes[..valid_bytes]) };
                Self::ground_dispatch(performer, parsed);

                match err.error_len() {
                    Some(len) => {
                        // Execute C1 escapes or emit replacement character.
                        if len == 1 && bytes[valid_bytes] <= 0x9F {
                            performer.execute(bytes[valid_bytes]);
                        } else {
                            performer.print('�');
                        }

                        // Restart processing after the invalid bytes.
                        //
                        // While we could theoretically try to just re-parse
                        // `bytes[valid_bytes + len..plain_chars]`, it's easier
                        // to just skip it and invalid utf8 is pretty rare anyway.
                        valid_bytes + len
                    },
                    None => {
                        if plain_chars < num_bytes {
                            // Process bytes cut off by escape.
                            performer.print('�');
                            self.state = State::Escape;
                            self.reset_params();
                            plain_chars + 1
                        } else {
                            // Process bytes cut off by the buffer end.
                            let extra_bytes = num_bytes - valid_bytes;
                            let partial_len = self.partial_utf8_len + extra_bytes;
                            self.partial_utf8[self.partial_utf8_len..partial_len]
                                .copy_from_slice(&bytes[valid_bytes..valid_bytes + extra_bytes]);
                            self.partial_utf8_len = partial_len;
                            num_bytes
                        }
                    },
                }
            },
        }
    }

    /// Advance the parser while processing a partial utf8 codepoint.
    #[inline]
    fn advance_partial_utf8<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // Try to copy up to 3 more characters, to ensure the codepoint is complete.
        let old_bytes = self.partial_utf8_len;
        let to_copy = bytes.len().min(self.partial_utf8.len() - old_bytes);
        self.partial_utf8[old_bytes..old_bytes + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.partial_utf8_len += to_copy;

        // Parse the unicode character.
        match str::from_utf8(&self.partial_utf8[..self.partial_utf8_len]) {
            // If the entire buffer is valid, use the first character and continue parsing.
            Ok(parsed) => {
                let c = unsafe { parsed.chars().next().unwrap_unchecked() };
                performer.print(c);

                self.partial_utf8_len = 0;
                c.len_utf8() - old_bytes
            },
            Err(err) => {
                let valid_bytes = err.valid_up_to();
                // If we have any valid bytes, that means we partially copied another
                // utf8 character into `partial_utf8`. Since we only care about the
                // first character, we just ignore the rest.
                if valid_bytes > 0 {
                    let c = unsafe {
                        let parsed = str::from_utf8_unchecked(&self.partial_utf8[..valid_bytes]);
                        parsed.chars().next().unwrap_unchecked()
                    };

                    performer.print(c);

                    self.partial_utf8_len = 0;
                    return valid_bytes - old_bytes;
                }

                match err.error_len() {
                    // If the partial character was also invalid, emit the replacement
                    // character.
                    Some(invalid_len) => {
                        performer.print('�');

                        self.partial_utf8_len = 0;
                        invalid_len - old_bytes
                    },
                    // If the character still isn't complete, wait for more data.
                    None => to_copy,
                }
            },
        }
    }

    /// Handle ground dispatch of print/execute for all characters in a string.
    #[inline]
    fn ground_dispatch<P: Perform>(performer: &mut P, text: &str) {
        for c in text.chars() {
            match c {
                '\x00'..='\x1f' | '\u{80}'..='\u{9f}' => performer.execute(c as u8),
                _ => performer.print(c),
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug, Default, Copy, Clone)]
enum State {
    CsiEntry,
    CsiIgnore,
    CsiIntermediate,
    CsiParam,
    DcsEntry,
    DcsIgnore,
    DcsIntermediate,
    DcsParam,
    DcsPassthrough,
    Escape,
    EscapeIntermediate,
    OscString,
    // feathercritty: the streamed payload of an OSC 1337 inline image (see `advance_osc_string`).
    OscImagePayload,
    SosPmApcString,
    // feathercritty: APC (`ESC _`), which streams; SOS and PM stay discarded in `SosPmApcString`.
    ApcString,
    #[default]
    Ground,
}

/// feathercritty: how far the current OSC string has been recognised as an inline image
/// (`ESC ] 1337 ; File = <args> : <payload>`).
///
/// The marker and the arguments stay in the ordinary OSC buffer, so a string that turns out not to
/// be an image dispatches exactly what vte dispatched. After the `:` that ends the args the parser
/// moves to `State::OscImagePayload` and the payload goes straight to the performer; `Dead` means
/// the args ran over `MAX_OSC_STREAM_ARGS` and the string is dropped instead of dispatched.
#[derive(Debug, Default, Copy, Clone)]
enum OscStream {
    /// An ordinary OSC string: buffer it and dispatch it at the terminator.
    #[default]
    Buffered,

    /// The first parameter is `1337`; matching `OSC_FILE_MARKER` from index `matched` on.
    Marker { matched: usize },

    /// The marker matched; the args are the buffered bytes from `begin` to the first `:`.
    Args { begin: usize },

    /// Aborted: read on to the terminator and dispatch nothing.
    Dead,
}

/// Performs actions requested by the Parser
///
/// Actions in this case mean, for example, handling a CSI escape sequence
/// describing cursor movement, or simply printing characters to the screen.
///
/// The methods on this type correspond to actions described in
/// <http://vt100.net/emu/dec_ansi_parser>. I've done my best to describe them in
/// a useful way in my own words for completeness, but the site should be
/// referenced if something isn't clear. If the site disappears at some point in
/// the future, consider checking archive.org.
pub trait Perform {
    /// Draw a character to the screen and update states.
    fn print(&mut self, _c: char) {}

    /// Execute a C0 or C1 control function.
    fn execute(&mut self, _byte: u8) {}

    /// Invoked when a final character arrives in first part of device control
    /// string.
    ///
    /// The control function should be determined from the private marker, final
    /// character, and execute with a parameter list. A handler should be
    /// selected for remaining characters in the string; the handler
    /// function should subsequently be called by `put` for every character in
    /// the control string.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    /// Pass bytes as part of a device control string to the handle chosen in
    /// `hook`. C0 controls will also be passed to the handler.
    fn put(&mut self, _byte: u8) {}

    /// Pass a run of device control string bytes to the handler chosen in `hook`.
    ///
    /// feathercritty: the parser calls this with whole runs instead of single bytes (the sixel
    /// payload of a `DCS ... q` is streamed). The default passes the run on to [`Self::put`] byte
    /// by byte, so a `Perform` that only implements `put` sees exactly what it saw before.
    #[inline]
    fn put_slice(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.put(byte);
        }
    }

    /// Called when a device control string is terminated.
    ///
    /// The previously selected handler should be notified that the DCS has
    /// terminated.
    fn unhook(&mut self) {}

    /// Called instead of [`Self::unhook`] when a device control string was cancelled by `CAN` or
    /// `SUB` instead of reaching its terminator.
    ///
    /// feathercritty: the default forwards to `unhook`, so a `Perform` that does not care how the
    /// string ended sees exactly what it saw before.
    fn unhook_cancelled(&mut self) {
        self.unhook();
    }

    /// Dispatch an operating system command.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    /// An APC string (`ESC _`) has begun; its content arrives with [`Self::apc_put`].
    ///
    /// feathercritty: vte discarded APC strings. They are streamed now (SOS and PM are still
    /// discarded), so an APC string costs the parser no memory at all.
    fn apc_start(&mut self) {}

    /// A run of APC string bytes, as far as the reader had them.
    fn apc_put(&mut self, _bytes: &[u8]) {}

    /// The APC string ended (its terminator arrived).
    fn apc_end(&mut self) {}

    /// The APC string was cancelled by `CAN` or `SUB` before its terminator arrived.
    fn apc_abort(&mut self) {}

    /// An OSC 1337 inline image (`ESC ] 1337 ; File = <args> : <payload>`) has begun.
    ///
    /// feathercritty: `args` are the bytes between `File=` and the first `:` of the string, at most
    /// 4 KiB of them (an image whose args are longer is aborted). The payload arrives with
    /// [`Self::osc_stream_put`] and is never buffered.
    fn osc_stream_start(&mut self, _args: &[u8]) {}

    /// A run of inline image payload bytes, as far as the reader had them.
    fn osc_stream_put(&mut self, _bytes: &[u8]) {}

    /// The inline image ended (`BEL` or `ST`).
    fn osc_stream_end(&mut self) {}

    /// The inline image was cancelled (`CAN` or `SUB`) or its args passed the limit.
    fn osc_stream_abort(&mut self) {}

    /// A final character has arrived for a CSI sequence
    ///
    /// The `ignore` flag indicates that either more than two intermediates
    /// arrived or the number of parameters exceeded the maximum supported
    /// length, and subsequent characters were ignored.
    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }

    /// The final character of an escape sequence has arrived.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}

    /// Whether the parser should terminate prematurely.
    ///
    /// This can be used in conjunction with
    /// [`Parser::advance_until_terminated`] to terminate the parser after
    /// receiving certain escape sequences like synchronized updates.
    ///
    /// This is checked after every parsed byte, so no expensive computation
    /// should take place in this function.
    #[inline(always)]
    fn terminated(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    const OSC_BYTES: &[u8] = &[
        0x1B, 0x5D, // Begin OSC
        b'2', b';', b'j', b'w', b'i', b'l', b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd',
        b'e', b's', b'k', b':', b' ', b'~', b'/', b'c', b'o', b'd', b'e', b'/', b'a', b'l', b'a',
        b'c', b'r', b'i', b't', b't', b'y', 0x07, // End OSC
    ];

    #[derive(Default)]
    struct Dispatcher {
        dispatched: Vec<Sequence>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Sequence {
        Osc(Vec<Vec<u8>>, bool),
        Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
        Esc(Vec<u8>, bool, u8),
        DcsHook(Vec<Vec<u16>>, Vec<u8>, bool, char),
        DcsPut(u8),
        Print(char),
        Execute(u8),
        DcsUnhook,
    }

    impl Perform for Dispatcher {
        fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
            let params = params.iter().map(|p| p.to_vec()).collect();
            self.dispatched.push(Sequence::Osc(params, bell_terminated));
        }

        fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(|subparam| subparam.to_vec()).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::Csi(params, intermediates, ignore, c));
        }

        fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::Esc(intermediates, ignore, byte));
        }

        fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(|subparam| subparam.to_vec()).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::DcsHook(params, intermediates, ignore, c));
        }

        fn put(&mut self, byte: u8) {
            self.dispatched.push(Sequence::DcsPut(byte));
        }

        fn unhook(&mut self) {
            self.dispatched.push(Sequence::DcsUnhook);
        }

        fn print(&mut self, c: char) {
            self.dispatched.push(Sequence::Print(c));
        }

        fn execute(&mut self, byte: u8) {
            self.dispatched.push(Sequence::Execute(byte));
        }
    }

    #[test]
    fn parse_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, OSC_BYTES);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], &OSC_BYTES[2..3]);
                assert_eq!(params[1], &OSC_BYTES[4..(OSC_BYTES.len() - 1)]);
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_empty_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &[0x1B, 0x5D, 0x07]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(..) => (),
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_osc_max_params() {
        let content = ";".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1b]{}\x1b", &content).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                // feathercritty: MAX_OSC_PARAMS parameters are kept and the rest stay joined in the
                // last one WITH their separators, so an all-empty OSC keeps 15 empty parameters and
                // a last one holding the remaining ones joined by ';' (see `action_osc_put_param`).
                let fields = content.split(';').collect::<Vec<_>>();
                assert_eq!(params.len(), MAX_OSC_PARAMS);
                for (i, field) in fields.iter().take(MAX_OSC_PARAMS - 1).enumerate() {
                    assert_eq!(params[i], field.as_bytes());
                }
                let joined = fields[MAX_OSC_PARAMS - 1..].join(";");
                assert_eq!(params[MAX_OSC_PARAMS - 1], joined.as_bytes());
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn osc_bell_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x07";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, true) => (),
            _ => panic!("expected osc with bell terminator"),
        }
    }

    #[test]
    fn osc_c0_st_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, false) => (),
            _ => panic!("expected osc with ST terminator"),
        }
    }

    #[test]
    fn parse_osc_with_utf8_arguments() {
        const INPUT: &[u8] = &[
            0x0D, 0x1B, 0x5D, 0x32, 0x3B, 0x65, 0x63, 0x68, 0x6F, 0x20, 0x27, 0xC2, 0xAF, 0x5C,
            0x5F, 0x28, 0xE3, 0x83, 0x84, 0x29, 0x5F, 0x2F, 0xC2, 0xAF, 0x27, 0x20, 0x26, 0x26,
            0x20, 0x73, 0x6C, 0x65, 0x65, 0x70, 0x20, 0x31, 0x07,
        ];
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(b'\r'));
        let osc_data = INPUT[5..(INPUT.len() - 1)].into();
        assert_eq!(dispatcher.dispatched[1], Sequence::Osc(vec![vec![b'2'], osc_data], true));
        assert_eq!(dispatcher.dispatched.len(), 2);
    }

    #[test]
    fn osc_containing_string_terminator() {
        const INPUT: &[u8] = b"\x1b]2;\xe6\x9c\xab\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params[1], &INPUT[4..(INPUT.len() - 2)]);
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn exceed_max_buffer_size() {
        const NUM_BYTES: usize = MAX_OSC_RAW + 100;
        const INPUT_START: &[u8] = b"\x1b]52;s";
        const INPUT_END: &[u8] = b"\x07";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        // Create valid OSC escape
        parser.advance(&mut dispatcher, INPUT_START);

        // Exceed max buffer size
        parser.advance(&mut dispatcher, &[b'a'; NUM_BYTES]);

        // Terminate escape for dispatch
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], b"52");
                assert_eq!(params[1].len(), NUM_BYTES + INPUT_END.len());
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn oversized_osc_is_discarded() {
        // feathercritty: an OSC string longer than MAX_OSC_BYTES whose first parameter is not 1337
        // dispatches nothing, and the parser keeps working after it.
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b]52;");
        parser.advance(&mut dispatcher, &[b'a'; 70_000]);
        parser.advance(&mut dispatcher, b"\x07");

        assert!(dispatcher.dispatched.is_empty());

        // The next sequence still parses and dispatches.
        parser.advance(&mut dispatcher, b"\x1b[1m");
        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, c) => {
                assert_eq!(params, &[[1]]);
                assert!(intermediates.is_empty());
                assert!(!ignore);
                assert_eq!(*c, 'm');
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn osc_params_past_sixteen_are_joined() {
        // feathercritty: parameters past MAX_OSC_PARAMS are kept in the last parameter instead of
        // dropped, together with their ';' separators, so it holds the remaining bytes exactly.
        let check = |args: &[String]| {
            let input = format!("\x1b]{}\x07", args.join(";")).into_bytes();
            let mut dispatcher = Dispatcher::default();
            let mut parser = Parser::new();
            parser.advance(&mut dispatcher, &input);

            assert_eq!(dispatcher.dispatched.len(), 1);
            match &dispatcher.dispatched[0] {
                Sequence::Osc(params, _) => {
                    // MAX_OSC_PARAMS parameters are kept; the rest stay joined in the last one.
                    assert_eq!(params.len(), MAX_OSC_PARAMS);
                    for (i, arg) in args.iter().take(MAX_OSC_PARAMS - 1).enumerate() {
                        assert_eq!(params[i], arg.as_bytes());
                    }
                    let joined = args[MAX_OSC_PARAMS - 1..].join(";");
                    assert_eq!(params[MAX_OSC_PARAMS - 1], joined.as_bytes());
                },
                _ => panic!("expected osc sequence"),
            }
        };

        // More than MAX_OSC_PARAMS parameters.
        let named: Vec<String> = (0..MAX_OSC_PARAMS + 9).map(|i| format!("p{i}")).collect();
        check(&named);

        // An empty parameter past the sixteenth (`;;`) is kept in the joined text.
        let mut with_empty = named;
        with_empty[MAX_OSC_PARAMS] = String::new();
        check(&with_empty);
    }

    #[test]
    fn parse_csi_max_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS - 1 because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS
        let params = "1;".repeat(params::MAX_PARAMS - 1);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_ignore_long_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS + 1
        let params = "1;".repeat(params::MAX_PARAMS);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_trailing_semicolon() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[4;m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[4], [0]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_leading_semicolon() {
        // Create dispatcher and check state
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[;4m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[0], [4]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_long_csi_param() {
        // The important part is the parameter, which is (i64::MAX + 1)
        const INPUT: &[u8] = b"\x1b[9223372036854775808m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[u16::MAX]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b[?1049h";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"?");
                assert_eq!(params, &[[1049]]);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_subparameters() {
        const INPUT: &[u8] = b"\x1b[38:2:255:0:255;1m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(params, &[vec![38, 2, 255, 0, 255], vec![1]]);
                // feathercritty: `is_empty` instead of `assert_eq!(.., &[])`; with `serde_json` as
                // a dev-dependency the empty slice's element type is ambiguous (`u8:
                // PartialEq<Value>`).
                assert!(intermediates.is_empty());
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_dcs_max_params() {
        let params = "1;".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1bP{}p", &params[..]).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(params.iter().all(|param| param == &[1]));
                assert!(ignore);
            },
            _ => panic!("expected dcs sequence"),
        }
    }

    #[test]
    fn dcs_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1bP1$tx\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"$");
                assert_eq!(params, &[[1]]);
                assert!(!ignore);
            },
            _ => panic!("expected dcs sequence"),
        }
        assert_eq!(dispatcher.dispatched[1], Sequence::DcsPut(b'x'));
        assert_eq!(dispatcher.dispatched[2], Sequence::DcsUnhook);
    }

    #[test]
    fn parse_dcs() {
        const INPUT: &[u8] = b"\x1bP0;1|17/ab\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 7);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, _, c) => {
                assert_eq!(params, &[[0], [1]]);
                assert_eq!(c, &'|');
            },
            _ => panic!("expected dcs sequence"),
        }
        for (i, byte) in b"17/ab".iter().enumerate() {
            assert_eq!(dispatcher.dispatched[1 + i], Sequence::DcsPut(*byte));
        }
        assert_eq!(dispatcher.dispatched[6], Sequence::DcsUnhook);
    }

    #[test]
    fn intermediate_reset_on_dcs_exit() {
        const INPUT: &[u8] = b"\x1bP=1sZZZ\x1b+\x5c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        match &dispatcher.dispatched[5] {
            Sequence::Esc(intermediates, ..) => assert_eq!(intermediates, b"+"),
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b(A";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Esc(intermediates, ignore, byte) => {
                assert_eq!(intermediates, b"(");
                assert_eq!(*byte, b'A');
                assert!(!ignore);
            },
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset_intermediates() {
        const INPUT: &[u8] = b"\x1b[?2004l\x1b#8";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Csi(vec![vec![2004]], vec![63], false, 'l'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Esc(vec![35], false, 56));
    }

    #[test]
    fn params_buffer_filled_with_subparam() {
        const INPUT: &[u8] = b"\x1b[::::::::::::::::::::::::::::::::x\x1b";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, c) => {
                // feathercritty: `is_empty` instead of `assert_eq!(.., &[])`; with `serde_json` as
                // a dev-dependency the empty slice's element type is ambiguous (`u8:
                // PartialEq<Value>`).
                assert!(intermediates.is_empty());
                assert_eq!(params, &[[0; 32]]);
                assert_eq!(c, &'x');
                assert!(ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn unicode() {
        const INPUT: &[u8] = b"\xF0\x9F\x8E\x89_\xF0\x9F\xA6\x80\xF0\x9F\xA6\x80_\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🎉'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[4], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[5], Sequence::Print('🎉'));
    }

    #[test]
    fn invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_utf8() {
        const INPUT: &[u8] = b"\xF0\x9F\x9A\x80";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🚀'));
    }

    #[test]
    fn partial_utf8_separating_utf8() {
        // This is different from the `partial_utf8` test since it has a multi-byte UTF8
        // character after the partial UTF8 state, causing a partial byte to be present
        // in the `partial_utf8` buffer after the 2-byte codepoint.

        // "ĸ🎉"
        const INPUT: &[u8] = b"\xC4\xB8\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..]);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('ĸ'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('🎉'));
    }

    #[test]
    fn partial_invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_invalid_utf8_split() {
        const INPUT: &[u8] = b"\xE4\xBF\x99\xB5";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..2]);
        parser.advance(&mut dispatcher, &INPUT[2..]);

        assert_eq!(dispatcher.dispatched[0], Sequence::Print('俙'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
    }

    #[test]
    fn partial_utf8_into_esc() {
        const INPUT: &[u8] = b"\xD8\x1b012";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 4);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Esc(Vec::new(), false, b'0'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('1'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('2'));
    }

    #[test]
    fn c1s() {
        const INPUT: &[u8] = b"\x00\x1f\x80\x90\x98\x9b\x9c\x9d\x9e\x9fa";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 11);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(31));
        assert_eq!(dispatcher.dispatched[2], Sequence::Execute(128));
        assert_eq!(dispatcher.dispatched[3], Sequence::Execute(144));
        assert_eq!(dispatcher.dispatched[4], Sequence::Execute(152));
        assert_eq!(dispatcher.dispatched[5], Sequence::Execute(155));
        assert_eq!(dispatcher.dispatched[6], Sequence::Execute(156));
        assert_eq!(dispatcher.dispatched[7], Sequence::Execute(157));
        assert_eq!(dispatcher.dispatched[8], Sequence::Execute(158));
        assert_eq!(dispatcher.dispatched[9], Sequence::Execute(159));
        assert_eq!(dispatcher.dispatched[10], Sequence::Print('a'));
    }

    #[test]
    fn execute_anywhere() {
        const INPUT: &[u8] = b"\x18\x1a";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0x18));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(0x1A));
    }
}
