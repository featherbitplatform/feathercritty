// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The codec of cold history blocks: a warm block (the plain byte block of `history.rs`) is
//! compressed with zstd at level 1, chosen in `benches/RESULTS.md` ("Compressor choice").
//!
//! A cold block is a one byte format tag, the uncompressed length as a `u32` in little endian, and
//! the zstd frame. Cold bytes are only ever produced by [`compress`], so a bad tag or a length
//! mismatch is a bug and panics.

/// Format tag of the only cold layout: zstd level 1 behind a `u32` length.
const TAG: u8 = 1;
const HEADER: usize = 1 + 4;
const LEVEL: i32 = 1;

/// Appends the cold form of `warm` to `out`.
pub(crate) fn compress(warm: &[u8], out: &mut Vec<u8>) {
    let len = u32::try_from(warm.len()).expect("history block exceeds u32 length");
    out.push(TAG);
    out.extend_from_slice(&len.to_le_bytes());
    let start = out.len();
    out.resize(start + zstd::zstd_safe::compress_bound(warm.len()), 0);
    let written = zstd::bulk::compress_to_buffer(warm, &mut out[start..], LEVEL)
        .expect("zstd compression into a bound-sized buffer");
    out.truncate(start + written);
}

/// Clears `out` and fills it with the bytes `cold` was made from.
pub(crate) fn decompress(cold: &[u8], out: &mut Vec<u8>) {
    let len = uncompressed_len(cold);
    out.clear();
    out.resize(len, 0);
    let written = zstd::bulk::decompress_to_buffer(&cold[HEADER..], &mut out[..])
        .expect("corrupt cold history block");
    assert_eq!(written, len, "cold history block length mismatch");
}

/// The length of the block `cold` decompresses to.
pub(crate) fn uncompressed_len(cold: &[u8]) -> usize {
    assert!(cold.len() >= HEADER, "truncated cold history block");
    assert_eq!(cold[0], TAG, "unknown cold history block format tag");
    u32::from_le_bytes([cold[1], cold[2], cold[3], cold[4]]) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(x: &[u8]) {
        let mut cold = Vec::new();
        compress(x, &mut cold);
        assert_eq!(uncompressed_len(&cold), x.len());
        let mut out = Vec::new();
        decompress(&cold, &mut out);
        assert_eq!(out, x);
    }

    #[test]
    fn round_trips_empty_and_full_blocks() {
        round_trip(&[]);
        round_trip(&[7]);
        round_trip(&vec![0; 64 * 1024]);
        // xorshift noise, incompressible.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let noise: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        round_trip(&noise);
        let text: Vec<u8> = (0..)
            .flat_map(|i: u32| {
                format!("2026-11-08 INFO service[{}] request {i} ok\r\n", i % 7).into_bytes()
            })
            .take(64 * 1024)
            .collect();
        round_trip(&text);
    }

    #[test]
    fn reuses_output_buffer() {
        let data = vec![42u8; 64 * 1024];
        let mut cold = Vec::new();
        compress(&data, &mut cold);
        let mut out = Vec::with_capacity(data.len());
        let ptr = out.as_ptr();
        decompress(&cold, &mut out);
        assert_eq!(out, data);
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(out.capacity(), data.len());
    }

    #[test]
    #[should_panic(expected = "unknown cold history block format tag")]
    fn rejects_unknown_tag() {
        let mut cold = Vec::new();
        compress(b"abc", &mut cold);
        cold[0] = 2;
        decompress(&cold, &mut Vec::new());
    }
}
