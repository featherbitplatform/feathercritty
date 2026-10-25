// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The unmodified `alacritty_terminal` 0.26.0 side of the differential tests (the oracle).

pub use alacritty_terminal as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;
