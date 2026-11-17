// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Tests for the kitty graphics protocol state machine (`kitty.rs`).
//!
//! Time is injected through `KittyParser::end(now)`, so nothing here sleeps: a stalled
//! transmission is produced by asking `end` with a `now` past the stall timeout, never by
//! waiting. Payload fixtures are built with the same `base64` and `flate2` crates the parser
//! uses; the PNG fixture is written by hand (with real CRC-32 values) because the parser only
//! reads its header.

use std::io::Write as _;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::kitty::*;
use super::limits::*;

/// Feed one complete APC content (the bytes between `ESC _ G` and the terminator) as its own
/// transmission sequence: `start`, one `put`, `end`.
fn parse(parser: &mut KittyParser, frame: &[u8], now: Instant) -> Option<KittyCommand> {
    parser.start();
    parser.put(frame);
    parser.end(now)
}

/// One frame into a fresh parser; returns the command and the rejection, if any.
fn once(frame: &[u8]) -> (Option<KittyCommand>, Option<KittyReject>) {
    let mut parser = KittyParser::new();
    let cmd = parse(&mut parser, frame, Instant::now());
    (cmd, parser.take_reject())
}

/// `G<control>;payload` as APC content bytes (the `ESC _ G` introducer's final byte is part of
/// the content the parser sees).
fn apc(control: &str, payload_b64: &[u8]) -> Vec<u8> {
    let mut v = b"G".to_vec();
    v.extend_from_slice(control.as_bytes());
    v.push(b';');
    v.extend_from_slice(payload_b64);
    v
}

/// The base64 text of `bytes`.
fn b64(bytes: &[u8]) -> Vec<u8> {
    STANDARD.encode(bytes).into_bytes()
}

/// RFC 1950 (zlib) compression, the format `o=z` specifies.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// The CRC-32 of the PNG chunk spec (ISO 3309 / ITU-T V.42, as zlib uses it).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & !(crc & 1));
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = (data.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(data);
    v.extend_from_slice(&crc32(&[kind.as_slice(), data].concat()).to_be_bytes());
    v
}

/// A complete minimal PNG (8-bit RGBA) of the given size. The IDAT is a valid zlib stream but
/// its pixel content does not have to match the header: the parser only reads the header, and
/// decoding the pixels is the embedder's job.
fn png_blob(width: u32, height: u32) -> Vec<u8> {
    let mut ihdr = (width).to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // depth 8, RGBA color type, no interlace
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    v.extend_from_slice(&png_chunk(b"IDAT", &zlib(&[0u8; 5])));
    v.extend_from_slice(&png_chunk(b"IEND", b""));
    v
}

