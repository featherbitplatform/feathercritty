// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Alacritty - The GPU Enhanced Terminal.

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(clippy, deny(warnings))]
// Flagged by clippy releases newer than the upstream code; allowed instead of rewriting it.
#![allow(clippy::question_mark)]

pub mod event;
pub mod event_loop;
pub mod grid;
pub mod index;
pub mod selection;
pub mod sync;
pub mod term;
pub mod thread;
pub mod tty;
pub mod vi_mode;

pub use crate::grid::Grid;
pub use crate::term::Term;
pub use vte;
