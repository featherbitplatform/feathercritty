// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Shared size limits for the graphics protocols (design spec section 6).
//!
//! `MAX_IMAGE_SIDE` and `MAX_IMAGE_BYTES` cap one image: no side above 10 000 pixels, and no
//! decoded RGBA8 buffer above 64 MiB (so a PNG is refused once `width * height * 4` would pass
//! the byte cap, raw formats once their pixel data would). `MAX_PAYLOAD_BYTES` caps one chunked
//! kitty transmission or OSC 1337 payload: past 16 MiB of transmitted data the transmission is
//! abandoned. `STALL` is how long a transmission may go without a new chunk before it is
//! dropped; the parsers take an injected clock so callers (and tests) decide when that happens.

use std::time::Duration;

/// The largest image side, in pixels.
pub const MAX_IMAGE_SIDE: u32 = 10_000;

/// The largest decoded image, in bytes (`width * height * 4` for RGBA8).
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// The largest chunked transmission payload, in transmitted (base64-decoded) bytes.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// How long a chunked transmission may make no progress before it is dropped.
pub const STALL: Duration = Duration::from_secs(10);