#[test]
fn parses_all_keys() {
    let now = Instant::now();
    let mut parser = KittyParser::new();

    // A transmit-and-display (`a=T`) of a PNG carrying every supported key.
    let png = png_blob(3, 2);
    let cmd = parse(
        &mut parser,
        &apc("a=T,f=100,i=7,p=3,c=4,r=5,x=6,y=7,w=8,h=9,X=1,Y=2,z=-3,C=1,U=1,q=0,m=0", &b64(&png)),
        now,
    );
    let Some(KittyCommand::Transmit { id, number, format, bytes, then_place, quiet }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(id, Some(7));
    assert_eq!(number, None);
    assert_eq!(format, KittyFormat::Png { width: 3, height: 2 });
    assert_eq!(bytes, png);
    assert_eq!(quiet, 0);
    let place = then_place.expect("a=T carries a placement");
    assert_eq!(place, KittyPlace {
        id: Some(7),
        number: None,
        placement_id: Some(3),
        cols: 4,
        rows: 5,
        src: Some(Rect { x: 6, y: 7, w: 8, h: 9 }),
        x_off: 1,
        y_off: 2,
        z: -3,
        virtual_: true,
        cursor_movement: false,
        quiet: 0,
    });

    // A placement of a stored image (`a=p`); the defaults are cursor-relative, moving.
    let cmd = parse(&mut parser, &apc("a=p,i=9,p=2,c=1,r=1", b""), now);
    assert_eq!(
        cmd,
        Some(KittyCommand::Place(KittyPlace {
            id: Some(9),
            number: None,
            placement_id: Some(2),
            cols: 1,
            rows: 1,
            src: None,
            x_off: 0,
            y_off: 0,
            z: 0,
            virtual_: false,
            cursor_movement: true,
            quiet: 0,
        }))
    );

    // The query action (`a=q`).
    let cmd = parse(&mut parser, &apc("a=q,i=5", b"AAAA"), now);
    assert_eq!(cmd, Some(KittyCommand::Query { id: 5, quiet: 0 }));

    // A plain transmit (`a=t`, the default) of raw RGB: no placement follows.
    let cmd = parse(&mut parser, &apc("f=24,s=1,v=1", &b64(&[1, 2, 3])), now);
    assert_eq!(
        cmd,
        Some(KittyCommand::Transmit {
            id: None,
            number: None,
            format: KittyFormat::Rgb { width: 1, height: 1 },
            bytes: vec![1, 2, 3],
            then_place: None,
            quiet: 0,
        })
    );

    // Every `d=` selector, lowercase and uppercase (uppercase also frees the stored data).
    let cases = [
        ("a=d", DeleteTarget::All),
        ("a=d,d=A", DeleteTarget::All),
        ("a=d,d=i,i=3,p=2", DeleteTarget::ImageId { image: Some(3), placement: Some(2) }),
        ("a=d,d=I,i=3", DeleteTarget::ImageId { image: Some(3), placement: None }),
        ("a=d,d=n,I=4", DeleteTarget::Number { number: Some(4), placement: None }),
        ("a=d,d=N,I=4,p=1", DeleteTarget::Number { number: Some(4), placement: Some(1) }),
        ("a=d,d=c", DeleteTarget::CursorCells),
        ("a=d,d=C", DeleteTarget::CursorCells),
        ("a=d,d=p,x=3,y=4", DeleteTarget::Cell { x: 3, y: 4 }),
        ("a=d,d=P,x=1,y=2", DeleteTarget::Cell { x: 1, y: 2 }),
        ("a=d,d=q,x=1,y=2,z=-5", DeleteTarget::CellZ { x: 1, y: 2, z: -5 }),
        ("a=d,d=Q,x=1,y=2,z=5", DeleteTarget::CellZ { x: 1, y: 2, z: 5 }),
        ("a=d,d=x,x=7", DeleteTarget::Column(7)),
        ("a=d,d=X,x=7", DeleteTarget::Column(7)),
        ("a=d,d=y,y=8", DeleteTarget::Row(8)),
        ("a=d,d=Y,y=8", DeleteTarget::Row(8)),
        ("a=d,d=z,z=-9", DeleteTarget::ZIndex(-9)),
        ("a=d,d=Z,z=2", DeleteTarget::ZIndex(2)),
        ("a=d,d=r,x=1,y=5", DeleteTarget::IdRange { from: 1, to: 5 }),
        ("a=d,d=R,x=1,y=5", DeleteTarget::IdRange { from: 1, to: 5 }),
    ];
    for (control, target) in cases {
        // The uppercase variants are those whose selector letter (`d=X` in `a=d,d=X`) is uppercase.
        let upper = control.len() >= 7 && control.as_bytes()[6].is_ascii_uppercase();
        let (cmd, rej) = once(&apc(control, b""));
        assert!(rej.is_none(), "{control}: {rej:?}");
        assert_eq!(
            cmd,
            Some(KittyCommand::Delete(KittyDelete { target, free_data: upper, quiet: 0 })),
            "{control}"
        );
    }

    // Quiet travels with the command it silences.
    let (cmd, rej) = once(&apc("a=q,i=5,q=2", b"AAAA"));
    assert!(rej.is_none());
    assert_eq!(cmd, Some(KittyCommand::Query { id: 5, quiet: 2 }));
    let (_, rej) = once(&apc("a=d,d=i,i=1,q=1", b""));
    assert!(rej.is_none());
}

#[test]
fn chunked_transmission_across_sequences() {
    let now = Instant::now();
    let mut parser = KittyParser::new();

    // Three chunks of three raw bytes each; only the last closes the transmission (`m=0`).
    assert_eq!(parse(&mut parser, &apc("f=32,s=1,v=2,i=11,m=1", &b64(&[1, 2, 3])), now), None);
    assert_eq!(parse(&mut parser, &apc("m=1", &b64(&[4, 5, 6])), now), None);
    let cmd = parse(&mut parser, &apc("m=0", &b64(&[7, 8])), now);
    let Some(KittyCommand::Transmit { id, number, format, bytes, then_place, quiet }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(id, Some(11));
    assert_eq!(number, None);
    assert_eq!(format, KittyFormat::Rgba { width: 1, height: 2 });
    assert_eq!(bytes, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(then_place, None);
    assert_eq!(quiet, 0);
    assert!(parser.take_reject().is_none());

    // A chunk carrying an `i` that names a different image starts a new transmission instead.
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=2,m=1,i=1", &b64(&[9, 9, 9])), now), None);
    let cmd = parse(&mut parser, &apc("f=24,s=1,v=2,m=1,i=2", &b64(&[8, 8, 8])), now);
    assert!(cmd.is_none()); // chunk one of a new transmission (m=1): still open
    let cmd = parse(&mut parser, &apc("m=0,i=2", &b64(&[7, 7, 7])), now);
    let Some(KittyCommand::Transmit { id, bytes, format, .. }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(id, Some(2));
    assert_eq!(format, KittyFormat::Rgb { width: 1, height: 2 });
    // The old transmission's data was dropped when the mismatched chunk replaced it.
    assert_eq!(bytes, vec![8, 8, 8, 7, 7, 7]);
}

#[test]
fn base64_split_mid_quad() {
    // Six raw pixels as eight base64 characters: a width of two for RGB24 is exactly these bytes.
    let raw = vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02];
    let frame = apc("f=24,s=2,v=1", &b64(&raw));
    let now = Instant::now();

    let (whole, rej) = once(&frame);
    assert!(rej.is_none());
    assert_eq!(
        whole,
        Some(KittyCommand::Transmit {
            id: None,
            number: None,
            format: KittyFormat::Rgb { width: 2, height: 1 },
            bytes: raw.clone(),
            then_place: None,
            quiet: 0,
        })
    );

    // Every possible split point of the sequence gives the same command.
    for split in 1..frame.len() {
        let mut parser = KittyParser::new();
        parser.start();
        parser.put(&frame[..split]);
        parser.put(&frame[split..]);
        let cmd = parser.end(now);
        assert_eq!(cmd, whole, "split at {split}");
        assert!(parser.take_reject().is_none(), "split at {split} rejected");
    }

    // Also splitting inside a chunked transmission's payload text.
    let chunk1 = apc("f=24,s=4,v=1,m=1", &b64(&raw)); // two chunks of 6 bytes make 12: 4x1 RGB
    for split in 1..chunk1.len() {
        let mut parser = KittyParser::new();
        parser.start();
        parser.put(&chunk1[..split]);
        parser.put(&chunk1[split..]);
        assert_eq!(parser.end(now), None, "split at {split} finished early");
        let cmd = parse(&mut parser, &apc("m=0", &b64(&raw)), now);
        let Some(KittyCommand::Transmit { bytes, .. }) = cmd else {
            panic!("split at {split}: expected a transmit command, got {cmd:?}");
        };
        assert_eq!(bytes, [raw.as_slice(), raw.as_slice()].concat());
    }
}

#[test]
fn padding_in_a_later_chunk_is_refused() {
    // A padded group ends the payload: base64 in a later chunk is refused exactly as it would be
    // inside the same one. The first chunk closes its quad with padding and leaves the
    // transmission open (`m=1`); data in the next one is EINVAL.
    let now = Instant::now();
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"QQ=="), now), None);
    assert!(parser.take_reject().is_none());
    assert_eq!(parse(&mut parser, &apc("m=0", b"QQ=="), now), None);
    assert_eq!(parser.take_reject().map(|r| r.error), Some(KittyError::EINVAL));

    // The same bytes arriving at once are refused too — mid-stream padding fails the decode of the
    // quad-aligned part, whether it lands in this chunk's text or was carried from the last one.
    let (cmd, rej) = once(&apc("f=24,s=1,v=1", b"QQ==QQ=="));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // Padding that legitimately ends the payload still closes a transmission normally: four bytes
    // base64 to `AQIDBA==`, and the killed parser is clean for it.
    let cmd = parse(&mut parser, &apc("f=32,s=1,v=1", &b64(&[1, 2, 3, 4])), now);
    assert!(matches!(cmd, Some(KittyCommand::Transmit { .. })));
    assert!(parser.take_reject().is_none());
}

