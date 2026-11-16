// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Graphics protocol state machines (feathercritty), fed by the streaming string-sequence
//! callbacks of [`crate::vte`]: `apc_start`/`apc_put`/`apc_end`/`apc_abort` drive
//! [`kitty::KittyParser`], `sixel_start`/`sixel_put` drive [`sixel::SixelDecoder`], and
//! `inline_image_start`/`inline_image_put`/`inline_image_end`/`inline_image_abort` drive
//! [`inline::InlineImageParser`].
//!
//! The parsers read image headers and decode pixels where the protocol itself defines a pixel
//! format (sixel); everything else hands payload bytes to the embedder through events. The limits
//! in [`limits`] and the incremental base64 in [`b64`] are shared by the two protocols that carry
//! base64 payloads, and every buffer here is bounded so remote input cannot make the parser panic,
//! hang or grow without bound.
//!
//! [`placements::PlacementTable`] keeps the terminal's image placements in step with the grid and
//! owns the client-facing image ids; [`kitty_diacritics`] holds the placeholder-cell table and
//! decoder for the graphics protocol's Unicode placeholders.

mod b64;
pub mod inline;
pub mod kitty;
pub mod kitty_diacritics;
pub mod limits;
pub mod placements;
pub mod sixel;

#[cfg(test)]
mod kitty_tests;

#[cfg(test)]
mod placement_tests;

#[cfg(test)]
mod sixel_tests;
