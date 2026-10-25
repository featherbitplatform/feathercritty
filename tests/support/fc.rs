// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The feathercritty side of the differential tests.

pub use feathercritty as k;

// The same source is compiled once per crate on purpose.
#[allow(clippy::duplicate_mod)]
#[path = "term_impl.rs"]
mod imp;

pub use imp::*;