#[test]
fn zlib_payload_inflates_within_cap() {
    let raw: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    let compressed = zlib(&raw);
    assert!(compressed.len() < raw.len());
    let frame = apc("f=32,s=10,v=25,o=z,i=1", &b64(&compressed));

    // Whole, and split at every byte: the same inflated payload either way.
    let (whole, rej) = once(&frame);
    assert!(rej.is_none(), "{rej:?}");
    assert_eq!(
        whole,
        Some(KittyCommand::Transmit {
            id: Some(1),
            number: None,
            format: KittyFormat::Rgba { width: 10, height: 25 },
            bytes: raw.clone(),
            then_place: None,
            quiet: 0,
        })
    );
    let now = Instant::now();
    for split in 1..frame.len() {
        let mut parser = KittyParser::new();
        parser.start();
        parser.put(&frame[..split]);
        parser.put(&frame[split..]);
        assert_eq!(parser.end(now), whole, "split at {split}");
    }

    // Chunked compressed data: the inflate state carries across sequences.
    let mut parser = KittyParser::new();
    let text = b64(&compressed);
    let half = text.len() / 2;
    assert_eq!(parse(&mut parser, &apc("f=32,s=10,v=25,o=z,m=1", &text[..half]), now), None);
    let cmd = parse(&mut parser, &apc("m=0", &text[half..]), now);
    let Some(KittyCommand::Transmit { bytes, .. }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(bytes, raw);
    assert!(parser.take_reject().is_none());

    // Truncated compressed data never reached the stream end: EINVAL.
    let cut = zlib(&raw);
    let frame = apc("f=32,s=10,v=25,o=z", &b64(&cut[..cut.len() - 3]));
    let (cmd, rej) = once(&frame);
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // A bad zlib stream is EINVAL too.
    let frame = apc("f=32,s=1,v=1,o=z", &b64(b"not compressed at all"));
    let (cmd, rej) = once(&frame);
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));
}

