// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Graphics protocol state machines (feathercritty), fed by the streaming string-sequence
//! callbacks of [`crate::vte`]: `apc_start`/`apc_put`/`apc_end`/`apc_abort` drive
//! [`kitty::KittyParser`], and the sixel and OSC 1337 parsers arrive with their tasks.
//!
//! The parsers read image headers and decode pixels where the protocol itself defines a pixel
//! format (sixel); everything else hands payload bytes to the embedder through events. The
//! limits in [`limits`] are shared by all of them, and every buffer here is bounded so remote
//! input cannot make the parser panic, hang or grow without bound.

pub mod kitty;
pub mod limits;

#[cfg(test)]
mod kitty_tests;
