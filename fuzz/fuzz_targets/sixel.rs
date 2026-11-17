// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Feeds arbitrary bytes through [`SixelDecoder`], the sixel decoder, the way `Term` feeds it:
//! parameters once at the start, then the data bytes in arbitrary slices, then `finish`.
//!
//! Framing, which the seeds under `fuzz/seeds/sixel/` follow: the leading run of ASCII digits and
//! `;` is the DCS parameter text (`P1;P2;P3`, at most three values, each saturating at
//! [`u16::MAX`]); everything after it is sixel data, fed in chunks whose size comes from the byte
//! they start at, so a mutation moves every split point after it. The last data byte sets the
//! terminal's background colour. The stream is finished twice: once complete, and once after half
//! its bytes, which is what an unterminated sequence or a `CAN` leaves behind. What `finish` returns
//! is checked against the size caps.

#![no_main]

use feathercritty::graphics::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE};
use feathercritty::graphics::sixel::{Rgba, SixelDecoder};
use libfuzzer_sys::fuzz_target;

/// The most parameters a DCS carries here (`P1`, `P2`, `P3`).
const PARAMS: usize = 3;

/// The parameter text: the leading run of digits and separators.
fn params(bytes: &[u8]) -> Vec<u16> {
    let end = bytes.iter().position(|&byte| !byte.is_ascii_digit() && byte != b';').unwrap_or(bytes.len());
    let mut values = Vec::new();
    for field in bytes[..end].split(|&byte| byte == b';') {
        if field.is_empty() || values.len() == PARAMS {
            continue; // vte drops empty parameters; only the first three matter here.
        }
        let value = field.iter().fold(0u64, |acc, &digit| (acc * 10 + (digit - b'0') as u64).min(u16::MAX as u64));
        values.push(value as u16);
    }
    values
}

/// One decode of `data` with `params` and a background colour, in chunks derived from the bytes.
fn decode(params: &[u16], bg: Rgba, data: &[u8]) -> Result<(), String> {
    let mut decoder = SixelDecoder::new(params, bg);
    let mut offset = 0;
    while offset < data.len() {
        // The chunk size comes from content, so editing a byte moves the splits after it.
        let chunk = 1 + data[offset % data.len()] as usize % 7;
        let end = (offset + chunk).min(data.len());
        decoder.put(&data[offset..end]);
        offset = end;
    }
    match decoder.finish() {
        Ok(image) => {
            let pixels = (image.width as u64) * (image.height as u64) * 4;
            if image.rgba.len() != pixels as usize {
                return Err(format!("{}x{} image with {} bytes", image.width, image.height, image.rgba.len()));
            }
            if image.width > MAX_IMAGE_SIDE || image.height > MAX_IMAGE_SIDE || pixels > MAX_IMAGE_BYTES {
                return Err(format!("{}x{} image over the caps", image.width, image.height));
            }
            Ok(())
        },
        Err(_) => Ok(()), // `Empty` is a valid answer for a stream that paints nothing.
    }
}

fuzz_target!(|data: &[u8]| {
    let params = params(data);
    let level = data.last().copied().unwrap_or(0);
    let bg = Rgba::new(level, level / 2, level / 3);
    // The parameter text is not sixel data; a leading `#` or any other byte makes it empty.
    let end = data.iter().position(|&byte| !byte.is_ascii_digit() && byte != b';').unwrap_or(data.len());
    let body = &data[end..];

    if let Err(why) = decode(&params, bg, body) {
        panic!("sixel decode of {params:?} produced {why}");
    }
    // The same stream cut short: a truncated image decodes as far as it got.
    if let Err(why) = decode(&params, bg, &body[..body.len() / 2]) {
        panic!("truncated sixel decode of {params:?} produced {why}");
    }
});