#[test]
fn zip_bomb_is_enomem() {
    // A payload that fits the input cap but inflates past `MAX_IMAGE_BYTES`. The declared
    // dimensions (4096x4096 RGBA = exactly the cap) pass validation; the 65th MiB does not.
    let bomb = vec![0u8; MAX_IMAGE_BYTES as usize + 1];
    let frame = apc("f=32,s=4096,v=4096,o=z,i=1", &b64(&zlib(&bomb)));
    let (cmd, rej) = once(&frame);
    assert_eq!(cmd, None);
    let reject = rej.expect("the bomb is refused");
    assert_eq!(reject.error, KittyError::ENOMEM);
    assert_eq!(reject.id, 1);
    assert_eq!(reject.quiet, 0);

    // Chunked: the drop happens mid-transmission and holds nothing afterwards.
    let now = Instant::now();
    let mut parser = KittyParser::new();
    let text = b64(&zlib(&bomb));
    let chunk = 4096;
    parse(&mut parser, &apc("f=32,s=4096,v=4096,o=z,m=1", &text[..chunk]), now);
    for part in text[chunk..].chunks(chunk) {
        let cmd = parse(&mut parser, &apc("m=1", part), now);
        if let Some(reject) = parser.take_reject() {
            assert_eq!(reject.error, KittyError::ENOMEM);
            assert_eq!(cmd, None);
            assert_eq!(parser.held_bytes(), 0);
            return;
        }
    }
    panic!("the chunked bomb was never refused");
}

#[test]
fn file_media_is_refused_ebadf() {
    // A path that exists where the tests run: refusing it must be about the medium, not the file.
    assert!(std::path::Path::new("README.md").exists());
    let payload = b64(b"README.md");
    for media in ["f", "t", "s"] {
        let frame = apc(&format!("a=t,t={media},i=4"), &payload);
        let (cmd, rej) = once(&frame);
        assert_eq!(cmd, None, "t={media} placed anyway");
        let reject = rej.unwrap_or_else(|| panic!("t={media} was not refused"));
        assert_eq!(reject.error, KittyError::EBADF, "t={media} got the wrong code");
        assert_eq!(reject.id, 4);
        // Still there: nothing was read, moved or deleted.
        assert!(std::path::Path::new("README.md").exists());
    }
    // `a=T` with file media is refused the same way (the medium is checked before anything else).
    let (cmd, rej) = once(&apc("a=T,t=f,i=4", &payload));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EBADF));
}

#[test]
fn oversized_header_is_enomem() {
    // A PNG side above `MAX_IMAGE_SIDE`.
    let (cmd, rej) = once(&apc("f=100", &b64(&png_blob(10_001, 1))));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOMEM));

    // Sides within `MAX_IMAGE_SIDE` whose RGBA size passes `MAX_IMAGE_BYTES`.
    let (cmd, rej) = once(&apc("f=100", &b64(&png_blob(4096, 4097))));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOMEM));

    // Raw formats are checked against the same limits from their `s`/`v` keys.
    let (cmd, rej) = once(&apc("f=32,s=10001,v=1", &b64(&[0u8; 3])));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOMEM));
    let (cmd, rej) = once(&apc("f=24,s=5000,v=5000", b""));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOMEM));

    // A 1x1 PNG is within both: accepted.
    let (cmd, _) = once(&apc("f=100", &b64(&png_blob(1, 1))));
    assert!(matches!(
        cmd,
        Some(KittyCommand::Transmit { format: KittyFormat::Png { width: 1, height: 1 }, .. })
    ));
}

#[test]
fn endless_transmission_is_dropped_at_cap() {
    let now = Instant::now();
    let mut parser = KittyParser::new();

    // The opening chunk of a raw transmission that will never end.
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1,i=1", b"AQID"), now), None);

    let chunk = vec![b'A'; 4096]; // 3072 decoded bytes per frame
    let mut dropped = false;
    for _ in 0..8000 {
        let cmd = parse(&mut parser, &apc("m=1", &chunk), now);
        assert_eq!(cmd, None);
        assert!(
            parser.held_bytes() <= MAX_PAYLOAD_BYTES + chunk.len(),
            "cap exceeded while growing"
        );
        if let Some(reject) = parser.take_reject() {
            assert_eq!(reject.error, KittyError::ENOMEM);
            assert_eq!(reject.id, 1);
            dropped = true;
            break;
        }
    }
    assert!(dropped, "the endless transmission was never dropped");

    // The drop releases what the transmission held.
    assert_eq!(parser.held_bytes(), 0);

    // And the next APC parses normally.
    let cmd = parse(&mut parser, &apc("a=q,i=21", b"AAAA"), now);
    assert_eq!(cmd, Some(KittyCommand::Query { id: 21, quiet: 0 }));
}

#[test]
fn stalled_transmission_is_dropped() {
    let t0 = Instant::now();
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=32,s=1,v=1,m=1,i=6,q=1", b"AAAA"), t0), None);

    // Within the stall window the transmission is kept.
    assert_eq!(parser.end(t0 + Duration::from_secs(9)), None);
    assert!(parser.take_reject().is_none());
    assert!(parser.held_bytes() > 0);

    // Past it, `end` drops it with ENOMEM and holds nothing.
    assert_eq!(parser.end(t0 + Duration::from_secs(11)), None);
    let reject = parser.take_reject().expect("the stall dropped the transmission");
    assert_eq!(reject.error, KittyError::ENOMEM);
    assert_eq!(reject.id, 6);
    assert_eq!(reject.quiet, 1);
    assert_eq!(parser.held_bytes(), 0);

    // The next APC parses normally.
    let cmd = parse(&mut parser, &apc("a=q,i=3", b"AAAA"), t0 + Duration::from_secs(12));
    assert_eq!(cmd, Some(KittyCommand::Query { id: 3, quiet: 0 }));

    // A chunk that arrives late is progress: it is not dropped on arrival.
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"AQID"), t0), None);
    assert_eq!(parse(&mut parser, &apc("m=1", b"AQID"), t0 + Duration::from_secs(30)), None);
    assert!(parser.take_reject().is_none());
}

#[test]
fn reply_quiet_levels() {
    assert_eq!(reply(5, None, Some(2), Ok(()), 0), Some(b"\x1b_Gi=5,p=2;OK\x1b\\".to_vec()));
    // q=1 suppresses OK responses.
    assert_eq!(reply(5, None, Some(2), Ok(()), 1), None);
    // ...but not failure responses.
    assert_eq!(
        reply(5, None, None, Err(KittyError::ENOENT), 1),
        Some(b"\x1b_Gi=5;ENOENT:image not found\x1b\\".to_vec())
    );
    // q=2 suppresses everything.
    assert_eq!(reply(5, None, Some(2), Ok(()), 2), None);
    assert_eq!(reply(5, None, None, Err(KittyError::ENOMEM), 2), None);
    // Without an id and without a number nothing is echoed; only the message travels.
    assert_eq!(reply(0, None, Some(3), Ok(()), 0), Some(b"\x1b_G;OK\x1b\\".to_vec()));
    assert_eq!(
        reply(9, None, None, Err(KittyError::EBADPNG), 0),
        Some(b"\x1b_Gi=9;EBADPNG:malformed PNG data\x1b\\".to_vec())
    );

    // An image addressed by number is answered with the id the terminal assigned and the number
    // echoed beside it (the protocol's example answers `I=13` with `i=99,I=13`).
    assert_eq!(reply(99, Some(13), None, Ok(()), 0), Some(b"\x1b_Gi=99,I=13;OK\x1b\\".to_vec()));
    // A failure that has no id yet still echoes the number it was addressed by.
    assert_eq!(
        reply(0, Some(13), None, Err(KittyError::EINVAL), 0),
        Some(b"\x1b_GI=13;EINVAL:invalid control data\x1b\\".to_vec())
    );

    // The rejection channel carries the number for exactly this answer.
    let (cmd, rej) = once(&apc("f=24,I=13,s=1,v=1", &b64(&[1, 2]))); // two of three bytes
    assert_eq!(cmd, None);
    let rej = rej.expect("short pixel data is rejected");
    assert_eq!((rej.id, rej.number), (0, Some(13)));
    assert_eq!(
        reply(rej.id, rej.number, None, Err(rej.error), rej.quiet),
        Some(b"\x1b_GI=13;EINVAL:invalid control data\x1b\\".to_vec())
    );
}

#[test]
fn garbage_keys_are_einval() {
    let cases = [
        "b=1",  // a key the protocol does not define
        "N=1",  // defined by the protocol but not supported here
        "S=10", // ...like the file-read keys and the feature-query key
        "e=T",
        "P=1,Q=1,H=1,V=1,F=1,O=1",
        "a=x",          // an action the protocol does not define
        "t=q",          // an invalid transmission medium
        "f=33",         // a data format the protocol does not define
        "o=q",          // only z is defined
        "m=2",          // m is zero or one
        "q=3",          // q is 0, 1 or 2
        "i=-1",         // i is an unsigned integer
        "i=4294967296", // and 32-bit
        "z=x",          // z is a signed integer
        "U=x",          // U is an integer
        "d=",           // an empty selector
        "abc",          // a pair without a value
        "=",            // a pair without a key
        "i=1,I=2",      // specifying both i and I is an error
        "a=p,i=",       // an empty value for a numeric key
    ];
    let now = Instant::now();
    for control in cases {
        let (cmd, rej) = once(&apc(control, b"AAAA"));
        assert_eq!(cmd, None, "{control} produced a command");
        let reject = rej.unwrap_or_else(|| panic!("{control} was not rejected"));
        assert_eq!(reject.error, KittyError::EINVAL, "{control} got the wrong code");
    }

    // A garbage frame does not disturb a chunked transmission in progress: its chunks resume it.
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=2,m=1", b"AQID"), now), None);
    assert_eq!(parse(&mut parser, &apc("k=9,m=1", b"AAAA"), now), None);
    assert_eq!(parser.take_reject().map(|r| r.error), Some(KittyError::EINVAL));
    let cmd = parse(&mut parser, &apc("m=0", b"AQID"), now);
    let Some(KittyCommand::Transmit { bytes, .. }) = cmd else {
        panic!("the transmission did not resume: {cmd:?}");
    };
    assert_eq!(bytes, vec![1, 2, 3, 1, 2, 3]); // the garbage chunk's payload was dropped

    // The reject slot is taken once.
    let mut parser = KittyParser::new();
    parse(&mut parser, &apc("a=x", b""), now);
    assert!(parser.take_reject().is_some());
    assert!(parser.take_reject().is_none());
}

#[test]
fn non_graphics_apc_is_ignored() {
    // APC content that does not start with `G` is not a graphics sequence: no command, no reply.
    let (cmd, rej) = once(b"xGi=1");
    assert_eq!(cmd, None);
    assert!(rej.is_none());
    let (cmd, rej) = once(b""); // an empty APC
    assert_eq!(cmd, None);
    assert!(rej.is_none());

    // A `G` frame whose control data is garbage after some keys still knows its id and quiet.
    let mut parser = KittyParser::new();
    parser.start();
    parser.put(b"Ga=q,i=42,q=1,bad key");
    assert_eq!(parser.end(Instant::now()), None);
    let reject = parser.take_reject().expect("the garbage was rejected");
    assert_eq!(reject.error, KittyError::EINVAL);
    // The keys parsed before the bad pair are kept, so the error can be answered for this image.
    assert_eq!(reject.id, 42);
    assert_eq!(reject.quiet, 1);
}

#[test]
fn png_format_rules() {
    let now = Instant::now();

    // Without `f`, PNG data is recognized by its magic and sized from its header.
    let png = png_blob(2, 3);
    let (cmd, rej) = once(&apc("", &b64(&png)));
    assert!(rej.is_none());
    assert_eq!(
        cmd,
        Some(KittyCommand::Transmit {
            id: None,
            number: None,
            format: KittyFormat::Png { width: 2, height: 3 },
            bytes: png.clone(),
            then_place: None,
            quiet: 0,
        })
    );

    // `f=100` requires real PNG data.
    let (cmd, rej) = once(&apc("f=100", &b64(b"not a png")));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EBADPNG));

    // Without `f`, non-PNG data is RGBA and needs its dimensions.
    let (cmd, rej) = once(&apc("", &b64(b"hello"))); // 5 bytes: not magic, no s/v
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // Raw payloads must be exactly width x height x bytes-per-pixel.
    let (cmd, rej) = once(&apc("f=32,s=1,v=1", &b64(b"123"))); // 3 of 4 bytes
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));
    let (cmd, rej) = once(&apc("f=24,s=1,v=1", &b64(b"1234"))); // 4 of 3 bytes
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // Indexed color formats are not supported here.
    for f in ["8", "16"] {
        let (cmd, rej) = once(&apc(&format!("f={f},s=1,v=1"), &b64(b"\x00")));
        assert_eq!(cmd, None, "f={f} transmitted anyway");
        assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOTSUPPORTED), "f={f} code");
    }

    // Animation actions are not supported here either.
    for control in ["a=f,i=1", "a=a,i=1", "a=c,i=1"] {
        let (cmd, rej) = once(&apc(control, b""));
        assert_eq!(cmd, None, "{control} produced a command");
        assert_eq!(rej.map(|r| r.error), Some(KittyError::ENOTSUPPORTED), "{control} code");
    }

    // A raw transmit missing its dimensions is EINVAL; zero dimensions too.
    let (cmd, rej) = once(&apc("f=32", &b64(b"1234")));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));
    let (cmd, rej) = once(&apc("f=32,s=0,v=1", &b64(b"1234")));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // A query without an id is EINVAL; i=0 counts as no id.
    let (cmd, rej) = once(&apc("a=q", b"AAAA"));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));
    let (cmd, rej) = once(&apc("a=q,i=0", b"AAAA"));
    assert_eq!(cmd, None);
    assert_eq!(rej.map(|r| r.error), Some(KittyError::EINVAL));

    // An empty transmission carries no image: nothing to do, no reply.
    let (cmd, rej) = once(&apc("f=24,s=1,v=1", b""));
    assert_eq!(cmd, None);
    assert!(rej.is_none());

    // A malformed base64 tail rejects the transmission: one leftover character cannot decode.
    let mut parser = KittyParser::new();
    parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"A"), now); // 1 char: an incomplete quad
    let cmd = parse(&mut parser, &apc("m=0", b"AAAA"), now); // completes AAAA, leaves A
    assert_eq!(cmd, None);
    assert_eq!(parser.take_reject().map(|r| r.error), Some(KittyError::EINVAL));
}

#[test]
fn delete_aborts_partial_upload() {
    let now = Instant::now();
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"AQID"), now), None);
    assert!(parser.held_bytes() > 0);

    // Any delete command aborts an incomplete chunked upload (protocol rule).
    let cmd = parse(&mut parser, &apc("a=d,d=i,i=2", b""), now);
    assert_eq!(
        cmd,
        Some(KittyCommand::Delete(KittyDelete {
            target: DeleteTarget::ImageId { image: Some(2), placement: None },
            free_data: false,
            quiet: 0,
        }))
    );
    assert_eq!(parser.held_bytes(), 0);
    assert!(parser.take_reject().is_none());

    // A CAN/SUB (`abort`) discards the frame in progress and keeps what earlier chunks stored.
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=2,m=1", b"AQID"), now), None);
    parser.start();
    parser.put(b"Gm=1;AAAA"); // half of another chunk...
    parser.abort();
    let cmd = parse(&mut parser, &apc("m=0", b"AQID"), now);
    let Some(KittyCommand::Transmit { bytes, .. }) = cmd else {
        panic!("the transmission did not finish: {cmd:?}");
    };
    assert_eq!(bytes, vec![1, 2, 3, 1, 2, 3]); // only the two completed chunks count

    // A new transmission replaces an unfinished one without a reply of its own.
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"AQID"), now), None);
    let cmd = parse(&mut parser, &apc("f=32,s=1,v=1", &b64(b"abcd")), now);
    assert!(matches!(cmd, Some(KittyCommand::Transmit { .. })));
    assert!(parser.take_reject().is_none());
}

#[test]
fn query_reports_refused_media() {
    // A client chooses its transfer medium by querying it. Answering OK for a medium a real
    // transmission would refuse would send every later image down a path that always fails.
    for control in ["a=q,i=31,t=f", "a=q,i=31,t=t", "a=q,i=31,t=s"] {
        let (cmd, rej) = once(&apc(control, b""));
        assert_eq!(cmd, None, "{control} must not be answered OK");
        let rej = rej.expect("{control} is refused");
        assert_eq!((rej.error, rej.id), (KittyError::EBADF, 31));
    }

    // Everything else a transmission is checked for applies to its query too.
    let (_, rej) = once(&apc("a=q,i=31,f=7", b""));
    assert_eq!(rej.expect("f=7 is not a format").error, KittyError::EINVAL);
    let (_, rej) = once(&apc("a=q,i=31,f=24", b""));
    assert_eq!(rej.expect("raw data needs s and v").error, KittyError::EINVAL);
    let (_, rej) = once(&apc("a=q,i=31,t=d,f=24,s=20000,v=1", b""));
    assert_eq!(rej.expect("the side passes the limit").error, KittyError::ENOMEM);

    // What this terminal can do is answered: direct data, with the id and quiet level echoed.
    let (cmd, rej) = once(&apc("a=q,i=31,t=d,f=24,s=1,v=1,q=1", &b64(&[1, 2, 3])));
    assert_eq!(cmd, Some(KittyCommand::Query { id: 31, quiet: 1 }));
    assert!(rej.is_none());
    let (cmd, _) = once(&apc("a=q,i=31", b"")); // a bare support probe carries no parameters
    assert_eq!(cmd, Some(KittyCommand::Query { id: 31, quiet: 0 }));
}

#[test]
fn tick_drops_stalled_transmission() {
    let t0 = Instant::now();
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=32,s=1,v=1,m=1,i=6,q=1", b"AAAA"), t0), None);

    // Nothing arrives on the wire; a tick inside the window changes nothing.
    parser.tick(t0 + Duration::from_secs(9));
    assert!(parser.take_reject().is_none());
    assert!(parser.held_bytes() > 0);

    // Past STALL the upload is released even though no APC sequence came in at all.
    parser.tick(t0 + Duration::from_secs(11));
    let rej = parser.take_reject().expect("tick dropped the stalled upload");
    assert_eq!((rej.error, rej.id, rej.quiet), (KittyError::ENOMEM, 6, 1));
    assert_eq!(parser.held_bytes(), 0);

    // The next sequence parses normally.
    let cmd = parse(&mut parser, &apc("a=q,i=3", b""), t0 + Duration::from_secs(12));
    assert_eq!(cmd, Some(KittyCommand::Query { id: 3, quiet: 0 }));

    // A chunk that arrived moved the clock: ticking soon after it keeps the upload alive.
    let late = t0 + Duration::from_secs(20);
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1", b"AQID"), late), None);
    parser.tick(late + Duration::from_secs(5));
    assert!(parser.take_reject().is_none());
    assert!(parser.held_bytes() > 0);

    // An open frame counts as progress only while bytes arrive into it. The first check after a
    // `put` sees new staged content and keeps the upload; a frame that has received nothing since
    // the previous check does not exempt a stalled transmission from being dropped.
    parser.start();
    parser.put(b"Ga=q,i=4");
    parser.tick(late + Duration::from_secs(25));
    assert!(parser.take_reject().is_none(), "a frame that just received bytes is progress");
    assert!(parser.held_bytes() > 0, "the upload is still held");

    // Nothing arrived since: the stalled upload is released even under the open frame.
    parser.tick(late + Duration::from_secs(30));
    let rej = parser.take_reject().expect("the stalled upload was dropped under the open frame");
    assert_eq!((rej.error, rej.id, rej.quiet), (KittyError::ENOMEM, 0, 0));

    // `tick` never disturbs the frame itself; its `end` still answers it.
    assert_eq!(
        parser.end(late + Duration::from_secs(31)),
        Some(KittyCommand::Query { id: 4, quiet: 0 })
    );
    assert!(parser.take_reject().is_none(), "the drop was already reported");
    assert_eq!(parser.held_bytes(), 0);

    // A client streaming a slow chunk is never dropped: every `put` moves the progress marker.
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=1,m=1,i=8", b"AQID"), late), None);
    parser.start();
    for t in 35..45 {
        parser.put(b"AAAA");
        parser.tick(late + Duration::from_secs(t));
        assert!(parser.take_reject().is_none(), "arriving bytes keep the upload alive");
    }
    // And once its puts stop, it goes on the next check past the stall window.
    parser.tick(late + Duration::from_secs(60));
    let rej = parser.take_reject().expect("a quiet frame past STALL drops the upload");
    assert_eq!((rej.error, rej.id), (KittyError::ENOMEM, 8));
}

#[test]
fn a_second_start_abandons_the_unfinished_frame() {
    let now = Instant::now();
    let mut parser = KittyParser::new();
    parser.start();
    parser.put(&apc("f=24,s=1,v=1,m=1", b"AQID")); // staged, but its `end` never came
    assert!(parser.held_bytes() > 0);

    // Starting again drops that frame silently and sets up the new one.
    parser.start();
    assert_eq!(parser.take_reject(), None); // abandoning a frame is not an error
    assert_eq!(parser.held_bytes(), 0); // its staged payload is gone
    parser.put(b"Ga=q,i=3");
    assert_eq!(parser.end(now), Some(KittyCommand::Query { id: 3, quiet: 0 }));

    // Chunks completed by earlier sequences are not "the frame in progress": they survive.
    let mut parser = KittyParser::new();
    assert_eq!(parse(&mut parser, &apc("f=24,s=1,v=2,m=1", b"AQID"), now), None);
    parser.start();
    parser.put(b"Gm=1;AAAA"); // an unfinished second chunk...
    parser.start(); // ...abandoned by a new sequence instead of CAN/SUB
    let cmd = parse(&mut parser, &apc("m=0", b"AQID"), now);
    let Some(KittyCommand::Transmit { bytes, .. }) = cmd else {
        panic!("the transmission did not finish: {cmd:?}");
    };
    assert_eq!(bytes, vec![1, 2, 3, 1, 2, 3]); // chunk one, the abandoned frame skipped, chunk three
}

#[test]
fn bytes_after_a_finished_zlib_stream_are_dropped() {
    // A transmission whose zlib stream ends early: whatever decodes after the stream end is not
    // image data, so it is dropped rather than appended to the pixels. The fixture uses unpadded
    // base64 on purpose: a padded encoding closes the payload at its `=`, and the one to three
    // trailing characters that only `finish` drains — the ones whose decoded bytes land after the
    // stream end, which is exactly what this test is about — would never reach it.
    let mut payload = zlib(&[1, 2, 3]);
    payload.extend_from_slice(b"trailing garbage past the end of the stream!");
    assert_ne!(payload.len() % 3, 0, "the fixture must leave a partial final base64 group");
    let (cmd, rej) = once(&apc("f=24,s=1,v=1,o=z", &STANDARD_NO_PAD.encode(&payload).into_bytes()));
    assert!(rej.is_none(), "trailing bytes must not fail the transmission: {rej:?}");
    let Some(KittyCommand::Transmit { bytes, format, .. }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(bytes, vec![1, 2, 3]); // the inflate output only
    assert_eq!(format, KittyFormat::Rgb { width: 1, height: 1 });
}

#[test]
fn compressed_raw_data_longer_than_declared_is_einval() {
    // The declared size is the inflate ceiling for a raw format: over-long compressed pixel data
    // is EINVAL, and the excess is never inflated at all.
    let (cmd, rej) = once(&apc("f=24,s=1,v=1,o=z", &b64(&zlib(&[1, 2, 3, 4, 5, 6]))));
    assert_eq!(cmd, None);
    assert_eq!(rej.expect("data longer than declared").error, KittyError::EINVAL);

    // The declared amount inflates and is accepted.
    let (cmd, rej) = once(&apc("f=24,s=1,v=1,o=z", &b64(&zlib(&[1, 2, 3]))));
    assert!(rej.is_none());
    assert!(matches!(cmd, Some(KittyCommand::Transmit { .. })));

    // Short data is still EINVAL: the pixel buffer it declared is incomplete.
    let (cmd, rej) = once(&apc("f=24,s=1,v=1,o=z", &b64(&zlib(&[1]))));
    assert_eq!(cmd, None);
    assert_eq!(rej.expect("short data").error, KittyError::EINVAL);
}

#[test]
fn compressed_output_is_capped_at_the_image_limit_not_the_payload_cap() {
    // 18 MB of RGB24 inflates from a payload far under the 16 MiB that caps transmitted bytes:
    // the inflate is bounded by the image limit (64 MiB), so this transmission is accepted.
    let (w, h) = (3000u32, 2000u32); // 18 MB of pixel data
    let raw = vec![7u8; w as usize * h as usize * 3];
    assert!(raw.len() > MAX_PAYLOAD_BYTES, "the fixture must exceed the transmitted-data cap");
    let payload = zlib(&raw);
    let (cmd, rej) = once(&apc(&format!("f=24,s={w},v={h},o=z"), &b64(&payload)));
    assert!(rej.is_none(), "{rej:?}");
    let Some(KittyCommand::Transmit { bytes, format, .. }) = cmd else {
        panic!("expected a transmit command, got {cmd:?}");
    };
    assert_eq!(format, KittyFormat::Rgb { width: w, height: h });
    assert_eq!(bytes.len(), raw.len());
}
